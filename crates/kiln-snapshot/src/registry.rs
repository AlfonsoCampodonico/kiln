//! Push and pull of snapshots through `kiln-registry` (design §5.5). Pull treats
//! everything it receives as hostile: the manifest and config are verified and
//! checked before any chunk is requested, every chunk enters the store through
//! `put_verified`, and the snapshot is tagged only after all of that (T1, T3).

use kiln_oci::Descriptor;
use kiln_oci::media::OCI_MANIFEST;
use kiln_registry::{Client, Reference};
use kiln_store::{Digest, Store, StoreError};

use crate::error::{Result, SnapshotError};
use crate::format::Limits;
use crate::load::{Loaded, load, resolve, snapshot_config, snapshot_manifest};

/// What a push or pull transferred.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferReport {
    pub digest: Digest,
    /// Blobs (the manifest excluded) actually transferred.
    pub blobs: usize,
    pub bytes: u64,
    /// Blobs the other side already had.
    pub skipped: usize,
}

impl TransferReport {
    fn new(digest: &Digest) -> Self {
        Self {
            digest: digest.clone(),
            blobs: 0,
            bytes: 0,
            skipped: 0,
        }
    }

    fn count(&mut self, transferred: bool, d: &Descriptor) {
        if transferred {
            self.blobs += 1;
            self.bytes += d.size;
        } else {
            self.skipped += 1;
        }
    }
}

/// Whether a pull fetches the chunks now or leaves them to be fetched on demand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fetch {
    /// Every chunk, before the snapshot is tagged.
    Eager,
    /// The manifest and config only (for `write_lazy` and lazy restore).
    Lazy,
}

/// A pulled snapshot and what the pull downloaded.
#[derive(Debug, Clone)]
pub struct Pulled {
    pub loaded: Loaded,
    pub report: TransferReport,
}

fn check_registry(client: &Client, reference: &Reference) -> Result<()> {
    if client.registry() == reference.registry() {
        Ok(())
    } else {
        Err(SnapshotError::BadOption(format!(
            "client for {} cannot reach {reference}",
            client.registry()
        )))
    }
}

/// Pushes the stored snapshot `name` (a ref or digest) to `reference`: every
/// chunk the registry does not have, then the config, then the manifest by the
/// reference's tag (or digest). Bytes are pushed unchanged, so the digest
/// survives the round trip. Every blob must be in the store before the first
/// upload, so a lazily pulled snapshot is refused until its chunks are fetched.
pub fn push(store: &Store, client: &Client, name: &str, reference: &Reference) -> Result<TransferReport> {
    check_registry(client, reference)?;
    let _lock = store.lock_shared()?;
    let digest = resolve(store, name)?;
    let loaded = load(store, &digest)?;
    if let Some(want) = reference.digest()
        && want != &digest
    {
        return Err(SnapshotError::BadOption(format!(
            "{name} is {digest}, but the reference names {want}"
        )));
    }
    let blobs: Vec<&Descriptor> = loaded
        .manifest
        .layers
        .iter()
        .chain(std::iter::once(&loaded.manifest.config))
        .collect();
    if let Some(d) = blobs.iter().find(|d| !store.has_blob(&d.digest)) {
        return Err(StoreError::NotFound(d.digest.clone()).into());
    }
    let repo = reference.repository();
    let mut report = TransferReport::new(&digest);
    for d in blobs {
        let pushed = client.push_blob(repo, &d.digest, &store.blob_path(&d.digest))?;
        report.count(pushed, d);
    }
    let target = reference.tag().map_or_else(|| digest.to_string(), str::to_string);
    let pushed = client.put_manifest(repo, &target, OCI_MANIFEST, &store.read_metadata(&digest)?)?;
    debug_assert_eq!(pushed, digest);
    Ok(report)
}

/// Pulls the snapshot at `reference` and tags it `tag`. The manifest is resolved
/// and verified, must be a snapshot manifest, and its config descriptor must be
/// within the limits before the config is fetched (verified). The config must
/// pass every check before any chunk is fetched. With [`Fetch::Eager`] every
/// chunk is then fetched (verified); only then is the snapshot tagged.
pub fn pull(store: &Store, client: &Client, reference: &Reference, tag: &str, fetch: Fetch) -> Result<Pulled> {
    check_registry(client, reference)?;
    kiln_store::check_ref_name(tag)?;
    let limits = Limits::default();
    let _lock = store.lock_shared()?;
    let repo = reference.repository();
    let what = reference.to_string();
    let top = client.resolve_manifest(repo, &reference.target(), store)?;
    if top.media_type != OCI_MANIFEST {
        return Err(SnapshotError::NotASnapshot(what));
    }
    let manifest = snapshot_manifest(&what, &top.bytes, &limits)?;
    let mut report = TransferReport::new(&top.digest);
    let config = &manifest.config;
    report.count(client.fetch_blob(repo, &config.digest, config.size, store)?, config);
    let config = snapshot_config(store, &manifest, &limits)?;
    store.put_bytes(&top.bytes)?;
    let loaded = Loaded {
        digest: top.digest.clone(),
        manifest,
        config,
    };
    if fetch == Fetch::Eager {
        let chunks = fetch_layers(store, client, repo, &loaded)?;
        report.blobs += chunks.blobs;
        report.bytes += chunks.bytes;
        report.skipped += chunks.skipped;
    }
    store.set_ref(tag, &top.digest)?;
    Ok(Pulled { loaded, report })
}

/// Fetches every chunk of `loaded` that is not in the store from `repo`, each
/// verified against its digest and size (an eager pull after a lazy one).
pub fn fetch_chunks(store: &Store, client: &Client, repo: &str, loaded: &Loaded) -> Result<TransferReport> {
    let _lock = store.lock_shared()?;
    fetch_layers(store, client, repo, loaded)
}

fn fetch_layers(store: &Store, client: &Client, repo: &str, loaded: &Loaded) -> Result<TransferReport> {
    let mut report = TransferReport::new(&loaded.digest);
    for d in &loaded.manifest.layers {
        report.count(client.fetch_blob(repo, &d.digest, d.size, store)?, d);
    }
    Ok(report)
}
