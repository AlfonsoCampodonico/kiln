//! The kiln image format (spec §5.1).

use kiln_oci::ContainerConfig;
use kiln_store::Digest;
use serde::{Deserialize, Serialize};

pub const KILN_ARTIFACT: &str = "application/vnd.kiln.image.v1";
pub const KILN_CONFIG: &str = "application/vnd.kiln.image.config.v1+json";
pub const KILN_LAYER: &str = "application/vnd.kiln.layer.v1.erofs";
pub const KILN_KERNEL: &str = "application/vnd.kiln.kernel.v1";
pub const KILN_INIT: &str = "application/vnd.kiln.init.v1.erofs";

/// Comma-separated OCI layer digests an erofs layer was built from (informational).
pub const ANN_SOURCE_DIGESTS: &str = "dev.kiln.source.digests";
/// `"true"` on layers converted with inherited parent attributes (informational).
pub const ANN_INHERITS: &str = "dev.kiln.inherits";

pub const SCHEMA_VERSION: u32 = 1;

/// The `kiln` image config blob.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KilnConfig {
    pub schema_version: u32,
    pub architecture: String,
    pub process: Process,
    /// Set by milestone M3, when kernel layers are added.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel: Option<KernelRef>,
    /// Set by milestone M3, when init layers are added.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub init: Option<InitRef>,
    pub source: SourceRef,
    pub erofs_format_version: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Process {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entrypoint: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cmd: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_signal: Option<String>,
}

impl Process {
    pub fn from_oci(c: Option<&ContainerConfig>) -> Self {
        let Some(c) = c else { return Self::default() };
        Self {
            entrypoint: c.entrypoint.clone().unwrap_or_default(),
            cmd: c.cmd.clone().unwrap_or_default(),
            env: c.env.clone().unwrap_or_default(),
            working_dir: c.working_dir.clone().filter(|w| !w.is_empty()),
            user: c.user.clone().filter(|u| !u.is_empty()),
            stop_signal: c.stop_signal.clone().filter(|s| !s.is_empty()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KernelRef {
    pub profile: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitRef {
    pub version: String,
}

/// Where an image came from. `reference` is set only for registry inputs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceRef {
    pub manifest_digest: Digest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
}

/// Lowercase hex of raw bytes (paths and xattrs are not always UTF-8).
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_from_oci_drops_empty_strings() {
        let c = ContainerConfig {
            cmd: Some(vec!["php".into()]),
            working_dir: Some(String::new()),
            user: Some("www-data".into()),
            ..Default::default()
        };
        let p = Process::from_oci(Some(&c));
        assert_eq!(p.cmd, vec!["php"]);
        assert_eq!(p.working_dir, None);
        assert_eq!(p.user.as_deref(), Some("www-data"));
        assert_eq!(Process::from_oci(None), Process::default());
    }

    #[test]
    fn hex_round_trips() {
        assert_eq!(hex(b"\x00\xffa"), "00ff61");
        assert_eq!(unhex("00ff61").unwrap(), b"\x00\xffa");
        assert!(unhex("0").is_none() && unhex("zz").is_none());
    }

    #[test]
    fn config_omits_unset_kernel_and_init() {
        let c = KilnConfig {
            schema_version: 1,
            architecture: "arm64".into(),
            process: Process::default(),
            kernel: None,
            init: None,
            source: SourceRef {
                manifest_digest: Digest::of(b"m"),
                reference: None,
            },
            erofs_format_version: 1,
        };
        let s = String::from_utf8(kiln_oci::canonical_json(&c)).unwrap();
        assert!(
            !s.contains("kernel") && !s.contains("init") && !s.contains("reference"),
            "{s}"
        );
        assert!(s.contains("\"erofsFormatVersion\":1"));
    }
}
