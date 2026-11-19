//! The kiln image format (spec §5.1).

use std::collections::BTreeMap;

use kiln_oci::ContainerConfig;
use kiln_proto::Ready;
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

/// The OCI image-config label whose JSON value lists the image's services
/// ([`Service`]); `kiln convert` copies them into [`KilnConfig::services`].
pub const LABEL_SERVICES: &str = "dev.kiln.services";

/// The config's `schemaVersion` for an image without services.
pub const SCHEMA_VERSION: u32 = 1;
/// The config's `schemaVersion` for an image with services: a kiln older than
/// M3d accepts only [`SCHEMA_VERSION`], so it refuses such an image instead of
/// running it without its services.
pub const SCHEMA_VERSION_SERVICES: u32 = 2;

/// The `schemaVersion` a config with these services is written with, and the
/// only one a reader accepts for it.
pub fn schema_version_for(services: &[Service]) -> u32 {
    if services.is_empty() {
        SCHEMA_VERSION
    } else {
        SCHEMA_VERSION_SERVICES
    }
}

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
    /// The source config's labels, written even when empty by every kiln that
    /// records them: `None` is a config from a kiln before labels (spec §5.1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<BTreeMap<String, String>>,
    /// The base/app boundary, set by `convert --base` (spec §16.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<BaseRecord>,
    /// Processes `kiln-init` starts, in order, before the main process (spec
    /// §9.8, protocol version 2). Omitted when empty. A config with services
    /// has `schemaVersion` 2 ([`schema_version_for`]), which a kiln older than
    /// M3d refuses.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<Service>,
}

/// One service: the shape of `dev.kiln.services` entries and of
/// `KilnConfig.services`. Unknown fields are refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Service {
    /// `[a-z0-9-]{1,32}`, unique in the image.
    pub name: String,
    pub process: ServiceProcess,
    /// `unix:<absolute path>`, `tcp:<port>` or `none` (the default, omitted).
    #[serde(default = "ready_none", skip_serializing_if = "Ready::is_none")]
    pub ready: Ready,
    /// A signal name or number, as `STOPSIGNAL` (default SIGTERM, omitted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_signal: Option<String>,
}

fn ready_none() -> Ready {
    Ready::None
}

/// A service's process: the main process's fields without `stopSignal`. Run-time
/// `--env` values are added to every process's environment, services' included.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ServiceProcess {
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
}

impl Service {
    /// The service as `Config` carries it, with `extra_env` appended to its
    /// environment (run-time `--env`). Checks only what the conversion needs
    /// (the stop signal); [`validate_services`] checks the rest.
    pub fn to_proto(&self, extra_env: &[String]) -> std::result::Result<kiln_proto::Service, String> {
        let stop_signal = match &self.stop_signal {
            Some(s) => kiln_proto::signal::parse_stop_signal(s).map_err(|e| format!("service {}: {e}", self.name))?,
            None => kiln_proto::signal::DEFAULT_STOP,
        };
        let p = &self.process;
        Ok(kiln_proto::Service {
            name: self.name.clone(),
            process: kiln_proto::Process {
                entrypoint: p.entrypoint.clone(),
                cmd: p.cmd.clone(),
                env: p.env.iter().chain(extra_env).cloned().collect(),
                working_dir: p.working_dir.clone(),
                user: p.user.clone(),
            },
            ready: self.ready.clone(),
            stop_signal,
        })
    }
}

/// Checks services with the rules `kiln-init` applies (kiln-proto's): at most 16,
/// unique `[a-z0-9-]{1,32}` names, each process as valid as a main process, an
/// absolute socket path or a port 1–65535, a stop signal 1–31.
pub fn validate_services(services: &[Service]) -> std::result::Result<(), String> {
    let wire = services
        .iter()
        .map(|s| s.to_proto(&[]))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    kiln_proto::validate_services(&wire).map_err(|e| match e {
        kiln_proto::ProtoError::Invalid { reason, .. } => reason,
        other => other.to_string(),
    })
}

/// Parses and checks a `dev.kiln.services` label: a JSON array of [`Service`]s.
pub fn services_from_label(value: &str) -> crate::Result<Vec<Service>> {
    let services: Vec<Service> = serde_json::from_str(value).map_err(|e| crate::ImageError::Services(e.to_string()))?;
    validate_services(&services).map_err(crate::ImageError::Services)?;
    Ok(services)
}

