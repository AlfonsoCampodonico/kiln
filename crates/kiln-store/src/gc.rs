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

/// The `<source digest>@<diff_id hex>@` prefixes of `cache/layers` entries that are
/// `parents [...]`: source layers whose conversion inherits from the layers below.
fn inheriting_prefixes(store: &Store) -> Result<BTreeSet<String>> {
    Ok(store
        .cache_entries(CacheKind::Layers)?
        .into_iter()
        .filter(|(_, v)| v.starts_with("parents "))
        .filter_map(|(path, _)| {
            let name = path.file_name()?.to_str()?.to_string();
            let cut = name.rfind('@')?;
            Some(name[..=cut].to_string())
        })
        .collect())
}

impl Store {
    /// What a kiln image config keeps of its source (spec §5.2, §16.4): the source
    /// manifest and config, small JSON that `kiln rebase` reads; and, when the
    /// image records a base, the tars of its source layers that inherit from the
    /// layers below, which rebase re-finalises. Other source tars are not kept.
    fn keep_sources(
        &self,
        json: &serde_json::Value,
        live: &mut BTreeSet<Digest>,
        inheriting: &mut Option<BTreeSet<String>>,
    ) -> Result<()> {
        let digest = |v: Option<&serde_json::Value>| v.and_then(|d| d.as_str()).and_then(|s| Digest::parse(s).ok());
        if json.get("erofsFormatVersion").is_none() {
            return Ok(());
        }
        let Some(src) = digest(json.pointer("/source/manifestDigest")) else {
            return Ok(());
        };
        let parse = |d: &Digest| -> Result<Option<serde_json::Value>> {
            if !self.has_blob(d) {
                return Ok(None);
            }
            match self.read_metadata(d) {
                Ok(b) => Ok(serde_json::from_slice(&b).ok()),
                Err(StoreError::TooLarge { .. }) => Ok(None),
                Err(e) => Err(e),
            }
        };
        let Some(manifest) = parse(&src)? else { return Ok(()) };
        live.insert(src);
        let Some(config_digest) = digest(manifest.pointer("/config/digest")) else {
            return Ok(());
        };
        live.insert(config_digest.clone());
        if json.get("base").is_none() {
            return Ok(());
        }
        let Some(config) = parse(&config_digest)? else {
            return Ok(());
        };
        let diff_ids = config.pointer("/rootfs/diff_ids").and_then(|v| v.as_array());
        let layers = manifest.get("layers").and_then(|v| v.as_array());
        let (Some(diff_ids), Some(layers)) = (diff_ids, layers) else {
            return Ok(());
        };
        if inheriting.is_none() {
            *inheriting = Some(inheriting_prefixes(self)?);
        }
        let prefixes = inheriting.as_ref().expect("just filled");
        for (layer, diff_id) in layers.iter().zip(diff_ids) {
            let (Some(layer), Some(diff_id)) = (digest(layer.get("digest")), digest(Some(diff_id))) else {
                continue;
            };
            if prefixes.contains(&format!("{layer}@{}@", diff_id.hex()).replace(':', "_")) {
                live.insert(layer);
            }
        }
        Ok(())
    }

