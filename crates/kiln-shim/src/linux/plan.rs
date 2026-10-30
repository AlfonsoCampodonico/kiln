//! What a VM worker is told, as the first line of its channel (design §7.4).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::netconf::PodNet;
use crate::passwd::Account;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerPlan {
    pub task_id: String,
    pub account: Account,
    pub kernel: PathBuf,
    /// The static kiln-init binary.
    pub init: PathBuf,
    /// `firecracker` or `cloud-hypervisor`.
    pub vmm: String,
    pub cpus: u8,
    pub memory_mib: u32,
    pub boot_timeout_secs: u64,
    /// The erofs layer files, lowest first (bind-mounted into `vm_dir/layers`).
    pub layers: Vec<PathBuf>,
    pub process: kiln_proto::Process,
    pub hostname: String,
    pub tty: bool,
    pub interactive: bool,
    /// The pod netns the worker joins, and its attachment (the guest's NIC).
    pub netns: Option<PathBuf>,
    pub pod: Option<PodNet>,
    pub network: Option<kiln_proto::Network>,
    pub cgroup: Option<PathBuf>,
    /// The worker's own directory (the kiln user's): layer mount points, the init
    /// layer and the VMM's run directory.
    pub vm_dir: PathBuf,
    /// The scratch disk's directory, on the store's filesystem.
    pub scratch_dir: PathBuf,
    pub scratch_bytes: u64,
    /// Lines for the container's log before the VM starts.
    pub warnings: Vec<String>,
    /// The guest's volumes, and the files volumes' first generation by volume index.
    #[serde(default)]
    pub volumes: Vec<kiln_proto::Volume>,
    #[serde(default)]
    pub files: Vec<(u32, Vec<kiln_proto::VolumeFile>)>,
    /// The pod disk's image (the kiln user's, created on first use) and its size.
    #[serde(default)]
    pub pod_disk: Option<PathBuf>,
    #[serde(default)]
    pub pod_disk_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_plan_round_trip() {
        let plan = WorkerPlan {
            task_id: "c1".into(),
            account: Account {
                name: "kiln".into(),
                uid: 991,
                gid: 990,
                groups: vec![990, 992],
                home: "/var/lib/kiln".into(),
            },
            kernel: "/k".into(),
            init: "/i".into(),
            vmm: "firecracker".into(),
            cpus: 1,
            memory_mib: 1024,
            boot_timeout_secs: 30,
            layers: vec!["/l0".into()],
            process: kiln_proto::Process {
                entrypoint: vec!["echo".into()],
                ..Default::default()
            },
            hostname: "web-0".into(),
            tty: false,
            interactive: false,
            netns: Some("/proc/1/ns/net".into()),
            pod: None,
            network: None,
            cgroup: None,
            vm_dir: "/run/kiln-shim/vm/k8s.io-c1".into(),
            scratch_dir: "/var/lib/kiln/shim-scratch/k8s.io-c1".into(),
            scratch_bytes: 4 << 30,
            warnings: vec!["kiln: warning: x".into()],
            volumes: vec![kiln_proto::Volume {
                path: "/cache".into(),
                kind: kiln_proto::VolumeKind::EmptyDir { name: "cache".into() },
            }],
            files: Vec::new(),
            pod_disk: Some("/var/lib/kiln/shim-scratch/k8s.io-pod-s1/pod.img".into()),
            pod_disk_bytes: 1 << 30,
        };
        let json = serde_json::to_string(&plan).unwrap();
        assert_eq!(serde_json::from_str::<WorkerPlan>(&json).unwrap(), plan);
    }
}
