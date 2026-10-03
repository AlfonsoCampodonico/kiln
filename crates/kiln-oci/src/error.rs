use thiserror::Error;

/// Errors from reading OCI inputs.
#[derive(Debug, Error)]
pub enum OciError {
    #[error(transparent)]
    Store(#[from] kiln_store::StoreError),
    #[error(transparent)]
    Registry(#[from] kiln_registry::RegistryError),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid {what}: {source}")]
    Json { what: String, source: serde_json::Error },
    #[error("{0} is not an OCI image layout or docker archive")]
    NotAnImage(String),
    #[error("missing {0} in the image source")]
    MissingFile(String),
    #[error("the source holds several images; choose one with --ref (available: {})", .available.join(", "))]
    AmbiguousRef { available: Vec<String> },
    #[error("no image named {wanted:?} in the source (available: {})", .available.join(", "))]
    RefNotFound { wanted: String, available: Vec<String> },
    #[error("no image for platform {wanted} (available: {})", .available.join(", "))]
    MissingPlatform { wanted: String, available: Vec<String> },
    #[error("unsupported media type {0:?}")]
    UnsupportedMediaType(String),
    #[error("foreign or non-distributable layer {0:?} is not supported")]
    ForeignLayer(String),
    #[error("image config lists {diff_ids} diff_ids for {layers} layers")]
    DiffIdCount { layers: usize, diff_ids: usize },
    #[error("invalid platform {0:?} (expected os/arch[/variant])")]
    BadPlatform(String),
    #[error("{path} is larger than the {max}-byte metadata limit")]
    MetadataTooLarge { path: String, max: u64 },
    #[error("malformed archive: {0}")]
    BadArchive(String),
}

pub type Result<T> = std::result::Result<T, OciError>;

pub(crate) fn json<T: serde::de::DeserializeOwned>(what: &str, bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|source| OciError::Json {
        what: what.to_string(),
        source,
    })
}
