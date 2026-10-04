//! kiln images: the OCI → kiln conversion pipeline, image format, inspect, import,
//! registry convert, pull and push, and the scratch disk.
#![forbid(unsafe_code)]

mod convert;
mod ctx;
mod decompress;
mod error;
mod import;
mod load;
mod pipeline;
mod registry;
pub mod scratch;
pub mod types;

pub use convert::{ConvertOptions, Converted, LayerReport, LayerSource, StoredLayers, convert_image};
pub use error::{ImageError, Result};
pub use import::{ImportReport, import_image};
pub use load::{KilnManifest, Loaded, load, load_manifest, resolve_name};
pub use pipeline::{LocalRequest, Output, convert_local, convert_resolved};
pub use registry::{RegistryRequest, TransferReport, convert_registry, pull_image, push_image};
