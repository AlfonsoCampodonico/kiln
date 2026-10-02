//! OCI image types and verified local inputs for kiln (spec §6.1): OCI image
//! layouts and `docker save` archives. Every blob is hashed into the store;
//! file names, `index.json` and `manifest.json` are never trusted.
#![forbid(unsafe_code)]

mod error;
pub mod media;
mod platform;
mod types;

pub use error::{OciError, Result};
pub use platform::Platform;
pub use types::{ContainerConfig, Descriptor, ImageConfig, ImageIndex, ImageManifest, RootFs, canonical_json};
