//! Build mode (protocol version 3, spec §9.8): kiln-init runs the host's
//! `RunStep`s one at a time and seals each successful step's changes.
//!
//! Disks: `vda` init, `vdb` scratch, `vdc` the output disk, then `Config.layers`
//! base layers (bottom first), then `Config.disks` (erofs layers a step may stack,
//! ext4 cache disks a step may mount). Nothing is hot-plugged.
//!
//! Init never pivots: it keeps its own root, and each step gets a fresh overlay at
//! `/kiln/root` (the lowers so far, a new upper and work directory on scratch;
//! `nodev`), with `/proc` (a clone of init's, but `/proc/sys`, `/proc/sysrq-trigger`,
//! `/proc/irq`, `/proc/bus` and `/proc/fs` are bound read-only, so a root step without
//! `CAP_SYS_ADMIN` cannot set `kernel.core_pattern` or `kernel.modprobe`, whose
//! helpers the kernel runs as root in init's namespace; `/proc/kcore`, `keys`,
//! `timer_list`, `sched_debug`, `acpi`, `scsi`, `latency_stats` and `timer_stats`
//! are masked when present), a read-only `/sys`, a minimal `/dev` (read-only, no block
//! devices: `null`, `zero`, `full`, `random`, `urandom`, `tty`, `ptmx` and the
//! `fd`/`stdin`/`stdout`/`stderr` links), `/dev/pts`, `/dev/shm`, its cache
//! disks, the `/etc` files and its secrets mounted in it. Every mount point that had to be
//! created is a "stub", removed again after the step, so no stub is in its diff.
//! The directories overlayfs copied up to make a stub (the `etc/` of an image
//! with no `/etc/resolv.conf`, the `root/` of a cache mount at `/root/.cache`)
//! stay, as plain directory entries, in the diff of every step that needed them:
//! a bind mount needs an existing target, so a stub is made, never skipped.
//! Secrets and the `/etc` files live on a detached tmpfs and are bind-mounted
//! onto their targets, so they are never written to the upper. Paths inside the
//! root are resolved with `RESOLVE_IN_ROOT`, so the image's symlinks cannot lead
//! a mount out of it. The step runs in its own mount namespace, pivoted into the
//! overlay with init's root detached, with Docker's default capability bounding
//! set less `CAP_MKNOD` (no `CAP_SYS_ADMIN`, so it cannot mount; no
//! `CAP_MKNOD`, so it cannot make a device node; no `CAP_SYS_PTRACE`, so it cannot
//! reach init's root through `/proc/1/root`), as its uid and gid, in its own
//! session and process group, in its working directory (created as root when
//! missing; that is part of the step's diff, as Docker's WORKDIR). So a step,
//! root or not, cannot open the output disk or a layer disk (ruling 1). Before
//! `Running`, init also sets `/proc/sys/user/max_user_namespaces` to 0, so no step
//! can create a user namespace (and get `CAP_SYS_ADMIN` inside it).
//!
//! A step that cannot be started because of itself (a cache or secret target, or
//! an `/etc` file's, that cannot be resolved or created in its root; its working
//! directory, its ids or its command) ends with `StepExited` code 125, 126 or 127
//! and kiln-init's reason on its stderr stream; the VM goes on. A message that
//! breaks the protocol, or a failure of init's own work (cloning `/proc`,
//! hardening it, creating sysfs, devpts or a tmpfs, mounting or growing a cache
//! disk, moving a mount), ends the VM.
//!
//! When the step's process exits, every process but init is killed (stragglers,
//! and anything that left the group), its output is drained, and `StepExited`
//! is sent. Then the step's mounts are detached, the stubs removed and the overlay
//! unmounted. After exit 0 the upper is sealed onto the output disk
//! ([`super::seal`]), `StepSealed` is sent, and the upper becomes the topmost
//! lower; otherwise it is discarded and the build continues from the state before
//! the step (the host decides whether it does).

use std::collections::BTreeSet;
use std::ffi::CString;
use std::fs::File;
use std::io::Write;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, Sender};

use kiln_proto::{
    Config, DiskKind, GuestMessage, HostMessage, MAX_BUILD_LOWERS, RunStep, StepExited, StepSealed, port,
};
use rustix::fs::{AtFlags, FileType, Mode, OFlags, ResolveFlags};
use rustix::io::Errno;
use rustix::mount::{
    FsMountFlags, FsOpenFlags, FsPickFlags, MountAttrFlags, MountFlags, MoveMountFlags, OpenTreeFlags, UnmountFlags,
};
use rustix::pipe::PipeFlags;
use rustix::process::{Gid, Pid, Uid};
use rustix::thread::CapabilitySet;
use signal_hook::consts::SIGCHLD;

use super::process::{connect, give, pipe, watch_signals};
use super::seal::Output;
use super::storage::{self, mkdir};
use super::{Event, enter, host_ok, identity, relay, send, sys};
use crate::disks::device;
use crate::error::{Context, Failure, Result};
use crate::passwd::Identity;

const ROOT: &str = "/kiln/root";

