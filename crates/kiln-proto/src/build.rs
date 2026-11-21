//! Build mode (protocol version 3, spec §9.8): the messages a build VM's host and
//! guest exchange after `Config { mode: build }`.
//!
//! The host sends one [`RunStep`] at a time; the guest runs it, streams its stdout
//! and stderr on fresh connections to ports 1026 and 1027 (EOF ends the step's
//! output), and answers [`StepExited`]. After exit 0 it seals the step's changes
//! into an OCI tar diff on the output disk and answers [`StepSealed`]. `Signal`
//! goes to the running step's process group. [`Finish`](crate::HostMessage::Finish)
//! ends the VM.
//!
//! Secrets travel inline, base64 in the JSON: a `RunStep` frame is at most
//! [`MAX_FRAME`](crate::MAX_FRAME) bytes, so a step's secrets are at most about
//! 44 KiB together (`RunStep::validate` and the host refuse more).

use serde::{Deserialize, Serialize};

use crate::error::{ProtoError, Result};

/// The most disks a step may mount, and the most secrets it may have.
pub const MAX_STEP_MOUNTS: usize = 64;
pub const MAX_STEP_SECRETS: usize = 64;
/// The output disk's alignment: every diff starts at a multiple of it.
pub const DIFF_ALIGN: u64 = 4096;
/// The most lower directories a build VM's step overlay may have: the base
/// layers, the layers stacked so far and one sealed upper per successful step.
/// The guest names them all in one mount(2) data page; the host refuses a
/// `RunStep` that would need more (the caller squashes or boots a new VM).
pub const MAX_BUILD_LOWERS: usize = 230;

/// What `Config` asks the guest to do.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Run the image's process (versions 1 and 2).
    #[default]
    Run,
    /// Run build steps and seal each one's changes (version 3).
    Build,
}

impl Mode {
    pub fn is_run(&self) -> bool {
        *self == Mode::Run
    }
}

/// A build VM's disk after its base layers (`Config.disks`), in disk order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BuildDisk {
    pub kind: DiskKind,
    /// Attached read-only: a step may mount it only read-only.
    pub read_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiskKind {
    /// A layer a step stacks (`RunStep.lowersAdded`).
    Erofs,
    /// A cache disk a step mounts (`RunStep.mounts`).
    Ext4,
}

/// What a step runs: an argv, or a script for `/bin/sh -c`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum Command {
    Argv(Vec<String>),
    Shell(String),
}

impl Command {
    /// The command line kiln-init executes.
    pub fn argv(&self) -> Vec<&str> {
        match self {
            Command::Argv(a) => a.iter().map(String::as_str).collect(),
            Command::Shell(s) => vec!["/bin/sh", "-c", s],
        }
    }
}

/// A cache disk mounted for one step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StepMount {
    /// The disk's index (0 is the init layer, `vda`); an `ext4` entry of `Config.disks`.
    pub disk: u32,
    pub target: String,
    pub read_only: bool,
}

/// A file the step sees at `target`, on a tmpfs: never in the upper or the output.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Secret {
    pub target: String,
    #[serde(with = "base64")]
    pub data: Vec<u8>,
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Secret {{ target: {:?}, data: <{} bytes> }}",
            self.target,
            self.data.len()
        )
    }
}

/// Runs one build step (host to guest).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunStep {
    pub id: u32,
    pub command: Command,
    /// `KEY=value` entries; kiln-init adds `PATH`, `HOSTNAME` and `HOME` when unset.
    #[serde(default)]
    pub env: Vec<String>,
    /// Absolute; created (as root) when missing, then entered.
    pub workdir: String,
    pub uid: u32,
    pub gid: u32,
    /// Disk indexes of `erofs` entries of `Config.disks` to stack on the root
    /// before the step runs, bottom first; each at most once per VM.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lowers_added: Vec<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<StepMount>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<Secret>,
    /// Whether the step may reach the network; the host enforces it (informational for the guest).
    pub egress: bool,
    /// The host's deadline for the step and its seal (informational for the guest).
    pub timeout_ms: u32,
}

