//! The OCI image-spec structures kiln reads and writes (unknown fields are ignored).

use std::collections::BTreeMap;

use kiln_store::Digest;
use serde::{Deserialize, Serialize};

use crate::platform::Platform;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Descriptor {
    pub media_type: String,
    pub digest: Digest,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub urls: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<Platform>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_type: Option<String>,
}

impl Descriptor {
    pub fn new(media_type: &str, digest: Digest, size: u64) -> Self {
        Self {
            media_type: media_type.to_string(),
            digest,
            size,
            urls: None,
            annotations: None,
            platform: None,
            artifact_type: None,
        }
    }

    pub fn annotation(&self, key: &str) -> Option<&str> {
        self.annotations.as_ref()?.get(key).map(String::as_str)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageIndex {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_type: Option<String>,
    pub manifests: Vec<Descriptor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageManifest {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_type: Option<String>,
    pub config: Descriptor,
    pub layers: Vec<Descriptor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<BTreeMap<String, String>>,
}

/// The parts of an OCI image config kiln uses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageConfig {
    pub architecture: String,
    pub os: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<ContainerConfig>,
    pub rootfs: RootFs,
}

impl ImageConfig {
    pub fn platform(&self) -> Platform {
        Platform {
            os: self.os.clone(),
            architecture: self.architecture.clone(),
            variant: self.variant.clone(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ContainerConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cmd: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_signal: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootFs {
    #[serde(rename = "type")]
    pub fs_type: String,
    pub diff_ids: Vec<Digest>,
}

/// JSON with sorted keys and no insignificant whitespace (spec §6.6).
pub fn canonical_json<T: Serialize>(value: &T) -> Vec<u8> {
    let v = serde_json::to_value(value).expect("serializable value");
    serde_json::to_vec(&v).expect("json value serializes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_json_sorts_keys_and_is_compact() {
        let d = Descriptor::new("x", Digest::of(b"a"), 3);
        let s = String::from_utf8(canonical_json(&d)).unwrap();
        assert!(s.starts_with("{\"digest\":"), "{s}");
        assert!(!s.contains(' '));
    }

    #[test]
    fn parses_docker_style_config() {
        let json = br#"{"architecture":"arm64","os":"linux","config":{"Env":["PATH=/bin"],"Cmd":["sh"],"WorkingDir":"/w","User":"1000:1000","StopSignal":"SIGQUIT","Labels":{"a":"b"}},"rootfs":{"type":"layers","diff_ids":[]},"history":[]}"#;
        let c: ImageConfig = serde_json::from_slice(json).unwrap();
        let cc = c.config.unwrap();
        assert_eq!(cc.cmd.unwrap(), vec!["sh"]);
        assert_eq!(cc.stop_signal.as_deref(), Some("SIGQUIT"));
        assert_eq!(cc.working_dir.as_deref(), Some("/w"));
    }
}