/// Stages 3 to 6, `Running`, then steps until `Finish`.
pub fn run(config: &Config, events: Sender<Event>, rx: &Receiver<Event>) -> Result<()> {
    enter(3)?;
    let rw = storage::mount_base(config, Config::OUTPUT_DISK as usize + 1)?;
    for dir in ["/kiln/rw/u", "/kiln/rw/w", "/kiln/disks"] {
        mkdir(dir)?;
    }
    enter(4)?;
    // Init's own descriptor: no step ever sees a block device.
    let output = Output::open(&device(Config::OUTPUT_DISK as usize))?;
    enter(5)?;
    identity::network(config)?;
    enter(6)?;
    disable_user_namespaces()?;
    watch_signals(events)?;
    // One descriptor per directory level while sealing.
    if let Some(max) = rustix::process::getrlimit(rustix::process::Resource::Nofile).maximum {
        let _ = rustix::process::setrlimit(
            rustix::process::Resource::Nofile,
            rustix::process::Rlimit {
                current: Some(max),
                maximum: Some(max),
            },
        );
    }
    host_ok()?;
    send(&GuestMessage::Running)?;
    let mut b = Builder {
        config,
        lowers: (0..config.layers as usize).map(crate::overlay::layer_dir).collect(),
        stacked: BTreeSet::new(),
        grown: BTreeSet::new(),
        output,
        epoch: config.source_date_epoch,
    };
    loop {
        match rx.recv().expect("the signal thread holds a sender") {
            Event::Host(HostMessage::RunStep(step)) => b.step(&step, rx)?,
            Event::Host(HostMessage::Finish) => break,
            // Nothing runs between steps.
            Event::Host(HostMessage::Signal(_) | HostMessage::WindowSize(_)) => {}
            Event::Host(HostMessage::Shutdown(_)) => break,
            Event::Host(HostMessage::Config(_)) => return Err(Failure::msg("protocol violation: a second Config")),
            // kiln-shim's messages: a build VM has no execs and no volumes.
            Event::Host(
                HostMessage::ExecStart(_)
                | HostMessage::ExecSignal(_)
                | HostMessage::ExecWindowSize(_)
                | HostMessage::FileUpdate(_),
            ) => {
                return Err(Failure::msg(
                    "protocol violation: an exec or file message in build mode",
                ));
            }
            Event::Signal(SIGCHLD) => {
                sys::reap();
            }
            Event::Signal(_) => break,
            Event::HostError(e) => return Err(Failure::msg(format!("protocol violation: {e}"))),
            Event::HostClosed => return Err(Failure::msg("the host closed the control connection")),
        }
    }
    storage::finish(&rw);
    Ok(())
}

/// No process may create a user namespace from now on, so a step cannot get
/// `CAP_SYS_ADMIN` inside one (to mount, or to reach what its bounding set and
/// its read-only `/proc` keep from it). Docker's default seccomp profile blocks
/// them too. A kernel with no user namespaces has no limit to set.
fn disable_user_namespaces() -> Result<()> {
    match std::fs::write("/proc/sys/user/max_user_namespaces", "0") {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Failure::os("disable user namespaces", &e)),
    }
}

struct Builder<'a> {
    config: &'a Config,
    /// The root's lower directories, bottom first: base layers, stacked layers
    /// and sealed uppers.
    lowers: Vec<String>,
    /// Disks stacked so far (each at most once).
    stacked: BTreeSet<u32>,
    /// Cache disks grown to their device's size.
    grown: BTreeSet<u32>,
    output: Output,
    /// `Config.sourceDateEpoch`: the latest mtime a sealed entry may have.
    epoch: Option<u64>,
}

