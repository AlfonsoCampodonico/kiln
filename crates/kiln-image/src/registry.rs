//! Registry inputs and outputs: `convert` from a reference, `pull` and `push` of
//! kiln images (spec §6.1, §6.2).

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use kiln_oci::media::{OCI_INDEX, OCI_MANIFEST};
use kiln_oci::{Descriptor, ImageIndex, ImageManifest, OciError, Platform, fetch_manifest, resolve_registry};
use kiln_registry::{Client, Reference};
use kiln_store::{Digest, MAX_METADATA_BLOB, Store, StoreError};

use crate::convert::{ConvertOptions, LayerSource};
use crate::error::{ImageError, Result, json};
use crate::load::{load, load_manifest, resolve_name};
use crate::pipeline::{Output, convert_resolved};
use crate::types::{KILN_ARTIFACT, KILN_CONFIG, KILN_INIT, KILN_KERNEL, KILN_LAYER};

/// What to convert from a registry.
#[derive(Debug, Clone, Default)]
pub struct RegistryRequest<'a> {
    /// Platforms to convert; empty means the host's.
    pub platforms: &'a [Platform],
    /// Name to record in `refs.json`.
    pub tag: Option<&'a str>,
}

/// Room above the uncompressed layer limit for compression framing (T3).
const FRAMING_ALLOWANCE: u64 = 1 << 20;

/// The most compressed bytes one layer blob may declare.
fn max_blob_bytes(opts: &ConvertOptions) -> u64 {
    opts.limits.max_layer_bytes.saturating_add(FRAMING_ALLOWANCE)
}

fn blob_too_large(max: u64) -> ImageError {
    ImageError::LimitExceeded {
        what: "compressed layer size",
        max,
    }
}

/// Fetches layer blobs on demand and counts what it downloads. A descriptor that
/// declares more than the per-layer limit is refused before any request, as is
/// one that would take the image's compressed total over its limit (T3).
struct RegistryLayers<'a> {
    store: &'a Store,
    client: &'a Client,
    repo: &'a str,
    max_blob: u64,
    max_image: u64,
    blobs: AtomicUsize,
    /// Bytes downloaded, plus those of fetches in flight.
    bytes: AtomicU64,
}

impl LayerSource for RegistryLayers<'_> {
    fn fetch(&self, layer: &Descriptor) -> Result<()> {
        if layer.size > self.max_blob {
            return Err(blob_too_large(self.max_blob));
        }
        let before = self.bytes.fetch_add(layer.size, Ordering::SeqCst);
        if before.saturating_add(layer.size) > self.max_image {
            self.bytes.fetch_sub(layer.size, Ordering::SeqCst);
            return Err(ImageError::LimitExceeded {
                what: "compressed image size",
                max: self.max_image,
            });
        }
        match self.client.fetch_blob(self.repo, &layer.digest, layer.size, self.store) {
            Ok(true) => {
                self.blobs.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            Ok(false) => {
                self.bytes.fetch_sub(layer.size, Ordering::SeqCst);
                Ok(())
            }
            Err(e) => {
                self.bytes.fetch_sub(layer.size, Ordering::SeqCst);
                Err(e.into())
            }
        }
    }
}

fn check_registry(client: &Client, reference: &Reference) -> Result<()> {
    if client.registry() == reference.registry() {
        Ok(())
    } else {
        Err(ImageError::BadOption(format!(
            "client for {} cannot reach {reference}",
            client.registry()
        )))
    }
}

/// Resolve, fetch and convert a registry image under the store's shared lock.
/// Only layers that are not cache hits are downloaded, so a warm convert fetches
/// just the manifests and config (spec §6.2). The kiln config records the
/// normalised reference.
pub fn convert_registry(
    store: &Store,
    client: &Client,
    reference: &Reference,
    req: &RegistryRequest,
    opts: &ConvertOptions,
) -> Result<Output> {
    check_registry(client, reference)?;
    let _lock = store.lock_shared()?;
    let host = [Platform::host()];
    let platforms = if req.platforms.is_empty() {
        &host[..]
    } else {
        req.platforms
    };
    let resolved = resolve_registry(store, client, reference, platforms)?;
    let layers = RegistryLayers {
        store,
        client,
        repo: reference.repository(),
        max_blob: max_blob_bytes(opts),
        max_image: opts.max_image_bytes,
        blobs: AtomicUsize::new(0),
        bytes: AtomicU64::new(0),
    };
    let mut out = convert_resolved(store, &resolved, Some(&reference.to_string()), opts, &layers)?;
    out.layers_downloaded = layers.blobs.into_inner();
    out.bytes_downloaded = layers.bytes.into_inner();
    if let Some(tag) = req.tag {
        store.set_ref(tag, &out.digest)?;
    }
    Ok(out)
}

/// What a pull or push transferred.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferReport {
    pub digest: Digest,
    /// Blobs (manifests excluded) actually transferred.
    pub blobs: usize,
    pub bytes: u64,
    /// Blobs the other side already had.
    pub skipped: usize,
}

const PULLED_LAYER_TYPES: [&str; 3] = [KILN_LAYER, KILN_KERNEL, KILN_INIT];

fn not_kiln(reference: &Reference) -> ImageError {
    ImageError::NotKilnRemote(reference.to_string())
}

