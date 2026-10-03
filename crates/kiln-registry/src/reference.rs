//! Docker-compatible image references, `[domain/]path[:tag][@digest]`, normalised
//! as Docker normalises them (spec §5.1 `source.reference`).

use std::fmt;

use kiln_store::Digest;

use crate::error::{RegistryError, Result};

/// The registry an unqualified reference names.
pub const DOCKER_HUB: &str = "docker.io";
/// The host that serves Docker Hub's distribution API.
const DOCKER_HUB_API: &str = "registry-1.docker.io";
/// Docker Hub's legacy name, normalised to [`DOCKER_HUB`].
const DOCKER_HUB_LEGACY: &str = "index.docker.io";
/// Longest `domain/repository`, as in the distribution reference grammar.
const MAX_NAME: usize = 255;
/// Longest tag (`[\w][\w.-]{0,127}`).
const MAX_TAG: usize = 128;

/// A parsed, normalised image reference.
///
/// A reference without a tag or digest gets the tag `latest`. A reference with both
/// keeps both; the digest is what gets fetched.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Reference {
    registry: String,
    repository: String,
    tag: Option<String>,
    digest: Option<Digest>,
}

impl Reference {
    pub fn parse(s: &str) -> Result<Self> {
        let bad = |reason: &'static str| RegistryError::BadReference {
            reference: s.to_string(),
            reason,
        };
        if s.is_empty() {
            return Err(bad("empty reference"));
        }
        if s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(bad("a 64-character hex string is an image ID, not a reference"));
        }
        let (rest, digest) = match s.split_once('@') {
            Some((rest, d)) => (
                rest,
                Some(Digest::parse(d).map_err(|_| bad("digest must be sha256:<64 lowercase hex>"))?),
            ),
            None => (s, None),
        };
        // A tag follows the last `:` that comes after the last `/` (a `:` before it
        // belongs to the domain's port).
        let after_slash = rest.rfind('/').map_or(0, |i| i + 1);
        let (name, tag) = match rest[after_slash..].rfind(':') {
            Some(i) => (&rest[..after_slash + i], Some(&rest[after_slash + i + 1..])),
            None => (rest, None),
        };
        if let Some(t) = tag
            && !valid_tag(t)
        {
            return Err(bad("tag must match [A-Za-z0-9_][A-Za-z0-9_.-]{0,127}"));
        }
        let (domain, path) = match name.split_once('/') {
            Some((first, path)) if first.contains(['.', ':']) || first == "localhost" => (first, path),
            _ => (DOCKER_HUB, name),
        };
        if !valid_domain(domain) {
            return Err(bad("invalid registry host"));
        }
        let registry = if domain == DOCKER_HUB_LEGACY {
            DOCKER_HUB
        } else {
            domain
        };
        let repository = if registry == DOCKER_HUB && !path.contains('/') {
            format!("library/{path}")
        } else {
            path.to_string()
        };
        if !valid_repository(&repository) {
            return Err(bad(
                "repository must be lowercase path components of [a-z0-9] joined by '.', '_', '__' or '-'",
            ));
        }
        if registry.len() + 1 + repository.len() > MAX_NAME {
            return Err(bad("name is longer than 255 characters"));
        }
        let tag = match (tag, &digest) {
            (Some(t), _) => Some(t.to_string()),
            (None, None) => Some("latest".to_string()),
            (None, Some(_)) => None,
        };
        Ok(Self {
            registry: registry.to_string(),
            repository,
            tag,
            digest,
        })
    }

    /// The registry, normalised: `docker.io`, `ghcr.io`, `localhost:5000`.
    pub fn registry(&self) -> &str {
        &self.registry
    }

    /// The repository inside the registry: `library/php`.
    pub fn repository(&self) -> &str {
        &self.repository
    }

    pub fn tag(&self) -> Option<&str> {
        self.tag.as_deref()
    }

    pub fn digest(&self) -> Option<&Digest> {
        self.digest.as_ref()
    }

    /// What to fetch: the digest when there is one, else the tag.
    pub fn target(&self) -> String {
        match (&self.digest, &self.tag) {
            (Some(d), _) => d.to_string(),
            (None, Some(t)) => t.clone(),
            (None, None) => unreachable!("a reference has a tag or a digest"),
        }
    }
}

impl fmt::Display for Reference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.registry, self.repository)?;
        if let Some(t) = &self.tag {
            write!(f, ":{t}")?;
        }
        if let Some(d) = &self.digest {
            write!(f, "@{d}")?;
        }
        Ok(())
    }
}

/// The host that serves a registry's API (`docker.io` is served elsewhere).
pub fn api_host(registry: &str) -> &str {
    if registry == DOCKER_HUB {
        DOCKER_HUB_API
    } else {
        registry
    }
}

fn valid_tag(t: &str) -> bool {
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    !t.is_empty() && t.len() <= MAX_TAG && word(t.as_bytes()[0]) && t.bytes().all(|b| word(b) || b == b'.' || b == b'-')
}

