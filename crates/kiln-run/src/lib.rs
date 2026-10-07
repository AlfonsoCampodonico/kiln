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
pub mod persist;
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
virtualization (an M3 or later Mac with macOS 15 or later): kiln's lima/kiln.yaml
sets one up with Firecracker, Cloud Hypervisor and pasta, and mounts this Mac's
store read-only (limactl start --name kiln lima/kiln.yaml; install kiln and
vmkit-sandbox in it as the template says). Then run images there with
limactl shell kiln kiln run <image>: images converted on the Mac are imported
from its store when they run (KILN_IMPORT_FROM), and ports published with -p
on 127.0.0.1 reach the Mac's localhost through Lima.";