/// Pulls a kiln image (all platforms of an index) and tags it `tag`. Every blob
/// is verified. Anything that is not a kiln image is refused, and pulled layers
/// never enter the conversion caches (spec §6.2).
pub fn pull_image(store: &Store, client: &Client, reference: &Reference, tag: &str) -> Result<TransferReport> {
    check_registry(client, reference)?;
    kiln_store::check_ref_name(tag)?;
    let _lock = store.lock_shared()?;
    let repo = reference.repository();
    let top = client.get_manifest(repo, &reference.target())?;
    store.put_bytes(&top.bytes)?;
    let manifests: Vec<Digest> = match top.media_type.as_str() {
        OCI_INDEX => {
            let index: ImageIndex = json("kiln index", &top.bytes)?;
            if index.artifact_type.as_deref() != Some(KILN_ARTIFACT) {
                return Err(not_kiln(reference));
            }
            for d in &index.manifests {
                if d.media_type != OCI_MANIFEST {
                    return Err(not_kiln(reference));
                }
                fetch_manifest(store, client, repo, d)?;
            }
            if index.manifests.is_empty() {
                return Err(not_kiln(reference));
            }
            index.manifests.into_iter().map(|d| d.digest).collect()
        }
        OCI_MANIFEST => vec![top.digest.clone()],
        _ => return Err(not_kiln(reference)),
    };
    // First pass: validate every platform before fetching anything of any of them.
    let limits = ConvertOptions::default();
    let max_blob = max_blob_bytes(&limits);
    let mut validated = Vec::with_capacity(manifests.len());
    let mut unique = HashSet::new();
    let mut declared = 0u64;
    for digest in &manifests {
        let m: ImageManifest = json("kiln manifest", &store.read_metadata(digest)?)?;
        if m.artifact_type.as_deref() != Some(KILN_ARTIFACT) || m.config.media_type != KILN_CONFIG {
            return Err(not_kiln(reference));
        }
        if m.config.size > MAX_METADATA_BLOB {
            return Err(StoreError::TooLarge {
                digest: m.config.digest.clone(),
                size: m.config.size,
                max: MAX_METADATA_BLOB,
            }
            .into());
        }
        // Check the media types before fetching any layer.
        if let Some(l) = m
            .layers
            .iter()
            .find(|l| !PULLED_LAYER_TYPES.contains(&l.media_type.as_str()))
        {
            return Err(OciError::UnsupportedMediaType(l.media_type.clone()).into());
        }
        // Bound the downloads: per blob, and over the image's distinct layers.
        for l in &m.layers {
            if l.size > max_blob {
                return Err(blob_too_large(max_blob));
            }
            if unique.insert(l.digest.clone()) {
                declared = declared.saturating_add(l.size);
            }
        }
        if declared > limits.max_image_bytes {
            return Err(ImageError::LimitExceeded {
                what: "compressed image size",
                max: limits.max_image_bytes,
            });
        }
        validated.push((digest, m));
    }
    let mut report = TransferReport {
        digest: top.digest.clone(),
        blobs: 0,
        bytes: 0,
        skipped: 0,
    };
    let mut fetch = |d: &Descriptor| -> Result<()> {
        if client.fetch_blob(repo, &d.digest, d.size, store)? {
            report.blobs += 1;
            report.bytes += d.size;
        } else {
            report.skipped += 1;
        }
        Ok(())
    };
    for (digest, m) in &validated {
        fetch(&m.config)?;
        load_manifest(store, digest)?;
        for l in &m.layers {
            fetch(l)?;
        }
    }
    // Refuses to tag anything that does not load as a kiln image.
    load(store, &top.digest)?;
    store.set_ref(tag, &top.digest)?;
    Ok(report)
}

/// Pushes a stored kiln image to `reference`: every blob of every platform
/// (skipping those the registry has), each per-platform manifest by digest, then
/// the top index or manifest by the reference's tag. Bytes are pushed unchanged,
/// so digests survive the round trip.
pub fn push_image(store: &Store, client: &Client, name: &str, reference: &Reference) -> Result<TransferReport> {
    check_registry(client, reference)?;
    let _lock = store.lock_shared()?;
    let digest = resolve_name(store, name)?;
    let loaded = load(store, &digest)?;
    if let Some(want) = reference.digest()
        && want != &digest
    {
        return Err(ImageError::BadOption(format!(
            "{name} is {digest}, but the reference names {want}"
        )));
    }
    let repo = reference.repository();
    let mut report = TransferReport {
        digest: digest.clone(),
        blobs: 0,
        bytes: 0,
        skipped: 0,
    };
    let mut seen = HashSet::new();
    let mut blobs = Vec::new();
    for (_, m) in &loaded.entries {
        for d in std::iter::once(&m.manifest.config).chain(&m.manifest.layers) {
            if seen.insert(d.digest.clone()) {
                blobs.push(d);
            }
        }
    }
    // Every blob must be here before the first upload.
    if let Some(d) = blobs.iter().find(|d| !store.has_blob(&d.digest)) {
        return Err(StoreError::NotFound(d.digest.clone()).into());
    }
    for d in blobs {
        if client.push_blob(repo, &d.digest, &store.blob_path(&d.digest))? {
            report.blobs += 1;
            report.bytes += d.size;
        } else {
            report.skipped += 1;
        }
    }
    if loaded.is_index {
        for (_, m) in &loaded.entries {
            client.put_manifest(
                repo,
                &m.digest.to_string(),
                OCI_MANIFEST,
                &store.read_metadata(&m.digest)?,
            )?;
        }
    }
    let media_type = if loaded.is_index { OCI_INDEX } else { OCI_MANIFEST };
    let target = reference.tag().map_or_else(|| digest.to_string(), str::to_string);
    let pushed = client.put_manifest(repo, &target, media_type, &store.read_metadata(&digest)?)?;
    debug_assert_eq!(pushed, digest);
    Ok(report)
}
