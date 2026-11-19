//! The `Config` message: everything `kiln-init` needs for one run (spec §9.5, §9.6).

use std::net::{Ipv4Addr, Ipv6Addr};

use serde::{Deserialize, Serialize};

use crate::error::{ProtoError, Result};
use crate::message::WindowSize;
use crate::volume::{MAX_VOLUMES, Volume, VolumeKind};

/// The most app layers a guest mounts (overlayfs allows 500; mount(2) data is one page).
pub const MAX_LAYERS: u32 = 128;
/// The smallest scratch disk: the size of the ext4 template (spec §9.4).
pub const MIN_SCRATCH_BYTES: u64 = 64 << 20;
/// The most `nameserver` lines resolv.conf honours.
const MAX_DNS: usize = 3;
/// The most `search` domains glibc's resolver honours.
const MAX_SEARCH: usize = 6;
/// The most services a run may have (protocol version 2).
pub const MAX_SERVICES: usize = 16;
/// The longest service name, in bytes.
pub const MAX_SERVICE_NAME: usize = 32;
/// The longest `unix:` readiness path, in bytes: `sun_path` holds 108 with its NUL.
pub const MAX_SOCKET_PATH: usize = 107;

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
    /// The grace period when init gets SIGINT (Ctrl-Alt-Del) instead of `Shutdown`,
    /// and when a process's exit stops the others (protocol version 2).
    pub shutdown_grace_secs: u32,
    /// A pod's volumes; omitted when empty. The host follows `Config` with the files
    /// volumes' first `FileUpdate`s.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<Volume>,
    /// The pod disk after the layers (`emptyDir` volumes), grown like the scratch disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_disk: Option<Scratch>,
    /// Protocol version 2: processes started, in order, before the main process,
    /// each waiting for the previous one's readiness. Omitted when empty, so a
    /// `Config` without services is a version 1 `Config`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<Service>,
    /// Protocol version 2, present exactly when `services` is not empty: how long,
    /// from `Config`'s arrival, the guest allows every service to become ready.
    /// The host derives it from what remains of its boot timeout, so that a
    /// service that never becomes ready is reported (`InitFailed` naming it)
    /// before the host's own boot timeout ends the VM.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_timeout_ms: Option<u32>,
}

/// A process kiln-init starts before the main process and stops after it
/// (protocol version 2; spec §9.8 `services`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Service {
    /// `[a-z0-9-]{1,32}`, unique in the run; names the service in failures and in `Exited`.
    pub name: String,
    /// As the main process's: argv, the final environment, working directory, user.
    pub process: Process,
    /// When the service counts as started.
    pub ready: Ready,
    /// Sent when the service is stopped (1-31).
    pub stop_signal: i32,
}

/// When a service is ready: `unix:<absolute path>` (a stream socket there accepts
/// a connection), `tcp:<port>` (127.0.0.1:<port> accepts one), or `none` (as soon
/// as it has been started). Carried as that string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Ready {
    None,
    Unix(String),
    Tcp(u16),
}

impl Ready {
    pub fn is_none(&self) -> bool {
        matches!(self, Ready::None)
    }

    /// Checks the value's own rules (the parser applies them too).
    pub fn validate(&self) -> std::result::Result<(), String> {
        match self {
            Ready::None => Ok(()),
            Ready::Tcp(0) => Err("tcp port 0".into()),
            Ready::Tcp(_) => Ok(()),
            Ready::Unix(p) if !p.starts_with('/') => Err(format!("unix socket path {p:?} is not absolute")),
            Ready::Unix(p) if p.contains('\0') => Err("unix socket path contains a NUL byte".into()),
            Ready::Unix(p) if p.len() > MAX_SOCKET_PATH => Err(format!(
                "unix socket path of {} bytes (at most {MAX_SOCKET_PATH})",
                p.len()
            )),
            Ready::Unix(_) => Ok(()),
        }
    }
}

impl TryFrom<String> for Ready {
    type Error = String;

