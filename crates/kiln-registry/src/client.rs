//! The blocking distribution client (spec §4.2 `kiln-registry`, §6.1, T1, T2).

use std::collections::HashMap;
use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::net::IpAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kiln_store::{Digest, Store};
use reqwest::blocking::{Body, Response};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, HeaderMap, LOCATION, WWW_AUTHENTICATE};
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use url::{Host, Url};

use crate::auth::{Challenge, Credential, DockerConfig, pick_challenge};
use crate::error::{RegistryError, Result, redact, redact_str};
use crate::policy::{CheckedResolver, Policy, Refusal, is_loopback_host, lookup};
use crate::reference::{api_host, valid_domain, valid_repository, valid_tag};

pub const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
pub const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
pub const DOCKER_MANIFEST_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
pub const DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";
const DOCKER_SCHEMA1: [&str; 2] = [
    "application/vnd.docker.distribution.manifest.v1+json",
    "application/vnd.docker.distribution.manifest.v1+prettyjws",
];
/// The manifest types kiln accepts, in the `Accept` header's order.
pub const MANIFEST_TYPES: [&str; 4] = [OCI_INDEX, OCI_MANIFEST, DOCKER_MANIFEST_LIST, DOCKER_MANIFEST];

/// Largest manifest or index the client reads.
pub const MAX_MANIFEST: u64 = 4 << 20;
/// Largest token endpoint response the client reads.
pub const MAX_TOKEN_RESPONSE: u64 = 1 << 20;
/// Redirects followed per request.
pub const MAX_REDIRECTS: usize = 5;
/// Largest error body read for its message.
const MAX_ERROR_BODY: u64 = 64 << 10;
const DIGEST_HEADER: &str = "docker-content-digest";
/// Time reqwest's blocking client allows per request, and the floor for uploads.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// The slowest upload kiln waits for, in bytes per second.
const MIN_UPLOAD_RATE: u64 = 128 << 10;

/// How long an upload of `size` bytes may take. reqwest's blocking `.timeout` covers
/// sending the body *and* waiting for the response as a whole, so a flat 60 s would
/// fail any large layer: the allowance grows with the size instead.
fn upload_timeout(size: u64) -> Duration {
    REQUEST_TIMEOUT + Duration::from_secs(size / MIN_UPLOAD_RATE)
}

/// A manifest or index, verified against its digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub bytes: Vec<u8>,
    pub digest: Digest,
    pub media_type: String,
}

/// What a manifest HEAD reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestHead {
    pub digest: Option<Digest>,
    pub size: Option<u64>,
    pub media_type: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Action {
    Pull,
    Push,
}

enum Payload<'a> {
    Empty,
    Bytes(&'a [u8], &'a str),
    File(&'a Path),
}

struct Call<'a> {
    method: Method,
    url: Url,
    repo: &'a str,
    action: Action,
    accept: Option<&'a str>,
    payload: Payload<'a>,
    /// Overrides the client's default timeout (uploads).
    timeout: Option<Duration>,
}

impl<'a> Call<'a> {
    fn new(method: Method, url: Url, repo: &'a str, action: Action) -> Self {
        Self {
            method,
            url,
            repo,
            action,
            accept: None,
            payload: Payload::Empty,
            timeout: None,
        }
    }
}

/// A client for one registry. Credentials are looked up on the first challenge.
pub struct Client {
    http: reqwest::blocking::Client,
    registry: String,
    base: Url,
    policy: Arc<Policy>,
    config: DockerConfig,
    credential: Mutex<Option<Option<Credential>>>,
    /// Bearer tokens by scope.
    tokens: Mutex<HashMap<String, String>>,
    /// The registry asked for basic auth.
    basic: AtomicBool,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("registry", &self.registry)
            .finish_non_exhaustive()
    }
}

