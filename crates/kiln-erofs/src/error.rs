use thiserror::Error;

/// Errors from converting or reading erofs layers.
#[derive(Debug, Error)]
pub enum Error {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("malformed tar: {0}")]
    MalformedTar(String),
    #[error("path {path:?} escapes the layer root")]
    PathEscapesRoot { path: String },
    #[error("invalid path {path:?}: {reason}")]
    InvalidPath { path: String, reason: &'static str },
    #[error("unsupported tar entry at {path:?}: {kind}")]
    UnsupportedEntry { path: String, kind: String },
    #[error("parent of {path:?} is not a directory")]
    ParentNotDirectory { path: String },
    #[error("invalid hardlink {path:?} -> {target:?}: {reason}")]
    InvalidHardlink {
        path: String,
        target: String,
        reason: &'static str,
    },
    #[error("limit exceeded: {limit} (max {max}) at {path:?}")]
    LimitExceeded {
        limit: &'static str,
        max: u64,
        path: String,
    },
    #[error("xattr {name:?} on {path:?} cannot be encoded: {reason}")]
    XattrUnencodable {
        path: String,
        name: String,
        reason: &'static str,
    },
    #[error("an inode needs more than 255 shared xattrs")]
    TooManyXattrs,
    #[error("not a kiln-profile erofs image: {0}")]
    ProfileViolation(String),
    #[error("corrupt erofs image: {0}")]
    Corrupt(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Renders raw path bytes for error messages.
pub(crate) fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}
