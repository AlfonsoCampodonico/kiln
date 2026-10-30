//! The part of an OCI runtime spec (a bundle's `config.json`) the shim reads, and the
//! CRI annotations that say what a task is (design §2, §4). Unknown fields are ignored:
//! the spec is containerd's, and most of it has no meaning inside a VM.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{Error, Result};

/// `sandbox` for the pod's pause container, `container` for the others.
pub const CONTAINER_TYPE: &str = "io.kubernetes.cri.container-type";
/// The pod: every task of one pod carries its sandbox's id.
pub const SANDBOX_ID: &str = "io.kubernetes.cri.sandbox-id";
/// The pod's uid, which names its directory under kubelet's pods.
pub const SANDBOX_UID: &str = "io.kubernetes.cri.sandbox-uid";

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Spec {
    #[serde(default)]
    pub process: Option<Process>,
    #[serde(default)]
    pub hostname: Option<String>,
    #[serde(default)]
    pub mounts: Vec<Mount>,
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
    #[serde(default)]
    pub linux: Option<Linux>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Process {
    #[serde(default)]
    pub terminal: bool,
    #[serde(default)]
    pub user: User,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub cwd: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct User {
    #[serde(default)]
    pub uid: u32,
    #[serde(default)]
    pub gid: u32,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Mount {
    pub destination: String,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub options: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Linux {
    #[serde(default)]
    pub namespaces: Vec<Namespace>,
    #[serde(default)]
    pub cgroups_path: Option<String>,
    #[serde(default)]
    pub resources: Option<Resources>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Namespace {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub path: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Resources {
    #[serde(default)]
    pub cpu: Option<Cpu>,
    #[serde(default)]
    pub memory: Option<Memory>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Cpu {
    #[serde(default)]
    pub quota: Option<i64>,
    #[serde(default)]
    pub period: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Memory {
    #[serde(default)]
    pub limit: Option<i64>,
}

/// What a task is to the shim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskKind {
    /// A pod's pause container: never a VM (design §4).
    Sandbox,
    /// A container that boots its own VM, in the pod `sandbox_id` when CRI made it.
    Container { sandbox_id: Option<String> },
}

/// The bundle's `config.json`.
pub fn read_spec(bundle: &Path) -> Result<Spec> {
    let text = std::fs::read(bundle.join("config.json"))?;
    serde_json::from_slice(&text).map_err(|e| Error::invalid(format!("the bundle's config.json: {e}")))
}

impl Spec {
    pub fn kind(&self) -> TaskKind {
        match self.annotations.get(CONTAINER_TYPE).map(String::as_str) {
            Some("sandbox") => TaskKind::Sandbox,
            _ => TaskKind::Container {
                sandbox_id: self.annotations.get(SANDBOX_ID).cloned(),
            },
        }
    }

    /// The pod's id: what one shim process serves.
    /// The pod's uid (CRI's annotation).
    pub fn pod_uid(&self) -> Option<&str> {
        self.annotations.get(SANDBOX_UID).map(String::as_str)
    }

    pub fn group(&self) -> Option<&str> {
        self.annotations.get(SANDBOX_ID).map(String::as_str)
    }

    fn namespace(&self, kind: &str) -> Option<&Namespace> {
        self.linux.as_ref()?.namespaces.iter().find(|n| n.kind == kind)
    }

    /// The network namespace the task joins: the CNI's pod netns, or for a container
    /// the sandbox's (`/proc/<pause pid>/ns/net`). `None` when the spec has no network
    /// namespace (the host's) or a new, unnamed one.
    pub fn netns(&self) -> Option<&Path> {
        self.namespace("network")?.path.as_deref()
    }

    /// The spec puts the task in the host's network namespace (`hostNetwork: true`).
    pub fn host_network(&self) -> bool {
        self.linux.is_some() && self.namespace("network").is_none()
    }

    pub fn cgroups_path(&self) -> Option<&str> {
        self.linux.as_ref()?.cgroups_path.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn spec(json: &str) -> Spec {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn cri_kinds() {
        let sandbox = spec(
            r#"{"annotations":{"io.kubernetes.cri.container-type":"sandbox","io.kubernetes.cri.sandbox-id":"s1"}}"#,
        );
        assert_eq!(sandbox.kind(), TaskKind::Sandbox);
        assert_eq!(sandbox.group(), Some("s1"));
        let container = spec(
            r#"{"annotations":{"io.kubernetes.cri.container-type":"container","io.kubernetes.cri.sandbox-id":"s1"}}"#,
        );
        assert_eq!(
            container.kind(),
            TaskKind::Container {
                sandbox_id: Some("s1".into())
            }
        );
        let ctr = spec(r#"{"ociVersion":"1.2.0","process":{"args":["sh"]}}"#);
        assert_eq!(ctr.kind(), TaskKind::Container { sandbox_id: None });
        assert_eq!(ctr.group(), None);
    }

    #[test]
    fn network_namespaces() {
        let pod = spec(r#"{"linux":{"namespaces":[{"type":"pid"},{"type":"network","path":"/var/run/netns/cni-1"}]}}"#);
        assert_eq!(pod.netns(), Some(Path::new("/var/run/netns/cni-1")));
        assert!(!pod.host_network());
        let new = spec(r#"{"linux":{"namespaces":[{"type":"network"}]}}"#);
        assert_eq!(new.netns(), None);
        assert!(!new.host_network());
        let host = spec(r#"{"linux":{"namespaces":[{"type":"pid"}]}}"#);
        assert!(host.host_network());
    }
}