impl Builder<'_> {
    fn step(&mut self, step: &RunStep, rx: &Receiver<Event>) -> Result<()> {
        self.check(step)?;
        for &disk in &step.lowers_added {
            let dir = format!("/kiln/disks/{disk}");
            mkdir(&dir)?;
            let dev = device(disk as usize);
            rustix::mount::mount(dev.as_str(), dir.as_str(), "erofs", MountFlags::RDONLY, None)
                .context(format!("mount layer {dev}"))?;
            self.stacked.insert(disk);
            self.lowers.push(dir);
        }
        // Named after the lower it becomes once sealed: a discarded upper's
        // name is used again, so every name in the mount data stays short.
        let k = self.lowers.len();
        let (upper, work) = (format!("/kiln/rw/u/{k}"), format!("/kiln/rw/w/{k}"));
        mkdir(&upper)?;
        mkdir(&work)?;
        let data = crate::overlay::step_mount_data(&self.lowers, &upper, &work)?;
        let data = CString::new(data).expect("no NUL in paths");
        // `nodev`, defence in depth: a step has no CAP_MKNOD, and a device node a
        // lower layer carries cannot be opened in the guest either.
        rustix::mount::mount("overlay", ROOT, "overlay", MountFlags::NODEV, data.as_c_str())
            .context("mount the step's overlay")?;

        let mut mounts = Mounts::new()?;
        // A mount target the step's own spec or image makes impossible fails the
        // step (125, the reason on its stderr), not the VM, as a working directory
        // that cannot be made does. A failure of init's own mounts ends the VM.
        let setup = match self.mount_all(step, &mut mounts) {
            Ok(()) => Ok(()),
            Err(SetupError::Step(f)) => Err(f),
            Err(SetupError::Init(f)) => return Err(f),
        };
        let ran = run_command(self.config, step, rx, setup);
        mounts.teardown();
        match rustix::mount::unmount(ROOT, UnmountFlags::empty()) {
            Ok(()) => {}
            Err(e) => {
                eprintln!("kiln-init: unmount the step's overlay: {e}; detaching it");
                rustix::mount::unmount(ROOT, UnmountFlags::DETACH).context("detach the step's overlay")?;
            }
        }
        let code = ran?;
        if code == Some(0) {
            let sealed = self.output.seal(&upper, &self.lowers, self.epoch)?;
            send(&GuestMessage::StepSealed(StepSealed {
                id: step.id,
                offset: sealed.offset,
                len: sealed.written.len,
                sha256: sealed.sha256,
                entries: sealed.written.entries,
            }))?;
            self.lowers.push(upper);
        } else {
            std::fs::remove_dir_all(&upper).context(format!("discard {upper}"))?;
        }
        std::fs::remove_dir_all(&work).context(format!("remove {work}"))?;
        Ok(())
    }

    /// The host checked the step against the VM's disks; a step that does not
    /// match them is a protocol violation.
    fn check(&self, step: &RunStep) -> Result<()> {
        let bad = |why: String| Err(Failure::msg(format!("protocol violation: RunStep {}: {why}", step.id)));
        if self.lowers.len() + step.lowers_added.len() > MAX_BUILD_LOWERS {
            return bad(format!(
                "{} lower layers and {} more (at most {MAX_BUILD_LOWERS})",
                self.lowers.len(),
                step.lowers_added.len()
            ));
        }
        let mut added = BTreeSet::new();
        for &d in &step.lowers_added {
            match self.config.build_disk(d) {
                Some(disk) if disk.kind == DiskKind::Erofs && !self.stacked.contains(&d) && added.insert(d) => {}
                _ => return bad(format!("disk {d} is not an erofs disk left to stack")),
            }
        }
        for m in &step.mounts {
            match self.config.build_disk(m.disk) {
                Some(disk) if disk.kind == DiskKind::Ext4 && (m.read_only || !disk.read_only) => {}
                _ => return bad(format!("disk {} is not a cache disk it may mount so", m.disk)),
            }
        }
        for t in step
            .mounts
            .iter()
            .map(|m| &m.target)
            .chain(step.secrets.iter().map(|s| &s.target))
        {
            // Defence in depth: the host already ran the same check.
            if let Err(e) = kiln_proto::check_target("target", t) {
                return bad(e);
            }
        }
        Ok(())
    }

    /// The API filesystems, the `/etc` files, the cache disks and the secrets.
    /// Only resolving or creating a cache or secret target (or an `/etc` file's)
    /// in the step's root is the step's own doing; every other failure is init's.
    fn mount_all(&mut self, step: &RunStep, m: &mut Mounts) -> std::result::Result<(), SetupError> {
        m.attach(clone_tree("/proc")?, "/proc", true)?;
        m.protect_proc()?;
        m.attach(new_fs("sysfs", None, &[], &[], true)?, "/sys", true)?;
        m.attach(minimal_dev()?, "/dev", true)?;
        m.attach(
            new_fs(
                "devpts",
                None,
                &[("ptmxmode", "0666"), ("mode", "0620"), ("gid", "5")],
                &["newinstance"],
                false,
            )?,
            "/dev/pts",
            true,
        )?;
        m.attach(
            new_fs("tmpfs", None, &[("mode", "1777")], &[], false)?,
            "/dev/shm",
            true,
        )?;

        for mount in &step.mounts {
            let target = m.target(&mount.target, true).map_err(SetupError::Step)?;
            let dev = device(mount.disk as usize);
            let fs = new_fs("ext4", Some(&dev), &[], &[], mount.read_only)?;
            if !mount.read_only && !self.grown.contains(&mount.disk) {
                let root = rustix::fs::openat(
                    &fs,
                    c".",
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .context(format!("open the cache disk {dev}"))?;
                storage::grow_to_device(&root, &dev)?;
                self.grown.insert(mount.disk);
            }
            m.attach_at(fs, &target, &mount.target)?;
        }

        // The `/etc` files and the secrets: one tmpfs, never part of the upper.
        let net = self.config.network.as_ref();
        let hostname = &self.config.hostname;
        let mut files: Vec<(String, Vec<u8>, u32, u32, u32)> = vec![
            (
                "/etc/hostname".into(),
                crate::etc::hostname(hostname).into_bytes(),
                0,
                0,
                0o644,
            ),
            (
                "/etc/hosts".into(),
                crate::etc::hosts(hostname, net).into_bytes(),
                0,
                0,
                0o644,
            ),
            (
                "/etc/resolv.conf".into(),
                crate::etc::resolv_conf(net).into_bytes(),
                0,
                0,
                0o644,
            ),
        ];
        let etc = files.len();
        for s in &step.secrets {
            files.push((s.target.clone(), s.data.clone(), step.uid, step.gid, 0o400));
        }
        let tmp = new_fs("tmpfs", None, &[("mode", "0700"), ("size", "4m")], &[], false)?;
        for (i, (target, data, uid, gid, mode)) in files.iter().enumerate() {
            // The image's own /etc file stays when it is a symlink (or under one).
            if i < etc && !m.plain(target).map_err(SetupError::Step)? {
                continue;
            }
            let name = format!("f{i}");
            let fd = rustix::fs::openat(
                &tmp,
                name.as_str(),
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
                Mode::from_raw_mode(*mode),
            )
            .context("create a file on the step's tmpfs")?;
            rustix::fs::fchown(&fd, Some(Uid::from_raw(*uid)), Some(Gid::from_raw(*gid))).context("chown a secret")?;
            File::from(fd).write_all(data).context("write a secret")?;
            let tree = rustix::mount::open_tree(
                &tmp,
                name.as_str(),
                OpenTreeFlags::OPEN_TREE_CLONE | OpenTreeFlags::OPEN_TREE_CLOEXEC,
            )
            .context("bind a file of the step's tmpfs")?;
            let at = m.target(target, false).map_err(SetupError::Step)?;
            m.attach_at(tree, &at, target)?;
        }
        Ok(())
    }
}

/// Why a step's mounts could not be set up.
enum SetupError {
    /// The step's own spec or image: a target that cannot be resolved or created.
    /// The step ends with 125; the VM goes on.
    Step(Failure),
    /// Init's own work (a filesystem, a clone, a mount, a grow): ends the VM.
    Init(Failure),
}

impl From<Failure> for SetupError {
    fn from(f: Failure) -> Self {
        Self::Init(f)
    }
}

/// `/proc` paths a step may read but not write (runc's `readonlyPaths`).
const PROC_READONLY: [&str; 5] = ["/proc/sys", "/proc/sysrq-trigger", "/proc/irq", "/proc/bus", "/proc/fs"];

