//! Registry credentials from Docker's `config.json` (including `credsStore` and
//! `credHelpers`).

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;

use crate::error::{RegistryError, Result};
use crate::reference::DOCKER_HUB;

/// The key `docker login` uses for Docker Hub.
const DOCKER_HUB_KEY: &str = "https://index.docker.io/v1/";
/// Largest `config.json` or helper output kiln reads.
const MAX_CONFIG: u64 = 1 << 20;

/// Credentials for one registry. `Debug` never shows secrets.
#[derive(Clone, PartialEq, Eq)]
pub enum Credential {
    Basic {
        username: String,
        password: String,
    },
    /// An OAuth2 refresh token (`identitytoken`, or a helper's `<token>` user).
    IdentityToken(String),
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Basic { username, .. } => write!(f, "Basic({username}, <redacted>)"),
            Self::IdentityToken(_) => f.write_str("IdentityToken(<redacted>)"),
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfigFile {
    #[serde(default)]
    auths: BTreeMap<String, AuthEntry>,
    #[serde(default)]
    cred_helpers: BTreeMap<String, String>,
    #[serde(default)]
    creds_store: Option<String>,
}

#[derive(Default, Deserialize)]
struct AuthEntry {
    #[serde(default)]
    auth: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    identitytoken: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct HelperOutput {
    username: String,
    secret: String,
}

/// Where credentials come from: a Docker `config.json` (which may not exist).
#[derive(Debug, Clone, Default)]
pub struct DockerConfig {
    file: Option<PathBuf>,
    helper_path: Option<OsString>,
}

impl DockerConfig {
    /// `$DOCKER_CONFIG/config.json`, else `~/.docker/config.json`.
    pub fn from_env() -> Self {
        let dir = std::env::var_os("DOCKER_CONFIG")
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".docker")));
        Self {
            file: dir.map(|d| d.join("config.json")),
            helper_path: None,
        }
    }

    /// `<dir>/config.json`.
    pub fn from_dir(dir: &Path) -> Self {
        Self {
            file: Some(dir.join("config.json")),
            helper_path: None,
        }
    }

    /// No credentials: every request is anonymous.
    pub fn anonymous() -> Self {
        Self::default()
    }

    /// Searches this `PATH` instead of the inherited one for credential helpers.
    pub fn with_helper_path(mut self, path: impl Into<OsString>) -> Self {
        self.helper_path = Some(path.into());
        self
    }

    fn load(&self) -> Result<Option<(&Path, ConfigFile)>> {
        let Some(path) = &self.file else { return Ok(None) };
        let bad = |reason: String| RegistryError::Config {
            path: path.display().to_string(),
            reason,
        };
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(bad(e.to_string())),
        };
        if bytes.len() as u64 > MAX_CONFIG {
            return Err(bad("larger than 1 MiB".into()));
        }
        // serde_json's message names a line and column, never the content.
        let file = serde_json::from_slice(&bytes).map_err(|e| bad(e.to_string()))?;
        Ok(Some((path, file)))
    }

    /// The credentials for `registry` (normalised, e.g. `docker.io`), as Docker
    /// finds them: `credHelpers[registry]`, else `credsStore`, else `auths`.
    pub fn credential(&self, registry: &str) -> Result<Option<Credential>> {
        let Some((path, file)) = self.load()? else {
            return Ok(None);
        };
        // Docker keys Docker Hub as `https://index.docker.io/v1/` and other registries
        // by host name; helpers are asked for the same key.
        let keys: Vec<&str> = if registry == DOCKER_HUB {
            vec![DOCKER_HUB_KEY, DOCKER_HUB, "index.docker.io", "registry-1.docker.io"]
        } else {
            vec![registry]
        };
        let server = keys[0];
        if let Some(helper) = keys.iter().find_map(|k| file.cred_helpers.get(*k)) {
            return self.run_helper(helper, server);
        }
        if let Some(helper) = file.creds_store.as_deref().filter(|h| !h.is_empty()) {
            return self.run_helper(helper, server);
        }
        let entry = keys.iter().find_map(|k| file.auths.get(*k)).or_else(|| {
            file.auths
                .iter()
                .find(|(k, _)| keys.contains(&auth_key_host(k)))
                .map(|(_, v)| v)
        });
        entry.map_or(Ok(None), |e| entry_credential(path, e))
    }

    fn run_helper(&self, helper: &str, server: &str) -> Result<Option<Credential>> {
        let fail = |reason: String| RegistryError::CredentialHelper {
            helper: helper.to_string(),
            reason,
        };
        if helper.is_empty() || !helper.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)) {
            return Err(fail("invalid helper name".into()));
        }
        let mut cmd = Command::new(format!("docker-credential-{helper}"));
        if let Some(p) = &self.helper_path {
            cmd.env("PATH", p);
        }
        let mut child = cmd
            .arg("get")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| fail(e.to_string()))?;
        if let Some(mut stdin) = child.stdin.take() {
            // A helper may exit without reading its input (EPIPE): it is judged by its
            // exit status and output below, like any other helper.
            if let Err(e) = stdin.write_all(server.as_bytes())
                && e.kind() != std::io::ErrorKind::BrokenPipe
            {
                return Err(fail(e.to_string()));
            }
        }
        let out = child.wait_with_output().map_err(|e| fail(e.to_string()))?;
        if !out.status.success() {
            let msg = String::from_utf8_lossy(if out.stdout.is_empty() {
                &out.stderr
            } else {
                &out.stdout
            });
            if msg.contains("credentials not found") {
                return Ok(None);
            }
            let first = msg.lines().next().unwrap_or("").chars().take(200).collect::<String>();
            return Err(fail(format!("{} {first}", out.status)));
        }
        if out.stdout.len() as u64 > MAX_CONFIG {
            return Err(fail("output larger than 1 MiB".into()));
        }
        let o: HelperOutput = serde_json::from_slice(&out.stdout).map_err(|e| fail(format!("invalid output: {e}")))?;
        Ok(Some(if o.username == "<token>" {
            Credential::IdentityToken(o.secret)
        } else {
            Credential::Basic {
                username: o.username,
                password: o.secret,
            }
        }))
    }
}

