//! The shim's configuration (design §10.1): a root-owned TOML file, named by
//! containerd's runtime options (`ConfigPath`, or the file's text inline), else by
//! `$KILN_SHIM_CONFIG`, else `/etc/kiln/shim.toml`. The node admin who writes it is
//! the trust root for the kernel and kiln-init that boot (kiln T4).

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kiln_run::VmmKind;
use kiln_run::options::{Cidr, parse_size};
use serde::Deserialize;

use crate::error::{Error, Result};

/// Where the configuration is when nothing names another file.
pub const DEFAULT_PATH: &str = "/etc/kiln/shim.toml";
/// The environment variable naming another file (tests, `ctr run`).
pub const ENV: &str = "KILN_SHIM_CONFIG";

/// The shim's settings, checked (`Default` is only a placeholder, never loaded).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    /// The unprivileged user VMs run as; it must be in the kvm group (kiln T6).
    pub user: String,
    /// That user's kiln store (`KILN_HOME`): the VMs' scratch disks.
    pub store: PathBuf,
    pub kernel: PathBuf,
    /// A static kiln-init binary, wrapped into each VM's init layer.
    pub init: PathBuf,
    pub vmm: VmmKind,
    pub default_cpus: u8,
    pub max_cpus: u8,
    /// Guest memory for a container without a memory limit (D3).
    pub default_memory_mib: u32,
    /// Each container VM's scratch disk, its writable layer.
    pub scratch_disk: u64,
    /// Each pod's disk for its `emptyDir` volumes, shared by its containers in turn.
    pub pod_disk: u64,
    pub boot_timeout: Duration,
    /// Link-local or node addresses the guest may reach anyway (design §6.1).
    pub egress_allow: Vec<Cidr>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    user: String,
    store: PathBuf,
    kernel: PathBuf,
    init: PathBuf,
    #[serde(default)]
    vmm: Option<String>,
    #[serde(default)]
    default_cpus: Option<u8>,
    #[serde(default)]
    max_cpus: Option<u8>,
    #[serde(default)]
    default_memory_mib: Option<u32>,
    #[serde(default)]
    scratch_disk: Option<String>,
    #[serde(default)]
    pod_disk: Option<String>,
    #[serde(default)]
    boot_timeout_secs: Option<u64>,
    #[serde(default)]
    egress_allow: Vec<String>,
}

fn invalid(what: &str, reason: impl std::fmt::Display) -> Error {
    Error::Config(format!("{what}: {reason}"))
}

impl Settings {
    /// Parses and checks the file's text.
    pub fn parse(text: &str) -> Result<Self> {
        let raw: Raw = toml::from_str(text).map_err(|e| Error::Config(e.message().to_string()))?;
        if raw.user.is_empty() {
            return Err(invalid("user", "is empty"));
        }
        for (what, p) in [("store", &raw.store), ("kernel", &raw.kernel), ("init", &raw.init)] {
            if !p.is_absolute() {
                return Err(invalid(what, format!("{} is not an absolute path", p.display())));
            }
        }
        let vmm = match raw.vmm.as_deref() {
            None => VmmKind::default(),
            Some(v) => v.parse().map_err(|e| invalid("vmm", e))?,
        };
        let default_cpus = raw.default_cpus.unwrap_or(1);
        let max_cpus = raw.max_cpus.unwrap_or(8);
        if default_cpus == 0 || max_cpus == 0 || default_cpus > max_cpus {
            return Err(invalid(
                "default_cpus/max_cpus",
                "at least 1, and default_cpus at most max_cpus",
            ));
        }
        let default_memory_mib = raw.default_memory_mib.unwrap_or(1024);
        if default_memory_mib < 64 {
            return Err(invalid("default_memory_mib", "at least 64"));
        }
        let scratch_disk = match &raw.scratch_disk {
            None => kiln_run::options::DEFAULT_DISK,
            Some(s) => parse_size(s).map_err(|e| invalid("scratch_disk", e))?,
        };
        kiln_run::options::check_disk(scratch_disk).map_err(|e| invalid("scratch_disk", e))?;
        let pod_disk = match &raw.pod_disk {
            None => 1 << 30,
            Some(s) => parse_size(s).map_err(|e| invalid("pod_disk", e))?,
        };
        kiln_run::options::check_disk(pod_disk).map_err(|e| invalid("pod_disk", e))?;
        let egress_allow = raw
            .egress_allow
            .iter()
            .map(|c| match c.parse::<Cidr>().map_err(|e| invalid("egress_allow", e))? {
                // The tap carries the guest's IPv4 only (design §6.1).
                Cidr {
                    addr: std::net::IpAddr::V6(_),
                    ..
                } => Err(invalid("egress_allow", format!("{c} is IPv6; the guest is IPv4 only"))),
                v4 => Ok(v4),
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Settings {
            user: raw.user,
            store: raw.store,
            kernel: raw.kernel,
            init: raw.init,
            vmm,
            default_cpus,
            max_cpus,
            default_memory_mib,
            scratch_disk,
            pod_disk,
            boot_timeout: Duration::from_secs(raw.boot_timeout_secs.unwrap_or(30)),
            egress_allow,
        })
    }

    /// The settings for a task: from containerd's runtime options when they name a
    /// file or carry its text, else from [`ENV`]'s file, else from [`DEFAULT_PATH`].
    pub fn load(options: Option<&RuntimeOptions>) -> Result<Self> {
        if let Some(o) = options {
            if !o.config_path.is_empty() {
                return Self::read(Path::new(&o.config_path));
            }
            if !o.config_body.is_empty() {
                return Self::parse(&o.config_body);
            }
        }
        match std::env::var_os(ENV) {
            Some(p) if !p.is_empty() => Self::read(Path::new(&p)),
            _ => Self::read(Path::new(DEFAULT_PATH)),
        }
    }

    fn read(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| invalid(&path.display().to_string(), e))?;
        Self::parse(&text).map_err(|e| Error::Config(format!("{}: {e}", path.display())))
    }