/// `/proc` paths a step sees empty (runc's `maskedPaths`, those procfs has).
const PROC_MASKED: [&str; 8] = [
    "/proc/kcore",
    "/proc/keys",
    "/proc/timer_list",
    "/proc/sched_debug",
    "/proc/acpi",
    "/proc/scsi",
    "/proc/latency_stats",
    "/proc/timer_stats",
];

/// A detached copy of init's mount at `path`.
fn clone_tree(path: &str) -> Result<OwnedFd> {
    rustix::mount::open_tree(
        rustix::fs::CWD,
        path,
        OpenTreeFlags::OPEN_TREE_CLONE | OpenTreeFlags::OPEN_TREE_CLOEXEC,
    )
    .context(format!("clone the mount of {path}"))
}

/// A new detached filesystem.
fn new_fs(fstype: &str, source: Option<&str>, opts: &[(&str, &str)], flags: &[&str], ro: bool) -> Result<OwnedFd> {
    let what = format!("mount {fstype}{}", source.map(|s| format!(" {s}")).unwrap_or_default());
    let fs = rustix::mount::fsopen(fstype, FsOpenFlags::FSOPEN_CLOEXEC).context(&what)?;
    if let Some(s) = source {
        rustix::mount::fsconfig_set_string(&fs, "source", s).context(&what)?;
    }
    for (k, v) in opts {
        rustix::mount::fsconfig_set_string(&fs, *k, *v).context(format!("{what}: {k}={v}"))?;
    }
    for f in flags.iter().copied().chain(ro.then_some("ro")) {
        rustix::mount::fsconfig_set_flag(&fs, f).context(format!("{what}: {f}"))?;
    }
    rustix::mount::fsconfig_create(&fs).context(&what)?;
    let mut attrs = MountAttrFlags::MOUNT_ATTR_NOSUID | MountAttrFlags::MOUNT_ATTR_NODEV;
    if ro {
        attrs |= MountAttrFlags::MOUNT_ATTR_RDONLY;
    }
    if fstype == "devpts" {
        attrs = MountAttrFlags::MOUNT_ATTR_NOSUID | MountAttrFlags::MOUNT_ATTR_NOEXEC;
    }
    rustix::mount::fsmount(&fs, FsMountFlags::FSMOUNT_CLOEXEC, attrs).context(&what)
}

/// The character devices a step's `/dev` holds (ruling 1): no block devices,
/// so not the output disk or a layer disk, whatever the step's uid.
const DEVICES: [(&str, u32, u32); 6] = [
    ("null", 1, 3),
    ("zero", 1, 5),
    ("full", 1, 7),
    ("random", 1, 8),
    ("urandom", 1, 9),
    ("tty", 5, 0),
];

/// A step's `/dev`: a small tmpfs with [`DEVICES`], `pts` and `shm` (mounted
/// over next), `ptmx` and the `fd`, `stdin`, `stdout` and `stderr` links.
fn minimal_dev() -> Result<OwnedFd> {
    let what = "mount the step's /dev";
    let fs = rustix::mount::fsopen("tmpfs", FsOpenFlags::FSOPEN_CLOEXEC).context(what)?;
    for (k, v) in [("mode", "0755"), ("size", "64k"), ("nr_inodes", "64")] {
        rustix::mount::fsconfig_set_string(&fs, k, v).context(format!("{what}: {k}={v}"))?;
    }
    rustix::mount::fsconfig_create(&fs).context(what)?;
    let dev = rustix::mount::fsmount(
        &fs,
        FsMountFlags::FSMOUNT_CLOEXEC,
        MountAttrFlags::MOUNT_ATTR_NOSUID | MountAttrFlags::MOUNT_ATTR_NOEXEC,
    )
    .context(what)?;
    for (name, major, minor) in DEVICES {
        rustix::fs::mknodat(
            &dev,
            name,
            FileType::CharacterDevice,
            Mode::from_raw_mode(0o666),
            rustix::fs::makedev(major, minor),
        )
        .context(format!("mknod /dev/{name}"))?;
        // mknod applies init's umask.
        rustix::fs::chmodat(&dev, name, Mode::from_raw_mode(0o666), AtFlags::empty())
            .context(format!("chmod /dev/{name}"))?;
    }
    for dir in ["pts", "shm"] {
        rustix::fs::mkdirat(&dev, dir, Mode::from_raw_mode(0o755)).context(format!("mkdir /dev/{dir}"))?;
    }
    for (name, target) in [
        ("ptmx", "pts/ptmx"),
        ("fd", "/proc/self/fd"),
        ("stdin", "/proc/self/fd/0"),
        ("stdout", "/proc/self/fd/1"),
        ("stderr", "/proc/self/fd/2"),
    ] {
        rustix::fs::symlinkat(target, &dev, name).context(format!("symlink /dev/{name}"))?;
    }
    // Populated: read-only from now on (the superblock, private to this step;
    // rustix has no mount_setattr), so no step can add a node to it. `pts` and
    // `shm` are mounts of their own over it.
    let fs = rustix::mount::fspick(&dev, "", FsPickFlags::FSPICK_EMPTY_PATH | FsPickFlags::FSPICK_CLOEXEC)
        .context("reopen the step's /dev")?;
    rustix::mount::fsconfig_set_flag(&fs, "ro").context("make the step's /dev read-only")?;
    rustix::mount::fsconfig_reconfigure(&fs).context("make the step's /dev read-only")?;
    Ok(dev)
}

/// The step's mounts and the mount points created for them.
struct Mounts {
    root: OwnedFd,
    /// Created mount points: parent, name, whether a directory.
    stubs: Vec<(OwnedFd, CString, bool)>,
    /// Attached mounts, each fd on its mount's root.
    attached: Vec<OwnedFd>,
}

impl Mounts {
    fn new() -> Result<Self> {
        let root = rustix::fs::open(ROOT, OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())
            .context("open the step's root")?;
        Ok(Self {
            root,
            stubs: Vec::new(),
            attached: Vec::new(),
        })
    }

