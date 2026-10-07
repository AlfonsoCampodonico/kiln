//! What the user asked for (`kiln run`'s flags, spec §9.7), and parsing the values
//! that have their own syntax: sizes, `--egress` and `-p`.

use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use crate::error::{Error, Result};

/// The default scratch disk (spec §9.4).
pub const DEFAULT_DISK: u64 = 4 << 30;
/// The smallest scratch disk: the ext4 template.
pub const MIN_DISK: u64 = 64 << 20;
pub const DEFAULT_STOP_TIMEOUT: u32 = 10;
pub const DEFAULT_BOOT_TIMEOUT: Duration = Duration::from_secs(30);
pub const DEFAULT_MEMORY_MIB: u32 = 512;
pub const DEFAULT_CPUS: u8 = 1;

/// Which VMM runs the guest.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum VmmKind {
    /// The reference VMM.
    #[default]
    Firecracker,
    CloudHypervisor,
}

impl VmmKind {
    pub fn name(self) -> &'static str {
        match self {
            VmmKind::Firecracker => "firecracker",
            VmmKind::CloudHypervisor => "cloud-hypervisor",
        }
    }
}

impl FromStr for VmmKind {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, String> {
        match s {
            "firecracker" | "fc" => Ok(VmmKind::Firecracker),
            "cloud-hypervisor" | "ch" => Ok(VmmKind::CloudHypervisor),
            _ => Err(format!("unknown VMM {s:?} (firecracker or cloud-hypervisor)")),
        }
    }
}

/// `--egress` (spec §9.3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Egress {
    /// Everything but private, link-local (cloud metadata), CGNAT, loopback,
    /// multicast and reserved ranges, and the host's own addresses.
    #[default]
    Restricted,
    /// Nothing but DNS and the `allow=` exceptions.
    DenyAll,
    /// Everything (kiln warns).
    Open,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Tcp,
    Udp,
}

/// `-p [ADDRESS:]HOST:GUEST[/tcp|udp]`: the guest's port `guest` on the host's
/// `address:host`. The address defaults to `127.0.0.1`, so a published port stays
/// on this host unless an address (`0.0.0.0` for all) is given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortSpec {
    pub address: Ipv4Addr,
    pub host: u16,
    pub guest: u16,
    pub protocol: Protocol,
}

/// The host address `-p` binds when none is given.
pub const DEFAULT_PUBLISH_ADDRESS: Ipv4Addr = Ipv4Addr::LOCALHOST;

impl FromStr for PortSpec {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, String> {
        let bad = || format!("{s:?} is not [ADDRESS:]HOST:GUEST[/tcp|udp] with an IPv4 address and ports 1-65535");
        let (rest, protocol) = match s.split_once('/') {
            Some((p, "tcp")) => (p, Protocol::Tcp),
            Some((p, "udp")) => (p, Protocol::Udp),
            Some(_) => return Err(bad()),
            None => (s, Protocol::Tcp),
        };
        let parts: Vec<&str> = rest.split(':').collect();
        let (address, h, g) = match parts.as_slice() {
            [h, g] => (DEFAULT_PUBLISH_ADDRESS, *h, *g),
            [a, h, g] => (a.parse().map_err(|_| bad())?, *h, *g),
            _ => return Err(bad()),
        };
        let port = |p: &str| p.parse::<u16>().ok().filter(|&n| n != 0).ok_or_else(bad);
        Ok(PortSpec {
            address,
            host: port(h)?,
            guest: port(g)?,
            protocol,
        })
    }
}

/// An IPv4 network for `--egress allow=`; a bare address is a /32.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    pub addr: Ipv4Addr,
    pub prefix: u8,
}

impl FromStr for Cidr {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, String> {
        let bad = || format!("{s:?} is not an IPv4 address or CIDR");
        let (a, p) = s.split_once('/').unwrap_or((s, "32"));
        let prefix = p.parse::<u8>().ok().filter(|&p| p <= 32).ok_or_else(bad)?;
        Ok(Cidr {
            addr: a.parse().map_err(|_| bad())?,
            prefix,
        })
    }
}

/// `--net` and its options.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetOptions {
    pub egress: Egress,
    pub allow: Vec<Cidr>,
    pub publish: Vec<PortSpec>,
}

impl NetOptions {
    /// Applies one `--egress` value: `restricted`, `deny-all`, `open` or `allow=CIDR`
    /// (repeatable; two different policies are an error).
    pub fn add_egress(&mut self, value: &str, policy_set: &mut bool) -> std::result::Result<(), String> {
        if let Some(cidr) = value.strip_prefix("allow=") {
            self.allow.push(cidr.parse()?);
            return Ok(());
        }
        let policy = match value {
            "restricted" => Egress::Restricted,
            "deny-all" => Egress::DenyAll,
            "open" => Egress::Open,
            _ => {
                return Err(format!(
                    "unknown --egress {value:?} (restricted, deny-all, open or allow=CIDR)"
                ));
            }
        };
        if *policy_set && self.egress != policy {
            return Err("--egress names two policies".into());
        }
        *policy_set = true;
        self.egress = policy;
        Ok(())
    }
}

