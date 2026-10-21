use thiserror::Error;

/// Why a task call failed; the service maps each kind to a ttrpc code.
#[derive(Debug, Error)]
pub enum Error {
    /// The shim's configuration (`InvalidArgument`).
    #[error("kiln-shim config: {0}")]
    Config(String),
    /// The request or its spec cannot run under kiln (`InvalidArgument`).
    #[error("{0}")]
    Invalid(String),
    /// Valid, but not now (`FailedPrecondition`).
    #[error("{0}")]
    Refused(String),
    #[error("{0}")]
    NotFound(String),
    /// kiln cannot do this yet (`Unimplemented`).
    #[error("{0}")]
    Unsupported(String),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// The VM worker or the pod's network setup failed (`Internal`).
    #[error("{0}")]
    Internal(String),
}

impl Error {
    pub(crate) fn invalid(msg: impl Into<String>) -> Self {
        Error::Invalid(msg.into())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