    fn resolve(&self, rel: &str, extra: ResolveFlags) -> rustix::io::Result<OwnedFd> {
        let rel = if rel.is_empty() { "." } else { rel };
        rustix::fs::openat2(
            &self.root,
            rel,
            OFlags::PATH | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::IN_ROOT | ResolveFlags::NO_MAGICLINKS | extra,
        )
    }

    /// Whether `path` in the root is a regular file or absent, with no symlink on the way.
    fn plain(&self, path: &str) -> Result<bool> {
        match self.resolve(path.trim_start_matches('/'), ResolveFlags::NO_SYMLINKS) {
            Ok(fd) => {
                Ok(rustix::fs::fstat(&fd).is_ok_and(|st| FileType::from_raw_mode(st.st_mode) == FileType::RegularFile))
            }
            Err(Errno::NOENT) => Ok(true),
            Err(Errno::LOOP | Errno::NOTDIR) => Ok(false),
            Err(e) => Err(e).context(format!("look up {path}")),
        }
    }

    /// An `O_PATH` descriptor of `path` in the root, a directory or a regular
    /// file, created (and recorded as a stub) when missing.
    fn target(&mut self, path: &str, dir: bool) -> Result<OwnedFd> {
        let comps: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
        let mut parent = self
            .resolve("", ResolveFlags::empty())
            .context("open the step's root")?;
        for (i, c) in comps.iter().enumerate() {
            let last = i + 1 == comps.len();
            let prefix = comps[..=i].join("/");
            match self.resolve(&prefix, ResolveFlags::empty()) {
                Ok(fd) => parent = fd,
                Err(Errno::NOENT) => {
                    let as_dir = !last || dir;
                    if as_dir {
                        rustix::fs::mkdirat(&parent, *c, Mode::from_raw_mode(0o755))
                    } else {
                        rustix::fs::openat(
                            &parent,
                            *c,
                            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                            Mode::from_raw_mode(0o644),
                        )
                        .map(drop)
                    }
                    .context(format!("create the mount point {path}"))?;
                    let name = CString::new(*c).expect("no NUL in targets");
                    let fd = rustix::fs::openat(
                        &parent,
                        *c,
                        OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    )
                    .context(format!("open the mount point {path}"))?;
                    self.stubs.push((parent, name, as_dir));
                    parent = fd;
                }
                Err(e) => return Err(e).context(format!("look up {path}")),
            }
        }
        let ty = rustix::fs::fstat(&parent)
            .map(|st| FileType::from_raw_mode(st.st_mode))
            .context(format!("stat {path}"))?;
        let want = if dir {
            FileType::Directory
        } else {
            FileType::RegularFile
        };
        if ty != want {
            return Err(Failure::msg(format!(
                "the mount point {path} is not a {}",
                if dir { "directory" } else { "regular file" }
            )));
        }
        Ok(parent)
    }

    fn attach(&mut self, mnt: OwnedFd, path: &str, dir: bool) -> Result<()> {
        let target = self.target(path, dir)?;
        self.attach_at(mnt, &target, path)
    }

    /// Moves `mnt` onto `target` (what [`Self::target`] returned for `path`).
    fn attach_at(&mut self, mnt: OwnedFd, target: &OwnedFd, path: &str) -> Result<()> {
        rustix::mount::move_mount(
            &mnt,
            "",
            target,
            "",
            MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH | MoveMountFlags::MOVE_MOUNT_T_EMPTY_PATH,
        )
        .context(format!("mount on {path}"))?;
        self.attached.push(mnt);
        Ok(())
    }

    /// Hardens the step's `/proc` as runc does: `PROC_READONLY` paths are bound
    /// onto themselves read-only, as non-recursive clones so a submount cannot
    /// come through writable (a uid-0 step without `CAP_SYS_ADMIN` could
    /// otherwise write `kernel.core_pattern` or `kernel.modprobe`, which the
    /// kernel runs as root in init's mount namespace, where the output disk is
    /// visible), and `PROC_MASKED` paths are covered by `/dev/null` (files) or an
    /// empty read-only tmpfs (directories). Paths the kernel lacks are skipped;
    /// any other failure fails the step's setup. Every mount is torn down with
    /// the step's others. This runs in init before the fork, so the pre-exec
    /// hook stays allocation-free.
    fn protect_proc(&mut self) -> Result<()> {
        let flags = MountFlags::BIND | MountFlags::RDONLY | MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC;
        for path in PROC_READONLY {
            let Some(is_dir) = self.proc_kind(path)? else { continue };
            let tree = rustix::mount::open_tree(
                rustix::fs::CWD,
                path,
                OpenTreeFlags::OPEN_TREE_CLONE | OpenTreeFlags::OPEN_TREE_CLOEXEC,
            )
            .context(format!("bind {path}"))?;
            self.attach(tree, path, is_dir)?;
            let mnt = self
                .attached
                .last()
                .map(|m| m.as_raw_fd())
                .ok_or_else(|| Failure::msg("no mount"))?;
            rustix::mount::mount_remount(format!("/proc/self/fd/{mnt}").as_str(), flags, "")
                .context(format!("make {path} read-only"))?;
        }
        for path in PROC_MASKED {
            let Some(is_dir) = self.proc_kind(path)? else { continue };
            let cover = if is_dir {
                new_fs("tmpfs", None, &[("mode", "0555"), ("size", "4k")], &[], true)?
            } else {
                clone_tree("/dev/null")?
            };
            self.attach(cover, path, is_dir)?;
        }
        Ok(())
    }

