//! The spec's `mounts` under kiln (design §7.2, and the exec and volumes design §5.1). A
//! VM shares no filesystem with the host, so a mount is something kiln-init already
//! provides, something the shim translates (the pod's resolv.conf), a kubelet volume
//! the shim delivers into the guest (files, an `emptyDir`, a tmpfs), or a host path,
//! which is refused.

use std::path::PathBuf;

use crate::error::{Error, Result};
use crate::oci::{Mount, Spec};

/// Where kubelet keeps its pods' volumes.
pub const KUBELET_PODS: &str = "/var/lib/kubelet/pods";

/// What kiln does with one mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountClass {
    /// kiln-init mounts its own (`/proc`, `/dev`, `/sys`, ...).
    Provided,
    /// The pod's resolv.conf, read for the guest's DNS (design §6.5).
    ResolvConf(PathBuf),
    /// kiln-init writes its own (`/etc/hosts`, `/etc/hostname`).
    Kiln,
    /// Not delivered, and not worth a warning (the termination log).
    Ignored,
    /// A kubelet volume or a tmpfs, delivered into the guest.
    Volume(Source),
    /// A host path the guest cannot see: hostPath, CSI and the like.
    Refused,
}

/// Where a delivered volume comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A Secret, ConfigMap, projected or downward-API volume's directory.
    Files(PathBuf),
    /// An `emptyDir`'s directory: on disk, or (kubelet's `medium: Memory`) a tmpfs.
    EmptyDir { name: String, dir: PathBuf },
    /// A `subPath` inside volume `volume` of the pod whose directory is `pod`.
    SubPath {
        pod: PathBuf,
        volume: String,
        source: PathBuf,
    },
    /// A tmpfs of the OCI spec, with its `size=` when it has one.
    Tmpfs { size: Option<u64> },
}

/// What `check` found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Checked {
    pub resolv_conf: Option<PathBuf>,
    /// The volumes to deliver, with their destinations.
    pub volumes: Vec<(String, Source)>,
}

const PROVIDED: [&str; 7] = [
    "/proc",
    "/dev",
    "/dev/pts",
    "/dev/shm",
    "/dev/mqueue",
    "/sys",
    "/sys/fs/cgroup",
];
/// Filesystems the kernel makes, with no host path behind them.
const KERNEL_FS: [&str; 8] = [
    "proc", "sysfs", "tmpfs", "devpts", "mqueue", "cgroup", "cgroup2", "devtmpfs",
];
/// kubelet's volume plugins whose content is files in its atomic-writer layout.
pub const FILES_PLUGINS: [&str; 4] = ["projected", "secret", "configmap", "downward-api"];

/// `/var/lib/kubelet/pods/<uid>/<rest>`: the pod's directory and `<rest>`'s parts.
fn kubelet_path(source: &str) -> Option<(PathBuf, Vec<&str>)> {
    let rest = source.strip_prefix(KUBELET_PODS)?.strip_prefix('/')?;
    let mut parts = rest.split('/');
    let uid = parts.next().filter(|u| !u.is_empty() && *u != "." && *u != "..")?;
    let parts: Vec<&str> = parts.collect();
    if parts.iter().any(|p| p.is_empty() || *p == "." || *p == "..") {
        return None;
    }
    Some((PathBuf::from(KUBELET_PODS).join(uid), parts))
}

fn kubelet_volume(source: &str) -> Option<MountClass> {
    let (pod, parts) = kubelet_path(source)?;
    match parts.as_slice() {
        ["volumes", plugin, name] => match plugin.strip_prefix("kubernetes.io~")? {
            p if FILES_PLUGINS.contains(&p) => Some(MountClass::Volume(Source::Files(source.into()))),
            "empty-dir" => Some(MountClass::Volume(Source::EmptyDir {
                name: name.to_string(),
                dir: source.into(),
            })),
            _ => None,
        },
        ["volume-subpaths", volume, _container, _n] => Some(MountClass::Volume(Source::SubPath {
            pod,
            volume: volume.to_string(),
            source: source.into(),
        })),
        // A file kubelet writes for the container: the termination log.
        ["containers", ..] => Some(MountClass::Ignored),
        _ => None,
    }
}

