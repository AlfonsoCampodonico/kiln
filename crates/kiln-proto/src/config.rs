//! The `Config` message: everything `kiln-init` needs for one run (spec §9.5, §9.6).

use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};

use crate::error::{ProtoError, Result};
use crate::message::WindowSize;

/// The most app layers a guest mounts (overlayfs allows 500; mount(2) data is one page).
pub const MAX_LAYERS: u32 = 128;
/// The smallest scratch disk: the size of the ext4 template (spec §9.4).
pub const MIN_SCRATCH_BYTES: u64 = 64 << 20;
/// The most `nameserver` lines resolv.conf honours.
const MAX_DNS: usize = 3;

/// One run's configuration, sent once in answer to the guest's `Hello`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    /// What to run, after the host applied its overrides to the image's config.
    pub process: Process,
    /// Sent to the main process on `Shutdown` (a signal number, see [`crate::signal`]).
    pub stop_signal: i32,
    /// `-t`: run the main process on a pseudo-terminal of this initial size,
    /// relayed on port 1028 instead of pipes on 1025–1027.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tty: Option<WindowSize>,
    /// `-i`: relay stdin. Without it the main process's stdin is at EOF.
    pub interactive: bool,
    pub hostname: String,
    /// `eth0`'s configuration, when the VM has a network.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<Network>,
    /// App layers on `vdc`, `vdd`, …, lowest first (spec §9.1).
    pub layers: u32,
    pub scratch: Scratch,
    /// How the guest ends the VM (`vmkit::Capabilities::guest_exit`).
    pub exit_method: ExitMethod,
    /// The grace period when init gets SIGINT (Ctrl-Alt-Del) instead of `Shutdown`.
    pub shutdown_grace_secs: u32,
}

/// The main process (spec §5.1 `process`, minus `stopSignal`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Process {
    #[serde(default)]
    pub entrypoint: Vec<String>,
    #[serde(default)]
    pub cmd: Vec<String>,
    /// `KEY=value` entries; `kiln-init` adds `PATH`, `HOME` and `HOSTNAME` when unset.
    #[serde(default)]
    pub env: Vec<String>,
    /// Absolute; created when missing, as Docker does. Default `/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    /// `user`, `uid`, `user:group` or `uid:gid`, resolved against the image's
    /// `/etc/passwd` and `/etc/group`. Default root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

impl Process {
    /// The command line: entrypoint followed by cmd.
    pub fn argv(&self) -> Vec<&str> {
        self.entrypoint.iter().chain(&self.cmd).map(String::as_str).collect()
    }
}

/// `eth0`'s address, gateway and resolvers (vmkit's tap network, spec §9.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Network {
    pub address: Ipv4Addr,
    pub prefix_len: u8,
    pub gateway: Ipv4Addr,
    pub dns: Vec<Ipv4Addr>,
}

/// The scratch disk on `vdb`: the guest grows its ext4 to `size_bytes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Scratch {
    pub size_bytes: u64,
}

/// How the guest makes the VMM exit (spec §4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExitMethod {
    Reboot,
    Poweroff,
}

fn invalid(reason: impl Into<String>) -> ProtoError {
    ProtoError::invalid("Config", reason)
}

/// Strings become C strings in the guest, so they cannot hold NUL.
fn no_nul(what: &str, s: &str) -> Result<()> {
    if s.contains('\0') {
        return Err(invalid(format!("{what} contains a NUL byte")));
    }
    Ok(())
}

fn valid_hostname(h: &str) -> bool {
    (1..=64).contains(&h.len())
        && h.as_bytes()[0].is_ascii_alphanumeric()
        && h.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
}

impl Config {
    /// Checks every field; both sides call it (on encode and on decode).
    pub fn validate(&self) -> Result<()> {
        let p = &self.process;
        let argv = p.argv();
        if argv.first().is_none_or(|a| a.is_empty()) {
            return Err(invalid("no command: entrypoint and cmd are empty"));
        }
        for a in &argv {
            no_nul("an argument", a)?;
        }
        for e in &p.env {
            no_nul("an environment entry", e)?;
            if e.split_once('=')
                .is_none_or(|(k, _)| k.is_empty() || k.chars().any(char::is_control))
            {
                return Err(invalid(format!("environment entry {e:?} is not KEY=value")));
            }
        }
        if let Some(w) = &p.working_dir {
            no_nul("workingDir", w)?;
            if !w.starts_with('/') {
                return Err(invalid(format!("workingDir {w:?} is not absolute")));
            }
        }
        if let Some(u) = &p.user {
            no_nul("user", u)?;
            let parts: Vec<&str> = u.split(':').collect();
            if parts.len() > 2 || parts.iter().any(|s| s.is_empty() || s.chars().any(char::is_control)) {
                return Err(invalid(format!("user {u:?} is not user[:group]")));
            }
        }
        if crate::signal::name(self.stop_signal).is_none() {
            return Err(invalid(format!(
                "stopSignal {} (signals 1-31 only; real-time signals are not supported)",
                self.stop_signal
            )));
        }
        if !valid_hostname(&self.hostname) {
            return Err(invalid(format!("hostname {:?}", self.hostname)));
        }
        if self.layers > MAX_LAYERS {
            return Err(invalid(format!("{} layers (at most {MAX_LAYERS})", self.layers)));
        }
        let size = self.scratch.size_bytes;
        if size < MIN_SCRATCH_BYTES || !size.is_multiple_of(4096) {
            return Err(invalid(format!(
                "scratch size {size} (a multiple of 4096, at least {MIN_SCRATCH_BYTES})"
            )));
        }
        if let Some(n) = &self.network {
            n.validate()?;
        }
        Ok(())
    }
}

