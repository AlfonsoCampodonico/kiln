//! Platforms (`os/arch[/variant]`) and matching.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::{OciError, Result};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Platform {
    pub architecture: String,
    pub os: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

impl Platform {
    pub fn parse(s: &str) -> Result<Self> {
        let parts: Vec<&str> = s.split('/').collect();
        let ok = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.');
        match parts.as_slice() {
            [os, arch] if ok(os) && ok(arch) => Ok(Self {
                os: os.to_string(),
                architecture: arch.to_string(),
                variant: None,
            }),
            [os, arch, v] if ok(os) && ok(arch) && ok(v) => Ok(Self {
                os: os.to_string(),
                architecture: arch.to_string(),
                variant: Some(v.to_string()),
            }),
            _ => Err(OciError::BadPlatform(s.to_string())),
        }
    }

    /// `linux/<host arch>`: the platform a native build targets by default.
    pub fn host() -> Self {
        let arch = match std::env::consts::ARCH {
            "aarch64" => "arm64",
            "x86_64" => "amd64",
            other => other,
        };
        Self {
            os: "linux".into(),
            architecture: arch.into(),
            variant: None,
        }
    }

    /// Whether `candidate` satisfies this wanted platform. A wanted platform without a
    /// variant accepts any variant; arm64 treats a missing variant as `v8`.
    pub fn matches(&self, candidate: &Platform) -> bool {
        let norm = |p: &Platform| match (p.architecture.as_str(), p.variant.as_deref()) {
            ("arm64", None) => Some("v8".to_string()),
            (_, v) => v.map(str::to_string),
        };
        self.os == candidate.os
            && self.architecture == candidate.architecture
            && (self.variant.is_none() || norm(self) == norm(candidate))
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.os, self.architecture)?;
        if let Some(v) = &self.variant {
            write!(f, "/{v}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_display_match() {
        let p = Platform::parse("linux/arm64/v8").unwrap();
        assert_eq!(p.to_string(), "linux/arm64/v8");
        assert!(Platform::parse("linux/arm64").unwrap().matches(&p));
        assert!(
            p.matches(&Platform::parse("linux/arm64").unwrap()),
            "arm64 without variant is v8"
        );
        assert!(!Platform::parse("linux/amd64").unwrap().matches(&p));
        assert!(
            !Platform::parse("linux/arm/v7")
                .unwrap()
                .matches(&Platform::parse("linux/arm/v6").unwrap())
        );
        for bad in ["linux", "linux/", "/arm64", "linux/arm64/v8/x", "linux/ar m64"] {
            assert!(Platform::parse(bad).is_err(), "{bad}");
        }
    }
}