/// The image's bottom `layers` kiln layers are `manifest_digest`'s, built from its
/// first `source_layers` OCI layers (spec §16.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BaseRecord {
    /// The name the base was given as (informational).
    pub name: String,
    pub manifest_digest: Digest,
    pub layers: usize,
    pub source_layers: usize,
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

    /// potter's PHP web image (potter spec §6.1.1): nginx is the main process,
    /// php-fpm a service ready on its socket and stopped with SIGQUIT.
    const LARAVEL: &str = r#"[{"name":"php-fpm","process":{"cmd":["-F"],"entrypoint":["php-fpm"],"env":["PHP_INI_DIR=/usr/local/etc/php"],"workingDir":"/app"},"ready":"unix:/run/php-fpm.sock","stopSignal":"SIGQUIT"}]"#;

    #[test]
    fn the_services_label_parses_and_round_trips() {
        let s = services_from_label(LARAVEL).unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].ready, Ready::Unix("/run/php-fpm.sock".into()));
        let wire = s[0].to_proto(&["APP_KEY=k".into()]).unwrap();
        assert_eq!((wire.stop_signal, wire.process.user.as_deref()), (3, None));
        assert_eq!(wire.process.env, ["PHP_INI_DIR=/usr/local/etc/php", "APP_KEY=k"]);
        // Canonical JSON of the parsed services is the label (keys sorted, defaults omitted).
        assert_eq!(String::from_utf8(kiln_oci::canonical_json(&s)).unwrap(), LARAVEL);
        let minimal = services_from_label(r#"[{"name":"a","process":{"cmd":["x"]}}]"#).unwrap();
        assert_eq!((&minimal[0].ready, &minimal[0].stop_signal), (&Ready::None, &None));
        assert_eq!(minimal[0].to_proto(&[]).unwrap().stop_signal, 15);
        assert_eq!(
            String::from_utf8(kiln_oci::canonical_json(&minimal)).unwrap(),
            r#"[{"name":"a","process":{"cmd":["x"]}}]"#
        );
    }

    #[test]
    fn bad_services_labels_are_refused() {
        for (bad, why) in [
            ("", "EOF"),
            ("{}", "a sequence"),
            (
                r#"[{"name":"a","process":{"cmd":["x"]},"restart":"always"}]"#,
                "unknown field `restart`",
            ),
            (
                r#"[{"name":"a","process":{"cmd":["x"],"stopSignal":"QUIT"}}]"#,
                "unknown field `stopSignal`",
            ),
            (r#"[{"name":"A","process":{"cmd":["x"]}}]"#, "service name"),
            (r#"[{"name":"a","process":{}}]"#, "no command"),
            (
                r#"[{"name":"a","process":{"cmd":["x"]},"ready":"unix:sock"}]"#,
                "not absolute",
            ),
            (r#"[{"name":"a","process":{"cmd":["x"]},"ready":"tcp:0"}]"#, "tcp:0"),
            (
                r#"[{"name":"a","process":{"cmd":["x"]},"stopSignal":"SIGRTMIN+3"}]"#,
                "real-time",
            ),
            (
                r#"[{"name":"a","process":{"cmd":["x"]}},{"name":"a","process":{"cmd":["y"]}}]"#,
                "two services",
            ),
            (
                r#"[{"name":"a","process":{"cmd":["x"],"workingDir":"app"}}]"#,
                "not absolute",
            ),
        ] {
            let err = services_from_label(bad).unwrap_err().to_string();
            assert!(
                err.starts_with("invalid dev.kiln.services label: ") && err.contains(why),
                "{bad}: {err}"
            );
        }
    }

    #[test]
    fn services_need_schema_version_2() {
        assert_eq!(schema_version_for(&[]), SCHEMA_VERSION);
        let s = services_from_label(LARAVEL).unwrap();
        assert_eq!(schema_version_for(&s), SCHEMA_VERSION_SERVICES);
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
            labels: None,
            base: None,
            services: Vec::new(),
        };
        let s = String::from_utf8(kiln_oci::canonical_json(&c)).unwrap();
        assert!(
            !s.contains("kernel")
                && !s.contains("init")
                && !s.contains("reference")
                && !s.contains("labels")
                && !s.contains("base")
                && !s.contains("services"),
            "{s}"
        );
        assert!(s.contains("\"erofsFormatVersion\":1"));
    }
}
