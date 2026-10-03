//! An in-process OCI registry on 127.0.0.1 for tests. Not for production use.
//!
//! Blobs live in one namespace for every repository. Tags pushed over HTTP belong
//! to their repository; the tags of a served OCI image layout (its `index.json`
//! ref names) are visible in every repository. Knobs add auth, blob redirects, a
//! corrupted digest, a lying digest header, repositories that refuse anonymous
//! tokens, and token revocation.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use kiln_store::Digest;

const REF_NAME: &str = "org.opencontainers.image.ref.name";
const MAX_BODY: usize = 256 << 20;

/// Bearer auth through the registry's own `/token` endpoint.
#[derive(Debug, Clone, Default)]
pub struct Bearer {
    /// Basic credentials the token endpoint requires (anonymous tokens if `None`).
    pub credentials: Option<(String, String)>,
    /// A refresh token the endpoint accepts with the `refresh_token` grant (POST).
    pub refresh_token: Option<String>,
}

/// Behaviour knobs.
#[derive(Debug, Clone, Default)]
pub struct Config {
    /// Require these basic credentials on `/v2/` requests.
    pub basic: Option<(String, String)>,
    /// Require bearer tokens from `/token`.
    pub bearer: Option<Bearer>,
    /// Answer blob GETs with a 307 to `<prefix><digest>`.
    pub redirect_blobs: Option<String>,
    /// Redirect only this blob (with `redirect_blobs`); every blob if `None`.
    pub redirect_only: Option<Digest>,
    /// Repositories that answer 401 to every request, even with a token, as Docker
    /// Hub does for a repository that does not exist (with bearer auth).
    pub hidden_repos: Vec<String>,
    /// Serve these bytes, with the last byte flipped, for this digest.
    pub corrupt: Option<Digest>,
    /// Send a `Docker-Content-Digest` header that does not match.
    pub wrong_digest_header: bool,
    /// Answer manifest HEADs without a `Docker-Content-Digest` header.
    pub head_without_digest: bool,
    /// The realm in bearer challenges (the registry's own `/token` if `None`).
    pub bearer_realm: Option<String>,
    /// The `Location` of started blob uploads (the registry's own if `None`).
    pub upload_location: Option<String>,
}

/// One request the registry saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Logged {
    pub method: String,
    pub path: String,
    pub authorization: Option<String>,
}

#[derive(Default)]
struct Content {
    blobs: HashMap<Digest, Vec<u8>>,
    media_types: HashMap<Digest, String>,
    /// `<tag>` for every repository, `<repo>:<tag>` for one.
    tags: HashMap<String, Digest>,
}

struct State {
    config: Config,
    addr: SocketAddr,
    content: Mutex<Content>,
    log: Mutex<Vec<Logged>>,
    /// Issued tokens and the scope each was issued for.
    tokens: Mutex<HashMap<String, String>>,
    next_id: AtomicU64,
    stop: AtomicBool,
}

/// A running test registry; it stops when dropped.
pub struct TestRegistry {
    state: Arc<State>,
}

