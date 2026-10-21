//! The kiln snapshot artifact (design §5): a vmkit snapshot bundle chunked into
//! content-addressed blobs, stored in `kiln-store` and moved through
//! `kiln-registry`. A pulled snapshot is hostile input: every chunk is verified
//! and the config is bounded and checked before anything is fetched or tagged.
#![forbid(unsafe_code)]

mod chunk;
mod error;
pub mod format;

pub use chunk::{Built, MAX_BUNDLE_JSON, chunk_bundle};
pub use error::{Result, SnapshotError};
pub use format::{
    BUNDLE_JSON, ChunkedFile, DEFAULT_CHUNK_SIZE, LAZY_JSON, Limits, SCHEMA_VERSION, SNAPSHOT_ARTIFACT, SNAPSHOT_CHUNK,
    SNAPSHOT_CONFIG, SnapshotConfig, check_name,
};