/// Everything `kiln run` was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOptions {
    /// A ref name or digest in the store.
    pub image: String,
    pub vmm: VmmKind,
    pub cpus: u8,
    pub memory_mib: u32,
    /// `-i`: relay stdin.
    pub interactive: bool,
    /// `-t`: a pseudo-terminal.
    pub tty: bool,
    /// `--disk`: the scratch disk's size; `None` is the default, 4 GiB, or a
    /// persisted disk's own size.
    pub disk: Option<u64>,
    /// `--persist PATH`: a scratch disk kept across runs (spec §9.4).
    pub persist: Option<PathBuf>,
    /// `--persist-reset`: recreate the persisted disk.
    pub persist_reset: bool,
    /// `--persist-force`: reuse it although its sidecar names other layers.
    pub persist_force: bool,
    /// `--net`, with its options.
    pub net: Option<NetOptions>,
    /// `--env K=V` or `--env K` (taken from kiln's environment, dropped when unset).
    pub env: Vec<String>,
    /// After `--`: replaces the image's cmd (the entrypoint stays), as with `docker run`.
    pub cmd: Vec<String>,
    pub stop_timeout: u32,
    pub boot_timeout: Duration,
    pub kernel: Option<PathBuf>,
    pub allow_custom_kernel: bool,
    pub init: Option<PathBuf>,
    pub allow_custom_init: bool,
}

impl RunOptions {
    pub fn new(image: impl Into<String>) -> Self {
        Self {
            image: image.into(),
            vmm: VmmKind::default(),
            cpus: DEFAULT_CPUS,
            memory_mib: DEFAULT_MEMORY_MIB,
            interactive: false,
            tty: false,
            disk: None,
            persist: None,
            persist_reset: false,
            persist_force: false,
            net: None,
            env: Vec::new(),
            cmd: Vec::new(),
            stop_timeout: DEFAULT_STOP_TIMEOUT,
            boot_timeout: DEFAULT_BOOT_TIMEOUT,
            kernel: None,
            allow_custom_kernel: false,
            init: None,
            allow_custom_init: false,
        }
    }

    /// Checks combinations that no single flag can: everything that does not need the
    /// image or the host.
    pub fn check(&self) -> Result<()> {
        if self.kernel.is_some() && !self.allow_custom_kernel {
            return Err(Error::refused(
                "--kernel boots a kernel kiln has not pinned; add --allow-custom-kernel to accept that",
            ));
        }
        if self.init.is_some() && !self.allow_custom_init {
            return Err(Error::refused(
                "--init boots a kiln-init kiln has not pinned; add --allow-custom-init to accept that",
            ));
        }
        if (self.persist_reset || self.persist_force) && self.persist.is_none() {
            return Err(Error::refused("--persist-reset and --persist-force need --persist"));
        }
        if self.persist_reset && self.persist_force {
            return Err(Error::refused("--persist-reset and --persist-force exclude each other"));
        }
        if let Some(d) = self.disk {
            check_disk(d)?;
        }
        if self.cpus == 0 || self.memory_mib < 64 {
            return Err(Error::invalid("--cpus/--memory", "at least 1 vCPU and 64 MiB"));
        }
        if let Some(net) = &self.net {
            // The VM's namespace sees only the port, whatever address it was published on.
            for (i, p) in net.publish.iter().enumerate() {
                if net.publish[..i]
                    .iter()
                    .any(|q| q.protocol == p.protocol && q.host == p.host)
                {
                    return Err(Error::invalid("-p", format!("host port {} is published twice", p.host)));
                }
            }
        }
        Ok(())
    }
}

/// A scratch disk size must be a multiple of 4096 and at least 64 MiB.
pub fn check_disk(size: u64) -> Result<()> {
    if size < MIN_DISK || !size.is_multiple_of(4096) {
        return Err(Error::invalid(
            "--disk",
            format!("{size} bytes: the size must be a multiple of 4096 and at least 64M"),
        ));
    }
    Ok(())
}