/// The step's command ended (guest to host), after its output reached EOF.
/// Exactly one of `code` (0–255) and `signal` (1–64).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepExited {
    pub id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
}

/// After `StepExited { code: 0 }`: the step's diff is on the output disk at
/// `[offset, offset + len)`, flushed, with `entries` tar entries (PAX headers not
/// counted). Guest-asserted: the host passes it through, and its reader checks it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepSealed {
    pub id: u32,
    pub offset: u64,
    pub len: u64,
    /// Lowercase hex.
    pub sha256: String,
    pub entries: u64,
}

fn invalid(name: &'static str, reason: impl Into<String>) -> ProtoError {
    ProtoError::invalid(name, reason)
}

/// A step's mount or secret target: an absolute path below `/`, with no NUL, no
/// `.` or `..` component and no empty one (`//`). One trailing `/` is allowed and
/// ignored. Shared by the host (before boot and before sending a step) and the
/// guest, so a target one side accepts the other does too.
pub fn check_target(what: &str, t: &str) -> std::result::Result<(), String> {
    let below_root = t.strip_prefix('/').map(|r| r.strip_suffix('/').unwrap_or(r));
    let ok = !t.contains('\0')
        && below_root.is_some_and(|r| !r.is_empty() && r.split('/').all(|c| !matches!(c, "" | "." | "..")));
    if !ok {
        return Err(format!(
            "{what} {t:?} is not an absolute path below / without empty, . or .. components"
        ));
    }
    Ok(())
}

impl RunStep {
    pub fn validate(&self) -> Result<()> {
        let v = |r: String| invalid("RunStep", r);
        match &self.command {
            Command::Argv(a) if a.first().is_none_or(String::is_empty) => {
                return Err(v("an empty argv".into()));
            }
            Command::Shell(s) if s.is_empty() => return Err(v("an empty shell script".into())),
            _ => {}
        }
        if self.command.argv().iter().any(|a| a.contains('\0')) {
            return Err(v("an argument contains a NUL byte".into()));
        }
        for e in &self.env {
            if e.contains('\0')
                || e.split_once('=')
                    .is_none_or(|(k, _)| k.is_empty() || k.chars().any(char::is_control))
            {
                return Err(v(format!("environment entry {e:?} is not KEY=value")));
            }
        }
        if !self.workdir.starts_with('/') || self.workdir.contains('\0') {
            return Err(v(format!("workdir {:?} is not absolute", self.workdir)));
        }
        if self.uid == u32::MAX || self.gid == u32::MAX {
            return Err(v("uid or gid 4294967295".into()));
        }
        let mut seen = std::collections::BTreeSet::new();
        if self.lowers_added.len() > crate::MAX_LAYERS as usize || !self.lowers_added.iter().all(|d| seen.insert(*d)) {
            return Err(v("lowersAdded repeats a disk or has too many".into()));
        }
        if self.mounts.len() > MAX_STEP_MOUNTS || self.secrets.len() > MAX_STEP_SECRETS {
            return Err(v(format!(
                "{} mounts and {} secrets (at most {MAX_STEP_MOUNTS} each)",
                self.mounts.len(),
                self.secrets.len()
            )));
        }
        let mut targets = std::collections::BTreeSet::new();
        let targets_of = self
            .mounts
            .iter()
            .map(|m| ("mount target", &m.target))
            .chain(self.secrets.iter().map(|s| ("secret target", &s.target)));
        for (what, t) in targets_of {
            check_target(what, t).map_err(v)?;
            if what == "secret target" && t.ends_with('/') {
                return Err(v(format!("secret target {t:?} ends in '/': a secret is a file")));
            }
            if !targets.insert(t.trim_end_matches('/')) {
                return Err(v(format!("two mounts or secrets at {t:?}")));
            }
        }
        if self.timeout_ms == 0 {
            return Err(v("timeoutMs 0".into()));
        }
        Ok(())
    }
}

impl StepExited {
    pub fn validate(&self) -> Result<()> {
        match (self.code, self.signal) {
            (Some(c), None) if (0..=255).contains(&c) => Ok(()),
            (None, Some(s)) if (1..=64).contains(&s) => Ok(()),
            _ => Err(invalid(
                "StepExited",
                format!("code {:?} and signal {:?}", self.code, self.signal),
            )),
        }
    }
}

