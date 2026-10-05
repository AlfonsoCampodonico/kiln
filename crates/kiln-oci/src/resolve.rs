//! Resolving a local image source to verified, per-platform images (spec §6.1).

use std::io::Read;
use std::path::{Path, PathBuf};

use kiln_registry::Reference;
use kiln_store::{Digest, MAX_METADATA_BLOB, Store, StoreError};
use serde::Deserialize;

use crate::error::{OciError, Result, json};
use crate::media::{self, OCI_CONFIG, OCI_MANIFEST};
use crate::platform::Platform;
use crate::source::{BlobSource, DirLayout, TarArchive};
use crate::types::{Descriptor, ImageConfig, ImageIndex, ImageManifest, canonical_json};

/// The OCI annotation naming an image in a layout's `index.json`.
pub const REF_NAME: &str = "org.opencontainers.image.ref.name";
/// containerd's annotation (used by `docker save` OCI exports) with the full reference.
pub const CONTAINERD_NAME: &str = "io.containerd.image.name";

/// A local image source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalSource {
    /// An OCI image layout directory.
    Layout(PathBuf),
    /// A `docker save` archive (legacy or OCI-in-tar).
    Archive(PathBuf),
}

impl LocalSource {
    /// A directory containing `oci-layout` is a layout; a regular file is an archive.
    pub fn detect(path: &Path) -> Result<Self> {
        if path.join("oci-layout").is_file() {
            Ok(Self::Layout(path.to_path_buf()))
        } else if path.is_file() {
            Ok(Self::Archive(path.to_path_buf()))
        } else {
            Err(OciError::NotAnImage(path.display().to_string()))
        }
    }
}

/// One platform's image. Its manifest and config are verified into the store; so
/// are its layers for local sources, while registry layers are fetched later,
/// only when a conversion needs them.
#[derive(Debug, Clone)]
pub struct ResolvedImage {
    pub platform: Platform,
    pub manifest_digest: Digest,
    pub manifest: ImageManifest,
    pub config_digest: Digest,
    pub config: ImageConfig,
    /// The name the source gave this image, if any.
    pub ref_name: Option<String>,
}

/// Resolves `source` for each wanted platform, verifying every blob into `store`.
pub fn resolve_local(
    store: &Store,
    source: &LocalSource,
    ref_name: Option<&str>,
    platforms: &[Platform],
) -> Result<Vec<ResolvedImage>> {
    match source {
        LocalSource::Layout(dir) => resolve_layout(store, &DirLayout::open(dir)?, ref_name, platforms),
        LocalSource::Archive(path) => {
            let ar = TarArchive::open(path)?;
            if ar.is_oci_layout() {
                resolve_layout(store, &ar, ref_name, platforms)
            } else if ar.has_path("manifest.json") {
                resolve_legacy(store, &ar, ref_name, platforms)
            } else {
                Err(OciError::NotAnImage(path.display().to_string()))
            }
        }
    }
}

/// Copies a blob into the store, verifying digest and size.
fn ingest(store: &Store, src: &dyn BlobSource, d: &Descriptor) -> Result<()> {
    if store.has_blob(&d.digest) {
        // Already stored: the descriptor's size must still agree with it.
        let actual = store.blob_size(&d.digest)?;
        if actual != d.size {
            return Err(StoreError::SizeMismatch {
                digest: d.digest.clone(),
                expected: d.size,
                actual,
            }
            .into());
        }
    } else {
        let mut r = src.open_blob(&d.digest)?;
        store.put_verified(&mut r, &d.digest, Some(d.size))?;
    }
    Ok(())
}

fn ingest_metadata(store: &Store, src: &dyn BlobSource, d: &Descriptor) -> Result<Vec<u8>> {
    if d.size > MAX_METADATA_BLOB {
        return Err(StoreError::TooLarge {
            digest: d.digest.clone(),
            size: d.size,
            max: MAX_METADATA_BLOB,
        }
        .into());
    }
    ingest(store, src, d)?;
    Ok(store.read_metadata(&d.digest)?)
}

/// Whether a name from the source selects the wanted `--ref`: equal normalised
/// references when both parse (`php:8.4-cli` is `docker.io/library/php:8.4-cli`),
/// else equal strings.
pub(crate) fn same_name(want: &str, name: &str) -> bool {
    match (Reference::parse(want), Reference::parse(name)) {
        (Ok(a), Ok(b)) => a == b,
        _ => want == name,
    }
}