impl Client {
    /// A client for `registry` (normalised, as [`crate::Reference::registry`] gives it).
    /// It speaks https, or plain http when the registry host is loopback.
    pub fn new(registry: &str, config: DockerConfig) -> Result<Self> {
        if !valid_domain(registry) {
            return Err(RegistryError::BadReference {
                reference: registry.to_string(),
                reason: "invalid registry host",
            });
        }
        let host = api_host(registry);
        let probe = Url::parse(&format!("http://{host}/")).map_err(|_| RegistryError::BadReference {
            reference: registry.to_string(),
            reason: "invalid registry host",
        })?;
        let plain_http = is_loopback_host(&probe);
        let scheme = if plain_http { "http" } else { "https" };
        let base = Url::parse(&format!("{scheme}://{host}/")).expect("validated host");
        let addrs = match base.host() {
            Some(Host::Ipv4(ip)) => vec![IpAddr::V4(ip)],
            Some(Host::Ipv6(ip)) => vec![IpAddr::V6(ip)],
            Some(Host::Domain(d)) => lookup(d).map_err(|source| RegistryError::Resolve {
                host: d.to_string(),
                source,
            })?,
            None => unreachable!("http URLs have hosts"),
        };
        let policy = Arc::new(Policy::new(&addrs, plain_http));
        let http = reqwest::blocking::Client::builder()
            .user_agent(concat!("kiln/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::none())
            // A proxy would resolve names itself, out of the destination checks' reach.
            .no_proxy()
            .dns_resolver(Arc::new(CheckedResolver { policy: policy.clone() }))
            .connect_timeout(Duration::from_secs(30))
            // Per read and per response: large blobs take as long as they take.
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| RegistryError::Http {
                method: "build".into(),
                url: base.to_string(),
                source: e.without_url(),
            })?;
        Ok(Self {
            http,
            registry: registry.to_string(),
            base,
            policy,
            config,
            credential: Mutex::new(None),
            tokens: Mutex::new(HashMap::new()),
            basic: AtomicBool::new(false),
        })
    }

    /// The registry this client talks to.
    pub fn registry(&self) -> &str {
        &self.registry
    }

    fn endpoint(&self, repo: &str, kind: &str, target: &str) -> Result<Url> {
        if !valid_repository(repo) {
            return Err(RegistryError::BadReference {
                reference: repo.to_string(),
                reason: "invalid repository",
            });
        }
        if !(valid_tag(target) || Digest::parse(target).is_ok()) {
            return Err(RegistryError::BadReference {
                reference: target.to_string(),
                reason: "invalid tag or digest",
            });
        }
        Ok(self
            .base
            .join(&format!("v2/{repo}/{kind}/{target}"))
            .expect("valid path"))
    }

    /// GETs a manifest or index by tag or digest. Its bytes are hashed; a digest
    /// target and any `Docker-Content-Digest` header must match them.
    pub fn get_manifest(&self, repo: &str, target: &str) -> Result<Manifest> {
        let url = self.endpoint(repo, "manifests", target)?;
        let accept = MANIFEST_TYPES.join(", ");
        let mut call = Call::new(Method::GET, url, repo, Action::Pull);
        call.accept = Some(&accept);
        let resp = self.success(self.execute(&call)?, &call, "manifest")?;
        let header_type = media_type(resp.headers());
        let header_digest = header_str(resp.headers(), DIGEST_HEADER);
        let where_ = redact(&call.url);
        let bytes = read_capped(resp, MAX_MANIFEST, "manifest")?;
        let digest = Digest::of(&bytes);
        let mismatch = |expected: &str| RegistryError::DigestMismatch {
            what: format!("manifest {where_}"),
            expected: expected.to_string(),
            actual: digest.clone(),
        };
        if let Ok(want) = Digest::parse(target)
            && want != digest
        {
            return Err(mismatch(target));
        }
        if let Some(h) = header_digest
            && h != digest.to_string()
        {
            return Err(mismatch(&h));
        }
        let media_type = manifest_media_type(&where_, header_type.as_deref(), &bytes)?;
        Ok(Manifest {
            bytes,
            digest,
            media_type,
        })
    }

    /// HEADs a manifest; `None` if the registry does not have it.
    pub fn head_manifest(&self, repo: &str, target: &str) -> Result<Option<ManifestHead>> {
        let url = self.endpoint(repo, "manifests", target)?;
        let accept = MANIFEST_TYPES.join(", ");
        let mut call = Call::new(Method::HEAD, url, repo, Action::Pull);
        call.accept = Some(&accept);
        let resp = self.execute(&call)?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let resp = self.success(resp, &call, "manifest")?;
        let h = resp.headers();
        let digest = match header_str(h, DIGEST_HEADER) {
            Some(d) => Some(Digest::parse(&d).map_err(|_| RegistryError::BadResponse {
                url: redact(&call.url),
                reason: "invalid Docker-Content-Digest header".into(),
            })?),
            None => None,
        };
        Ok(Some(ManifestHead {
            digest,
            size: header_str(h, CONTENT_LENGTH.as_str()).and_then(|s| s.parse().ok()),
            media_type: media_type(h),
        }))
    }

    /// Streams a blob into `store`, which commits it only if it has `size` bytes
    /// and hashes to `digest` (T1). Returns whether it was downloaded: a blob
    /// already in the store is not fetched again. Descriptor `urls` are never used.
    pub fn fetch_blob(&self, repo: &str, digest: &Digest, size: u64, store: &Store) -> Result<bool> {
        if store.has_blob(digest) {
            store.put_verified(&mut io::empty(), digest, Some(size))?;
            return Ok(false);
        }
        let call = Call::new(
            Method::GET,
            self.endpoint(repo, "blobs", &digest.to_string())?,
            repo,
            Action::Pull,
        );
        let resp = self.success(self.execute(&call)?, &call, "blob")?;
        store.put_verified(&mut RedactedBody(resp), digest, Some(size))?;
        Ok(true)
    }

    /// Whether the registry has a blob (HEAD).
    pub fn has_blob(&self, repo: &str, digest: &Digest) -> Result<bool> {
        self.blob_exists(repo, digest, Action::Pull)
    }

    fn blob_exists(&self, repo: &str, digest: &Digest, action: Action) -> Result<bool> {
        let call = Call::new(
            Method::HEAD,
            self.endpoint(repo, "blobs", &digest.to_string())?,
            repo,
            action,
        );
        let resp = self.execute(&call)?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(false);
        }
        self.success(resp, &call, "blob")?;
        Ok(true)
    }