impl StepSealed {
    pub fn validate(&self) -> Result<()> {
        let hex = self.sha256.len() == 64
            && self
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !hex {
            return Err(invalid("StepSealed", "sha256 is not 64 lowercase hex digits"));
        }
        Ok(())
    }
}

/// Standard base64 with padding, decoded strictly (no whitespace, canonical padding).
pub mod base64 {
    use serde::{Deserialize, Deserializer, Serializer};

    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn encode(data: &[u8]) -> String {
        let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
        for chunk in data.chunks(3) {
            let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(ALPHABET[(n >> (18 - 6 * i)) as usize & 63] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    pub fn decode(text: &str) -> Option<Vec<u8>> {
        let s = text.as_bytes();
        if !s.len().is_multiple_of(4) {
            return None;
        }
        let value = |c: u8| ALPHABET.iter().position(|&a| a == c).map(|p| p as u32);
        let mut out = Vec::with_capacity(s.len() / 4 * 3);
        for (i, q) in s.chunks(4).enumerate() {
            let last = i == s.len() / 4 - 1;
            let pad = q.iter().rev().take_while(|&&c| c == b'=').count();
            if pad > 2 || (pad > 0 && !last) {
                return None;
            }
            let mut n = 0u32;
            for &c in &q[..4 - pad] {
                n = (n << 6) | value(c)?;
            }
            n <<= 6 * pad as u32;
            let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
            // Canonical: the bits padding drops must be zero.
            if bytes[3 - pad..].iter().any(|&b| b != 0) {
                return None;
            }
            out.extend_from_slice(&bytes[..3 - pad]);
        }
        Some(out)
    }

    pub fn serialize<S: Serializer>(data: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&encode(data))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(d)?;
        decode(&text).ok_or_else(|| serde::de::Error::custom("invalid base64"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_have_no_dot_dotdot_or_empty_components() {
        for bad in [
            "/a/../b", "/a/./b", "//a", "/a//b", "/a/..", "/./a", "/", "//", "a/b", "", "/a//", "/a\0b",
        ] {
            assert!(check_target("t", bad).is_err(), "{bad:?} must be refused");
        }
        for good in ["/a/b", "/a", "/a/", "/root/.cache", "/a/.b/..c"] {
            assert!(check_target("t", good).is_ok(), "{good:?} must be accepted");
        }
    }

    #[test]
    fn a_step_refuses_non_canonical_targets_and_duplicates() {
        for bad in ["/a/../b", "/a/./b", "//a", "/a//b"] {
            let mut s = step();
            s.mounts[0].target = bad.into();
            assert!(s.validate().is_err(), "mount {bad:?}");
            let mut s = step();
            s.secrets[0].target = bad.into();
            assert!(s.validate().is_err(), "secret {bad:?}");
        }
        let mut s = step();
        s.secrets[0].target = "/root/.cache/".into();
        assert!(s.validate().is_err(), "a trailing slash names the same target");
    }

    #[test]
    fn a_secret_target_is_a_file_not_a_directory_path() {
        let mut s = step();
        s.secrets[0].target = "/run/secrets/npmrc/".into();
        let why = s.validate().unwrap_err().to_string();
        assert!(why.contains("ends in '/'"), "{why}");
        // A mount may still end in a slash.
        let mut s = step();
        s.mounts[0].target = "/root/.cache/".into();
        s.validate().unwrap();
    }

    pub(crate) fn step() -> RunStep {
        RunStep {
            id: 1,
            command: Command::Shell("echo hi > /out".into()),
            env: vec!["A=1".into()],
            workdir: "/app".into(),
            uid: 0,
            gid: 0,
            lowers_added: vec![5],
            mounts: vec![StepMount {
                disk: 6,
                target: "/root/.cache".into(),
                read_only: false,
            }],
            secrets: vec![Secret {
                target: "/run/secrets/npmrc".into(),
                data: b"token=\x00\xff".to_vec(),
            }],
            egress: false,
            timeout_ms: 60_000,
        }
    }

    #[test]
    fn base64_round_trips_and_decodes_strictly() {
        for (raw, text) in [
            (&b""[..], ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
            (b"\x00\xff\xfe", "AP/+"),
        ] {
            assert_eq!(base64::encode(raw), text);
            assert_eq!(base64::decode(text).as_deref(), Some(raw));
        }
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(base64::decode(&base64::encode(&all)).unwrap(), all);
        for bad in [
            "Zg=", "Zg===", "Z===", "Zh==", "Zm9=v", "Zg==Zg==", " Zg==", "Zm9v\n", "Zm-v",
        ] {
            assert_eq!(base64::decode(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_step_serialises_in_camel_case_and_round_trips() {
        let s = step();
        s.validate().unwrap();
        let json = serde_json::to_string(&s).unwrap();
        for key in [
            r#""command":{"shell":"echo hi > /out"}"#,
            r#""lowersAdded":[5]"#,
            r#""mounts":[{"disk":6,"target":"/root/.cache","readOnly":false}]"#,
            r#""secrets":[{"target":"/run/secrets/npmrc","data":"dG9rZW49AP8="}]"#,
            r#""timeoutMs":60000"#,
        ] {
            assert!(json.contains(key), "{key} missing in {json}");
        }
        assert_eq!(serde_json::from_str::<RunStep>(&json).unwrap(), s);
        let argv = RunStep {
            command: Command::Argv(vec!["make".into(), "-j4".into()]),
            ..step()
        };
        assert!(
            serde_json::to_string(&argv)
                .unwrap()
                .contains(r#""command":{"argv":["make","-j4"]}"#)
        );
        assert_eq!(argv.command.argv(), ["make", "-j4"]);
        assert_eq!(step().command.argv(), ["/bin/sh", "-c", "echo hi > /out"]);
        assert!(!format!("{:?}", step()).contains("token"), "secrets are not printed");
    }

    #[test]
    fn invalid_steps_are_refused() {
        type Mutation = Box<dyn Fn(&mut RunStep)>;
        let cases: Vec<(&str, Mutation)> = vec![
            ("empty argv", Box::new(|s| s.command = Command::Argv(vec![]))),
            (
                "empty argv0",
                Box::new(|s| s.command = Command::Argv(vec![String::new()])),
            ),
            ("empty script", Box::new(|s| s.command = Command::Shell(String::new()))),
            ("NUL", Box::new(|s| s.command = Command::Shell("a\0".into()))),
            ("env", Box::new(|s| s.env = vec!["X".into()])),
            ("workdir", Box::new(|s| s.workdir = "app".into())),
            ("uid", Box::new(|s| s.uid = u32::MAX)),
            ("lowers twice", Box::new(|s| s.lowers_added = vec![5, 5])),
            ("root target", Box::new(|s| s.mounts[0].target = "/".into())),
            ("relative target", Box::new(|s| s.secrets[0].target = "run/x".into())),
            (
                "same target",
                Box::new(|s| s.secrets[0].target = "/root/.cache/".into()),
            ),
            ("timeout", Box::new(|s| s.timeout_ms = 0)),
        ];
        for (name, mutate) in cases {
            let mut s = step();
            mutate(&mut s);
            assert!(s.validate().is_err(), "{name} was accepted");
        }
    }

    #[test]
    fn step_exits_and_seals_are_range_checked() {
        let e = |code, signal| StepExited { id: 1, code, signal }.validate();
        assert!(e(Some(0), None).is_ok() && e(None, Some(9)).is_ok());
        assert!(e(None, None).is_err() && e(Some(0), Some(9)).is_err());
        assert!(e(Some(256), None).is_err() && e(None, Some(0)).is_err());
        let sealed = |sha: &str| {
            StepSealed {
                id: 1,
                offset: 0,
                len: 1024,
                sha256: sha.into(),
                entries: 1,
            }
            .validate()
        };
        assert!(sealed(&"ab".repeat(32)).is_ok());
        assert!(sealed(&"AB".repeat(32)).is_err());
        assert!(sealed("ab").is_err());
    }
}