    /// Whether `path` exists in the step's root, and if so whether it is a directory.
    fn proc_kind(&self, path: &str) -> Result<Option<bool>> {
        match self.resolve(path.trim_start_matches('/'), ResolveFlags::NO_SYMLINKS) {
            Ok(fd) => {
                let ty = rustix::fs::fstat(&fd)
                    .map(|st| FileType::from_raw_mode(st.st_mode))
                    .context(format!("stat {path}"))?;
                Ok(Some(ty == FileType::Directory))
            }
            Err(Errno::NOENT) => Ok(None),
            Err(e) => Err(e).context(format!("look up {path}")),
        }
    }

    /// Detaches the mounts (last first), then removes the stubs (last first);
    /// a stub the step put something in stays, as its own change.
    fn teardown(mut self) {
        for m in self.attached.drain(..).rev() {
            let path = format!("/proc/self/fd/{}", m.as_raw_fd());
            if let Err(e) = rustix::mount::unmount(path.as_str(), UnmountFlags::DETACH) {
                eprintln!("kiln-init: detach a step mount: {e}");
            }
        }
        for (parent, name, dir) in self.stubs.drain(..).rev() {
            let flags = if dir { AtFlags::REMOVEDIR } else { AtFlags::empty() };
            let _ = rustix::fs::unlinkat(&parent, name.as_c_str(), flags);
        }
    }
}

/// What the child's setup was doing when it failed.
const STEP_HOOK: [&str; 9] = [
    "setsid",
    "unshare the mount namespace",
    "pivot into the step's root",
    "drop capabilities",
    "create the working directory",
    "enter the working directory",
    "setgroups",
    "setresgid",
    "setresuid",
];

/// The bounding set a step keeps: Docker's default capabilities without
/// `MKNOD` (the guest has no device cgroup to stop a node from being opened):
/// `CHOWN`, `DAC_OVERRIDE`, `FOWNER`, `FSETID`, `KILL`, `SETGID`, `SETUID`,
/// `SETPCAP`, `NET_BIND_SERVICE`, `NET_RAW`, `SYS_CHROOT`, `AUDIT_WRITE`, `SETFCAP`.
const STEP_CAPS: u64 = (1 << 0)
    | (1 << 1)
    | (1 << 3)
    | (1 << 4)
    | (1 << 5)
    | (1 << 6)
    | (1 << 7)
    | (1 << 8)
    | (1 << 10)
    | (1 << 13)
    | (1 << 18)
    | (1 << 29)
    | (1 << 31);

/// Runs the command; returns its exit code (`None` when a signal killed it),
/// once `StepExited` is sent. When `setup` (the step's mounts) failed, nothing
/// runs: the step ends with 125 and the reason on its stderr, like any other
/// setup failure.
fn run_command(config: &Config, step: &RunStep, rx: &Receiver<Event>, setup: Result<()>) -> Result<Option<i32>> {
    let id = Identity {
        uid: step.uid,
        gid: step.gid,
        groups: vec![step.gid],
        home: if step.uid == 0 { "/root".into() } else { "/".into() },
    };
    let mut outputs = Vec::new();
    let mut ends = Vec::new();
    for (name, p) in [("stdout", port::STDOUT), ("stderr", port::STDERR)] {
        let (r, w) = pipe()?;
        give(&w, &id)?;
        outputs.push(relay::spawn(name, r, connect(p)?, |v: super::sys::Vsock| {
            v.shutdown_write()
        })?);
        ends.push(w);
    }
    let mut err = ends.pop().expect("two pipes");
    let out = ends.pop().expect("two pipes");
    let env = crate::env::build(&step.env, &id.home, &config.hostname, false);
    let argv = step.command.argv();
    let mut cmd = Command::new(argv[0]);
    cmd.args(&argv[1..])
        .env_clear()
        .envs(env.iter().filter_map(|e| e.split_once('=')))
        .stdin(Stdio::null())
        .stdout(out.try_clone().context("dup the stdout pipe")?)
        .stderr(err.try_clone().context("dup the stderr pipe")?);
    drop(out);
    let (hook_failed, report) = rustix::pipe::pipe_with(PipeFlags::CLOEXEC | PipeFlags::NONBLOCK).context("pipe")?;
    step_hook(&mut cmd, &id, &step.workdir, report);
    let spawned = match setup {
        Ok(()) => cmd.spawn().map_err(|e| match failed_hook_step(&hook_failed) {
            Some(step) => (125, format!("kiln-init: {step}: {e}\n")),
            None => (
                if e.raw_os_error() == Some(2) { 127 } else { 126 },
                format!("kiln-init: exec {}: {e}\n", argv[0]),
            ),
        }),
        Err(f) => Err((125, format!("kiln-init: set up the step's mounts: {f}\n"))),
    };
    drop(cmd);
    let code = match spawned {
        Err((code, msg)) => {
            let _ = err.write_all(msg.as_bytes());
            drop(err);
            for r in outputs {
                let _ = r.join();
            }
            send(&GuestMessage::StepExited(StepExited {
                id: step.id,
                code: Some(code),
                signal: None,
            }))?;
            return Ok(Some(code));
        }
        Ok(child) => {
            drop(err);
            let pid = Pid::from_raw(child.id() as i32).expect("a child's pid is positive");
            wait(pid, rx)?
        }
    };
    sys::kill_all();
    sys::reap_all();
    for r in outputs {
        let _ = r.join();
    }
    let (code, signal) = match (code.exit_status(), code.terminating_signal()) {
        (Some(c), _) => (Some(c), None),
        (None, s) => (None, s),
    };
    send(&GuestMessage::StepExited(StepExited {
        id: step.id,
        code,
        signal,
    }))?;
    Ok(code)
}