    /// Whether the guest may reach `addr` although the fixed deny list covers it.
    pub fn allowed(&self, addr: Ipv4Addr) -> bool {
        self.egress_allow.iter().any(|c| match c.addr {
            std::net::IpAddr::V4(net) => {
                let mask = u32::MAX.checked_shl(32 - u32::from(c.prefix)).unwrap_or(0);
                u32::from(addr) & mask == u32::from(net) & mask
            }
            std::net::IpAddr::V6(_) => false,
        })
    }
}

/// containerd's `runtimeoptions.v1.Options`, which the CRI plugin sends for any shim
/// but runc's: `type_url` (1), `config_path` (2), `config_body` (3). The Rust protos
/// do not include it, and three string fields need no generated code.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeOptions {
    pub type_url: String,
    pub config_path: String,
    pub config_body: String,
}

impl RuntimeOptions {
    /// The protobuf type of `CreateTaskRequest.options` that carries it.
    pub const TYPE_URL: &'static str = "runtimeoptions.v1.Options";

    /// Decodes the message; fields of other numbers or wire types are skipped.
    pub fn decode(mut bytes: &[u8]) -> Result<Self> {
        let bad = || Error::Config("runtime options: malformed protobuf".into());
        let mut out = RuntimeOptions::default();
        while !bytes.is_empty() {
            let key = varint(&mut bytes).ok_or_else(bad)?;
            match key & 7 {
                0 => {
                    varint(&mut bytes).ok_or_else(bad)?;
                }
                1 => bytes = bytes.get(8..).ok_or_else(bad)?,
                5 => bytes = bytes.get(4..).ok_or_else(bad)?,
                2 => {
                    let len = usize::try_from(varint(&mut bytes).ok_or_else(bad)?).map_err(|_| bad())?;
                    let value = bytes.get(..len).ok_or_else(bad)?;
                    bytes = &bytes[len..];
                    let text = || String::from_utf8(value.to_vec()).map_err(|_| bad());
                    match key >> 3 {
                        1 => out.type_url = text()?,
                        2 => out.config_path = text()?,
                        3 => out.config_body = text()?,
                        _ => {}
                    }
                }
                _ => return Err(bad()),
            }
        }
        Ok(out)
    }
}

fn varint(bytes: &mut &[u8]) -> Option<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let (&b, rest) = bytes.split_first()?;
        *bytes = rest;
        v |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
user = "kiln"
store = "/var/lib/kiln"
kernel = "/var/lib/kiln/boot/vmlinux"
init = "/usr/local/lib/kiln/kiln-init"
vmm = "firecracker"
default_cpus = 1
max_cpus = 8
default_memory_mib = 1024
scratch_disk = "4G"
boot_timeout_secs = 30
egress_allow = ["169.254.20.10/32"]
"#;

    const MINIMAL: &str = r#"