impl Network {
    fn validate(&self) -> Result<()> {
        if !(8..=30).contains(&self.prefix_len) {
            return Err(invalid(format!("network prefix /{}", self.prefix_len)));
        }
        let mask = u32::MAX << (32 - u32::from(self.prefix_len));
        let (addr, gw) = (u32::from(self.address), u32::from(self.gateway));
        let host = addr & !mask;
        if addr & mask != gw & mask || addr == gw || host == 0 || host == !mask {
            return Err(invalid(format!(
                "address {}/{} with gateway {}",
                self.address, self.prefix_len, self.gateway
            )));
        }
        if self.dns.len() > MAX_DNS {
            return Err(invalid(format!("{} DNS servers (at most {MAX_DNS})", self.dns.len())));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config {
            process: Process {
                entrypoint: vec!["/bin/sh".into(), "-c".into()],
                cmd: vec!["echo hi".into()],
                env: vec!["A=1".into(), "EMPTY=".into()],
                working_dir: Some("/srv".into()),
                user: Some("app:staff".into()),
            },
            stop_signal: 15,
            tty: None,
            interactive: false,
            hostname: "kiln-1.local".into(),
            network: Some(Network {
                address: Ipv4Addr::new(172, 30, 0, 2),
                prefix_len: 30,
                gateway: Ipv4Addr::new(172, 30, 0, 1),
                dns: vec![Ipv4Addr::new(172, 30, 0, 1)],
            }),
            layers: 3,
            scratch: Scratch { size_bytes: 4 << 30 },
            exit_method: ExitMethod::Reboot,
            shutdown_grace_secs: 10,
        }
    }

    #[test]
    fn a_full_config_is_valid_and_serialises_in_camel_case() {
        let c = config();
        c.validate().unwrap();
        let json = serde_json::to_string(&c).unwrap();
        for key in [
            "\"workingDir\"",
            "\"stopSignal\"",
            "\"exitMethod\":\"reboot\"",
            "\"prefixLen\"",
            "\"sizeBytes\"",
            "\"shutdownGraceSecs\"",
            "\"address\":\"172.30.0.2\"",
        ] {
            assert!(json.contains(key), "{key} missing in {json}");
        }
        assert!(!json.contains("\"tty\""), "unset options are omitted: {json}");
        assert_eq!(serde_json::from_str::<Config>(&json).unwrap(), c);
        assert_eq!(c.process.argv(), ["/bin/sh", "-c", "echo hi"]);
    }

    #[test]
    fn each_invalid_field_is_refused() {
        type Mutation = Box<dyn Fn(&mut Config)>;
        let cases: Vec<(&str, Mutation)> = vec![
            ("no command", Box::new(|c| c.process = Process::default())),
            ("empty argv0", Box::new(|c| c.process.entrypoint = vec![String::new()])),
            ("NUL", Box::new(|c| c.process.cmd = vec!["a\0b".into()])),
            ("env", Box::new(|c| c.process.env = vec!["NOEQUALS".into()])),
            ("env key", Box::new(|c| c.process.env = vec!["=v".into()])),
            ("workdir", Box::new(|c| c.process.working_dir = Some("srv".into()))),
            ("user", Box::new(|c| c.process.user = Some("a:b:c".into()))),
            ("user empty", Box::new(|c| c.process.user = Some(":0".into()))),
            ("user newline", Box::new(|c| c.process.user = Some("a\nb".into()))),
            ("user escape", Box::new(|c| c.process.user = Some("a\x1bb".into()))),
            ("env key control", Box::new(|c| c.process.env = vec!["A\x1bB=v".into()])),
            ("env key delete", Box::new(|c| c.process.env = vec!["A\x7fB=v".into()])),
            ("stop signal", Box::new(|c| c.stop_signal = 0)),
            ("hostname", Box::new(|c| c.hostname = "-x".into())),
            ("hostname chars", Box::new(|c| c.hostname = "a b".into())),
            ("hostname long", Box::new(|c| c.hostname = "a".repeat(65))),
            ("layers", Box::new(|c| c.layers = MAX_LAYERS + 1)),
            (
                "scratch small",
                Box::new(|c| c.scratch.size_bytes = MIN_SCRATCH_BYTES - 4096),
            ),
            ("scratch unaligned", Box::new(|c| c.scratch.size_bytes = (1 << 30) + 1)),
            ("prefix", Box::new(|c| c.network.as_mut().unwrap().prefix_len = 31)),
            (
                "gateway",
                Box::new(|c| c.network.as_mut().unwrap().gateway = Ipv4Addr::new(10, 0, 0, 1)),
            ),
            (
                "address is gateway",
                Box::new(|c| c.network.as_mut().unwrap().address = Ipv4Addr::new(172, 30, 0, 1)),
            ),
            (
                "broadcast",
                Box::new(|c| c.network.as_mut().unwrap().address = Ipv4Addr::new(172, 30, 0, 3)),
            ),
            (
                "dns",
                Box::new(|c| c.network.as_mut().unwrap().dns = vec![Ipv4Addr::LOCALHOST; 4]),
            ),
        ];
        for (name, mutate) in cases {
            let mut c = config();
            mutate(&mut c);
            assert!(c.validate().is_err(), "{name} was accepted");
        }
    }

    #[test]
    fn unknown_fields_and_methods_are_refused() {
        let mut v = serde_json::to_value(config()).unwrap();
        v["extra"] = true.into();
        assert!(serde_json::from_value::<Config>(v).is_err());
        let mut v = serde_json::to_value(config()).unwrap();
        v["exitMethod"] = "halt".into();
        assert!(serde_json::from_value::<Config>(v).is_err());
        let mut v = serde_json::to_value(config()).unwrap();
        v["process"]["argv"] = serde_json::json!([]);
        assert!(serde_json::from_value::<Config>(v).is_err());
    }
}
