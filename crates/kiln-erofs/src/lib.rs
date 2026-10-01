//! Deterministic erofs writer and reader implementing kiln's erofs profile
//! (spec §7). One OCI layer tar becomes one erofs image.
#![forbid(unsafe_code)]
// Modules are wired together incrementally; Task 10 removes this allowance.
#![allow(dead_code)]

mod apply;
mod error;
mod layout;
mod limits;
pub mod ondisk;
mod path;
mod pax;
mod tarstream;
#[doc(hidden)]
pub mod testtar;
mod tree;
mod writer;

pub use error::{Error, Result};
pub use limits::Limits;
pub use tree::{DirAttrs, Meta, Timestamp, XattrKey, Xattrs};
pub use writer::{LayerSummary, LayerWriter};

/// Version of kiln's erofs profile. Bump whenever output bytes change.
pub const FORMAT_VERSION: u32 = 1;
