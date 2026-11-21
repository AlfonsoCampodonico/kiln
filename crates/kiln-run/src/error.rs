use thiserror::Error;

/// Why a run could not start, or failed on the host's side.
#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Image(#[from] kiln_image::ImageError),
    #[error(transparent)]
    Store(#[from] kiln_store::StoreError),
    #[error(transparent)]
    Proto(#[from] kiln_proto::ProtoError),
    #[cfg(target_os = "linux")]
    #[error(transparent)]
    Vmm(#[from] vmkit::Error),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// The run is refused as asked; the message says why and what to do.
    #[error("{0}")]
    Refused(String),
    #[error("invalid {what}: {reason}")]
    Invalid { what: &'static str, reason: String },
    /// A build step would mount more lower layers than a build VM's overlay can
    /// name (`kiln_proto::MAX_BUILD_LOWERS`): base layers, stacked layers and
    /// one sealed upper per successful step. Refused before anything is sent, so
    /// the VM is still usable: finish it, then squash layers or boot a new one.
    #[error(
        "step {step} would mount {lowers} lower layers, more than a build VM's {max}: squash layers or boot a new \
         build VM"
    )]
    TooManyLowers { step: u32, lowers: usize, max: usize },
    /// SIGINT or SIGTERM arrived during setup: the run was abandoned (exit 137).
    #[error("interrupted before the guest started")]
    Interrupted,
}

impl Error {
    pub(crate) fn refused(msg: impl Into<String>) -> Self {
        Error::Refused(msg.into())
    }

    pub(crate) fn invalid(what: &'static str, reason: impl Into<String>) -> Self {
        Error::Invalid {
            what,
            reason: reason.into(),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
