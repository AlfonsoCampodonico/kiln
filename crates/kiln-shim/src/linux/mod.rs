//! The Linux backend (design §8): the pod sandbox's `pause` process, the pod's network
//! attachment, and the VM workers.

pub mod attach;
mod backend;
pub mod pause;
pub mod plan;
pub mod worker;

pub use backend::LinuxBackend;

/// Where the shim keeps its state (root, 0700).
pub const STATE: &str = "/run/kiln-shim";
/// The environment variable naming a worker's channel fd.
pub const FD_ENV: &str = "KILN_SHIM_FD";