    fn try_from(s: String) -> std::result::Result<Self, String> {
        let r = if s == "none" {
            Ready::None
        } else if let Some(path) = s.strip_prefix("unix:") {
            Ready::Unix(path.to_string())
        } else if let Some(port) = s.strip_prefix("tcp:") {
            // Digits only: no sign, no spaces.
            if port.is_empty() || port.len() > 5 || !port.bytes().all(|b| b.is_ascii_digit()) {
                return Err(format!("ready {s:?}: the port is not a number 1-65535"));
            }
            Ready::Tcp(
                port.parse()
                    .map_err(|_| format!("ready {s:?}: the port is not a number 1-65535"))?,
            )
        } else {
            return Err(format!("ready {s:?} (unix:<path>, tcp:<port> or none)"));
        };
        r.validate().map_err(|e| format!("ready {s:?}: {e}"))?;
        Ok(r)
    }
}

impl From<Ready> for String {
    fn from(r: Ready) -> String {
        r.to_string()
    }
}

impl std::fmt::Display for Ready {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Ready::None => f.write_str("none"),
            Ready::Unix(p) => write!(f, "unix:{p}"),
            Ready::Tcp(port) => write!(f, "tcp:{port}"),
        }
    }
}

/// A valid service name: `[a-z0-9-]{1,32}`.
pub fn valid_service_name(name: &str) -> bool {
    (1..=MAX_SERVICE_NAME).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Checks a run's services: at most [`MAX_SERVICES`], unique valid names, each
/// process as valid as the main process, a valid readiness and stop signal.
/// kiln-image calls it for an image's services, and `Config` for its own.
pub fn validate_services(services: &[Service]) -> Result<()> {
    if services.len() > MAX_SERVICES {
        return Err(invalid(format!("{} services (at most {MAX_SERVICES})", services.len())));
    }
    let mut seen = std::collections::BTreeSet::new();
    for s in services {
        if !valid_service_name(&s.name) {
            return Err(invalid(format!(
                "service name {:?} (1-{MAX_SERVICE_NAME} of [a-z0-9-])",
                s.name
            )));
        }
        if !seen.insert(s.name.as_str()) {
            return Err(invalid(format!("two services named {:?}", s.name)));
        }
        if let Err(e) = s.process.validate("Config") {
            let why = match e {
                ProtoError::Invalid { reason, .. } => reason,
                other => other.to_string(),
            };
            return Err(invalid(format!("service {}: {why}", s.name)));
        }
        s.ready
            .validate()
            .map_err(|e| invalid(format!("service {}: ready: {e}", s.name)))?;
        crate::signal::name(s.stop_signal).ok_or_else(|| {
            invalid(format!(
                "service {}: stopSignal {} (signals 1-31 only; real-time signals are not supported)",
                s.name, s.stop_signal
            ))
        })?;
    }
    Ok(())
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
    /// Checks the command line, environment, working directory and user; `name` is the
    /// message's, for errors.
    pub fn validate(&self, name: &'static str) -> Result<()> {
        let p = self;
        let argv = p.argv();
        if argv.first().is_none_or(|a| a.is_empty()) {
            return Err(ProtoError::invalid(name, "no command: entrypoint and cmd are empty"));
        }
        for a in &argv {
            no_nul(name, "an argument", a)?;
        }
        for e in &p.env {
            no_nul(name, "an environment entry", e)?;
            if e.split_once('=')
                .is_none_or(|(k, _)| k.is_empty() || k.chars().any(char::is_control))
            {
                return Err(ProtoError::invalid(
                    name,
                    format!("environment entry {e:?} is not KEY=value"),
                ));
            }
        }
        if let Some(w) = &p.working_dir {
            no_nul(name, "workingDir", w)?;
            if !w.starts_with('/') {
                return Err(ProtoError::invalid(name, format!("workingDir {w:?} is not absolute")));
            }
        }
        if let Some(u) = &p.user {
            no_nul(name, "user", u)?;
            let parts: Vec<&str> = u.split(':').collect();
            if parts.len() > 2 || parts.iter().any(|s| s.is_empty() || s.chars().any(char::is_control)) {
                return Err(ProtoError::invalid(name, format!("user {u:?} is not user[:group]")));
            }
        }
        Ok(())
    }

    /// The command line: entrypoint followed by cmd.
    pub fn argv(&self) -> Vec<&str> {
        self.entrypoint.iter().chain(&self.cmd).map(String::as_str).collect()
    }
}

/// `eth0`'s address, gateway and resolvers: vmkit's tap network (spec §9.3), or a pod's
/// own address under kiln-shim. A gateway outside the prefix is routed to on-link (a
/// CNI's `/32` pod address with a link-local gateway).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Network {
    pub address: Ipv4Addr,
    pub prefix_len: u8,
    pub gateway: Ipv4Addr,
    pub dns: Vec<Ipv4Addr>,
    /// `search` domains for resolv.conf (a pod's cluster domains); omitted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub search: Vec<String>,
    /// resolv.conf `options` (e.g. `ndots:5`); omitted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    /// `eth0`'s IPv6 address and default route, when the VM has IPv6 egress. DNS stays on
    /// the IPv4 resolvers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ipv6: Option<Ipv6Network>,
}

