use thiserror::Error;

use crate::Digest;

/// Errors from the kiln store.
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid digest {0:?} (expected sha256:<64 lowercase hex>)")]
    BadDigest(String),
    #[error("digest mismatch: expected {expected}, got {actual}")]
    DigestMismatch { expected: Digest, actual: Digest },
    #[error("size mismatch for {digest}: expected {expected} bytes, got {actual}")]
    SizeMismatch { digest: Digest, expected: u64, actual: u64 },
    #[error("blob {0} is not in the store")]
    NotFound(Digest),
    #[error("blob {digest} is {size} bytes, more than the {max}-byte limit for metadata")]
    TooLarge { digest: Digest, size: u64, max: u64 },
    #[error("invalid {what} {value:?}")]
    Invalid { what: &'static str, value: String },
    #[error("corrupt store file {path}: {reason}")]
    Corrupt { path: String, reason: String },
}

pub type Result<T> = std::result::Result<T, StoreError>;