    /// Blobs reachable from refs.
    pub fn live_blobs(&self) -> Result<BTreeSet<Digest>> {
        let mut live = BTreeSet::new();
        let mut inheriting = None;
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
                self.keep_sources(&json, &mut live, &mut inheriting)?;
            }
        }
        Ok(live)
    }

    /// Takes the exclusive lock, then removes unreferenced cache entries and blobs.
    pub fn gc(&self) -> Result<GcReport> {
        self.gc_with_roots(|_| Ok(Vec::new()))
    }

    /// [`Store::gc`], also keeping the blobs `roots` names (and the cache entries
    /// naming them) although no ref reaches them: kiln's pinned boot artifacts and
    /// the init layers built from them (spec §5.2). `roots` runs under the lock.
    pub fn gc_with_roots(&self, roots: impl FnOnce(&Store) -> Result<Vec<Digest>>) -> Result<GcReport> {
        let _lock = self.lock_exclusive()?;
        let mut live = self.live_blobs()?;
        live.extend(roots(self)?);
        let mut report = GcReport::default();
        for kind in CacheKind::ALL {
            for (path, value) in self.cache_entries(kind)? {
                let dead = if kind == CacheKind::Warnings {
                    // Annotates the `Layers` entry of the same name, which ran just before.
                    let layer = self
                        .root
                        .join(CacheKind::Layers.dir())
                        .join(path.file_name().unwrap_or_default());
                    !layer.exists()
                } else {
                    let target = value.strip_prefix("erofs ").unwrap_or(&value).trim();
                    match Digest::parse(target) {
                        Ok(d) => !live.contains(&d),
                        // `parents [...]` entries name no blob; keep them.
                        Err(_) => false,
                    }
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
    fn warnings_entries_live_as_long_as_their_layer_entry() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let live = s.put_bytes(b"live layer").unwrap();
        let dead = s.put_bytes(b"dead layer").unwrap();
        let manifest = serde_json::json!({"layers": [{"digest": live.to_string()}]});
        let m = s.put_bytes(&serde_json::to_vec(&manifest).unwrap()).unwrap();
        s.set_ref("app:1", &m).unwrap();
        s.cache_put(CacheKind::Layers, "keep@1", &format!("erofs {live}"))
            .unwrap();
        s.cache_put(CacheKind::Warnings, "keep@1", r#"["w1"]"#).unwrap();
        s.cache_put(CacheKind::Layers, "drop@1", &format!("erofs {dead}"))
            .unwrap();
        s.cache_put(CacheKind::Warnings, "drop@1", r#"["w2"]"#).unwrap();
        s.cache_put(CacheKind::Warnings, "orphan@1", r#"["w3"]"#).unwrap();

        let report = s.gc().unwrap();
        assert_eq!(
            s.cache_get(CacheKind::Warnings, "keep@1").unwrap().as_deref(),
            Some(r#"["w1"]"#)
        );
        assert_eq!(s.cache_get(CacheKind::Warnings, "drop@1").unwrap(), None);
        assert_eq!(s.cache_get(CacheKind::Warnings, "orphan@1").unwrap(), None);
        assert_eq!(
            report.cache_entries_removed, 3,
            "drop's layer and warnings, and the orphan"
        );
    }

    /// `kiln run` keeps a running VM's scratch disk in `scratch/<run-id>/` (kiln-run's
    /// rundir); GC never touches it.
    #[test]
    fn gc_leaves_run_scratch_directories_alone() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let disk = dir.path().join("scratch/0123456789abcdef/scratch.img");
        fs::create_dir_all(disk.parent().unwrap()).unwrap();
        fs::write(&disk, b"disk").unwrap();
        s.gc().unwrap();
        assert_eq!(fs::read(&disk).unwrap(), b"disk");
    }

    /// Pinned artifacts and the cache entries naming them survive GC (ruling 3);
    /// other cache entries of the same kind do not.
    #[test]
    fn roots_are_kept_with_their_cache_entries() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let (kernel, init, layer) = (
            s.put_bytes(b"a pinned kernel").unwrap(),
            s.put_bytes(b"a pinned kiln-init").unwrap(),
            s.put_bytes(b"its init layer").unwrap(),
        );
        let old = s.put_bytes(b"an older init layer").unwrap();
        let key = |d: &Digest| format!("{d}@1");
        s.cache_put(CacheKind::InitLayers, &key(&init), &layer.to_string())
            .unwrap();
        let old_init = Digest::of(b"an older kiln-init");
        s.cache_put(CacheKind::InitLayers, &key(&old_init), &old.to_string())
            .unwrap();
        let roots = vec![kernel.clone(), init.clone(), layer.clone()];
        let r = s.gc_with_roots(|_| Ok(roots)).unwrap();
        assert_eq!((r.blobs_removed, r.cache_entries_removed), (1, 1));
        for d in [&kernel, &init, &layer] {
            assert!(s.has_blob(d), "{d}");
        }
        assert!(!s.has_blob(&old));
        assert_eq!(
            s.cache_get_blob(CacheKind::InitLayers, &key(&init)).unwrap(),
            Some(layer)
        );
        assert_eq!(s.cache_get(CacheKind::InitLayers, &key(&old_init)).unwrap(), None);
        // Without roots, all of it goes.
        assert_eq!(s.gc().unwrap().blobs_removed, 3);
    }

    /// A kiln config keeps its source manifest and config; one that records a base
    /// also keeps the tars of its inheriting source layers, and no others.
    #[test]
    fn kiln_configs_keep_what_rebase_needs_of_their_sources() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let (plain, inheriting) = (
            s.put_bytes(b"plain tar").unwrap(),
            s.put_bytes(b"inheriting tar").unwrap(),
        );
        let (d1, d2) = (Digest::of(b"diff 1"), Digest::of(b"diff 2"));
        let src_config =
            serde_json::json!({"rootfs": {"type": "layers", "diff_ids": [d1.to_string(), d2.to_string()]}});
        let src_config = s.put_bytes(&serde_json::to_vec(&src_config).unwrap()).unwrap();
        let src = serde_json::json!({
            "config": {"digest": src_config.to_string()},
            "layers": [{"digest": plain.to_string()}, {"digest": inheriting.to_string()}],
        });
        let src = s.put_bytes(&serde_json::to_vec(&src).unwrap()).unwrap();
        s.cache_put(CacheKind::Layers, &format!("{plain}@{}@1", d1.hex()), "erofs sha256:00")
            .unwrap();
        s.cache_put(
            CacheKind::Layers,
            &format!("{inheriting}@{}@1", d2.hex()),
            "parents [\"61\"]",
        )
        .unwrap();
        let tag = |name: &str, base: bool| {
            let mut config =
                serde_json::json!({"erofsFormatVersion": 1, "source": {"manifestDigest": src.to_string()}});
            if base {
                config["base"] = serde_json::json!({"layers": 1});
            }
            let c = s.put_bytes(&serde_json::to_vec(&config).unwrap()).unwrap();
            let m = s
                .put_bytes(
                    &serde_json::to_vec(&serde_json::json!({"config": {"digest": c.to_string()}, "layers": []}))
                        .unwrap(),
                )
                .unwrap();
            s.set_ref(name, &m).unwrap();
        };
        tag("plain", false);
        let live = s.live_blobs().unwrap();
        assert!(live.contains(&src) && live.contains(&src_config));
        assert!(!live.contains(&plain) && !live.contains(&inheriting));
        tag("based", true);
        let live = s.live_blobs().unwrap();
        assert!(live.contains(&inheriting), "rebase re-finalises it");
        assert!(!live.contains(&plain), "a cache hit needs no tar");
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