/// A tmpfs mount's `size=` option, in bytes (`k`, `m` and `g` suffixes, as mount(8)).
fn tmpfs_size(options: &[String]) -> Option<u64> {
    let v = options.iter().find_map(|o| o.strip_prefix("size="))?;
    let (num, mul) = match v.as_bytes().last()? {
        b'k' | b'K' => (&v[..v.len() - 1], 1 << 10),
        b'm' | b'M' => (&v[..v.len() - 1], 1 << 20),
        b'g' | b'G' => (&v[..v.len() - 1], 1 << 30),
        _ => (v, 1),
    };
    num.parse::<u64>().ok()?.checked_mul(mul).filter(|n| *n > 0)
}

pub fn classify(m: &Mount) -> MountClass {
    let dest = m.destination.trim_end_matches('/');
    let source = m.source.as_deref().unwrap_or("");
    let bind = m.kind.as_deref().is_none_or(|k| k == "bind" || k == "rbind")
        || m.options.iter().any(|o| o == "bind" || o == "rbind");
    match dest {
        "/etc/resolv.conf" if bind && !source.is_empty() => MountClass::ResolvConf(source.into()),
        "/etc/hosts" | "/etc/hostname" => MountClass::Kiln,
        d if PROVIDED.contains(&d) => MountClass::Provided,
        "/dev/termination-log" => MountClass::Ignored,
        // kiln-init provides /proc, /sys and /dev; nothing more is mounted there.
        d if ["/proc/", "/sys/", "/dev/"].iter().any(|p| d.starts_with(p)) => MountClass::Refused,
        _ if !bind && m.kind.as_deref() == Some("tmpfs") => MountClass::Volume(Source::Tmpfs {
            size: tmpfs_size(&m.options),
        }),
        _ if !bind && m.kind.as_deref().is_some_and(|k| KERNEL_FS.contains(&k)) => MountClass::Refused,
        _ => kubelet_volume(source).unwrap_or(MountClass::Refused),
    }
}