/// `host[:port]`, where `host` is dot-separated labels or a bracketed IPv6 address.
pub(crate) fn valid_domain(d: &str) -> bool {
    let (host, port) = if let Some(rest) = d.strip_prefix('[') {
        let Some((ip, after)) = rest.split_once(']') else {
            return false;
        };
        if ip.parse::<std::net::Ipv6Addr>().is_err() {
            return false;
        }
        match after {
            "" => return true,
            a => match a.strip_prefix(':') {
                Some(port) => ("", Some(port)),
                None => return false,
            },
        }
    } else {
        match d.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (d, None),
        }
    };
    let label = |l: &str| {
        let b = l.as_bytes();
        !b.is_empty()
            && b[0].is_ascii_alphanumeric()
            && b[b.len() - 1].is_ascii_alphanumeric()
            && b.iter().all(|&c| c.is_ascii_alphanumeric() || c == b'-')
    };
    let host_ok = d.starts_with('[') || host.split('.').all(label);
    let port_ok = port.is_none_or(|p| !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit()));
    host_ok && port_ok
}

/// Path components of `[a-z0-9]+` joined by the separators `.`, `_`, `__` or `-+`.
pub(crate) fn valid_repository(r: &str) -> bool {
    !r.is_empty() && r.split('/').all(valid_component)
}

fn valid_component(c: &str) -> bool {
    let alnum = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    let b = c.as_bytes();
    if b.is_empty() || !alnum(b[0]) || !alnum(b[b.len() - 1]) {
        return false;
    }
    // Every run of separator characters must be `.`, `_`, `__` or one or more `-`.
    b.split(|&x| alnum(x))
        .all(|sep| sep.is_empty() || sep == b"." || sep == b"_" || sep == b"__" || sep.iter().all(|&x| x == b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: &str = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn norm(s: &str) -> String {
        Reference::parse(s).unwrap().to_string()
    }

    #[test]
    fn normalises_like_docker() {
        for (input, want) in [
            ("php", "docker.io/library/php:latest"),
            ("php:8.4-cli", "docker.io/library/php:8.4-cli"),
            ("library/php:8.4-cli", "docker.io/library/php:8.4-cli"),
            ("docker.io/php:8.4-cli", "docker.io/library/php:8.4-cli"),
            ("index.docker.io/library/php", "docker.io/library/php:latest"),
            ("bitnami/redis:7", "docker.io/bitnami/redis:7"),
            ("ghcr.io/org/app:v1", "ghcr.io/org/app:v1"),
            ("ghcr.io/app", "ghcr.io/app:latest"),
            ("localhost/app", "localhost/app:latest"),
            ("localhost:5000/a/b/c:t", "localhost:5000/a/b/c:t"),
            ("127.0.0.1:5000/app:1.0", "127.0.0.1:5000/app:1.0"),
            ("[::1]:5000/app", "[::1]:5000/app:latest"),
            ("Registry.Example.COM/app", "Registry.Example.COM/app:latest"),
            ("org/a.b_c__d-----e", "docker.io/org/a.b_c__d-----e:latest"),
            ("localhost:5000", "docker.io/library/localhost:5000"),
        ] {
            assert_eq!(norm(input), want, "{input}");
        }
    }

    #[test]
    fn digests_win_for_fetching_and_both_are_kept() {
        let r = Reference::parse(&format!("php:8.4-cli@{D}")).unwrap();
        assert_eq!(r.to_string(), format!("docker.io/library/php:8.4-cli@{D}"));
        assert_eq!(r.target(), D);
        let r = Reference::parse(&format!("localhost:5000/app@{D}")).unwrap();
        assert_eq!(r.tag(), None, "no implicit latest beside a digest");
        assert_eq!(r.to_string(), format!("localhost:5000/app@{D}"));
        assert_eq!(Reference::parse("app:v2").unwrap().target(), "v2");
    }

    #[test]
    fn rejects_invalid_references() {
        for bad in [
            "",
            "PHP",
            "php:",
            "php:-x",
            "php:a+b",
            &format!("php:{}", "t".repeat(129)),
            "php@sha256:abc",
            "php@sha512:00",
            "a//b",
            "/a",
            "a/",
            "a..b",
            "a___b",
            "-a",
            "a-",
            "a.-b",
            "exa mple.com/app",
            "example.com:/app",
            "example.com:123456/app",
            "-example.com/app",
            "[::1/app",
            "[zz]:5000/app",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            &format!("example.com/{}", "a".repeat(250)),
        ] {
            assert!(Reference::parse(bad).is_err(), "{bad:?}");
        }
        assert!(Reference::parse(&format!("example.com/{}", "a".repeat(243))).is_ok());
    }

    #[test]
    fn api_host_maps_docker_hub() {
        assert_eq!(api_host("docker.io"), "registry-1.docker.io");
        assert_eq!(api_host("ghcr.io"), "ghcr.io");
        assert_eq!(api_host("localhost:5000"), "localhost:5000");
    }
}
