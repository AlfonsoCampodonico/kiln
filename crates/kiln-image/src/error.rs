use kiln_store::Digest;
use thiserror::Error;

/// Errors from converting, inspecting or importing kiln images.
#[derive(Debug, Error)]
pub enum ImageError {
    #[error(transparent)]
    Store(#[from] kiln_store::StoreError),
    #[error(transparent)]
    Oci(#[from] kiln_oci::OciError),
    #[error(transparent)]
    Registry(#[from] kiln_registry::RegistryError),
    #[error("erofs: {0}")]
    Erofs(#[from] kiln_erofs::Error),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid {what}: {source}")]
    Json {
        what: &'static str,
        source: serde_json::Error,
    },
    #[error("layer {layer}: uncompressed content is {actual}, but the image config says {expected}")]
    DiffIdMismatch {
        layer: usize,
        expected: Digest,
        actual: Digest,
    },
    #[error("limit exceeded: {what} (max {max})")]
    LimitExceeded { what: &'static str, max: u64 },
    #[error("unsupported platform {0} (kiln converts linux/amd64 and linux/arm64)")]
    UnsupportedPlatform(String),
    #[error("layer {layer}: non-zero data after the tar end-of-archive marker")]
    TrailingData { layer: usize },
    #[error("{0} is not a kiln image")]
    NotAKilnImage(Digest),
    #[error("unsupported kiln image schema version {0}")]
    UnknownSchema(u32),
    #[error("{0} is not a kiln image (use `kiln convert` for OCI images)")]
    NotKilnRemote(String),
    #[error("{0} is a kiln image, not an OCI image to convert (use `kiln pull` to fetch it)")]
    KilnRemote(String),
    #[error("no image named {0:?}")]
    RefNotFound(String),
    #[error("the image is not built on {0}: {1}")]
    NotOnBase(String, String),
    #[error("rebase refused:{}", .0.iter().map(|r| format!("\n  - {r}")).collect::<String>())]
    RebaseRefused(Vec<crate::rebase::gates::Refusal>),
    #[error("{0}; re-convert the image (with --base) to restore its sources")]
    SourceMissing(String),
    #[error("invalid option: {0}")]
    BadOption(String),
    #[error("fetching the pinned {what}: {source}")]
    Artifact { what: String, source: Box<ImageError> },
}

pub type Result<T> = std::result::Result<T, ImageError>;

pub(crate) fn json<T: serde::de::DeserializeOwned>(what: &'static str, bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|source| ImageError::Json { what, source })
}
