//! kiln images: the OCI → kiln conversion pipeline, image format, inspect and import.
#![forbid(unsafe_code)]

mod convert;
mod ctx;
mod decompress;
mod error;
mod load;
mod pipeline;
pub mod types;

pub use convert::{ConvertOptions, Converted, LayerReport, convert_image};
pub use error::{ImageError, Result};
pub use load::{KilnManifest, Loaded, load, load_manifest, resolve_name};
pub use pipeline::{LocalRequest, Output, convert_local, convert_resolved};