/// Waits for the step's process; `Signal` goes to its process group.
fn wait(pid: Pid, rx: &Receiver<Event>) -> Result<rustix::process::WaitStatus> {
    loop {
        match rx.recv().expect("the signal thread holds a sender") {
            Event::Signal(SIGCHLD) => {
                for (p, status) in sys::reap() {
                    if p == pid {
                        return Ok(status);
                    }
                }
            }
            Event::Host(HostMessage::Signal(s)) => {
                if let Some(sig) = rustix::process::Signal::from_named_raw(s.sig) {
                    let _ = rustix::process::kill_process_group(pid, sig);
                }
            }
            Event::Host(HostMessage::WindowSize(_)) => {}
            Event::Host(other) => {
                sys::kill_all();
                return Err(Failure::msg(format!(
                    "protocol violation: {} while a step runs",
                    match other {
                        HostMessage::RunStep(_) => "RunStep",
                        HostMessage::Finish => "Finish",
                        HostMessage::Shutdown(_) => "Shutdown",
                        HostMessage::Config(_) => "Config",
                        _ => "an exec or file message",
                    }
                )));
            }
            Event::Signal(_) => {
                sys::kill_all();
                return Err(Failure::msg("init was asked to stop during a step"));
            }
            Event::HostError(e) => return Err(Failure::msg(format!("protocol violation: {e}"))),
            Event::HostClosed => return Err(Failure::msg("the host closed the control connection")),
        }
    }
}

/// In the child: a new session; a mount namespace of its own, pivoted into the
/// step's root with init's root detached; the bounding set cut to
/// [`STEP_CAPS`]; the working directory (created as root when missing); then
/// groups, gid and uid.
fn step_hook(cmd: &mut Command, id: &Identity, workdir: &str, report: OwnedFd) {
    let mut prefixes = Vec::new();
    let mut acc = String::new();
    for c in workdir.split('/').filter(|c| !c.is_empty()) {
        acc.push('/');
        acc.push_str(c);
        prefixes.push(CString::new(acc.clone()).expect("no NUL in workdir"));
    }
    let wd = CString::new(workdir).expect("no NUL in workdir");
    let groups: Vec<Gid> = id.groups.iter().map(|&g| Gid::from_raw(g)).collect();
    let (uid, gid) = (Uid::from_raw(id.uid), Gid::from_raw(id.gid));
    let hook = move || -> std::io::Result<()> {
        let step = |n: u8, r: rustix::io::Result<()>| {
            r.map_err(|e| {
                let _ = rustix::io::write(&report, &[n]);
                std::io::Error::from(e)
            })
        };
        step(0, rustix::process::setsid().map(drop))?;
        // SAFETY: only a new mount namespace (not `CLONE_FILES`): no descriptor
        // table is unshared, and the child is single-threaded.
        #[allow(unsafe_code)]
        step(1, unsafe {
            rustix::thread::unshare_unsafe(rustix::thread::UnshareFlags::NEWNS)
        })?;
        step(
            1,
            rustix::mount::mount_change(
                c"/",
                rustix::mount::MountPropagationFlags::PRIVATE | rustix::mount::MountPropagationFlags::REC,
            ),
        )?;
        // runc's `pivot_root(".", ".")`: the old root stacks under the new one
        // and is detached at once.
        step(2, rustix::process::chdir(c"/kiln/root"))?;
        step(2, rustix::process::pivot_root(c".", c"."))?;
        step(2, rustix::mount::unmount(c".", UnmountFlags::DETACH))?;
        step(2, rustix::process::chdir(c"/"))?;
        for n in 0..64u32 {
            if STEP_CAPS & (1 << n) == 0 {
                match rustix::thread::remove_capability_from_bounding_set(CapabilitySet::from_bits_retain(1 << n)) {
                    // Past the kernel's last capability.
                    Ok(()) | Err(Errno::INVAL) => {}
                    Err(e) => step(3, Err(e))?,
                }
            }
        }
        for p in &prefixes {
            match rustix::fs::mkdir(p.as_c_str(), Mode::from_raw_mode(0o755)) {
                Ok(()) | Err(Errno::EXIST) => {}
                Err(e) => step(4, Err(e))?,
            }
        }
        step(5, rustix::process::chdir(wd.as_c_str()))?;
        step(6, rustix::thread::set_thread_groups(&groups))?;
        step(7, rustix::thread::set_thread_res_gid(gid, gid, gid))?;
        step(8, rustix::thread::set_thread_res_uid(uid, uid, uid))?;
        Ok(())
    };
    // SAFETY: the hook runs in the forked child before exec. It only makes raw
    // system calls through rustix on values moved in beforehand (C strings and
    // ids prepared in the parent; a failure writes one byte to a pipe and
    // converts an errno): no allocation, locks or state shared with the parent's
    // threads.
    #[allow(unsafe_code)]
    unsafe {
        cmd.pre_exec(hook);
    }
}

