use std::path::PathBuf;

use kiln_store::Digest;
use thiserror::Error;

/// Errors from building, reading, pushing or pulling a snapshot. Every check of
/// untrusted input has its own variant, and a bound names its limit (T3).
#[derive(Debug, Error)]
pub enum SnapshotError {
    #[error(transparent)]
    Store(#[from] kiln_store::StoreError),
    #[error(transparent)]
    Registry(#[from] kiln_registry::RegistryError),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid {what}: {source}")]
    Json {
        what: &'static str,
        source: serde_json::Error,
    },
    #[error("limit exceeded: {what} (max {max})")]
    LimitExceeded { what: &'static str, max: u64 },
    #[error("chunk size {size} is not a power of two from {min} to {max} bytes")]
    BadChunkSize { size: u64, min: u64, max: u64 },
    #[error("unsupported kiln snapshot schema version {0}")]
    UnknownSchema(u64),
    #[error("unsupported vmkit bundle version {0}")]
    UnknownBundleVersion(String),
    #[error("invalid file name {0:?} in a snapshot (expected [a-z0-9][a-z0-9._-]{{0,63}})")]
    BadName(String),
    #[error("file {0:?} appears twice in the snapshot")]
    DuplicateName(String),
    #[error("file {name:?}: {reason}")]
    BadFile { name: String, reason: String },
    #[error("layer {0} is listed twice")]
    DuplicateLayer(Digest),
    #[error("layer {0} is not used by any file")]
    UnusedLayer(Digest),
    #[error("layer {digest} has media type {media_type:?}, not a snapshot chunk")]
    BadLayerType { digest: Digest, media_type: String },
    #[error("{0} is not a kiln snapshot")]
    NotASnapshot(String),
    #[error("bundle {path}: {reason}")]
    Bundle { path: PathBuf, reason: String },
    #[error("{0} is not empty")]
    DestNotEmpty(PathBuf),
    #[error("no snapshot named {0:?}")]
    RefNotFound(String),
    #[error("invalid option: {0}")]
    BadOption(String),
}

pub type Result<T> = std::result::Result<T, SnapshotError>;

pub(crate) fn json<T: serde::de::DeserializeOwned>(what: &'static str, bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|source| SnapshotError::Json { what, source })
}