user = "kiln"
store = "/var/lib/kiln"
kernel = "/k"
init = "/i"
"#;

    #[test]
    fn parses_the_documented_example() {
        let s = Settings::parse(EXAMPLE).unwrap();
        assert_eq!(s.user, "kiln");
        assert_eq!(s.store, Path::new("/var/lib/kiln"));
        assert_eq!(s.vmm, VmmKind::Firecracker);
        assert_eq!(s.scratch_disk, 4 << 30);
        assert_eq!(s.egress_allow, ["169.254.20.10/32".parse::<Cidr>().unwrap()]);
        assert!(s.allowed(Ipv4Addr::new(169, 254, 20, 10)));
        assert!(!s.allowed(Ipv4Addr::new(169, 254, 169, 254)));
    }

    #[test]
    fn defaults() {
        let s = Settings::parse(MINIMAL).unwrap();
        assert_eq!(s.vmm, VmmKind::Firecracker);
        assert_eq!((s.default_cpus, s.max_cpus), (1, 8));
        assert_eq!(s.default_memory_mib, 1024);
        assert_eq!(s.scratch_disk, 4 << 30);
        assert_eq!(s.boot_timeout, Duration::from_secs(30));
        assert!(s.egress_allow.is_empty());
        let ch = Settings::parse(&format!("{MINIMAL}vmm = \"cloud-hypervisor\"\n")).unwrap();
        assert_eq!(ch.vmm, VmmKind::CloudHypervisor);
    }

    #[test]
    fn refusals() {
        let cases = [
            (format!("{MINIMAL}colour = 1\n"), "colour"),
            (MINIMAL.replace("user = \"kiln\"", ""), "user"),
            (format!("{MINIMAL}default_memory_mib = 32\n"), "default_memory_mib"),
            (format!("{MINIMAL}max_cpus = 0\n"), "max_cpus"),
            (format!("{MINIMAL}egress_allow = [\"10.0.0.0/33\"]\n"), "egress_allow"),
            (format!("{MINIMAL}egress_allow = [\"fd00:ec2::254/128\"]\n"), "IPv6"),
            (MINIMAL.replace("\"/k\"", "\"vmlinux\""), "kernel"),
            (MINIMAL.replace("\"/i\"", "\"kiln-init\""), "init"),
            (format!("{MINIMAL}scratch_disk = \"1M\"\n"), "scratch_disk"),
            (format!("{MINIMAL}vmm = \"qemu\"\n"), "vmm"),
        ];
        for (text, what) in cases {
            let err = Settings::parse(&text).unwrap_err().to_string();
            assert!(err.contains(what), "{what}: {err}");
        }
    }

    fn field(n: u8, s: &str) -> Vec<u8> {
        let mut v = vec![(n << 3) | 2, s.len() as u8];
        v.extend_from_slice(s.as_bytes());
        v
    }

    #[test]
    fn runtime_options_decode() {
        let mut bytes = field(1, "type.googleapis.com/x");
        bytes.extend(field(2, "/etc/kiln/shim.toml"));
        // An unknown varint field is skipped.
        bytes.extend([4 << 3, 0x96, 0x01]);
        let o = RuntimeOptions::decode(&bytes).unwrap();
        assert_eq!(o.config_path, "/etc/kiln/shim.toml");
        assert_eq!(o.type_url, "type.googleapis.com/x");
        assert!(o.config_body.is_empty());
        assert!(RuntimeOptions::decode(&[2 << 3 | 2, 9, b'a']).is_err(), "truncated");
        assert_eq!(RuntimeOptions::decode(&[]).unwrap(), RuntimeOptions::default());
    }

    #[test]
    fn the_inline_body_is_used_when_no_path_is_given() {
        let o = RuntimeOptions {
            config_body: MINIMAL.into(),
            ..RuntimeOptions::default()
        };
        assert_eq!(Settings::load(Some(&o)).unwrap().kernel, Path::new("/k"));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shim.toml");
        std::fs::write(&path, EXAMPLE).unwrap();
        let o = RuntimeOptions {
            config_path: path.display().to_string(),
            config_body: "not toml [".into(),
            ..RuntimeOptions::default()
        };
        assert_eq!(Settings::load(Some(&o)).unwrap().user, "kiln", "the path wins");
    }
}