/// `eth0`'s IPv6 side: a unicast address and its gateway in the same `/prefix_len` (64–126).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Ipv6Network {
    pub address: Ipv6Addr,
    pub prefix_len: u8,
    pub gateway: Ipv6Addr,
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
fn no_nul(name: &'static str, what: &str, s: &str) -> Result<()> {
    if s.contains('\0') {
        return Err(ProtoError::invalid(name, format!("{what} contains a NUL byte")));
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
        self.process.validate("Config")?;
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
        if self.volumes.len() > MAX_VOLUMES {
            return Err(invalid(format!(
                "{} volumes (at most {MAX_VOLUMES})",
                self.volumes.len()
            )));
        }
        for v in &self.volumes {
            v.validate()?;
            if matches!(v.kind, VolumeKind::EmptyDir { .. }) && self.pod_disk.is_none() {
                return Err(invalid(format!("emptyDir {} without a pod disk", v.path)));
            }
        }
        if let Some(d) = &self.pod_disk
            && (d.size_bytes < MIN_SCRATCH_BYTES || !d.size_bytes.is_multiple_of(4096))
        {
            return Err(invalid(format!("pod disk size {}", d.size_bytes)));
        }
        validate_services(&self.services)?;
        match (self.services.is_empty(), self.ready_timeout_ms) {
            (true, Some(_)) => return Err(invalid("readyTimeoutMs without services")),
            (false, None) => return Err(invalid("services without readyTimeoutMs")),
            (false, Some(0)) => return Err(invalid("readyTimeoutMs 0")),
            _ => {}
        }
        Ok(())
    }

    /// The lowest protocol version that defines everything this `Config` uses
    /// (spec §9.8 rule 3): 2 with services, else 1. Everything before version 2
    /// counts as version 1.
    pub fn required_version(&self) -> u32 {
        if self.services.is_empty() { 1 } else { 2 }
    }
}

impl Network {
    fn mask(&self) -> u32 {
        u32::MAX.checked_shl(32 - u32::from(self.prefix_len)).unwrap_or(0)
    }

    /// The gateway is inside the address's prefix; otherwise the route to it is on-link.
    pub fn gateway_in_prefix(&self) -> bool {
        let mask = self.mask();
        u32::from(self.address) & mask == u32::from(self.gateway) & mask
    }

