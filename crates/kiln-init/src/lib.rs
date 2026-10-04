//! `kiln-init`, PID 1 of kiln guests (spec §9.6). The binary is `src/main.rs`; this
//! library holds its parts so that the pure ones are unit-tested on any host.
//!
//! `unsafe` is denied crate-wide and allowed only on the items that need it, each
//! with a `SAFETY` comment: the vsock address (rustix has no `sockaddr_vm`), the
//! ext4 resize ioctl, and the `pre_exec` hook that drops privileges and takes the
//! controlling terminal in the child.
#![deny(unsafe_code)]

pub mod disks;
pub mod env;
pub mod error;
pub mod etc;
pub mod netlink;
pub mod overlay;
pub mod passwd;

#[cfg(target_os = "linux")]
pub mod linux;