    /// Uploads the file at `path` as blob `digest` in one request (POST, then PUT
    /// to the upload location), unless the registry already has it. Returns
    /// whether it was uploaded.
    pub fn push_blob(&self, repo: &str, digest: &Digest, path: &Path) -> Result<bool> {
        if self.blob_exists(repo, digest, Action::Push)? {
            return Ok(false);
        }
        let start = self.endpoint(repo, "blobs", "uploads")?;
        let start = Url::parse(&format!("{start}/")).expect("valid url");
        let call = Call::new(Method::POST, start, repo, Action::Push);
        let resp = self.success(self.execute(&call)?, &call, "upload")?;
        let mut location = location(&resp, &call.url)?;
        self.policy.check_url(&location)?;
        location.query_pairs_mut().append_pair("digest", &digest.to_string());
        let mut put = Call::new(Method::PUT, location, repo, Action::Push);
        put.payload = Payload::File(path);
        put.timeout = Some(upload_timeout(std::fs::metadata(path)?.len()));
        let resp = self.success(self.execute(&put)?, &put, "upload")?;
        check_digest_header(&resp, digest, &put.url)?;
        Ok(true)
    }

    /// PUTs a manifest or index under a tag or its digest; returns its digest.
    pub fn put_manifest(&self, repo: &str, target: &str, media_type: &str, bytes: &[u8]) -> Result<Digest> {
        let digest = Digest::of(bytes);
        let mut call = Call::new(
            Method::PUT,
            self.endpoint(repo, "manifests", target)?,
            repo,
            Action::Push,
        );
        call.payload = Payload::Bytes(bytes, media_type);
        let resp = self.success(self.execute(&call)?, &call, "manifest")?;
        check_digest_header(&resp, &digest, &call.url)?;
        Ok(digest)
    }

