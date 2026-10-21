//! The container's cgroup (design §8): `linux.cgroupsPath` as a cgroup v2 directory,
//! for the cgroupfs form (`/kubepods/besteffort/pod<uid>/<id>`) and the systemd form
//! (`kubepods-besteffort-pod<uid>.slice:cri-containerd:<id>`). The VM worker joins it,
//! so the VMM is accounted to the pod, whose limits include `RuntimeClass.overhead`.

use std::path::{Component, Path, PathBuf};

/// The cgroup v2 mount.
pub const ROOT: &str = "/sys/fs/cgroup";

/// `name.slice` nested in its parents, as systemd lays slices out:
/// `a-b-c.slice` is `a.slice/a-b.slice/a-b-c.slice`.
fn slice_path(slice: &str) -> Option<PathBuf> {
    let name = slice.strip_suffix(".slice")?;
    if name.is_empty() || name == "-" {
        return Some(PathBuf::new());
    }
    if name.starts_with('-') || name.ends_with('-') || name.contains("--") || name.contains('/') {
        return None;
    }
    let mut out = PathBuf::new();
    let mut prefix = String::new();
    for part in name.split('-') {
        if !prefix.is_empty() {
            prefix.push('-');
        }
        prefix.push_str(part);
        out.push(format!("{prefix}.slice"));
    }
    Some(out)
}

fn plain(rel: &Path) -> bool {
    rel.components().all(|c| matches!(c, Component::Normal(_)))
}

/// The directory under `root`, or `None` for an empty or malformed path.
pub fn resolve(cgroups_path: &str, root: &Path) -> Option<PathBuf> {
    if cgroups_path.is_empty() {
        return None;
    }
    let parts: Vec<&str> = cgroups_path.split(':').collect();
    let rel = match parts.as_slice() {
        [slice, prefix, name] if !cgroups_path.starts_with('/') => {
            if name.is_empty() || name.contains('/') || prefix.contains('/') {
                return None;
            }
            let slice = if slice.is_empty() { "system.slice" } else { slice };
            let leaf = if prefix.is_empty() {
                format!("{name}.scope")
            } else {
                format!("{prefix}-{name}.scope")
            };
            slice_path(slice)?.join(leaf)
        }
        [path] if path.starts_with('/') => PathBuf::from(&path[1..]),
        _ => return None,
    };
    (plain(&rel) && rel.components().next().is_some()).then(|| root.join(rel))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cgroupfs_and_systemd() {
        let root = Path::new("/sys/fs/cgroup");
        assert_eq!(
            resolve("/kubepods/besteffort/podu/abc", root).unwrap(),
            Path::new("/sys/fs/cgroup/kubepods/besteffort/podu/abc")
        );
        assert_eq!(
            resolve("kubepods-besteffort-podu.slice:cri-containerd:abc", root).unwrap(),
            Path::new(
                "/sys/fs/cgroup/kubepods.slice/kubepods-besteffort.slice/kubepods-besteffort-podu.slice/cri-containerd-abc.scope"
            )
        );
        assert_eq!(
            resolve("system.slice:cri-containerd:abc", root).unwrap(),
            Path::new("/sys/fs/cgroup/system.slice/cri-containerd-abc.scope")
        );
        assert_eq!(
            resolve("/default/t1", root).unwrap(),
            Path::new("/sys/fs/cgroup/default/t1")
        );
        for bad in [
            "",
            "../x",
            "/a/../b",
            "/",
            "a-.slice:p:n",
            "s.slice:p:a/b",
            "relative/path",
            "a:b",
        ] {
            assert_eq!(resolve(bad, root), None, "{bad:?}");
        }
    }
}
