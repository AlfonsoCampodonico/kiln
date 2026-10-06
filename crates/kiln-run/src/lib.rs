//! The host side of `kiln run` (spec §5.3, §8.1, §9): resolving an image and the
//! kernel and init that boot it, the per-run state directory, the scratch disk,
//! the control protocol session with the guest (T9), stdio, the terminal and
//! signals. The VMMs run through `vmkit` on Linux; on other systems only the
//! platform-independent parts build.
#![forbid(unsafe_code)]

pub mod config;
mod error;
pub mod escape;
pub mod options;
pub mod protocol;

pub use error::{Error, Result};
pub use options::{RunOptions, VmmKind};
