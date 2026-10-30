//! Reading a pod's volumes for its guest (kiln-shim exec and volumes design §5.2). The
//! shim reads them as root, since kubelet often makes their files `0400` root, so it
//! reads only what kubelet wrote: a files volume's directory under the pod's
//! `volumes/kubernetes.io~<plugin>/`, walked without following a link, taking regular
//! files and directories only, within the generation limits (threat V-1, V-2).

use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use kiln_proto::{
    MAX_VOLUME_BYTES, MAX_VOLUME_FILES, Volume, VolumeFile, VolumeKind, check_generation, check_relative_path,
};

use crate::error::{Error, Result};
use crate::mounts::{FILES_PLUGINS, Source};

/// kubelet's link to a files volume's current generation.
pub const DATA: &str = "..data";
/// The most bytes of files across a container's volumes (they travel in its worker's plan).
pub const MAX_TOTAL_BYTES: u64 = 32 << 20;

/// The volumes of one container, as the guest gets them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolved {
    pub volumes: Vec<Volume>,
    /// The files volumes' content, by index in `volumes`.
    pub files: Vec<FilesVolume>,
    /// Some volume is an `emptyDir` on the pod disk.
    pub pod_disk: bool,
}

/// One files volume's content, and where to look for its updates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilesVolume {
    pub index: u32,
    /// The directory kubelet updates (`None`: a `subPath` file, which kubelet never updates).
    pub dir: Option<PathBuf>,
    /// The `..data` target the content was read from.
    pub generation: String,
    pub files: Vec<VolumeFile>,
}

/// How the host sees a directory's filesystem: a tmpfs's size, or `None` when it is not
/// a tmpfs.
pub type TmpfsSize<'a> = &'a dyn Fn(&Path) -> Option<u64>;

fn refused(what: impl std::fmt::Display) -> Error {
    Error::invalid(format!("kiln cannot deliver {what}"))
}

/// Resolves a container's volumes, reading the files volumes' content.
pub fn resolve(volumes: &[(String, Source)], tmpfs: TmpfsSize<'_>) -> Result<Resolved> {
    let mut out = Resolved::default();
    let mut total = 0u64;
    for (dest, source) in volumes {
        let index = out.volumes.len() as u32;
        let kind = match source {
            Source::Files(dir) => {
                let (generation, files) = read_files(dir)?;
                total += files.iter().map(|f| f.data.len() as u64).sum::<u64>();
                out.files.push(FilesVolume {
                    index,
                    dir: Some(dir.clone()),
                    generation,
                    files,
                });
                VolumeKind::Files { file: false }
            }
            Source::SubPath { pod, volume, source } => {
                let file = read_subpath(pod, volume, source, dest)?;
                total += file.data.len() as u64;
                out.files.push(FilesVolume {
                    index,
                    dir: None,
                    generation: String::new(),
                    files: vec![file],
                });
                VolumeKind::Files { file: true }
            }
            Source::EmptyDir { name, dir } => match tmpfs(dir) {
                Some(size) => VolumeKind::Tmpfs { size_bytes: Some(size) },
                None => {
                    out.pod_disk = true;
                    VolumeKind::EmptyDir { name: name.clone() }
                }
            },
            Source::Tmpfs { size } => VolumeKind::Tmpfs { size_bytes: *size },
        };
        let v = Volume {
            path: dest.clone(),
            kind,
        };
        v.validate()
            .map_err(|e| refused(format!("the volume at {dest}: {e}")))?;
        out.volumes.push(v);
    }
    if out.volumes.len() > kiln_proto::MAX_VOLUMES {
        return Err(refused(format!(
            "{} volumes (at most {})",
            out.volumes.len(),
            kiln_proto::MAX_VOLUMES
        )));
    }
    if total > MAX_TOTAL_BYTES {
        return Err(refused(format!(
            "{total} bytes of volume files (at most {MAX_TOTAL_BYTES} per container)"
        )));
    }
    Ok(out)
}