    fn validate(&self) -> Result<()> {
        if !(8..=32).contains(&self.prefix_len) {
            return Err(invalid(format!("network prefix /{}", self.prefix_len)));
        }
        let mask = self.mask();
        let (addr, gw) = (u32::from(self.address), u32::from(self.gateway));
        // On prefixes up to /30 the all-zeros and all-ones hosts are the network and broadcast.
        let reserved = |a: u32| self.prefix_len <= 30 && (a & !mask == 0 || a & !mask == !mask);
        if addr == gw || reserved(addr) || (self.gateway_in_prefix() && reserved(gw)) {
            return Err(invalid(format!(
                "address {}/{} with gateway {}",
                self.address, self.prefix_len, self.gateway
            )));
        }
        if self.dns.len() > MAX_DNS {
            return Err(invalid(format!("{} DNS servers (at most {MAX_DNS})", self.dns.len())));
        }
        if self.search.len() > MAX_SEARCH {
            return Err(invalid(format!(
                "{} search domains (at most {MAX_SEARCH})",
                self.search.len()
            )));
        }
        let word = |s: &str, max: usize| (1..=max).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_graphic());
        if let Some(d) = self.search.iter().find(|d| !word(d, 253)) {
            return Err(invalid(format!("search domain {d:?}")));
        }
        if let Some(o) = self.options.iter().find(|o| !word(o, 64)) {
            return Err(invalid(format!("resolver option {o:?}")));
        }
        if let Some(n) = &self.ipv6 {
            n.validate()?;
        }
        Ok(())
    }
}

