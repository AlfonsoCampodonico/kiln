//! The host side of `kiln run` (spec §5.3, §8.1, §9): resolving an image and the
//! kernel and init that boot it, the per-run state directory, the scratch disk,
//! the control protocol session with the guest (T9), stdio, the terminal and
//! signals. The VMMs run through `vmkit` on Linux; on other systems only the
//! platform-independent parts build, and `kiln run` explains how to use a Lima VM
//! instead ([`LIMA_INSTRUCTIONS`]).
#![forbid(unsafe_code)]

pub mod config;
pub mod console;
mod error;
pub mod escape;
pub mod options;
pub mod prepare;
pub mod protocol;
pub mod rundir;

#[cfg(target_os = "linux")]
mod linux;

pub use error::{Error, Result};
#[cfg(target_os = "linux")]
pub use linux::*;
pub use options::{RunOptions, VmmKind};

/// What `kiln run` prints where it cannot run VMs (spec §9.7, §10).
pub const LIMA_INSTRUCTIONS: &str = "\
kiln run needs Linux with KVM. On macOS, run it in a Lima VM with nested
virtualization (vmType: vz, nestedVirtualization: true; an M3 or later Mac with
macOS 15 or later) where Firecracker or Cloud Hypervisor and vmkit-sandbox are
installed as vmkit's README describes. Copy images into the VM's store with
`kiln import --from-store <this store> <image>`.";