/// The target of a files volume's `..data`, when it has kubelet's layout: one name
/// starting with `..`.
pub fn generation(dir: &Path) -> Result<Option<String>> {
    match std::fs::symlink_metadata(dir.join(DATA)) {
        Ok(m) if m.file_type().is_symlink() => {
            let target = std::fs::read_link(dir.join(DATA))?;
            let target = target.to_str().unwrap_or("");
            if !target.starts_with("..") || target.contains('/') || target == ".." {
                return Err(refused(format!("{}: ..data points at {target:?}", dir.display())));
            }
            Ok(Some(target.to_string()))
        }
        Ok(_) => Err(refused(format!("{}: ..data is not a link", dir.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// A files volume's current files and their generation: the `..data` generation's
/// directory, or (without kubelet's layout) the directory itself.
pub fn read_files(dir: &Path) -> Result<(String, Vec<VolumeFile>)> {
    let generation = generation(dir)?;
    let root = match &generation {
        Some(g) => dir.join(g),
        None => dir.to_path_buf(),
    };
    let mut files = Vec::new();
    walk(&root, "", &mut files, &mut 0)?;
    check_generation(&files).map_err(|e| refused(format!("{}: {e}", dir.display())))?;
    Ok((generation.unwrap_or_default(), files))
}

fn walk(dir: &Path, rel: &str, out: &mut Vec<VolumeFile>, bytes: &mut u64) -> Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<std::io::Result<_>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| refused(format!("a file name in {} that is not UTF-8", dir.display())))?;
        let path = if rel.is_empty() {
            name.to_string()
        } else {
            format!("{rel}/{name}")
        };
        check_relative_path("volume", &path).map_err(|e| refused(e.to_string()))?;
        let t = e.file_type()?;
        if t.is_dir() {
            walk(&e.path(), &path, out, bytes)?;
        } else if t.is_file() {
            if out.len() >= MAX_VOLUME_FILES {
                return Err(refused(format!("{}: over {MAX_VOLUME_FILES} files", dir.display())));
            }
            let file = read_file(&e.path(), path, MAX_VOLUME_BYTES - *bytes)?;
            *bytes += file.data.len() as u64;
            out.push(file);
        } else {
            return Err(refused(format!(
                "{}: {path} is not a regular file or directory",
                dir.display()
            )));
        }
    }
    Ok(())
}

/// One regular file, opened without following a link and without blocking (a FIFO),
/// read up to `max` bytes.
fn read_file(path: &Path, name: String, max: u64) -> Result<VolumeFile> {
    use rustix::fs::{Mode, OFlags};
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let f = std::fs::File::from(
        rustix::fs::open(path, flags, Mode::empty()).map_err(|e| refused(format!("{}: {e}", path.display())))?,
    );
    let m = f.metadata()?;
    if !m.is_file() {
        return Err(refused(format!("{}: not a regular file", path.display())));
    }
    if m.len() > max {
        return Err(refused(format!(
            "{}: over the {MAX_VOLUME_BYTES}-byte limit of a volume",
            path.display()
        )));
    }
    let mut data = Vec::new();
    f.take(max + 1).read_to_end(&mut data)?;
    if data.len() as u64 > max {
        return Err(refused(format!(
            "{}: over the {MAX_VOLUME_BYTES}-byte limit of a volume",
            path.display()
        )));
    }
    Ok(VolumeFile {
        path: name,
        mode: m.mode() & 0o7777,
        uid: m.uid(),
        gid: m.gid(),
        data,
    })
}

/// A `subPath` mount: read only when its volume is a files volume of the same pod, and
/// only a regular file. A directory, or a subPath of any other volume, is refused.
fn read_subpath(pod: &Path, volume: &str, source: &Path, dest: &str) -> Result<VolumeFile> {
    let files_volume = FILES_PLUGINS.iter().any(|p| {
        std::fs::symlink_metadata(pod.join("volumes").join(format!("kubernetes.io~{p}")).join(volume))
            .is_ok_and(|m| m.is_dir())
    });
    if !files_volume {
        return Err(refused(format!(
            "{dest}: a subPath of volume {volume}, which is not a Secret, ConfigMap, projected or downward-API \
             volume"
        )));
    }
    let name = Path::new(dest)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| check_relative_path("volume", n).is_ok())
        .unwrap_or("file")
        .to_string();
    match std::fs::symlink_metadata(source) {
        Ok(m) if m.is_file() => read_file(source, name, MAX_VOLUME_BYTES),
        Ok(_) => Err(refused(format!(
            "{dest}: a subPath directory (only single files of a volume are supported)"
        ))),
        Err(e) => Err(refused(format!("{dest}: {e}"))),
    }
}

/// The host's view of an `emptyDir`: kubelet mounts a tmpfs for `medium: Memory`, sized to
/// its `sizeLimit` (or the node's allocatable memory).
#[cfg(target_os = "linux")]
pub fn host_tmpfs(dir: &Path) -> Option<u64> {
    const TMPFS_MAGIC: i64 = 0x0102_1994;
    let st = rustix::fs::statfs(dir).ok()?;
    #[allow(clippy::unnecessary_cast)]
    let (kind, blocks, bsize) = (st.f_type as i64, st.f_blocks as u64, st.f_bsize as u64);
    (kind == TMPFS_MAGIC).then(|| blocks.saturating_mul(bsize))
}

#[cfg(not(target_os = "linux"))]
pub fn host_tmpfs(_dir: &Path) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;

    /// A files volume laid out as kubelet's atomic writer does.
    fn kubelet_volume(dir: &Path, generation: &str, files: &[(&str, &str)]) {
        let g = dir.join(generation);
        for (path, data) in files {
            let p = g.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, data).unwrap();
            let top = path.split('/').next().unwrap();
            let _ = symlink(Path::new(DATA).join(top), dir.join(top));
        }
        let tmp = dir.join("..data_tmp");
        symlink(generation, &tmp).unwrap();
        std::fs::rename(&tmp, dir.join(DATA)).unwrap();
    }

    #[test]
    fn reads_the_current_generation() {
        let d = tempfile::tempdir().unwrap();
        kubelet_volume(
            d.path(),
            "..2026_10_30_10_00_00.1",
            &[("app.conf", "v1"), ("tls/key", "k")],
        );
        std::fs::set_permissions(
            d.path().join("..2026_10_30_10_00_00.1/tls/key"),
            std::fs::Permissions::from_mode(0o400),
        )
        .unwrap();
        let (g, files) = read_files(d.path()).unwrap();
        assert_eq!(g, "..2026_10_30_10_00_00.1");
        let names: Vec<_> = files
            .iter()
            .map(|f| (f.path.as_str(), f.data.as_slice(), f.mode))
            .collect();
        assert_eq!(names, [("app.conf", &b"v1"[..], 0o644), ("tls/key", &b"k"[..], 0o400)]);
        // kubelet's update: a new generation and a swap of ..data.
        kubelet_volume(d.path(), "..2026_10_30_10_01_00.2", &[("app.conf", "v2")]);
        assert_eq!(
            generation(d.path()).unwrap().as_deref(),
            Some("..2026_10_30_10_01_00.2")
        );
        let (_, files) = read_files(d.path()).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].data, b"v2");
    }

    #[test]
    fn a_plain_directory_is_read_as_is() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("namespace"), "default").unwrap();
        let (g, files) = read_files(d.path()).unwrap();
        assert_eq!((g.as_str(), files.len()), ("", 1));
    }

    #[test]
    fn links_fifos_and_bad_data_links_are_refused() {
        let d = tempfile::tempdir().unwrap();
        kubelet_volume(d.path(), "..g", &[("a", "1")]);
        symlink("/etc/shadow", d.path().join("..g/evil")).unwrap();
        assert!(read_files(d.path()).is_err(), "a link inside the generation");

        let d = tempfile::tempdir().unwrap();
        symlink("../../etc", d.path().join(DATA)).unwrap();
        assert!(read_files(d.path()).is_err(), "..data pointing out");

        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("big"), vec![0u8; (MAX_VOLUME_BYTES + 1) as usize]).unwrap();
        assert!(read_files(d.path()).is_err(), "too big");
    }

    #[test]
    fn resolve_sorts_volumes_into_kinds() {
        let pods = tempfile::tempdir().unwrap();
        let pod = pods.path().join("uid");
        let cfg = pod.join("volumes/kubernetes.io~configmap/cfg");
        std::fs::create_dir_all(&cfg).unwrap();
        kubelet_volume(&cfg, "..g1", &[("nginx.conf", "server {}")]);
        let sub = pod.join("volume-subpaths/cfg/web/0");
        std::fs::create_dir_all(sub.parent().unwrap()).unwrap();
        std::fs::write(&sub, "server {}").unwrap();
        let cache = pod.join("volumes/kubernetes.io~empty-dir/cache");
        let mem = pod.join("volumes/kubernetes.io~empty-dir/mem");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::create_dir_all(&mem).unwrap();
        let vols = vec![
            ("/etc/cfg".to_string(), Source::Files(cfg.clone())),
            (
                "/etc/nginx/nginx.conf".to_string(),
                Source::SubPath {
                    pod: pod.clone(),
                    volume: "cfg".into(),
                    source: sub,
                },
            ),
            (
                "/cache".to_string(),
                Source::EmptyDir {
                    name: "cache".into(),
                    dir: cache,
                },
            ),
            (
                "/mem".to_string(),
                Source::EmptyDir {
                    name: "mem".into(),
                    dir: mem.clone(),
                },
            ),
            ("/run".to_string(), Source::Tmpfs { size: Some(1 << 20) }),
        ];
        let is_tmpfs = |p: &Path| (p == mem).then_some(64u64 << 20);
        let r = resolve(&vols, &is_tmpfs).unwrap();
        let kinds: Vec<_> = r.volumes.iter().map(|v| v.kind.clone()).collect();
        assert_eq!(
            kinds,
            [
                VolumeKind::Files { file: false },
                VolumeKind::Files { file: true },
                VolumeKind::EmptyDir { name: "cache".into() },
                VolumeKind::Tmpfs {
                    size_bytes: Some(64 << 20)
                },
                VolumeKind::Tmpfs {
                    size_bytes: Some(1 << 20)
                },
            ]
        );
        assert!(r.pod_disk);
        assert_eq!(r.files.len(), 2);
        assert_eq!((r.files[0].index, r.files[0].dir.as_deref()), (0, Some(cfg.as_path())));
        assert_eq!(r.files[1].files[0].path, "nginx.conf");
        assert_eq!(r.files[1].dir, None);
    }

    #[test]
    fn subpaths_of_other_volumes_and_directories_are_refused() {
        let pods = tempfile::tempdir().unwrap();
        let pod = pods.path().join("uid");
        std::fs::create_dir_all(pod.join("volumes/kubernetes.io~empty-dir/e")).unwrap();
        std::fs::create_dir_all(pod.join("volumes/kubernetes.io~secret/s")).unwrap();
        let file = pod.join("volume-subpaths/x/c/0");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "x").unwrap();
        let dir = pod.join("volume-subpaths/x/c/1");
        std::fs::create_dir_all(&dir).unwrap();
        for (volume, source) in [("e", &file), ("hostpath", &file), ("s", &dir)] {
            let v = vec![(
                "/x".to_string(),
                Source::SubPath {
                    pod: pod.clone(),
                    volume: volume.into(),
                    source: source.clone(),
                },
            )];
            assert!(resolve(&v, &|_| None).is_err(), "{volume} {}", source.display());
        }
    }
}
