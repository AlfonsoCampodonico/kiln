//! Conversion caches (spec §5.2): small text entries keyed by source digests.

use std::fs;
use std::path::PathBuf;

use crate::error::{Result, StoreError};
use crate::{Digest, Store, write_atomic};

/// Which cache an entry lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheKind {
    /// `<src-digest>@<diff_id hex>@<fmt>` → `erofs <digest>` or `parents <json>`.
    Layers,
    /// `<src-digest>@<diff_id hex>@<fmt>@<ctx>` → erofs digest of a layer with inherited parents.
    LayersCtx,
    /// `<sha256 of ordered erofs digests>@<fmt>` → squashed erofs digest.
    Squash,
}

impl CacheKind {
    fn dir(self) -> &'static str {
        match self {
            CacheKind::Layers => "cache/layers",
            CacheKind::LayersCtx => "cache/layers-ctx",
            CacheKind::Squash => "cache/squash",
        }
    }

    pub(crate) const ALL: [CacheKind; 3] = [CacheKind::Layers, CacheKind::LayersCtx, CacheKind::Squash];
}

fn check_key(key: &str) -> Result<()> {
    let ok =
        !key.is_empty() && key.len() <= 255 && key.bytes().all(|b| b.is_ascii_alphanumeric() || b"@:._-".contains(&b));
    if ok {
        Ok(())
    } else {
        Err(StoreError::Invalid {
            what: "cache key",
            value: key.to_string(),
        })
    }
}

impl Store {
    fn cache_path(&self, kind: CacheKind, key: &str) -> Result<PathBuf> {
        check_key(key)?;
        Ok(self.root.join(kind.dir()).join(key.replace(':', "_")))
    }

    pub fn cache_get(&self, kind: CacheKind, key: &str) -> Result<Option<String>> {
        match fs::read_to_string(self.cache_path(kind, key)?) {
            Ok(v) => Ok(Some(v)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Writes an entry atomically. Callers commit the blob it names first.
    pub fn cache_put(&self, kind: CacheKind, key: &str, value: &str) -> Result<()> {
        let path = self.cache_path(kind, key)?;
        write_atomic(&self.tmp_dir(), &path, value.as_bytes())
    }

    /// An entry whose value is a digest; a missing blob counts as a miss.
    pub fn cache_get_blob(&self, kind: CacheKind, key: &str) -> Result<Option<Digest>> {
        let Some(v) = self.cache_get(kind, key)? else {
            return Ok(None);
        };
        let d = Digest::parse(v.trim())?;
        Ok(self.has_blob(&d).then_some(d))
    }

    /// All `(file name, value)` pairs of one cache (for GC).
    pub(crate) fn cache_entries(&self, kind: CacheKind) -> Result<Vec<(PathBuf, String)>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(self.root.join(kind.dir()))? {
            let path = entry?.path();
            if let Ok(v) = fs::read_to_string(&path) {
                out.push((path, v));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get_and_blob_hits() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let src = Digest::of(b"src");
        let key = format!("{src}@1");
        assert_eq!(s.cache_get(CacheKind::Layers, &key).unwrap(), None);
        s.cache_put(CacheKind::Layers, &key, "parents []").unwrap();
        assert_eq!(
            s.cache_get(CacheKind::Layers, &key).unwrap().as_deref(),
            Some("parents []")
        );

        let out = s.put_bytes(b"erofs").unwrap();
        s.cache_put(CacheKind::Squash, &key, &out.to_string()).unwrap();
        assert_eq!(s.cache_get_blob(CacheKind::Squash, &key).unwrap(), Some(out.clone()));
        fs::remove_file(s.blob_path(&out)).unwrap();
        assert_eq!(
            s.cache_get_blob(CacheKind::Squash, &key).unwrap(),
            None,
            "missing blob is a miss"
        );
    }

    #[test]
    fn keys_cannot_escape_the_cache_dir() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        for bad in ["../refs.json", "a/b", "", "x y"] {
            assert!(s.cache_put(CacheKind::Layers, bad, "v").is_err(), "{bad}");
        }
    }
}