/// The host part of an `auths` key (`https://host/v1/` → `host`), with Docker
/// Hub's aliases mapped to `docker.io`.
fn auth_key_host(key: &str) -> &str {
    let host = key
        .strip_prefix("https://")
        .or_else(|| key.strip_prefix("http://"))
        .unwrap_or(key);
    let host = host.split('/').next().unwrap_or(host);
    match host {
        "index.docker.io" | "registry-1.docker.io" => DOCKER_HUB,
        h => h,
    }
}

fn entry_credential(path: &Path, e: &AuthEntry) -> Result<Option<Credential>> {
    if let Some(t) = e.identitytoken.as_deref().filter(|t| !t.is_empty()) {
        return Ok(Some(Credential::IdentityToken(t.to_string())));
    }
    if let Some(auth) = e.auth.as_deref().filter(|a| !a.is_empty()) {
        let bad = || RegistryError::Config {
            path: path.display().to_string(),
            reason: "an \"auth\" value is not base64 of user:password".into(),
        };
        let decoded = STANDARD.decode(auth.trim()).map_err(|_| bad())?;
        let decoded = String::from_utf8(decoded).map_err(|_| bad())?;
        let (username, password) = decoded.split_once(':').ok_or_else(bad)?;
        return Ok(Some(Credential::Basic {
            username: username.to_string(),
            password: password.to_string(),
        }));
    }
    match (&e.username, &e.password) {
        (Some(u), Some(p)) if !u.is_empty() => Ok(Some(Credential::Basic {
            username: u.clone(),
            password: p.clone(),
        })),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(json: &str) -> (tempfile::TempDir, DockerConfig) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.json"), json).unwrap();
        let c = DockerConfig::from_dir(dir.path());
        (dir, c)
    }

    fn basic(u: &str, p: &str) -> Option<Credential> {
        Some(Credential::Basic {
            username: u.into(),
            password: p.into(),
        })
    }

    #[test]
    fn reads_auths_entries() {
        let auth = STANDARD.encode("alice:pa:ss");
        let (_d, c) = config(&format!(
            r#"{{"auths":{{"https://index.docker.io/v1/":{{"auth":"{auth}"}},
                "https://ghcr.io":{{"identitytoken":"refresh"}},
                "registry.example.com:5000":{{"username":"bob","password":"pw"}},
                "empty.example.com":{{}}}}}}"#
        ));
        assert_eq!(c.credential("docker.io").unwrap(), basic("alice", "pa:ss"));
        assert_eq!(
            c.credential("ghcr.io").unwrap(),
            Some(Credential::IdentityToken("refresh".into()))
        );
        assert_eq!(c.credential("registry.example.com:5000").unwrap(), basic("bob", "pw"));
        assert_eq!(c.credential("empty.example.com").unwrap(), None);
        assert_eq!(c.credential("quay.io").unwrap(), None);
    }

    #[test]
    fn docker_hub_aliases_match() {
        let auth = STANDARD.encode("u:p");
        let (_d, c) = config(&format!(r#"{{"auths":{{"index.docker.io":{{"auth":"{auth}"}}}}}}"#));
        assert_eq!(c.credential("docker.io").unwrap(), basic("u", "p"));
    }

    #[test]
    fn missing_or_absent_config_is_anonymous_and_corrupt_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            DockerConfig::from_dir(dir.path()).credential("docker.io").unwrap(),
            None
        );
        assert_eq!(DockerConfig::anonymous().credential("docker.io").unwrap(), None);
        let (_d, c) = config(r#"{"auths": {"x": {"auth": "!!!"}}}"#);
        assert!(matches!(c.credential("x"), Err(RegistryError::Config { .. })));
        let (_d, c) = config("{not json");
        assert!(matches!(c.credential("x"), Err(RegistryError::Config { .. })));
    }

    fn helper(dir: &Path, name: &str, script: &str) {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(format!("docker-credential-{name}"));
        std::fs::write(&p, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn credential_helpers_take_precedence_and_get_the_server_on_stdin() {
        let bin = tempfile::tempdir().unwrap();
        // Echoes the server it was asked about back as the username.
        helper(
            bin.path(),
            "echo",
            r#"read s; printf '{"ServerURL":"%s","Username":"%s","Secret":"s3cret"}' "$s" "$s""#,
        );
        helper(
            bin.path(),
            "token",
            r#"printf '{"Username":"<token>","Secret":"refresh"}'"#,
        );
        helper(
            bin.path(),
            "none",
            "echo 'credentials not found in native keychain'; exit 1",
        );
        helper(bin.path(), "broken", "echo 'boom' >&2; exit 3");
        let auth = STANDARD.encode("file:user");
        let (_d, c) = config(&format!(
            r#"{{"credsStore":"echo","credHelpers":{{"tok.example.com":"token","none.example.com":"none",
                "broken.example.com":"broken"}},"auths":{{"other.example.com":{{"auth":"{auth}"}}}}}}"#
        ));
        let c = c.with_helper_path(bin.path());
        assert_eq!(
            c.credential("docker.io").unwrap(),
            basic("https://index.docker.io/v1/", "s3cret")
        );
        assert_eq!(
            c.credential("other.example.com").unwrap(),
            basic("other.example.com", "s3cret"),
            "credsStore wins over auths, as in Docker"
        );
        assert_eq!(
            c.credential("tok.example.com").unwrap(),
            Some(Credential::IdentityToken("refresh".into()))
        );
        assert_eq!(c.credential("none.example.com").unwrap(), None);
        let err = c.credential("broken.example.com").unwrap_err().to_string();
        assert!(
            err.contains("docker-credential-broken") && err.contains("boom"),
            "{err}"
        );
        let (_d, missing) = config(r#"{"credsStore":"does-not-exist"}"#);
        assert!(matches!(
            missing.with_helper_path(bin.path()).credential("docker.io"),
            Err(RegistryError::CredentialHelper { .. })
        ));
        let (_d, evil) = config(r#"{"credsStore":"../../bin/sh"}"#);
        assert!(evil.credential("docker.io").is_err());
    }

    #[test]
    fn debug_hides_secrets() {
        let s = format!(
            "{:?} {:?}",
            basic("u", "hunter2").unwrap(),
            Credential::IdentityToken("tok".into())
        );
        assert!(!s.contains("hunter2") && !s.contains("tok)"), "{s}");
    }

    #[test]
    fn a_helper_that_exits_without_reading_its_input_is_judged_by_its_exit() {
        let bin = tempfile::tempdir().unwrap();
        helper(
            bin.path(),
            "none",
            "echo 'credentials not found in native keychain'; exit 1",
        );
        helper(bin.path(), "broken", "echo 'boom' >&2; exit 3");
        // Larger than any pipe buffer, and the helpers never read it: writing it fails
        // with EPIPE every time, whichever process runs first.
        let server = format!("{}.example.com", "a".repeat(1 << 20));
        let (_d, none) = config(r#"{"credsStore":"none"}"#);
        assert_eq!(none.with_helper_path(bin.path()).credential(&server).unwrap(), None);
        let (_d, broken) = config(r#"{"credsStore":"broken"}"#);
        let err = broken
            .with_helper_path(bin.path())
            .credential(&server)
            .unwrap_err()
            .to_string();
        assert!(err.contains("boom") && !err.contains("Broken pipe"), "{err}");
    }
}