    /// Sends a call, answering one auth challenge and following redirects.
    fn execute(&self, call: &Call) -> Result<Response> {
        let mut answered = false;
        loop {
            let resp = self.send(call, &call.url)?;
            if resp.status() == StatusCode::UNAUTHORIZED && !answered {
                self.answer(&resp, call)?;
                answered = true;
                continue;
            }
            let follow = matches!(call.method, Method::GET | Method::HEAD) && resp.status().is_redirection();
            return if follow { self.follow(call, resp) } else { Ok(resp) };
        }
    }

    /// Follows up to [`MAX_REDIRECTS`] redirects, checking each hop (T2).
    fn follow(&self, call: &Call, mut resp: Response) -> Result<Response> {
        let mut url = call.url.clone();
        for _ in 0..MAX_REDIRECTS {
            url = location(&resp, &url)?;
            self.policy.check_url(&url)?;
            resp = self.send(call, &url)?;
            if !resp.status().is_redirection() {
                return Ok(resp);
            }
        }
        Err(RegistryError::TooManyRedirects {
            url: redact(&call.url),
            max: MAX_REDIRECTS,
        })
    }

    /// One request. Credentials go only to the registry's own origin, never to a
    /// redirect target elsewhere.
    fn send(&self, call: &Call, url: &Url) -> Result<Response> {
        let mut rb = self.http.request(call.method.clone(), url.clone());
        if let Some(a) = call.accept {
            rb = rb.header(ACCEPT, a);
        }
        if url.origin() == self.base.origin()
            && let Some(auth) = self.authorization(call)?
        {
            rb = rb.header(AUTHORIZATION, auth);
        }
        rb = match &call.payload {
            Payload::Empty if call.method == Method::POST || call.method == Method::PUT => rb.header(CONTENT_LENGTH, 0),
            Payload::Empty => rb,
            Payload::Bytes(b, media_type) => rb.header(CONTENT_TYPE, *media_type).body(b.to_vec()),
            Payload::File(path) => rb
                .header(CONTENT_TYPE, "application/octet-stream")
                .body(Body::from(File::open(path)?)),
        };
        if let Some(t) = call.timeout {
            rb = rb.timeout(t);
        }
        rb.send().map_err(|e| http_error(&call.method, url, e))
    }

    fn scope(call: &Call) -> String {
        match call.action {
            Action::Pull => format!("repository:{}:pull", call.repo),
            Action::Push => format!("repository:{}:pull,push", call.repo),
        }
    }

    fn authorization(&self, call: &Call) -> Result<Option<String>> {
        if let Some(t) = self.tokens.lock().expect("no poisoning").get(&Self::scope(call)) {
            return Ok(Some(format!("Bearer {t}")));
        }
        if self.basic.load(Ordering::SeqCst)
            && let Some(Credential::Basic { username, password }) = self.credential()?
        {
            return Ok(Some(basic_header(&username, &password)));
        }
        Ok(None)
    }

    fn credential(&self) -> Result<Option<Credential>> {
        let mut slot = self.credential.lock().expect("no poisoning");
        if slot.is_none() {
            *slot = Some(self.config.credential(&self.registry)?);
        }
        Ok(slot.clone().flatten())
    }

    /// Answers a 401: basic credentials, or a bearer token from the challenge's realm.
    fn answer(&self, resp: &Response, call: &Call) -> Result<()> {
        let values = resp.headers().get_all(WWW_AUTHENTICATE);
        let challenge = pick_challenge(values.iter().filter_map(|v| v.to_str().ok()));
        let unauthorized = |detail: &str| RegistryError::Unauthorized {
            url: redact(&call.url),
            status: 401,
            detail: detail.to_string(),
        };
        match challenge {
            Some(Challenge::Basic) => match self.credential()? {
                Some(Credential::Basic { .. }) => {
                    self.basic.store(true, Ordering::SeqCst);
                    Ok(())
                }
                _ => Err(unauthorized(&format!(": no credentials for {}", self.registry))),
            },
            Some(Challenge::Bearer { realm, service }) => {
                let scope = Self::scope(call);
                let token = self.fetch_token(&realm, service.as_deref(), &scope)?;
                self.tokens.lock().expect("no poisoning").insert(scope, token);
                Ok(())
            }
            None => Err(unauthorized(": unsupported WWW-Authenticate challenge")),
        }
    }