fn names(d: &Descriptor) -> Vec<String> {
    [REF_NAME, CONTAINERD_NAME]
        .iter()
        .filter_map(|k| d.annotation(k).map(str::to_string))
        .collect()
}

/// Attestation manifests that Docker 25+ (`docker save`) and BuildKit list beside
/// images. They are never images, so selection skips them.
pub(crate) fn is_attestation(d: &Descriptor) -> bool {
    d.annotation("io.containerd.manifest.subject").is_some()
        || d.annotation("vnd.docker.reference.type") == Some("attestation-manifest")
        || d.platform.as_ref().is_some_and(|p| p.os == "unknown")
}

fn resolve_layout(
    store: &Store,
    src: &dyn BlobSource,
    ref_name: Option<&str>,
    platforms: &[Platform],
) -> Result<Vec<ResolvedImage>> {
    let index: ImageIndex = json("index.json", &src.read_small("index.json", MAX_METADATA_BLOB)?)?;
    let candidates: Vec<&Descriptor> = index.manifests.iter().filter(|d| !is_attestation(d)).collect();
    let top = match ref_name {
        Some(want) => *candidates
            .iter()
            .find(|d| names(d).iter().any(|n| same_name(want, n)))
            .ok_or_else(|| OciError::RefNotFound {
                wanted: want.to_string(),
                available: candidates.iter().flat_map(|d| names(d)).collect(),
            })?,
        None => match candidates.as_slice() {
            [one] => *one,
            [] => {
                return Err(OciError::NotAnImage(format!(
                    "{} (its index.json lists no images)",
                    src.describe()
                )));
            }
            many => {
                return Err(OciError::AmbiguousRef {
                    available: many.iter().flat_map(|d| names(d)).collect(),
                });
            }
        },
    };
    let image_name = ref_name.map(str::to_string).or_else(|| names(top).into_iter().next());
    let bytes = ingest_metadata(store, src, top)?;
    if media::is_index(&top.media_type) {
        let inner: ImageIndex = json("image index", &bytes)?;
        platforms
            .iter()
            .map(|want| {
                let d = pick_platform(&inner, want)?;
                let bytes = ingest_metadata(store, src, d)?;
                finish_manifest(store, src, d, &bytes, want, image_name.clone())
            })
            .collect()
    } else if media::is_manifest(&top.media_type) {
        platforms
            .iter()
            .map(|want| finish_manifest(store, src, top, &bytes, want, image_name.clone()))
            .collect()
    } else {
        Err(OciError::UnsupportedMediaType(top.media_type.clone()))
    }
}

/// The entry of a multi-platform index for `want` (attestations never match).
pub(crate) fn pick_platform<'a>(index: &'a ImageIndex, want: &Platform) -> Result<&'a Descriptor> {
    index
        .manifests
        .iter()
        .filter(|m| !is_attestation(m))
        .find(|m| m.platform.as_ref().is_some_and(|p| want.matches(p)))
        .ok_or_else(|| OciError::MissingPlatform {
            wanted: want.to_string(),
            available: index
                .manifests
                .iter()
                .filter(|m| !is_attestation(m))
                .filter_map(|m| m.platform.as_ref())
                .map(|p| p.to_string())
                .collect(),
        })
}

/// Parses a per-platform manifest and rejects unsupported config and layer types
/// before anything large is fetched (spec §6.1 step 2).
pub(crate) fn parse_manifest(d: &Descriptor, bytes: &[u8]) -> Result<ImageManifest> {
    if !media::is_manifest(&d.media_type) {
        return Err(OciError::UnsupportedMediaType(d.media_type.clone()));
    }
    let manifest: ImageManifest = json("image manifest", bytes)?;
    if !media::is_config(&manifest.config.media_type) {
        return Err(OciError::UnsupportedMediaType(manifest.config.media_type.clone()));
    }
    for l in &manifest.layers {
        media::layer_compression(&l.media_type)?;
    }
    if manifest.config.size > MAX_METADATA_BLOB {
        return Err(StoreError::TooLarge {
            digest: manifest.config.digest.clone(),
            size: manifest.config.size,
            max: MAX_METADATA_BLOB,
        }
        .into());
    }
    Ok(manifest)
}

