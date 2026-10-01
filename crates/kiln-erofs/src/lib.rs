//! Deterministic erofs writer and reader implementing kiln's erofs profile
//! (spec §7). One OCI layer tar becomes one erofs image.
#![forbid(unsafe_code)]

mod apply;
mod error;
mod layout;
mod limits;
mod merge;
pub mod ondisk;
mod path;
mod pax;
mod reader;
mod tarstream;
#[doc(hidden)]
pub mod testtar;
mod tree;
mod writer;

pub use error::{Error, Result};
pub use limits::Limits;
pub use merge::{resolve_inherited, squash};
pub use reader::{DataReader, DirEntry, Image, InodeInfo};
pub use tree::{DirAttrs, Meta, Timestamp, XattrKey, Xattrs};
pub use writer::{LayerSummary, LayerWriter};

/// Version of kiln's erofs profile. Bump whenever output bytes change.
pub const FORMAT_VERSION: u32 = 1;