    /// Gets a bearer token: GET with basic credentials (or anonymously), or a POST
    /// with the refresh-token grant for an identity token.
    fn fetch_token(&self, realm: &str, service: Option<&str>, scope: &str) -> Result<String> {
        let mut url = Url::parse(realm).map_err(|_| RegistryError::BadResponse {
            url: redact(&self.base),
            reason: format!("invalid token realm {}", redact_str(realm)),
        })?;
        self.policy.check_url(&url)?;
        let mut params = vec![("scope", scope)];
        if let Some(s) = service {
            params.push(("service", s));
        }
        let credential = self.credential()?;
        let rb = match &credential {
            Some(Credential::IdentityToken(refresh)) => {
                let mut form = url::form_urlencoded::Serializer::new(String::new());
                form.extend_pairs(&params)
                    .append_pair("grant_type", "refresh_token")
                    .append_pair("refresh_token", refresh)
                    .append_pair("client_id", "kiln");
                self.http
                    .post(url.clone())
                    .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(form.finish())
            }
            other => {
                url.query_pairs_mut().extend_pairs(&params);
                let rb = self.http.get(url.clone());
                match other {
                    Some(Credential::Basic { username, password }) => {
                        rb.header(AUTHORIZATION, basic_header(username, password))
                    }
                    _ => rb,
                }
            }
        };
        let method = if matches!(credential, Some(Credential::IdentityToken(_))) {
            Method::POST
        } else {
            Method::GET
        };
        let resp = rb.send().map_err(|e| http_error(&method, &url, e))?;
        let status = resp.status();
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(RegistryError::Unauthorized {
                url: redact(&url),
                status: status.as_u16(),
                detail: error_detail(resp),
            });
        }
        if !status.is_success() {
            return Err(RegistryError::Status {
                method: method.to_string(),
                url: redact(&url),
                status: status.as_u16(),
                detail: error_detail(resp),
            });
        }
        #[derive(Deserialize)]
        struct TokenResponse {
            #[serde(default)]
            token: Option<String>,
            #[serde(default)]
            access_token: Option<String>,
        }
        let bytes = read_capped(resp, MAX_TOKEN_RESPONSE, "token response")?;
        let bad = |reason: &str| RegistryError::BadResponse {
            url: redact(&url),
            reason: reason.to_string(),
        };
        let parsed: TokenResponse = serde_json::from_slice(&bytes).map_err(|_| bad("token response is not JSON"))?;
        let token = parsed
            .token
            .filter(|t| !t.is_empty())
            .or(parsed.access_token.filter(|t| !t.is_empty()))
            .ok_or_else(|| bad("token response has no token"))?;
        // The token goes into a header: refuse anything that is not a header value.
        if !token.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(bad("token is not printable ASCII"));
        }
        Ok(token)
    }

    /// Maps a non-success status to an error.
    fn success(&self, resp: Response, call: &Call, what: &'static str) -> Result<Response> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let url = redact(resp.url());
        Err(match status {
            StatusCode::NOT_FOUND => RegistryError::NotFound {
                what,
                url,
                detail: error_detail(resp),
            },
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => RegistryError::Unauthorized {
                url,
                status: status.as_u16(),
                detail: error_detail(resp),
            },
            _ => RegistryError::Status {
                method: call.method.to_string(),
                url,
                status: status.as_u16(),
                detail: error_detail(resp),
            },
        })
    }
}

fn basic_header(username: &str, password: &str) -> String {
    use base64::Engine as _;
    let raw = format!("{username}:{password}");
    format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(raw))
}

/// A reqwest error without its URL, or the policy's refusal when the resolver
/// refused the host.
fn http_error(method: &Method, url: &Url, e: reqwest::Error) -> RegistryError {
    let mut source: Option<&dyn std::error::Error> = Some(&e);
    while let Some(s) = source {
        if let Some(r) = s.downcast_ref::<Refusal>() {
            return RegistryError::Refused {
                url: redact(url),
                reason: r.to_string(),
            };
        }
        source = s.source();
    }
    RegistryError::Http {
        method: method.to_string(),
        url: redact(url),
        source: e.without_url(),
    }
}