impl TestRegistry {
    /// An empty registry.
    pub fn start(config: Config) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1");
        let state = Arc::new(State {
            config,
            addr: listener.local_addr().expect("local addr"),
            content: Mutex::default(),
            log: Mutex::default(),
            tokens: Mutex::default(),
            next_id: AtomicU64::new(1),
            stop: AtomicBool::new(false),
        });
        let s = state.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                if s.stop.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(conn) = conn else { continue };
                let s = s.clone();
                std::thread::spawn(move || {
                    let _ = handle(&s, conn);
                });
            }
        });
        Self { state }
    }

    /// A registry serving the blobs and tags of an OCI image layout.
    pub fn serve_layout(dir: &Path, config: Config) -> Self {
        let r = Self::start(config);
        r.load_layout(dir);
        r
    }

    /// Adds a layout's blobs and `index.json` tags.
    pub fn load_layout(&self, dir: &Path) {
        let mut c = self.state.content.lock().unwrap();
        for e in std::fs::read_dir(dir.join("blobs/sha256")).unwrap() {
            let e = e.unwrap();
            let bytes = std::fs::read(e.path()).unwrap();
            c.blobs.insert(Digest::of(&bytes), bytes);
        }
        let index: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("index.json")).unwrap()).unwrap();
        for m in index["manifests"].as_array().unwrap() {
            let d = Digest::parse(m["digest"].as_str().unwrap()).unwrap();
            c.media_types
                .insert(d.clone(), m["mediaType"].as_str().unwrap().to_string());
            if let Some(tag) = m["annotations"][REF_NAME].as_str() {
                c.tags.insert(tag.to_string(), d);
            }
        }
    }

    /// `127.0.0.1:<port>`, usable as a reference's registry.
    pub fn host(&self) -> String {
        self.state.addr.to_string()
    }

    /// `http://127.0.0.1:<port>`.
    pub fn url(&self) -> String {
        format!("http://{}", self.state.addr)
    }

    /// Every request so far.
    pub fn log(&self) -> Vec<Logged> {
        self.state.log.lock().unwrap().clone()
    }

    /// How many requests (any method) hit `path`.
    pub fn requests(&self, path: &str) -> usize {
        self.log().iter().filter(|l| l.path == path).count()
    }

    /// Requests with `method` whose path contains `/manifests/`.
    pub fn manifest_requests(&self, method: &str) -> usize {
        self.log()
            .iter()
            .filter(|l| l.method == method && l.path.contains("/manifests/"))
            .count()
    }

    /// Forgets every issued bearer token, as if each had expired.
    pub fn revoke_tokens(&self) {
        self.state.tokens.lock().unwrap().clear();
    }

    /// Requests whose path contains `/blobs/sha256:` (blob GETs and HEADs).
    pub fn blob_requests(&self, method: &str) -> usize {
        self.log()
            .iter()
            .filter(|l| l.method == method && l.path.contains("/blobs/sha256:"))
            .count()
    }

    pub fn has_blob(&self, d: &Digest) -> bool {
        self.state.content.lock().unwrap().blobs.contains_key(d)
    }

    /// Forgets a blob (to simulate a layer stored elsewhere).
    pub fn remove_blob(&self, d: &Digest) {
        self.state.content.lock().unwrap().blobs.remove(d);
    }

    /// Stores a manifest under its digest and `tag` (in every repository).
    pub fn put_manifest(&self, tag: &str, media_type: &str, bytes: &[u8]) -> Digest {
        let d = Digest::of(bytes);
        let mut c = self.state.content.lock().unwrap();
        c.blobs.insert(d.clone(), bytes.to_vec());
        c.media_types.insert(d.clone(), media_type.to_string());
        c.tags.insert(tag.to_string(), d.clone());
        d
    }
}

impl Drop for TestRegistry {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop so it sees the flag.
        let _ = TcpStream::connect(self.state.addr);
    }
}

