//! The spec's `mounts` under kiln (design §7.2). A VM shares no filesystem with the
//! host, so a mount is something kiln-init already provides, something the shim
//! translates (the pod's resolv.conf), a kubelet volume kiln does not deliver yet
//! (ignored with a warning the pod's log shows), or a host path, which is refused.

use std::path::PathBuf;

use crate::error::{Error, Result};
use crate::oci::{Mount, Spec};

/// What kiln does with one mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountClass {
    /// kiln-init mounts its own (`/proc`, `/dev`, `/sys`, ...).
    Provided,
    /// The pod's resolv.conf, read for the guest's DNS (design §6.5).
    ResolvConf(PathBuf),
    /// kiln-init writes its own (`/etc/hosts`, `/etc/hostname`).
    Kiln,
    /// A kubelet volume or file kiln does not deliver yet (kiln-shim spec §4.5).
    NotDelivered,
    /// A host path the guest cannot see: hostPath, CSI and the like.
    Refused,
}

/// What `check` found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Checked {
    pub resolv_conf: Option<PathBuf>,
    /// The destinations of the volumes the guest does not get, for the warning.
    pub not_delivered: Vec<String>,
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
/// kubelet's volume plugins whose files are small and are delivered later (spec §4.5).
const KUBELET_VOLUMES: [&str; 5] = ["projected", "secret", "configmap", "downward-api", "empty-dir"];

fn kubelet_volume(source: &str) -> bool {
    let Some(rest) = source.strip_prefix("/var/lib/kubelet/pods/") else {
        return false;
    };
    // <uid>/volumes/kubernetes.io~<plugin>/<name>, or a file kubelet writes for the
    // container (<uid>/containers/<name>/<id>: the termination log).
    let mut parts = rest.split('/');
    let _uid = parts.next();
    match (parts.next(), parts.next()) {
        (Some("volumes"), Some(plugin)) => plugin
            .strip_prefix("kubernetes.io~")
            .is_some_and(|p| KUBELET_VOLUMES.contains(&p)),
        (Some("containers"), Some(_)) => true,
        _ => false,
    }
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
        "/dev/termination-log" => MountClass::NotDelivered,
        _ if !bind && m.kind.as_deref().is_some_and(|k| KERNEL_FS.contains(&k)) => MountClass::NotDelivered,
        _ if kubelet_volume(source) => MountClass::NotDelivered,
        _ => MountClass::Refused,
    }
}

/// Classifies every mount; the first host path refuses the task, naming it.
pub fn check(spec: &Spec) -> Result<Checked> {
    let mut out = Checked::default();
    for m in &spec.mounts {
        match classify(m) {
            MountClass::Provided | MountClass::Kiln => {}
            MountClass::ResolvConf(p) => out.resolv_conf = Some(p),
            MountClass::NotDelivered => out.not_delivered.push(m.destination.clone()),
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

/// The line the pod's log gets about volumes the guest does not have.
pub fn warning(not_delivered: &[String]) -> Option<String> {
    (!not_delivered.is_empty()).then(|| {
        format!(
            "kiln: warning: these volumes are not inside the VM yet (kiln-shim spec §4.5): {}",
            not_delivered.join(", ")
        )
    })
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
            (fs("/run", "tmpfs"), MountClass::NotDelivered),
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
                MountClass::NotDelivered,
            ),
            (
                bind("/cfg", &format!("{pod}/volumes/kubernetes.io~configmap/c")),
                MountClass::NotDelivered,
            ),
            (
                bind("/cache", &format!("{pod}/volumes/kubernetes.io~empty-dir/e")),
                MountClass::NotDelivered,
            ),
            (
                bind("/dev/termination-log", &format!("{pod}/containers/app/5d1c")),
                MountClass::NotDelivered,
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
        assert_eq!(c.not_delivered, ["/var/run/secrets/kubernetes.io/serviceaccount"]);
        assert!(warning(&c.not_delivered).unwrap().contains("serviceaccount"));
        assert_eq!(warning(&[]), None);
    }
}