impl Ipv6Network {
    fn validate(&self) -> Result<()> {
        let bad = || {
            invalid(format!(
                "IPv6 address {}/{} with gateway {}",
                self.address, self.prefix_len, self.gateway
            ))
        };
        if !(64..=126).contains(&self.prefix_len) {
            return Err(invalid(format!("IPv6 network prefix /{}", self.prefix_len)));
        }
        let mask = u128::MAX << (128 - u32::from(self.prefix_len));
        let (addr, gw) = (u128::from(self.address), u128::from(self.gateway));
        // Unicast, routable beyond the link: not unspecified, loopback, IPv4-mapped,
        // link-local or multicast. The prefix's first address is the subnet-router anycast.
        let unicast = |a: Ipv6Addr| {
            !(a.is_unspecified()
                || a.is_loopback()
                || a.to_ipv4_mapped().is_some()
                || a.is_unicast_link_local()
                || a.is_multicast())
        };
        if addr & mask != gw & mask
            || addr == gw
            || addr & !mask == 0
            || gw & !mask == 0
            || !unicast(self.address)
            || !unicast(self.gateway)
        {
            return Err(bad());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v6(c: &mut Config) -> &mut Ipv6Network {
        c.network.as_mut().unwrap().ipv6.as_mut().unwrap()
    }

    fn net6(address: &str, prefix_len: u8, gateway: &str) -> Ipv6Network {
        Ipv6Network {
            address: address.parse().unwrap(),
            prefix_len,
            gateway: gateway.parse().unwrap(),
        }
    }

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
                search: Vec::new(),
                options: Vec::new(),
                ipv6: Some(Ipv6Network {
                    address: "fd76:6d6b:6974::2".parse().unwrap(),
                    prefix_len: 64,
                    gateway: "fd76:6d6b:6974::1".parse().unwrap(),
                }),
            }),
            layers: 3,
            scratch: Scratch { size_bytes: 4 << 30 },
            exit_method: ExitMethod::Reboot,
            shutdown_grace_secs: 10,
            volumes: Vec::new(),
            pod_disk: None,
            services: Vec::new(),
            ready_timeout_ms: None,
        }
    }

    fn fpm() -> Service {
        Service {
            name: "php-fpm".into(),
            process: Process {
                entrypoint: vec!["php-fpm".into()],
                cmd: vec!["-F".into()],
                env: vec!["APP_ENV=production".into()],
                working_dir: Some("/app".into()),
                user: None,
            },
            ready: Ready::Unix("/run/php-fpm.sock".into()),
            stop_signal: 3,
        }
    }

    fn with_services(services: Vec<Service>) -> Config {
        Config {
            services,
            ready_timeout_ms: Some(20_000),
            ..config()
        }
    }

    #[test]
    fn a_config_without_services_is_a_version_1_config() {
        let c = config();
        assert_eq!(c.required_version(), 1);
        let json = serde_json::to_string(&c).unwrap();
        assert!(!json.contains("services") && !json.contains("readyTimeoutMs"), "{json}");
        let c = with_services(vec![fpm()]);
        c.validate().unwrap();
        assert_eq!(c.required_version(), 2);
        let json = serde_json::to_string(&c).unwrap();
        assert!(
            json.contains(r#""services":[{"name":"php-fpm","process":{"#)
                && json.contains(r#""ready":"unix:/run/php-fpm.sock","stopSignal":3}]"#)
                && json.contains(r#""readyTimeoutMs":20000"#),
            "{json}"
        );
        assert_eq!(serde_json::from_str::<Config>(&json).unwrap(), c);
    }

    #[test]
    fn ready_parses_and_prints_its_three_forms() {
        for (text, ready) in [
            ("none", Ready::None),
            ("unix:/run/php-fpm.sock", Ready::Unix("/run/php-fpm.sock".into())),
            ("tcp:9000", Ready::Tcp(9000)),
            ("tcp:65535", Ready::Tcp(65535)),
        ] {
            assert_eq!(Ready::try_from(text.to_string()).unwrap(), ready);
            assert_eq!(ready.to_string(), text);
        }
        let long = format!("unix:/{}", "s".repeat(MAX_SOCKET_PATH));
        for bad in [
            "",
            "None",
            "unix:",
            "unix:run/x",
            "unix:/a\0b",
            "tcp:",
            "tcp:0",
            "tcp:65536",
            "tcp:+80",
            "tcp: 80",
            "tcp:80x",
            "tcp:0080000",
            "http:80",
            &long,
        ] {
            assert!(Ready::try_from(bad.to_string()).is_err(), "{bad:?} was accepted");
        }
        let ok = format!("unix:/{}", "s".repeat(MAX_SOCKET_PATH - 1));
        Ready::try_from(ok).unwrap();
    }

    #[test]
    fn each_invalid_service_is_refused() {
        type Mutation = Box<dyn Fn(&mut Vec<Service>)>;
        let cases: Vec<(&str, &str, Mutation)> = vec![
            ("empty name", "service name", Box::new(|s| s[0].name = String::new())),
            ("upper case", "service name", Box::new(|s| s[0].name = "PHP".into())),
            ("underscore", "service name", Box::new(|s| s[0].name = "php_fpm".into())),
            (
                "long name",
                "service name",
                Box::new(|s| s[0].name = "a".repeat(MAX_SERVICE_NAME + 1)),
            ),
            ("duplicate", "two services", Box::new(|s| s.push(fpm()))),
            (
                "too many",
                "services (at most",
                Box::new(|s| {
                    *s = (0..=MAX_SERVICES)
                        .map(|i| Service {
                            name: format!("s{i}"),
                            ..fpm()
                        })
                        .collect()
                }),
            ),
            ("no argv", "no command", Box::new(|s| s[0].process = Process::default())),
            (
                "relative workdir",
                "workingDir",
                Box::new(|s| s[0].process.working_dir = Some("app".into())),
            ),
            (
                "bad user",
                "user[:group]",
                Box::new(|s| s[0].process.user = Some("a:b:c".into())),
            ),
            (
                "bad env",
                "KEY=value",
                Box::new(|s| s[0].process.env = vec!["X".into()]),
            ),
            ("stop signal 0", "stopSignal", Box::new(|s| s[0].stop_signal = 0)),
            ("stop signal 34", "stopSignal", Box::new(|s| s[0].stop_signal = 34)),
            (
                "relative socket",
                "unix socket path",
                Box::new(|s| s[0].ready = Ready::Unix("run/x".into())),
            ),
            ("port 0", "tcp port 0", Box::new(|s| s[0].ready = Ready::Tcp(0))),
        ];
        for (name, fragment, mutate) in cases {
            let mut services = vec![fpm()];
            mutate(&mut services);
            let err = with_services(services).validate().unwrap_err().to_string();
            assert!(err.contains(fragment), "{name}: {err:?} lacks {fragment:?}");
        }
        let max: Vec<Service> = (0..MAX_SERVICES)
            .map(|i| Service {
                name: format!("s{i}"),
                ..fpm()
            })
            .collect();
        with_services(max).validate().unwrap();
        let mut c = with_services(vec![fpm()]);
        c.ready_timeout_ms = None;
        assert!(c.validate().unwrap_err().to_string().contains("readyTimeoutMs"));
        c.ready_timeout_ms = Some(0);
        assert!(c.validate().is_err());
        let mut c = config();
        c.ready_timeout_ms = Some(1);
        assert!(c.validate().unwrap_err().to_string().contains("without services"));
    }

    #[test]
    fn unknown_service_fields_are_refused() {
        let mut v = serde_json::to_value(with_services(vec![fpm()])).unwrap();
        v["services"][0]["restart"] = "always".into();
        assert!(serde_json::from_value::<Config>(v).is_err());
        let mut v = serde_json::to_value(with_services(vec![fpm()])).unwrap();
        v["services"][0]["ready"] = "tcp:x".into();
        assert!(serde_json::from_value::<Config>(v).is_err());
    }

    #[test]
    fn volumes_and_the_pod_disk_are_optional_and_checked() {
        let plain = serde_json::to_string(&config()).unwrap();
        assert!(!plain.contains("volumes") && !plain.contains("podDisk"), "{plain}");
        let mut c = config();
        c.volumes = vec![
            Volume {
                path: "/etc/cfg".into(),
                kind: VolumeKind::Files { file: false },
            },
            Volume {
                path: "/cache".into(),
                kind: VolumeKind::EmptyDir { name: "cache".into() },
            },
        ];
        assert!(c.validate().is_err(), "an emptyDir needs the pod disk");
        c.pod_disk = Some(Scratch { size_bytes: 1 << 30 });
        c.validate().unwrap();
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains(r#""podDisk":{"sizeBytes":1073741824}"#), "{json}");
        assert_eq!(serde_json::from_str::<Config>(&json).unwrap(), c);
        c.pod_disk = Some(Scratch { size_bytes: 4096 });
        assert!(c.validate().is_err());
        c.pod_disk = Some(Scratch { size_bytes: 1 << 30 });
        c.volumes[0].path = "/".into();
        assert!(c.validate().is_err());
        c.volumes = vec![c.volumes[1].clone(); MAX_VOLUMES + 1];
        assert!(c.validate().is_err());
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
            "\"ipv6\":{\"address\":\"fd76:6d6b:6974::2\",\"prefixLen\":64,\"gateway\":\"fd76:6d6b:6974::1\"}",
        ] {
            assert!(json.contains(key), "{key} missing in {json}");
        }
        assert!(!json.contains("\"tty\""), "unset options are omitted: {json}");
        assert_eq!(serde_json::from_str::<Config>(&json).unwrap(), c);
        assert_eq!(c.process.argv(), ["/bin/sh", "-c", "echo hi"]);
    }

    #[test]
    fn ipv6_is_optional() {
        let mut c = config();
        c.network.as_mut().unwrap().ipv6 = None;
        c.validate().unwrap();
        let json = serde_json::to_string(&c).unwrap();
        assert!(!json.contains("ipv6"), "{json}");
        assert_eq!(serde_json::from_str::<Config>(&json).unwrap(), c);
        let mut v = serde_json::to_value(&c).unwrap();
        v["network"]["ipv6"] = serde_json::Value::Null;
        assert_eq!(serde_json::from_value::<Config>(v).unwrap(), c);
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
            ("prefix", Box::new(|c| c.network.as_mut().unwrap().prefix_len = 7)),
            ("prefix long", Box::new(|c| c.network.as_mut().unwrap().prefix_len = 33)),
            (
                "gateway is the network",
                Box::new(|c| c.network.as_mut().unwrap().gateway = Ipv4Addr::new(172, 30, 0, 0)),
            ),
            (
                "search count",
                Box::new(|c| c.network.as_mut().unwrap().search = vec!["a".into(); 7]),
            ),
            (
                "search long",
                Box::new(|c| c.network.as_mut().unwrap().search = vec!["a".repeat(254)]),
            ),
            (
                "search space",
                Box::new(|c| c.network.as_mut().unwrap().search = vec!["a b".into()]),
            ),
            (
                "search empty",
                Box::new(|c| c.network.as_mut().unwrap().search = vec![String::new()]),
            ),
            (
                "option space",
                Box::new(|c| c.network.as_mut().unwrap().options = vec!["ndots: 5".into()]),
            ),
            (
                "option NUL",
                Box::new(|c| c.network.as_mut().unwrap().options = vec!["a\0".into()]),
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
            ("ipv6 prefix 63", Box::new(|c| v6(c).prefix_len = 63)),
            ("ipv6 prefix 127", Box::new(|c| v6(c).prefix_len = 127)),
            (
                "ipv6 gateway",
                Box::new(|c| v6(c).gateway = "fd76:6d6b:6975::1".parse().unwrap()),
            ),
            ("ipv6 address is gateway", Box::new(|c| v6(c).address = v6(c).gateway)),
            (
                "ipv6 subnet router",
                Box::new(|c| v6(c).address = "fd76:6d6b:6974::".parse().unwrap()),
            ),
            ("ipv6 link-local", Box::new(|c| *v6(c) = net6("fe80::2", 64, "fe80::1"))),
            ("ipv6 multicast", Box::new(|c| *v6(c) = net6("ff02::2", 64, "ff02::1"))),
            ("ipv6 loopback", Box::new(|c| *v6(c) = net6("::1", 64, "::2"))),
            ("ipv6 unspecified", Box::new(|c| *v6(c) = net6("::", 64, "::1"))),
            (
                "ipv6 mapped",
                Box::new(|c| *v6(c) = net6("::ffff:10.0.0.2", 96, "::ffff:10.0.0.1")),
            ),
        ];
        for (name, mutate) in cases {
            let mut c = config();
            mutate(&mut c);
            assert!(c.validate().is_err(), "{name} was accepted");
        }
    }

    #[test]
    fn search_domains_and_resolver_options_round_trip_and_are_optional() {
        let mut c = config();
        let n = c.network.as_mut().unwrap();
        n.search = vec!["default.svc.cluster.local".into(), "svc.cluster.local".into()];
        n.options = vec!["ndots:5".into()];
        c.validate().unwrap();
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains("\"search\":[\"default.svc.cluster.local\""), "{json}");
        assert_eq!(serde_json::from_str::<Config>(&json).unwrap(), c);
        let plain = serde_json::to_string(&config()).unwrap();
        assert!(!plain.contains("search") && !plain.contains("options"), "{plain}");
        assert!(
            serde_json::from_str::<Config>(&plain)
                .unwrap()
                .network
                .unwrap()
                .search
                .is_empty()
        );
    }

    /// A pod IP can be a /32 with a gateway outside it (Calico's 169.254.1.1): the guest
    /// routes to it on-link.
    #[test]
    fn a_gateway_outside_the_prefix_is_on_link() {
        let mut c = config();
        let n = c.network.as_mut().unwrap();
        n.address = Ipv4Addr::new(10, 244, 1, 5);
        n.prefix_len = 32;
        n.gateway = Ipv4Addr::new(169, 254, 1, 1);
        c.validate().unwrap();
        assert!(!c.network.as_ref().unwrap().gateway_in_prefix());
        assert!(config().network.unwrap().gateway_in_prefix());
        let mut c = config();
        c.network.as_mut().unwrap().prefix_len = 31;
        c.network.as_mut().unwrap().gateway = Ipv4Addr::new(172, 30, 0, 3);
        c.validate().unwrap();
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
