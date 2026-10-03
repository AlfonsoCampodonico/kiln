use kiln_store::Digest;
use thiserror::Error;
use url::Url;

/// Errors from the registry client. URLs in them are redacted (no userinfo, query
/// or fragment) and they never contain credentials or tokens (spec §13).
#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("invalid image reference {reference:?}: {reason}")]
    BadReference { reference: String, reason: &'static str },
    #[error(transparent)]
    Store(#[from] kiln_store::StoreError),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("cannot resolve registry host {host}: {source}")]
    Resolve { host: String, source: std::io::Error },
    #[error("{method} {url}: {source}")]
    Http {
        method: String,
        url: String,
        source: reqwest::Error,
    },
    #[error("{method} {url}: HTTP {status}{detail}")]
    Status {
        method: String,
        url: String,
        status: u16,
        detail: String,
    },
    #[error("{what} not found: {url}{detail}")]
    NotFound {
        what: &'static str,
        url: String,
        detail: String,
    },
    #[error("{url}: authentication failed (HTTP {status}){detail}")]
    Unauthorized { url: String, status: u16, detail: String },
    #[error("refusing to contact {url}: {reason}")]
    Refused { url: String, reason: String },
    #[error("too many redirects (more than {max}) fetching {url}")]
    TooManyRedirects { url: String, max: usize },
    #[error("{what} is larger than the {max}-byte limit")]
    TooLarge { what: &'static str, max: u64 },
    #[error("Docker schema 1 manifests are not supported")]
    Schema1,
    #[error("unsupported manifest media type {0:?}")]
    UnsupportedManifest(String),
    #[error("{what}: digest mismatch: expected {expected}, got {actual}")]
    DigestMismatch {
        what: String,
        expected: String,
        actual: Digest,
    },
    #[error("{url}: invalid response: {reason}")]
    BadResponse { url: String, reason: String },
    #[error("docker config {path}: {reason}")]
    Config { path: String, reason: String },
    #[error("credential helper docker-credential-{helper}: {reason}")]
    CredentialHelper { helper: String, reason: String },
}

pub type Result<T> = std::result::Result<T, RegistryError>;

/// `scheme://host[:port]/path`: a URL without userinfo, query or fragment, which
/// may carry signatures or tokens (pre-signed blob URLs do).
pub fn redact(url: &Url) -> String {
    let host = url.host_str().unwrap_or("");
    match url.port() {
        Some(p) => format!("{}://{host}:{p}{}", url.scheme(), url.path()),
        None => format!("{}://{host}{}", url.scheme(), url.path()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_drops_userinfo_query_and_fragment() {
        let u = Url::parse("https://user:secret@cdn.example.com:8443/blobs/x?X-Amz-Signature=abc#frag").unwrap();
        assert_eq!(redact(&u), "https://cdn.example.com:8443/blobs/x");
    }
}
