//! Reading snapshots back from the store: resolve, load (with every check of
//! design §5.4), tag.

use kiln_oci::ImageManifest;
use kiln_oci::media::OCI_MANIFEST;
use kiln_registry::Reference;
use kiln_store::{Digest, Store};

use crate::error::{Result, SnapshotError, json};
use crate::format::{Limits, SNAPSHOT_ARTIFACT, SNAPSHOT_CONFIG, SnapshotConfig};

/// A stored snapshot whose manifest and config passed every check.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub digest: Digest,
    pub manifest: ImageManifest,
    pub config: SnapshotConfig,
}

impl Loaded {
    /// The bundle's size: every file in full, zero chunks included.
    pub fn logical_size(&self) -> u64 {
        self.config.files.iter().map(|f| f.size).sum()
    }

    /// The bytes of the distinct non-zero chunks (what a registry stores).
    pub fn stored_size(&self) -> u64 {
        self.manifest.layers.iter().map(|l| l.size).sum()
    }
}

/// Parses a manifest that must be a snapshot's, and bounds its config before
/// anything reads it. `what` names it in errors (a digest or reference).
pub(crate) fn snapshot_manifest(what: &str, bytes: &[u8], limits: &Limits) -> Result<ImageManifest> {
    let not = || SnapshotError::NotASnapshot(what.to_string());
    let m: ImageManifest = serde_json::from_slice(bytes).map_err(|_| not())?;
    if m.schema_version != 2
        || m.media_type.as_deref() != Some(OCI_MANIFEST)
        || m.artifact_type.as_deref() != Some(SNAPSHOT_ARTIFACT)
        || m.config.media_type != SNAPSHOT_CONFIG
    {
        return Err(not());
    }
    if m.config.size > limits.max_config {
        return Err(SnapshotError::LimitExceeded {
            what: "snapshot config size",
            max: limits.max_config,
        });
    }
    Ok(m)
}

/// Reads the config `m` names from the store and checks it against `m`.
pub(crate) fn snapshot_config(store: &Store, m: &ImageManifest, limits: &Limits) -> Result<SnapshotConfig> {
    let bytes = store.read_metadata(&m.config.digest)?;
    if bytes.len() as u64 != m.config.size {
        return Err(kiln_store::StoreError::SizeMismatch {
            digest: m.config.digest.clone(),
            expected: m.config.size,
            actual: bytes.len() as u64,
        }
        .into());
    }
    let config: SnapshotConfig = json("snapshot config", &bytes)?;
    config.check(&m.layers, limits)?;
    Ok(config)
}

/// Loads the snapshot whose manifest is `digest`, with the default [`Limits`].
pub fn load(store: &Store, digest: &Digest) -> Result<Loaded> {
    load_with(store, digest, &Limits::default())
}

/// Loads a snapshot: its manifest must be a snapshot manifest and its config must
/// pass [`SnapshotConfig::check`]. Chunks need not be in the store (a lazy pull
/// fetches them on demand).
pub fn load_with(store: &Store, digest: &Digest, limits: &Limits) -> Result<Loaded> {
    let bytes = store.read_metadata(digest)?;
    let manifest = snapshot_manifest(&digest.to_string(), &bytes, limits)?;
    let config = snapshot_config(store, &manifest, limits)?;
    Ok(Loaded {
        digest: digest.clone(),
        manifest,
        config,
    })
}

/// Whether `digest` is a stored manifest with the snapshot `artifactType` (for
/// telling snapshots from images in `ls`, and refusing them where an image is
/// expected). It says nothing about validity; [`load`] checks that.
pub fn is_snapshot(store: &Store, digest: &Digest) -> bool {
    store
        .read_metadata(digest)
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .is_some_and(|v| v.get("artifactType").and_then(|t| t.as_str()) == Some(SNAPSHOT_ARTIFACT))
}

/// A digest of a blob in the store, or a ref name: as given, else normalised as a
/// reference (`host/team/vm:1` as `kiln snapshot pull` tags it).
pub fn resolve(store: &Store, name: &str) -> Result<Digest> {
    if let Ok(d) = Digest::parse(name)
        && store.has_blob(&d)
    {
        return Ok(d);
    }
    if let Some(d) = store.get_ref(name)? {
        return Ok(d);
    }
    if let Ok(r) = Reference::parse(name)
        && let Some(d) = store.get_ref(&r.to_string())?
    {
        return Ok(d);
    }
    Err(SnapshotError::RefNotFound(name.to_string()))
}

/// Tags `digest` as `name` once it loads as a snapshot. Callers that just wrote
/// it hold `store.lock_shared()`, so GC cannot remove its blobs first.
pub fn tag(store: &Store, name: &str, digest: &Digest) -> Result<()> {
    kiln_store::check_ref_name(name)?;
    load(store, digest)?;
    store.set_ref(name, digest)?;
    Ok(())
}
