//! `containerd-shim-kiln-v1`: a containerd shim v2 that runs a Kubernetes pod's
//! containers as kiln microVMs (the kiln-shim spec, and its v1 implementation
//! design `docs/superpowers/specs/2026-10-21-kiln-shim-v1-design.md`).
//!
//! The pure parts build everywhere: the configuration, the OCI spec subset and its
//! translation into kiln-proto's `Config`, the spec's mounts, the erofs snapshot
//! mounts, the pod network description, cgroup paths, the worker protocol and the
//! task state machine. The Linux backend runs the processes: `pause` for the pod
//! sandbox, and one VM worker per container.
#![cfg_attr(not(target_os = "linux"), forbid(unsafe_code))]

pub mod cgroup;
mod error;
pub mod mounts;
pub mod netconf;
pub mod oci;
pub mod passwd;
pub mod rootfs;
pub mod service;
pub mod settings;
pub mod tasks;
pub mod translate;
pub mod volumes;
pub mod wire;

#[cfg(target_os = "linux")]
pub mod linux;

pub use error::{Error, Result};

/// The runtime type containerd names this shim by (`containerd-shim-kiln-v1`).
pub const RUNTIME_ID: &str = "io.containerd.kiln.v1";
/// The tap the guest's NIC uses in the pod netns (design §6.1).
pub const TAP: &str = "kiln0";