fn header_str(h: &HeaderMap, name: &str) -> Option<String> {
    h.get(name).and_then(|v| v.to_str().ok()).map(|s| s.trim().to_string())
}

/// The media type of a `Content-Type` header, without parameters.
fn media_type(h: &HeaderMap) -> Option<String> {
    header_str(h, CONTENT_TYPE.as_str())
        .map(|s| s.split(';').next().unwrap_or("").trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
}

/// The manifest's type. The document's own `mediaType` wins, so a registry cannot
/// choose how digest-pinned bytes are read; an accepted `Content-Type` that
/// disagrees with it is refused, and the header decides only when the document has
/// no `mediaType`. A document with both `manifests` and `layers` is ambiguous and
/// refused. Schema 1 is refused either way.
fn manifest_media_type(url: &str, header: Option<&str>, bytes: &[u8]) -> Result<String> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Probe {
        #[serde(default)]
        schema_version: Option<u64>,
        #[serde(default)]
        media_type: Option<String>,
        #[serde(default)]
        manifests: Option<serde::de::IgnoredAny>,
        #[serde(default)]
        layers: Option<serde::de::IgnoredAny>,
    }
    let bad = |reason: String| RegistryError::BadResponse {
        url: url.to_string(),
        reason,
    };
    let probe: Option<Probe> = serde_json::from_slice(bytes).ok();
    let body_type = probe.as_ref().and_then(|p| p.media_type.clone());
    let schema1 = probe.as_ref().is_some_and(|p| p.schema_version == Some(1));
    if schema1
        || [header, body_type.as_deref()]
            .iter()
            .flatten()
            .any(|t| DOCKER_SCHEMA1.contains(t))
    {
        return Err(RegistryError::Schema1);
    }
    if probe
        .as_ref()
        .is_some_and(|p| p.manifests.is_some() && p.layers.is_some())
    {
        return Err(bad("document has both manifests and layers".into()));
    }
    match body_type.as_deref() {
        Some(t) if MANIFEST_TYPES.contains(&t) => match header {
            Some(h) if MANIFEST_TYPES.contains(&h) && h != t => Err(bad(format!(
                "Content-Type {h:?} disagrees with the document's mediaType {t:?}"
            ))),
            _ => Ok(t.to_string()),
        },
        Some(t) => Err(RegistryError::UnsupportedManifest(t.to_string())),
        None => match header {
            Some(h) if MANIFEST_TYPES.contains(&h) => Ok(h.to_string()),
            other => Err(RegistryError::UnsupportedManifest(other.unwrap_or("").to_string())),
        },
    }
}

fn check_digest_header(resp: &Response, digest: &Digest, url: &Url) -> Result<()> {
    match header_str(resp.headers(), DIGEST_HEADER) {
        Some(h) if h != digest.to_string() => Err(RegistryError::DigestMismatch {
            what: format!("upload to {}", redact(url)),
            expected: digest.to_string(),
            actual: Digest::parse(&h).unwrap_or_else(|_| Digest::of(h.as_bytes())),
        }),
        _ => Ok(()),
    }
}

fn location(resp: &Response, base: &Url) -> Result<Url> {
    let bad = |reason: &str| RegistryError::BadResponse {
        url: redact(base),
        reason: reason.to_string(),
    };
    let loc = header_str(resp.headers(), LOCATION.as_str()).ok_or_else(|| bad("no Location header"))?;
    base.join(&loc).map_err(|_| bad("invalid Location header"))
}