/// Parses a verified config and checks its platform and `diff_ids` against the manifest.
pub(crate) fn parse_config(manifest: &ImageManifest, bytes: &[u8], want: &Platform) -> Result<ImageConfig> {
    let config: ImageConfig = json("image config", bytes)?;
    let platform = config.platform();
    if !want.matches(&platform) {
        return Err(OciError::MissingPlatform {
            wanted: want.to_string(),
            available: vec![platform.to_string()],
        });
    }
    if config.rootfs.diff_ids.len() != manifest.layers.len() {
        return Err(OciError::DiffIdCount {
            layers: manifest.layers.len(),
            diff_ids: config.rootfs.diff_ids.len(),
        });
    }
    Ok(config)
}

/// Validates a manifest, then verifies its config and layers into the store.
fn finish_manifest(
    store: &Store,
    src: &dyn BlobSource,
    d: &Descriptor,
    bytes: &[u8],
    want: &Platform,
    ref_name: Option<String>,
) -> Result<ResolvedImage> {
    let manifest = parse_manifest(d, bytes)?;
    let config = parse_config(&manifest, &ingest_metadata(store, src, &manifest.config)?, want)?;
    for l in &manifest.layers {
        ingest(store, src, l)?;
    }
    Ok(ResolvedImage {
        platform: config.platform(),
        manifest_digest: d.digest.clone(),
        config_digest: manifest.config.digest.clone(),
        manifest,
        config,
        ref_name,
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct LegacyEntry {
    config: String,
    #[serde(default)]
    repo_tags: Option<Vec<String>>,
    layers: Vec<String>,
}

/// Legacy `docker save`: `manifest.json` names files whose digests kiln computes itself.
fn resolve_legacy(
    store: &Store,
    src: &dyn BlobSource,
    ref_name: Option<&str>,
    platforms: &[Platform],
) -> Result<Vec<ResolvedImage>> {
    let entries: Vec<LegacyEntry> = json("manifest.json", &src.read_small("manifest.json", MAX_METADATA_BLOB)?)?;
    let tags = |e: &LegacyEntry| e.repo_tags.clone().unwrap_or_default();
    let entry = match ref_name {
        Some(want) => entries
            .iter()
            .find(|e| tags(e).iter().any(|t| same_name(want, t)))
            .ok_or_else(|| OciError::RefNotFound {
                wanted: want.to_string(),
                available: entries.iter().flat_map(tags).collect(),
            })?,
        None => match entries.as_slice() {
            [one] => one,
            [] => {
                return Err(OciError::NotAnImage(format!(
                    "{} (its manifest.json lists no images)",
                    src.describe()
                )));
            }
            many => {
                return Err(OciError::AmbiguousRef {
                    available: many.iter().flat_map(tags).collect(),
                });
            }
        },
    };
    let config_bytes = src.read_small(&entry.config, MAX_METADATA_BLOB)?;
    let config_digest = store.put_bytes(&config_bytes)?;
    let config: ImageConfig = json("image config", &config_bytes)?;
    if config.rootfs.diff_ids.len() != entry.layers.len() {
        return Err(OciError::DiffIdCount {
            layers: entry.layers.len(),
            diff_ids: config.rootfs.diff_ids.len(),
        });
    }
    // Check the platform before copying any layer into the store.
    let platform = config.platform();
    if let Some(want) = platforms.iter().find(|want| !want.matches(&platform)) {
        return Err(OciError::MissingPlatform {
            wanted: want.to_string(),
            available: vec![platform.to_string()],
        });
    }
    let mut layers = Vec::new();
    for path in &entry.layers {
        let (digest, size) = store.put_reader(&mut src.open_path(path)?)?;
        let mut head = [0u8; 4];
        let n = store.open_blob(&digest)?.read(&mut head)?;
        layers.push(Descriptor::new(
            media::oci_layer_media_type(media::sniff_compression(&head[..n])),
            digest,
            size,
        ));
    }
    let manifest = ImageManifest {
        schema_version: 2,
        media_type: Some(OCI_MANIFEST.to_string()),
        artifact_type: None,
        config: Descriptor::new(OCI_CONFIG, config_digest.clone(), config_bytes.len() as u64),
        layers,
        annotations: None,
    };
    let manifest_bytes = canonical_json(&manifest);
    let manifest_digest = store.put_bytes(&manifest_bytes)?;
    let ref_name = ref_name.map(str::to_string).or_else(|| tags(entry).into_iter().next());
    Ok(platforms
        .iter()
        .map(|_| ResolvedImage {
            platform: platform.clone(),
            manifest_digest: manifest_digest.clone(),
            manifest: manifest.clone(),
            config_digest: config_digest.clone(),
            config: config.clone(),
            ref_name: ref_name.clone(),
        })
        .collect())
}