/// Classifies every mount; the first host path refuses the task, naming it.
pub fn check(spec: &Spec) -> Result<Checked> {
    let mut out = Checked::default();
    for m in &spec.mounts {
        match classify(m) {
            MountClass::Provided | MountClass::Kiln | MountClass::Ignored => {}
            MountClass::ResolvConf(p) => out.resolv_conf = Some(p),
            MountClass::Volume(source) => out.volumes.push((m.destination.clone(), source)),
            MountClass::Refused => {
                return Err(Error::invalid(format!(
                    "kiln cannot mount {} into a VM (from {}): host paths, hostPath and CSI volumes are \
                     not supported",
                    m.destination,
                    m.source.as_deref().unwrap_or("?")
                )));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bind(dest: &str, source: &str) -> Mount {
        Mount {
            destination: dest.into(),
            kind: Some("bind".into()),
            source: Some(source.into()),
            options: vec!["rbind".into(), "ro".into()],
        }
    }

    fn fs(dest: &str, kind: &str) -> Mount {
        Mount {
            destination: dest.into(),
            kind: Some(kind.into()),
            source: Some(kind.into()),
            options: Vec::new(),
        }
    }

    #[test]
    fn classes() {
        let pod = "/var/lib/kubelet/pods/0e1f";
        let cases = [
            (fs("/proc", "proc"), MountClass::Provided),
            (fs("/dev", "tmpfs"), MountClass::Provided),
            (fs("/sys/fs/cgroup", "cgroup"), MountClass::Provided),
            (
                bind("/dev/shm", "/run/containerd/io.containerd.grpc.v1.cri/sandboxes/s/shm"),
                MountClass::Provided,
            ),
            (fs("/run", "tmpfs"), MountClass::Volume(Source::Tmpfs { size: None })),
            (fs("/run/x", "proc"), MountClass::Refused),
            (bind("/etc/hosts", &format!("{pod}/etc-hosts")), MountClass::Kiln),
            (
                bind("/etc/hostname", "/var/lib/containerd/x/hostname"),
                MountClass::Kiln,
            ),
            (
                bind("/etc/resolv.conf", "/var/lib/containerd/x/resolv.conf"),
                MountClass::ResolvConf("/var/lib/containerd/x/resolv.conf".into()),
            ),
            (
                bind(
                    "/var/run/secrets/kubernetes.io/serviceaccount",
                    &format!("{pod}/volumes/kubernetes.io~projected/kube-api-access-x"),
                ),
                MountClass::Volume(Source::Files(
                    format!("{pod}/volumes/kubernetes.io~projected/kube-api-access-x").into(),
                )),
            ),
            (
                bind("/cfg", &format!("{pod}/volumes/kubernetes.io~configmap/c")),
                MountClass::Volume(Source::Files(format!("{pod}/volumes/kubernetes.io~configmap/c").into())),
            ),
            (
                bind("/cache", &format!("{pod}/volumes/kubernetes.io~empty-dir/e")),
                MountClass::Volume(Source::EmptyDir {
                    name: "e".into(),
                    dir: format!("{pod}/volumes/kubernetes.io~empty-dir/e").into(),
                }),
            ),
            (
                bind("/etc/nginx/nginx.conf", &format!("{pod}/volume-subpaths/cfg/web/0")),
                MountClass::Volume(Source::SubPath {
                    pod: pod.into(),
                    volume: "cfg".into(),
                    source: format!("{pod}/volume-subpaths/cfg/web/0").into(),
                }),
            ),
            (
                bind("/dev/termination-log", &format!("{pod}/containers/app/5d1c")),
                MountClass::Ignored,
            ),
            (
                bind("/x", &format!("{pod}/volumes/kubernetes.io~configmap/../../etc")),
                MountClass::Refused,
            ),
            (
                bind("/x", "/var/lib/kubelet/pods/../../etc/volumes/kubernetes.io~secret/s"),
                MountClass::Refused,
            ),
            (
                bind("/dev/x", &format!("{pod}/volumes/kubernetes.io~configmap/c")),
                MountClass::Refused,
            ),
            (
                bind("/data", &format!("{pod}/volumes/kubernetes.io~csi/pvc-1/mount")),
                MountClass::Refused,
            ),
            (bind("/data", "/srv/data"), MountClass::Refused),
        ];
        for (m, want) in cases {
            assert_eq!(classify(&m), want, "{m:?}");
        }
    }

    #[test]
    fn check_refuses_naming_the_destination() {
        let spec = Spec {
            mounts: vec![fs("/proc", "proc"), bind("/data", "/srv/data")],
            ..Spec::default()
        };
        let err = check(&spec).unwrap_err().to_string();
        assert!(err.contains("/data") && err.contains("/srv/data"), "{err}");
        let spec = Spec {
            mounts: vec![
                bind("/etc/resolv.conf", "/r"),
                bind(
                    "/var/run/secrets/kubernetes.io/serviceaccount",
                    "/var/lib/kubelet/pods/u/volumes/kubernetes.io~projected/t",
                ),
            ],
            ..Spec::default()
        };
        let c = check(&spec).unwrap();
        assert_eq!(c.resolv_conf.as_deref(), Some(std::path::Path::new("/r")));
        assert_eq!(c.volumes.len(), 1);
        assert_eq!(c.volumes[0].0, "/var/run/secrets/kubernetes.io/serviceaccount");
    }

    #[test]
    fn tmpfs_sizes() {
        let o = |s: &str| vec!["nosuid".to_string(), s.to_string()];
        assert_eq!(tmpfs_size(&o("size=64m")), Some(64 << 20));
        assert_eq!(tmpfs_size(&o("size=1G")), Some(1 << 30));
        assert_eq!(tmpfs_size(&o("size=4096")), Some(4096));
        assert_eq!(tmpfs_size(&o("size=0")), None);
        assert_eq!(tmpfs_size(&o("size=x")), None);
        assert_eq!(tmpfs_size(&o("mode=1777")), None);
    }
}
