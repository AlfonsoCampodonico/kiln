//! A pod's volumes (kiln-shim exec and volumes design §3.1). Each files volume is a
//! detached tmpfs that only init can reach (through its descriptor), filled in kubelet's
//! atomic-writer layout; the pod disk is a detached ext4. After the pivot, every volume
//! is attached at its path inside the container's root: a symlink in the image then
//! resolves inside that root, never into init's. Files volumes are attached read-only,
//! and init keeps writing their updates through its own detached mount.

use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};

use kiln_proto::{Config, FileUpdate, MAX_VOLUME_BYTES, MAX_VOLUME_FILES, VolumeKind};
use rustix::fs::{CWD, Mode, OFlags, SeekFrom};
use rustix::mount::{
    FsMountFlags, FsOpenFlags, MountAttrFlags, MountFlags, MoveMountFlags, OpenTreeFlags, fsconfig_create,
    fsconfig_set_string, fsmount, fsopen, mount, mount_bind, mount_remount, move_mount, open_tree,
};

use super::sys;
use crate::atomic::Writer;
use crate::disks::layer_device;
use crate::error::{Context, Failure, Result};

/// The volumes' detached mounts, by index in `Config.volumes`.
pub struct Volumes {
    files: Vec<Option<(OwnedFd, Writer)>>,
    /// The pod disk's mount (to clone) and its root opened as a directory (for ioctls
    /// and syncfs, which a mount's path descriptor does not take).
    pod: Option<(OwnedFd, OwnedFd)>,
}

/// The path through which init reaches a detached mount.
fn fd_path(fd: &OwnedFd) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", fd.as_raw_fd()))
}

/// A detached filesystem: `fstype` with `options`, mounted nosuid and nodev.
fn detached(fstype: &str, options: &[(&str, &str)]) -> Result<OwnedFd> {
    let fs = fsopen(fstype, FsOpenFlags::FSOPEN_CLOEXEC).context(format!("fsopen {fstype}"))?;
    for (k, v) in options {
        fsconfig_set_string(&fs, *k, *v).context(format!("{fstype} option {k}={v}"))?;
    }
    fsconfig_create(&fs).context(format!("create the {fstype} filesystem"))?;
    fsmount(
        &fs,
        FsMountFlags::FSMOUNT_CLOEXEC,
        MountAttrFlags::MOUNT_ATTR_NOSUID | MountAttrFlags::MOUNT_ATTR_NODEV,
    )
    .context(format!("fsmount {fstype}"))
}

