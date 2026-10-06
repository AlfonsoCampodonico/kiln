//! What the user asked for (`kiln run`'s flags, spec §9.7), and parsing sizes.

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
    /// `--disk`: the scratch disk's size; `None` is the default, 4 GiB.
    pub disk: Option<u64>,
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
        if let Some(d) = self.disk {
            check_disk(d)?;
        }
        if self.cpus == 0 || self.memory_mib < 64 {
            return Err(Error::invalid("--cpus/--memory", "at least 1 vCPU and 64 MiB"));
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
        o.memory_mib = 32;
        assert!(o.check().is_err());
    }
}