/// The setup step the hook reported on `pipe`, if it failed.
fn failed_hook_step(pipe: &OwnedFd) -> Option<&'static str> {
    let mut b = [0u8; 1];
    match rustix::io::read(pipe, &mut b) {
        Ok(1) => STEP_HOOK.get(usize::from(b[0])).copied(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_proto::{
        BuildDisk, Command as StepCommand, ExitMethod, Mode as ConfigMode, Process, Scratch, Secret, StepMount,
    };

    /// One base layer (disk 3), so `Config.disks` start at disk 4: two erofs
    /// layers (4, 5), a writable cache (6) and a read-only one (7).
    fn config() -> Config {
        let disk = |kind, read_only| BuildDisk { kind, read_only };
        Config {
            process: Process::default(),
            stop_signal: 15,
            tty: None,
            interactive: false,
            hostname: "build".into(),
            network: None,
            layers: 1,
            scratch: Scratch { size_bytes: 1 << 30 },
            exit_method: ExitMethod::Poweroff,
            shutdown_grace_secs: 1,
            volumes: Vec::new(),
            pod_disk: None,
            services: Vec::new(),
            ready_timeout_ms: None,
            mode: ConfigMode::Build,
            disks: vec![
                disk(DiskKind::Erofs, true),
                disk(DiskKind::Erofs, true),
                disk(DiskKind::Ext4, false),
                disk(DiskKind::Ext4, true),
            ],
            source_date_epoch: None,
        }
    }

    fn step() -> RunStep {
        RunStep {
            id: 7,
            command: StepCommand::Shell("true".into()),
            env: Vec::new(),
            workdir: "/".into(),
            uid: 0,
            gid: 0,
            lowers_added: Vec::new(),
            mounts: Vec::new(),
            secrets: Vec::new(),
            egress: false,
            timeout_ms: 1000,
        }
    }

    fn mount(disk: u32, target: &str, read_only: bool) -> StepMount {
        StepMount {
            disk,
            target: target.into(),
            read_only,
        }
    }

    fn secret(target: &str) -> Secret {
        Secret {
            target: target.into(),
            data: b"s".to_vec(),
        }
    }

    /// `check` on a builder over `config`, with `lowers` lower directories and
    /// `stacked` disks already stacked.
    fn check(config: &Config, lowers: usize, stacked: &[u32], step: &RunStep) -> std::result::Result<(), String> {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("output.img");
        std::fs::File::create(&img).unwrap();
        let b = Builder {
            config,
            lowers: (0..lowers).map(|n| format!("/kiln/layers/{n}")).collect(),
            stacked: stacked.iter().copied().collect(),
            grown: BTreeSet::new(),
            output: Output::open(img.to_str().unwrap()).unwrap(),
            epoch: None,
        };
        b.check(step).map_err(|f| f.to_string())
    }

    fn refused(config: &Config, lowers: usize, stacked: &[u32], step: &RunStep, why: &str) {
        let e = check(config, lowers, stacked, step).expect_err(why);
        assert!(e.starts_with("protocol violation: RunStep 7: "), "{e}");
        assert!(e.contains(why), "wanted {why:?} in {e:?}");
    }

    #[test]
    fn a_step_that_matches_the_disks_is_accepted() {
        let c = config();
        let mut s = step();
        s.lowers_added = vec![4, 5];
        s.mounts = vec![
            mount(6, "/root/.cache", false),
            mount(7, "/ro", true),
            mount(6, "/again/", true),
        ];
        s.secrets = vec![secret("/run/secrets/token")];
        check(&c, 1, &[], &s).unwrap();
        // A layer stacked by an earlier step is not offered again, but the others are.
        s.lowers_added = vec![5];
        check(&c, 3, &[4], &s).unwrap();
    }

    #[test]
    fn layers_must_be_erofs_disks_not_stacked_yet() {
        let c = config();
        let mut s = step();
        for (disk, why) in [
            (6, "disk 6 is not an erofs disk left to stack"),
            (7, "disk 7 is not an erofs disk left to stack"),
            (3, "disk 3 is not an erofs disk left to stack"),
            (8, "disk 8 is not an erofs disk left to stack"),
            (0, "disk 0 is not an erofs disk left to stack"),
        ] {
            s.lowers_added = vec![disk];
            refused(&c, 1, &[], &s, why);
        }
        s.lowers_added = vec![4];
        refused(&c, 2, &[4], &s, "disk 4 is not an erofs disk left to stack");
        // The same disk twice in one step: the second is already stacked.
        s.lowers_added = vec![4, 4];
        refused(&c, 1, &[], &s, "disk 4 is not an erofs disk left to stack");
    }

    #[test]
    fn a_plain_failure_is_init_s_own_and_ends_the_vm() {
        // The `?` on init's own work (a mount, a clone, a grow) converts to the
        // variant that ends the VM; only a target of the step's own is `Step`.
        assert!(matches!(SetupError::from(Failure::msg("x")), SetupError::Init(_)));
    }

    #[test]
    fn mounts_must_be_cache_disks_and_never_writable_when_the_disk_is_read_only() {
        let c = config();
        let mut s = step();
        s.mounts = vec![mount(4, "/x", true)];
        refused(&c, 1, &[], &s, "disk 4 is not a cache disk it may mount so");
        s.mounts = vec![mount(7, "/x", false)];
        refused(&c, 1, &[], &s, "disk 7 is not a cache disk it may mount so");
        s.mounts = vec![mount(9, "/x", false)];
        refused(&c, 1, &[], &s, "disk 9 is not a cache disk it may mount so");
        s.mounts = vec![mount(7, "/x", true)];
        check(&c, 1, &[], &s).unwrap();
    }

    #[test]
    fn the_overlay_has_at_most_max_build_lowers_lower_directories() {
        let c = config();
        let mut s = step();
        s.lowers_added = vec![4, 5];
        check(&c, MAX_BUILD_LOWERS - 2, &[], &s).unwrap();
        refused(
            &c,
            MAX_BUILD_LOWERS - 1,
            &[],
            &s,
            &format!(
                "{} lower layers and 2 more (at most {MAX_BUILD_LOWERS})",
                MAX_BUILD_LOWERS - 1
            ),
        );
        s.lowers_added.clear();
        check(&c, MAX_BUILD_LOWERS, &[], &s).unwrap();
        refused(&c, MAX_BUILD_LOWERS + 1, &[], &s, "at most");
    }

    #[test]
    fn targets_follow_the_shared_rule() {
        let c = config();
        for bad in ["relative", "/", "", "/a/../b", "/a//b", "/a/./b", "/a\0b"] {
            let mut s = step();
            s.mounts = vec![mount(6, bad, false)];
            refused(&c, 1, &[], &s, "target");
            let mut s = step();
            s.secrets = vec![secret(bad)];
            refused(&c, 1, &[], &s, "target");
        }
        // One trailing slash is ignored by the rule.
        let mut s = step();
        s.mounts = vec![mount(6, "/ok/", false)];
        check(&c, 1, &[], &s).unwrap();
    }
}