/// Parses `4G`, `512M`, `65536K` or a byte count (`k`/`m`/`g`/`t`, optionally with
/// `B` or `iB`; every unit is binary, as Docker's are).
pub fn parse_size(s: &str) -> std::result::Result<u64, String> {
    let bad = || format!("{s:?} is not a size such as 4G, 512M or 1073741824");
    let t = s.trim();
    let lower = t.to_ascii_lowercase();
    let digits = lower.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let unit = &lower[digits.len()..];
    let shift = match unit {
        "" | "b" => 0,
        "k" | "kb" | "kib" => 10,
        "m" | "mb" | "mib" => 20,
        "g" | "gb" | "gib" => 30,
        "t" | "tb" | "tib" => 40,
        _ => return Err(bad()),
    };
    let n: u64 = digits.parse().map_err(|_| bad())?;
    n.checked_mul(1 << shift).ok_or_else(bad)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("4G").unwrap(), 4 << 30);
        assert_eq!(parse_size("512m").unwrap(), 512 << 20);
        assert_eq!(parse_size("64MiB").unwrap(), 64 << 20);
        assert_eq!(parse_size("1073741824").unwrap(), 1 << 30);
        for bad in ["", "G", "4X", "-1", "4.5G", "99999999999T"] {
            assert!(parse_size(bad).is_err(), "{bad}");
        }
        assert!(check_disk(64 << 20).is_ok());
        assert!(check_disk((64 << 20) - 4096).is_err());
        assert!(check_disk((1 << 30) + 1).is_err());
    }

    #[test]
    fn vmm_names() {
        assert_eq!("fc".parse::<VmmKind>().unwrap(), VmmKind::Firecracker);
        assert_eq!(
            "cloud-hypervisor".parse::<VmmKind>().unwrap().name(),
            "cloud-hypervisor"
        );
        assert!(
            "qemu"
                .parse::<VmmKind>()
                .unwrap_err()
                .contains("firecracker or cloud-hypervisor")
        );
    }

    #[test]
    fn published_ports() {
        let p: PortSpec = "8080:80".parse().unwrap();
        assert_eq!(
            p,
            PortSpec {
                address: Ipv4Addr::LOCALHOST,
                host: 8080,
                guest: 80,
                protocol: Protocol::Tcp
            }
        );
        let p: PortSpec = "0.0.0.0:5353:53/udp".parse().unwrap();
        assert_eq!(
            (p.address, p.host, p.guest, p.protocol),
            (Ipv4Addr::UNSPECIFIED, 5353, 53, Protocol::Udp)
        );
        assert_eq!(
            "192.168.5.15:1:2/tcp".parse::<PortSpec>().unwrap().address,
            Ipv4Addr::new(192, 168, 5, 15)
        );
        for bad in [
            "80",
            "0:80",
            "80:0",
            "a:80",
            "80:80/sctp",
            "::1:80:80",
            "1.2.3:80:80",
            "x:1:2:3",
            "70000:80",
        ] {
            let e = bad.parse::<PortSpec>().unwrap_err();
            assert!(e.contains("[ADDRESS:]HOST:GUEST"), "{bad}: {e}");
        }
        let mut o = RunOptions::new("img");
        let mut net = NetOptions {
            publish: vec!["8080:80".parse().unwrap(), "0.0.0.0:8080:81".parse().unwrap()],
            ..NetOptions::default()
        };
        o.net = Some(net.clone());
        assert!(o.check().unwrap_err().to_string().contains("published twice"));
        net.publish[1].protocol = Protocol::Udp;
        o.net = Some(net);
        o.check().unwrap();
    }

    #[test]
    fn egress_values() {
        let mut n = NetOptions::default();
        let mut set = false;
        n.add_egress("allow=10.1.0.0/16", &mut set).unwrap();
        n.add_egress("allow=1.1.1.1", &mut set).unwrap();
        n.add_egress("deny-all", &mut set).unwrap();
        n.add_egress("deny-all", &mut set).unwrap();
        assert_eq!(n.egress, Egress::DenyAll);
        assert_eq!(
            n.allow[1],
            Cidr {
                addr: Ipv4Addr::new(1, 1, 1, 1),
                prefix: 32
            }
        );
        assert!(n.add_egress("open", &mut set).unwrap_err().contains("two policies"));
        assert!(n.add_egress("allow=10.0.0.0/33", &mut set).is_err());
        assert!(
            n.add_egress("everything", &mut set)
                .unwrap_err()
                .contains("unknown --egress")
        );
    }

    #[test]
    fn custom_artifacts_need_their_flags() {
        let mut o = RunOptions::new("img");
        o.check().unwrap();
        o.kernel = Some("/k".into());
        assert!(o.check().unwrap_err().to_string().contains("--allow-custom-kernel"));
        o.allow_custom_kernel = true;
        o.init = Some("/i".into());
        assert!(o.check().unwrap_err().to_string().contains("--allow-custom-init"));
        o.allow_custom_init = true;
        o.check().unwrap();
        o.disk = Some(1000);
        assert!(o.check().unwrap_err().to_string().contains("--disk"));
        o.disk = None;
        o.persist_reset = true;
        assert!(o.check().unwrap_err().to_string().contains("need --persist"));
        o.persist = Some("/disk.img".into());
        o.check().unwrap();
        o.persist_force = true;
        assert!(o.check().unwrap_err().to_string().contains("exclude each other"));
        o.persist_reset = false;
        o.check().unwrap();
        o.memory_mib = 32;
        assert!(o.check().is_err());
    }
}