/// Reads at most `max` bytes of a body, refusing a larger one.
fn read_capped(resp: Response, max: u64, what: &'static str) -> Result<Vec<u8>> {
    if resp.content_length().is_some_and(|n| n > max) {
        return Err(RegistryError::TooLarge { what, max });
    }
    let mut buf = Vec::new();
    RedactedBody(resp).take(max + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > max {
        return Err(RegistryError::TooLarge { what, max });
    }
    Ok(buf)
}

/// The registry's error code and message, if the body has one (`: CODE: message`).
fn error_detail(resp: Response) -> String {
    #[derive(Deserialize)]
    struct Errors {
        errors: Vec<ErrorEntry>,
    }
    #[derive(Deserialize)]
    struct ErrorEntry {
        #[serde(default)]
        code: String,
        #[serde(default)]
        message: String,
    }
    let mut buf = Vec::new();
    if RedactedBody(resp).take(MAX_ERROR_BODY).read_to_end(&mut buf).is_err() {
        return String::new();
    }
    match serde_json::from_slice::<Errors>(&buf) {
        Ok(e) => e
            .errors
            .first()
            .map(|e| {
                let text = format!(": {}: {}", e.code, e.message);
                text.chars().take(300).collect()
            })
            .unwrap_or_default(),
        Err(_) => String::new(),
    }
}

/// A response body whose read errors carry no URL.
struct RedactedBody(Response);

impl Read for RedactedBody {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf).map_err(|e| {
            let kind = e.kind();
            match e.into_inner().map(|inner| inner.downcast::<reqwest::Error>()) {
                Some(Ok(re)) => io::Error::new(kind, re.without_url()),
                Some(Err(other)) => io::Error::new(kind, other),
                None => io::Error::from(kind),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_types_come_from_the_header_or_the_body() {
        assert_eq!(manifest_media_type("u", Some(OCI_INDEX), b"{}").unwrap(), OCI_INDEX);
        let body = format!(r#"{{"schemaVersion":2,"mediaType":"{DOCKER_MANIFEST}"}}"#);
        assert_eq!(
            manifest_media_type("u", Some("application/json"), body.as_bytes()).unwrap(),
            DOCKER_MANIFEST
        );
        assert!(matches!(
            manifest_media_type("u", Some(DOCKER_SCHEMA1[1]), b"{}"),
            Err(RegistryError::Schema1)
        ));
        assert!(matches!(
            manifest_media_type("u", Some(OCI_MANIFEST), br#"{"schemaVersion":1}"#),
            Err(RegistryError::Schema1)
        ));
        assert!(matches!(
            manifest_media_type("u", Some("text/html"), b"<html>"),
            Err(RegistryError::UnsupportedManifest(t)) if t == "text/html"
        ));
    }

    #[test]
    fn the_documents_media_type_wins_over_the_header() {
        let manifest = format!(r#"{{"schemaVersion":2,"mediaType":"{OCI_MANIFEST}","layers":[]}}"#);
        // No header, or one that is not an accepted type: the body decides.
        assert_eq!(
            manifest_media_type("u", None, manifest.as_bytes()).unwrap(),
            OCI_MANIFEST
        );
        assert_eq!(
            manifest_media_type("u", Some("text/plain"), manifest.as_bytes()).unwrap(),
            OCI_MANIFEST
        );
        // An accepted header that disagrees is refused, whichever way round.
        assert!(matches!(
            manifest_media_type("u", Some(OCI_INDEX), manifest.as_bytes()),
            Err(RegistryError::BadResponse { .. })
        ));
        let index = format!(r#"{{"schemaVersion":2,"mediaType":"{OCI_INDEX}","manifests":[]}}"#);
        assert!(matches!(
            manifest_media_type("u", Some(OCI_MANIFEST), index.as_bytes()),
            Err(RegistryError::BadResponse { .. })
        ));
        // The header decides only when the body has no mediaType.
        assert_eq!(
            manifest_media_type("u", Some(OCI_INDEX), br#"{"schemaVersion":2,"manifests":[]}"#).unwrap(),
            OCI_INDEX
        );
        // A body type that is not a manifest type is unsupported even under an accepted header.
        assert!(matches!(
            manifest_media_type("u", Some(OCI_MANIFEST), br#"{"mediaType":"text/html"}"#),
            Err(RegistryError::UnsupportedManifest(t)) if t == "text/html"
        ));
        // Both manifests and layers: ambiguous.
        for doc in [
            br#"{"schemaVersion":2,"manifests":[],"layers":[]}"#.as_slice(),
            format!(r#"{{"mediaType":"{OCI_MANIFEST}","manifests":[],"layers":[]}}"#).as_bytes(),
        ] {
            assert!(matches!(
                manifest_media_type("u", Some(OCI_MANIFEST), doc),
                Err(RegistryError::BadResponse { .. })
            ));
        }
    }

    #[test]
    fn upload_timeouts_grow_with_the_size() {
        assert_eq!(upload_timeout(0), Duration::from_secs(60));
        assert_eq!(upload_timeout(MIN_UPLOAD_RATE - 1), Duration::from_secs(60));
        assert_eq!(upload_timeout(MIN_UPLOAD_RATE), Duration::from_secs(61));
        // A 1 GiB layer gets 60 s plus 8192 s at 128 KiB/s.
        assert_eq!(upload_timeout(1 << 30), Duration::from_secs(60 + 8192));
    }

    #[test]
    fn basic_header_is_base64_of_user_and_password() {
        assert_eq!(basic_header("aladdin", "opensesame"), "Basic YWxhZGRpbjpvcGVuc2VzYW1l");
    }

    #[test]
    fn endpoints_validate_their_parts() {
        let c = Client::new("127.0.0.1:1", DockerConfig::anonymous()).unwrap();
        assert_eq!(
            c.endpoint("a/b", "manifests", "v1").unwrap().as_str(),
            "http://127.0.0.1:1/v2/a/b/manifests/v1"
        );
        assert!(c.endpoint("../x", "manifests", "v1").is_err());
        assert!(c.endpoint("a", "manifests", "v1/../../x").is_err());
        assert!(c.endpoint("a", "manifests", "v1?x").is_err());
        let digest = format!("sha256:{}", "a".repeat(64));
        assert!(c.endpoint("a", "manifests", &digest).is_ok());
        assert!(c.endpoint("a", "manifests", "v1.2-rc_3").is_ok());
        for bad in [
            "..",
            ".x",
            "-x",
            "",
            "a:b",
            &format!("sha512:{}", "a".repeat(128)),
            &format!("sha256:{}", "A".repeat(64)),
            &format!("sha256:{}", "a".repeat(63)),
        ] {
            assert!(c.endpoint("a", "manifests", bad).is_err(), "{bad}");
        }
        assert!(Client::new("bad host", DockerConfig::anonymous()).is_err());
    }

    #[test]
    fn public_registries_use_https_and_loopback_ones_http() {
        assert_eq!(
            Client::new("localhost:5000", DockerConfig::anonymous())
                .unwrap()
                .base
                .as_str(),
            "http://localhost:5000/"
        );
        assert_eq!(
            Client::new("[::1]:5000", DockerConfig::anonymous())
                .unwrap()
                .base
                .as_str(),
            "http://[::1]:5000/"
        );
        assert_eq!(
            Client::new("10.1.2.3:5000", DockerConfig::anonymous())
                .unwrap()
                .base
                .as_str(),
            "https://10.1.2.3:5000/"
        );
    }

    /// A name that resolves to a refused class fails inside reqwest's connector;
    /// the refusal must come back as a typed error, not a generic connect error.
    #[test]
    fn the_resolver_refuses_names_and_the_error_says_why() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut c = Client::new("127.0.0.1:1", DockerConfig::anonymous()).unwrap();
        // Pretend the registry is public: loopback destinations are then refused.
        let policy = Arc::new(Policy::new(&["93.184.216.34".parse().unwrap()], true));
        c.http = reqwest::blocking::Client::builder()
            .no_proxy()
            .dns_resolver(Arc::new(CheckedResolver { policy }))
            .build()
            .unwrap();
        let url = Url::parse(&format!("http://localhost:{port}/x?token=secret")).unwrap();
        let call = Call::new(Method::GET, url.clone(), "a", Action::Pull);
        let err = c.send(&call, &url).unwrap_err();
        assert!(matches!(err, RegistryError::Refused { .. }), "{err}");
        let msg = err.to_string();
        assert!(msg.contains("loopback") && !msg.contains("secret"), "{msg}");
        listener.set_nonblocking(true).unwrap();
        assert!(listener.accept().is_err(), "no connection was made");
    }
}
