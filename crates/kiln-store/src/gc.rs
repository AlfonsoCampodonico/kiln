//! Garbage collection (spec §5.2): mark from refs, drop dead cache entries, then blobs.

use std::collections::BTreeSet;
use std::fs;

use crate::cache::CacheKind;
use crate::error::{Result, StoreError};
use crate::{Digest, Store};

/// What a GC run removed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GcReport {
    pub blobs_removed: u64,
    pub bytes_freed: u64,
    pub cache_entries_removed: u64,
}

/// Digests a manifest or index references: `manifests[]`, `config`, `layers[]`.
pub fn references(json: &serde_json::Value) -> Vec<Digest> {
    let mut out = Vec::new();
    let mut push = |v: &serde_json::Value| {
        if let Some(d) = v
            .get("digest")
            .and_then(|d| d.as_str())
            .and_then(|s| Digest::parse(s).ok())
        {
            out.push(d);
        }
    };
    for key in ["manifests", "layers"] {
        if let Some(arr) = json.get(key).and_then(|a| a.as_array()) {
            arr.iter().for_each(&mut push);
        }
    }
    if let Some(c) = json.get("config") {
        push(c);
    }
    out
}

impl Store {
    /// Blobs reachable from refs.
    pub fn live_blobs(&self) -> Result<BTreeSet<Digest>> {
        let mut live = BTreeSet::new();
        let mut stack: Vec<Digest> = self.refs()?.into_values().collect();
        while let Some(d) = stack.pop() {
            if !live.insert(d.clone()) || !self.has_blob(&d) {
                continue;
            }
            let bytes = match self.read_metadata(&d) {
                Ok(b) => b,
                Err(StoreError::TooLarge { .. }) => continue,
                Err(e) => return Err(e),
            };
            if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                stack.extend(references(&json));
            }
        }
        Ok(live)
    }

    /// Takes the exclusive lock, then removes unreferenced cache entries and blobs.
    pub fn gc(&self) -> Result<GcReport> {
        let _lock = self.lock_exclusive()?;
        let live = self.live_blobs()?;
        let mut report = GcReport::default();
        for kind in CacheKind::ALL {
            for (path, value) in self.cache_entries(kind)? {
                let target = value.strip_prefix("erofs ").unwrap_or(&value).trim();
                let dead = match Digest::parse(target) {
                    Ok(d) => !live.contains(&d),
                    // `parents [...]` entries name no blob; keep them.
                    Err(_) => false,
                };
                if dead {
                    fs::remove_file(path)?;
                    report.cache_entries_removed += 1;
                }
            }
        }
        for d in self.list_blobs()? {
            if !live.contains(&d) {
                let path = self.blob_path(&d);
                report.bytes_freed += fs::metadata(&path)?.len();
                fs::remove_file(path)?;
                report.blobs_removed += 1;
            }
        }
        for entry in fs::read_dir(self.tmp_dir())? {
            let _ = fs::remove_file(entry?.path());
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_reachable_chain_and_removes_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let layer = s.put_bytes(b"layer").unwrap();
        let config = s.put_bytes(b"{}").unwrap();
        let manifest =
            serde_json::json!({"config": {"digest": config.to_string()}, "layers": [{"digest": layer.to_string()}]});
        let m = s.put_bytes(serde_json::to_vec(&manifest).unwrap().as_slice()).unwrap();
        let index = serde_json::json!({"manifests": [{"digest": m.to_string()}]});
        let i = s.put_bytes(serde_json::to_vec(&index).unwrap().as_slice()).unwrap();
        s.set_ref("app:1", &i).unwrap();
        let garbage = s.put_bytes(b"garbage").unwrap();
        s.cache_put(CacheKind::Layers, "k1@1", &format!("erofs {garbage}"))
            .unwrap();
        s.cache_put(CacheKind::Layers, "k2@1", &format!("erofs {layer}"))
            .unwrap();
        s.cache_put(CacheKind::Layers, "k3@1", "parents [\"61\"]").unwrap();

        let report = s.gc().unwrap();
        assert_eq!(report.blobs_removed, 1);
        assert_eq!(report.bytes_freed, 7);
        assert_eq!(report.cache_entries_removed, 1);
        for d in [&layer, &config, &m, &i] {
            assert!(s.has_blob(d));
        }
        assert!(!s.has_blob(&garbage));
        assert!(s.cache_get(CacheKind::Layers, "k1@1").unwrap().is_none());
        assert!(s.cache_get(CacheKind::Layers, "k2@1").unwrap().is_some());
        assert!(s.cache_get(CacheKind::Layers, "k3@1").unwrap().is_some());
    }

    #[test]
    fn gc_waits_for_shared_holders() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let shared = s.lock_shared().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let s2 = s.clone();
        let t = std::thread::spawn(move || {
            s2.gc().unwrap();
            tx.send(()).unwrap();
        });
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(rx.try_recv().is_err(), "gc must block while a shared lock is held");
        drop(shared);
        rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        t.join().unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn gc_fails_closed_on_unreadable_live_metadata() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();

        // Create a live manifest with layer reference
        let layer = s.put_bytes(b"layer").unwrap();
        let config = s.put_bytes(b"{}").unwrap();
        let manifest = serde_json::json!({
            "config": {"digest": config.to_string()},
            "layers": [{"digest": layer.to_string()}]
        });
        let manifest_digest = s.put_bytes(serde_json::to_vec(&manifest).unwrap().as_slice()).unwrap();
        s.set_ref("app:1", &manifest_digest).unwrap();

        // Make the manifest unreadable
        let manifest_path = s.blob_path(&manifest_digest);
        let original_perms = fs::metadata(&manifest_path).unwrap().permissions();
        fs::set_permissions(&manifest_path, fs::Permissions::from_mode(0o000)).unwrap();

        // Check if running as root by trying to read the file
        if fs::read(&manifest_path).is_ok() {
            // Running as root, skip test but restore permissions
            fs::set_permissions(&manifest_path, original_perms).unwrap();
            return;
        }

        // GC should fail because it can't read the live manifest
        let gc_result = s.gc();

        // Restore permissions for cleanup
        fs::set_permissions(&manifest_path, original_perms).unwrap();

        // Verify gc failed and nothing was deleted
        assert!(gc_result.is_err(), "gc should fail on unreadable live metadata");
        assert!(s.has_blob(&manifest_digest));
        assert!(s.has_blob(&layer));
        assert!(s.has_blob(&config));
    }
}
