//! OCI image types and verified inputs for kiln (spec §6.1): registries, OCI image
//! layouts and `docker save` archives. Every blob is hashed into the store;
//! file names, `index.json` and `manifest.json` are never trusted.
#![forbid(unsafe_code)]

mod error;
pub mod media;
mod platform;
mod remote;
mod resolve;
mod source;
#[doc(hidden)]
pub mod testlayout;
mod types;

pub use error::{OciError, Result};
pub use platform::Platform;
pub use remote::{fetch_manifest, resolve_registry};
pub use resolve::{CONTAINERD_NAME, LocalSource, REF_NAME, ResolvedImage, resolve_local};
pub use source::{BlobSource, DirLayout, TarArchive};
pub use types::{ContainerConfig, Descriptor, ImageConfig, ImageIndex, ImageManifest, RootFs, canonical_json};
