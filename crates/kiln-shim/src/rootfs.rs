//! A container's rootfs from containerd's erofs snapshotter (design §5): one erofs
//! layer file per image layer, which the VM attaches as read-only disks and kiln-init
//! stacks with its own overlay. containerd hands the mounts over unmounted because the
//! shim's `-info` says it handles `erofs`, `format/*` and `mkdir/*` itself.

use std::path::PathBuf;

use crate::error::{Error, Result};

/// One entry of `CreateTaskRequest.rootfs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootfsMount {
    pub kind: String,
    pub source: String,
    pub options: Vec<String>,
}

fn wrong_snapshotter(what: &str) -> Error {
    Error::invalid(format!(
        "kiln needs containerd's erofs snapshotter (snapshotter = \"erofs\" on the kiln runtime), \
         but the rootfs is {what}"
    ))
}

/// The layer files, lowest first. containerd sends them top first, between an
/// optional block-mode `mkfs/*` upper (kiln has its own scratch disk) and the overlay
/// that would stack them on the host (kiln-init builds its own).
pub fn layers(mounts: &[RootfsMount]) -> Result<Vec<PathBuf>> {
    if mounts.is_empty() {
        return Err(wrong_snapshotter("empty (a pre-mounted bundle)"));
    }
    let mut rest = mounts;
    while let Some((m, tail)) = rest.split_first()
        && m.kind.starts_with("mkfs/")
    {
        rest = tail;
    }
    let mut top_first = Vec::new();
    for (i, m) in rest.iter().enumerate() {
        match m.kind.as_str() {
            "erofs" => {
                if m.options.iter().any(|o| o.starts_with("device=")) {
                    return Err(Error::invalid(
                        "the image's layers are fsmerged into one erofs mount with devices; kiln attaches one \
                         disk per layer (turn off the erofs differ's fsmerge for the kiln runtime)",
                    ));
                }
                if !m.source.starts_with('/') {
                    return Err(Error::invalid(format!(
                        "erofs layer {:?} is not an absolute path",
                        m.source
                    )));
                }
                top_first.push(PathBuf::from(&m.source));
            }
            k if k.ends_with("overlay") && i + 1 == rest.len() && !top_first.is_empty() => {}
            k => return Err(wrong_snapshotter(&format!("a {k:?} mount from {}", m.source))),
        }
    }
    if top_first.is_empty() {
        return Err(wrong_snapshotter("not erofs"));
    }
    top_first.reverse();
    Ok(top_first)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(kind: &str, source: &str, options: &[&str]) -> RootfsMount {
        RootfsMount {
            kind: kind.into(),
            source: source.into(),
            options: options.iter().map(|o| o.to_string()).collect(),
        }
    }

    const A: &str = "/var/lib/containerd/io.containerd.snapshotter.v1.erofs/snapshots/9/layer.erofs";
    const B: &str = "/var/lib/containerd/io.containerd.snapshotter.v1.erofs/snapshots/4/layer.erofs";

    #[test]
    fn erofs_layers_of_containerd_2_2() {
        let mounts = [
            m("mkfs/ext4", "/s/rw.img", &["X-containerd.mkfs.fs=ext4", "rw", "loop"]),
            m("erofs", A, &["ro", "loop"]),
            m("erofs", B, &["ro", "loop"]),
            m(
                "format/mkdir/overlay",
                "overlay",
                &[
                    "workdir={{ mount 0 }}/work",
                    "upperdir={{ mount 0 }}/upper",
                    "lowerdir={{ overlay 1 2 }}",
                ],
            ),
        ];
        assert_eq!(layers(&mounts).unwrap(), [PathBuf::from(B), PathBuf::from(A)]);
    }

    #[test]
    fn a_single_erofs_layer() {
        assert_eq!(layers(&[m("erofs", A, &["ro", "loop"])]).unwrap(), [PathBuf::from(A)]);
    }

    #[test]
    fn a_dm_verity_option_is_accepted() {
        let mounts = [m("erofs", A, &["ro", "loop", "X-containerd.dmverity=roothash"])];
        assert_eq!(layers(&mounts).unwrap(), [PathBuf::from(A)]);
    }

    #[test]
    fn other_snapshotters_are_refused() {
        let cases = [
            vec![m(
                "overlay",
                "overlay",
                &["lowerdir=/x:/y", "upperdir=/u", "workdir=/w"],
            )],
            vec![],
            vec![m("bind", "/run/containerd/rootfs", &["rbind"])],
            vec![m("erofs", A, &[]), m("bind", "/x", &[])],
            vec![m("format/mkdir/overlay", "overlay", &[]), m("erofs", A, &[])],
        ];
        for mounts in cases {
            let err = layers(&mounts).unwrap_err().to_string();
            assert!(err.contains("snapshotter = \"erofs\""), "{mounts:?}: {err}");
        }
        let fsmerged = [m("erofs", A, &["ro", "loop", "device=/s/layer1.erofs"])];
        assert!(layers(&fsmerged).unwrap_err().to_string().contains("fsmerge"));
        assert!(layers(&[m("erofs", "layer.erofs", &[])]).is_err());
    }
}