struct Request {
    method: String,
    path: String,
    query: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Reply {
    fn new(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    fn header(mut self, k: &str, v: impl Into<String>) -> Self {
        self.headers.push((k.to_string(), v.into()));
        self
    }

    fn body(mut self, b: impl Into<Vec<u8>>) -> Self {
        self.body = b.into();
        self
    }

    fn error(status: u16, code: &str) -> Self {
        Self::new(status)
            .header("Content-Type", "application/json")
            .body(format!(r#"{{"errors":[{{"code":"{code}","message":"{code}"}}]}}"#))
    }
}

fn read_request(stream: &TcpStream) -> std::io::Result<Request> {
    let mut r = BufReader::new(stream);
    let mut line = String::new();
    r.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let (method, target) = (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("/"));
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut headers = HashMap::new();
    loop {
        let mut h = String::new();
        r.read_line(&mut h)?;
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut body = vec![0; len.min(MAX_BODY)];
    r.read_exact(&mut body)?;
    Ok(Request {
        method,
        path: path.to_string(),
        query: query.to_string(),
        headers,
        body,
    })
}

fn handle(s: &State, mut stream: TcpStream) -> std::io::Result<()> {
    if s.stop.load(Ordering::SeqCst) {
        return Ok(());
    }
    let req = read_request(&stream)?;
    s.log.lock().unwrap().push(Logged {
        method: req.method.clone(),
        path: req.path.clone(),
        authorization: req.headers.get("authorization").cloned(),
    });
    let reply = route(s, &req);
    let mut out = format!("HTTP/1.1 {} X\r\n", reply.status);
    for (k, v) in &reply.headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        reply.body.len()
    ));
    stream.write_all(out.as_bytes())?;
    if req.method != "HEAD" {
        stream.write_all(&reply.body)?;
    }
    stream.flush()
}

fn basic_matches(req: &Request, user: &str, pass: &str) -> bool {
    let want = format!("Basic {}", STANDARD.encode(format!("{user}:{pass}")));
    req.headers.get("authorization") == Some(&want)
}

fn route(s: &State, req: &Request) -> Reply {
    if req.path == "/token" {
        return token(s, req);
    }
    if let Some(d) = req.path.strip_prefix("/redirected/") {
        return match Digest::parse(d) {
            Ok(d) => serve(s, &d, false),
            Err(_) => Reply::error(404, "BLOB_UNKNOWN"),
        };
    }
    let Some(rest) = req.path.strip_prefix("/v2/") else {
        return Reply::error(404, "NOT_FOUND");
    };
    let repo = ["/manifests/", "/blobs/"]
        .iter()
        .find_map(|k| rest.split_once(k).map(|(r, _)| r))
        .unwrap_or("");
    if let Some(denied) = authorize(s, req, repo) {
        return denied;
    }
    if rest.is_empty() {
        return Reply::new(200).body("{}");
    }
    if let Some((repo, target)) = rest.rsplit_once("/manifests/") {
        return manifest(s, req, repo, target);
    }
    if let Some((repo, upload)) = rest.split_once("/blobs/uploads/") {
        return blob_upload(s, req, repo, upload);
    }
    if let Some((_, d)) = rest.rsplit_once("/blobs/") {
        return match Digest::parse(d) {
            Ok(d) => {
                if req.method == "GET"
                    && let Some(prefix) = &s.config.redirect_blobs
                    && s.config.redirect_only.as_ref().is_none_or(|only| *only == d)
                {
                    return Reply::new(307).header("Location", format!("{prefix}{d}"));
                }
                serve(s, &d, false)
            }
            Err(_) => Reply::error(400, "DIGEST_INVALID"),
        };
    }
    Reply::error(404, "NOT_FOUND")
}

/// Whether a token issued for `scope` may make this request: reads need `pull`,
/// everything else `pull,push`, both on the request's own repository.
fn scope_allows(scope: &str, repo: &str, method: &str) -> bool {
    if repo.is_empty() {
        return true;
    }
    let read = matches!(method, "GET" | "HEAD");
    scope == format!("repository:{repo}:pull,push") || (read && scope == format!("repository:{repo}:pull"))
}

fn authorize(s: &State, req: &Request, repo: &str) -> Option<Reply> {
    if let Some((u, p)) = &s.config.basic
        && !basic_matches(req, u, p)
    {
        return Some(Reply::error(401, "UNAUTHORIZED").header("WWW-Authenticate", r#"Basic realm="kiln-test""#));
    }
    if s.config.bearer.is_some() {
        let hidden = s.config.hidden_repos.iter().any(|r| r == repo);
        let ok = !hidden
            && req
                .headers
                .get("authorization")
                .and_then(|a| a.strip_prefix("Bearer "))
                .and_then(|t| s.tokens.lock().unwrap().get(t).cloned())
                .is_some_and(|scope| scope_allows(&scope, repo, &req.method));
        if !ok {
            let realm = s
                .config
                .bearer_realm
                .clone()
                .unwrap_or_else(|| format!("http://{}/token", s.addr));
            let challenge = format!(r#"Bearer realm="{realm}",service="kiln-test",scope="repository:{repo}:pull""#);
            return Some(Reply::error(401, "UNAUTHORIZED").header("WWW-Authenticate", challenge));
        }
    }
    None
}

fn token(s: &State, req: &Request) -> Reply {
    let Some(bearer) = &s.config.bearer else {
        return Reply::error(404, "NOT_FOUND");
    };
    let issue = |field: &str, scope: String| {
        let t = format!("tok-{}", s.next_id.fetch_add(1, Ordering::SeqCst));
        s.tokens.lock().unwrap().insert(t.clone(), scope);
        Reply::new(200)
            .header("Content-Type", "application/json")
            .body(format!(r#"{{"{field}":"{t}","expires_in":300}}"#))
    };
    match req.method.as_str() {
        "GET" => match &bearer.credentials {
            Some((u, p)) if !basic_matches(req, u, p) => Reply::error(401, "UNAUTHORIZED"),
            _ => {
                let scope = url::form_urlencoded::parse(req.query.as_bytes())
                    .find(|(k, _)| k == "scope")
                    .map(|(_, v)| v.into_owned())
                    .unwrap_or_default();
                issue("token", scope)
            }
        },
        "POST" => {
            let form: HashMap<String, String> = url::form_urlencoded::parse(&req.body).into_owned().collect();
            let ok = form.get("grant_type").map(String::as_str) == Some("refresh_token")
                && bearer.refresh_token.is_some()
                && form.get("refresh_token") == bearer.refresh_token.as_ref();
            if ok {
                issue("access_token", form.get("scope").cloned().unwrap_or_default())
            } else {
                Reply::error(401, "UNAUTHORIZED")
            }
        }
        _ => Reply::error(405, "UNSUPPORTED"),
    }
}

/// Serves a blob (or a manifest, when `manifest`).
fn serve(s: &State, d: &Digest, manifest: bool) -> Reply {
    let c = s.content.lock().unwrap();
    let Some(bytes) = c.blobs.get(d) else {
        return Reply::error(404, if manifest { "MANIFEST_UNKNOWN" } else { "BLOB_UNKNOWN" });
    };
    let mut bytes = bytes.clone();
    if s.config.corrupt.as_ref() == Some(d)
        && let Some(last) = bytes.last_mut()
    {
        *last ^= 1;
    }
    let digest = if s.config.wrong_digest_header {
        Digest::of(b"lie").to_string()
    } else {
        d.to_string()
    };
    let mut reply = Reply::new(200).header("Docker-Content-Digest", digest);
    if manifest {
        let media_type = c
            .media_types
            .get(d)
            .cloned()
            .unwrap_or_else(|| sniff_media_type(&bytes));
        reply = reply.header("Content-Type", media_type);
    } else {
        reply = reply.header("Content-Type", "application/octet-stream");
    }
    reply.body(bytes)
}

fn sniff_media_type(bytes: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .and_then(|v| v["mediaType"].as_str().map(str::to_string))
        .unwrap_or_else(|| "application/vnd.oci.image.manifest.v1+json".into())
}

fn manifest(s: &State, req: &Request, repo: &str, target: &str) -> Reply {
    match req.method.as_str() {
        "GET" | "HEAD" => {
            let d = match Digest::parse(target) {
                Ok(d) => Some(d),
                Err(_) => {
                    let c = s.content.lock().unwrap();
                    c.tags.get(&format!("{repo}:{target}")).or(c.tags.get(target)).cloned()
                }
            };
            match d {
                Some(d) => {
                    let mut reply = serve(s, &d, true);
                    if req.method == "HEAD" && s.config.head_without_digest {
                        reply
                            .headers
                            .retain(|(k, _)| !k.eq_ignore_ascii_case("docker-content-digest"));
                    }
                    reply
                }
                None => Reply::error(404, "MANIFEST_UNKNOWN"),
            }
        }
        "PUT" => {
            let d = Digest::of(&req.body);
            let mut c = s.content.lock().unwrap();
            c.blobs.insert(d.clone(), req.body.clone());
            let media_type = req.headers.get("content-type").cloned().unwrap_or_default();
            c.media_types.insert(d.clone(), media_type);
            if Digest::parse(target).is_err() {
                c.tags.insert(format!("{repo}:{target}"), d.clone());
            } else if target != d.to_string() {
                return Reply::error(400, "DIGEST_INVALID");
            }
            Reply::new(201).header("Docker-Content-Digest", d.to_string())
        }
        _ => Reply::error(405, "UNSUPPORTED"),
    }
}

fn blob_upload(s: &State, req: &Request, repo: &str, upload: &str) -> Reply {
    match (req.method.as_str(), upload) {
        ("POST", "") => {
            let id = s.next_id.fetch_add(1, Ordering::SeqCst);
            let location = s
                .config
                .upload_location
                .clone()
                .unwrap_or_else(|| format!("/v2/{repo}/blobs/uploads/{id}?_state=s{id}"));
            Reply::new(202).header("Location", location)
        }
        ("PUT", _) => {
            let digest = url::form_urlencoded::parse(req.query.as_bytes())
                .find(|(k, _)| k == "digest")
                .and_then(|(_, v)| Digest::parse(&v).ok());
            match digest {
                Some(d) if d == Digest::of(&req.body) => {
                    s.content.lock().unwrap().blobs.insert(d.clone(), req.body.clone());
                    Reply::new(201).header("Docker-Content-Digest", d.to_string())
                }
                _ => Reply::error(400, "DIGEST_INVALID"),
            }
        }
        _ => Reply::error(405, "UNSUPPORTED"),
    }
}
