//! Deterministic erofs writer and reader implementing kiln's erofs profile
//! (spec §7). One OCI layer tar becomes one erofs image.
#![forbid(unsafe_code)]
// Modules are wired together incrementally; Task 10 removes this allowance.
#![allow(dead_code)]

mod apply;
mod error;
mod limits;
pub mod ondisk;
mod path;
mod pax;
#[doc(hidden)]
pub mod testtar;
mod tree;

pub use error::{Error, Result};
pub use limits::Limits;
pub use tree::{DirAttrs, Meta, Timestamp, XattrKey, Xattrs};