impl Volumes {
    /// Stage 3: a tmpfs per files volume, with its first generation (`initial`, which
    /// the host sent right after `Config`); the pod disk, grown to its size.
    pub fn prepare(config: &Config, initial: &[FileUpdate]) -> Result<Volumes> {
        let mut files = Vec::new();
        for v in &config.volumes {
            files.push(match v.kind {
                VolumeKind::Files { .. } => {
                    // Room for two generations (the old one stays until the new one is in),
                    // each of its most bytes and files, every file rounded up to whole pages.
                    let one = MAX_VOLUME_BYTES + MAX_VOLUME_FILES as u64 * 4096;
                    let size = format!("{}", 2 * one + (1 << 20));
                    let fd = detached("tmpfs", &[("size", &size), ("mode", "0755")])?;
                    let writer = Writer::new(fd_path(&fd));
                    Some((fd, writer))
                }
                _ => None,
            });
        }
        let mut vols = Volumes { files, pod: None };
        for u in initial {
            vols.update(u)?;
        }
        if let Some(disk) = &config.pod_disk {
            let dev = layer_device(config.layers as usize);
            let fd = detached("ext4", &[("source", &dev)])?;
            let blockdev = rustix::fs::open(dev.as_str(), OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
                .context(format!("open {dev}"))?;
            let size = rustix::fs::seek(&blockdev, SeekFrom::End(0)).context(format!("size of {dev}"))?;
            if size < disk.size_bytes {
                return Err(Failure::msg(format!(
                    "{dev} has {size} bytes, but the pod disk should have {}",
                    disk.size_bytes
                )));
            }
            // The mount's descriptor is a path one (fsmount): ioctls and syncfs need its
            // root opened as a directory, which also keeps the detached mount alive.
            let root = rustix::fs::openat(
                &fd,
                ".",
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .context("open the pod disk")?;
            sys::ext4_resize(&root, disk.size_bytes / 4096).context("grow the pod disk")?;
            vols.pod = Some((fd, root));
        }
        Ok(vols)
    }

    /// Applies a `FileUpdate` (a host message: its volume must be a files volume).
    pub fn update(&mut self, u: &FileUpdate) -> Result<()> {
        let Some(Some((_, w))) = self.files.get_mut(u.volume as usize) else {
            return Err(Failure::msg(format!(
                "protocol violation: FileUpdate for volume {}, which is not a files volume",
                u.volume
            )));
        };
        w.apply(u).map(drop)
    }

    /// Stage 4, after the pivot: attaches every volume at its path. A whole detached
    /// mount is cloned straight onto its path; a part of one (an `emptyDir` on the pod
    /// disk, a single file) is bound from the mount attached at a staging directory,
    /// since the kernel clones only a detached mount's root. The staging mounts are
    /// detached again, and their directory removed, before anything else runs.
    pub fn attach(&self, config: &Config) -> Result<()> {
        let mut staging = Staging::default();
        let attached = self.attach_all(config, &mut staging);
        staging.remove();
        attached
    }

    fn attach_all(&self, config: &Config, staging: &mut Staging) -> Result<()> {
        for (n, v) in config.volumes.iter().enumerate() {
            let dest = Path::new(&v.path);
            match &v.kind {
                VolumeKind::Files { file } => {
                    let (fd, w) = self.files[n].as_ref().expect("a files volume has its mount");
                    if *file {
                        touch(dest)?;
                        let root = staging.attach(fd, &format!("files-{n}"))?;
                        let source = w.single_file()?;
                        let rel = source.strip_prefix(w.base()).unwrap_or(&source);
                        mount_bind(root.join(rel), dest).context(format!("mount {}", v.path))?;
                    } else {
                        mkdir_all(dest)?;
                        let clone = open_tree(fd, "", clone_flags() | OpenTreeFlags::AT_EMPTY_PATH)
                            .context(format!("clone the volume for {}", v.path))?;
                        place(&clone, dest)?;
                    }
                    mount_remount(
                        dest,
                        MountFlags::BIND | MountFlags::RDONLY | MountFlags::NOSUID | MountFlags::NODEV,
                        "",
                    )
                    .context(format!("make {} read-only", v.path))?;
                }
                VolumeKind::EmptyDir { name } => {
                    let pod = self
                        .pod
                        .as_ref()
                        .ok_or_else(|| Failure::msg("an emptyDir without the pod disk"))?;
                    let (mount, root) = pod;
                    let dir = fd_path(root).join(name);
                    match std::fs::create_dir(&dir) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                        Err(e) => return Err(Failure::os(format!("create the emptyDir {name}"), &e)),
                    }
                    // kubelet makes emptyDirs world-writable, so any user of the pod can use them.
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777))
                        .context(format!("chmod the emptyDir {name}"))?;
                    mkdir_all(dest)?;
                    let root = staging.attach(mount, "pod")?;
                    mount_bind(root.join(name), dest).context(format!("mount the emptyDir {name}"))?;
                }
                VolumeKind::Tmpfs { size_bytes } => {
                    mkdir_all(dest)?;
                    let data = match size_bytes {
                        Some(n) => format!("mode=0777,size={n}"),
                        None => "mode=0777".into(),
                    };
                    let data = std::ffi::CString::new(data).expect("no NUL");
                    mount(
                        "tmpfs",
                        dest,
                        "tmpfs",
                        MountFlags::NOSUID | MountFlags::NODEV,
                        data.as_c_str(),
                    )
                    .context(format!("mount a tmpfs on {}", v.path))?;
                }
            }
        }
        Ok(())
    }

    /// Before `Exited`: the pod disk synced and made read-only, like the scratch disk,
    /// so the next container's VM finds it clean.
    pub fn finish(&self) {
        if let Some((_, root)) = &self.pod {
            super::storage::finish(root);
        }
    }
}

/// Where detached mounts are attached while parts of them are bound onto volume paths.
const STAGING: &str = "/.kiln-volumes";

/// The detached mounts attached at [`STAGING`], by name.
#[derive(Default)]
struct Staging {
    attached: Vec<PathBuf>,
}

impl Staging {
    /// `fd`'s mount attached at `STAGING/<name>` (once): its path.
    fn attach(&mut self, fd: &OwnedFd, name: &str) -> Result<PathBuf> {
        let at = Path::new(STAGING).join(name);
        if self.attached.contains(&at) {
            return Ok(at);
        }
        if self.attached.is_empty() {
            std::fs::create_dir_all(STAGING).context(format!("create {STAGING}"))?;
        }
        std::fs::create_dir_all(&at).context(format!("create {}", at.display()))?;
        let clone = open_tree(fd, "", clone_flags() | OpenTreeFlags::AT_EMPTY_PATH).context("clone a volume")?;
        place(&clone, &at)?;
        self.attached.push(at.clone());
        Ok(at)
    }

    /// Detaches the staging mounts (the binds made from them stay) and removes their
    /// directory, so the container's root keeps no trace of them.
    fn remove(self) {
        for at in &self.attached {
            let _ = rustix::mount::unmount(at.as_path(), rustix::mount::UnmountFlags::DETACH);
        }
        if !self.attached.is_empty() {
            let _ = std::fs::remove_dir_all(STAGING);
        }
    }
}

fn clone_flags() -> OpenTreeFlags {
    OpenTreeFlags::OPEN_TREE_CLONE | OpenTreeFlags::OPEN_TREE_CLOEXEC
}

fn place(clone: &OwnedFd, dest: &Path) -> Result<()> {
    move_mount(clone, "", CWD, dest, MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH)
        .context(format!("mount on {}", dest.display()))
}

fn mkdir_all(dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest).context(format!("create {}", dest.display()))
}

/// A file to mount a single file onto: created empty when missing.
fn touch(dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        mkdir_all(parent)?;
    }
    if !dest.exists() {
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(dest)
            .context(format!("create {}", dest.display()))?;
    }
    Ok(())
}
