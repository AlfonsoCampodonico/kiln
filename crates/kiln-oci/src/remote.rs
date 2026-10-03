//! Resolving a registry reference to verified, per-platform images (spec §6.1).

use kiln_registry::{Client, Reference};
use kiln_store::{MAX_METADATA_BLOB, Store, StoreError};

use crate::error::{OciError, Result, json};
use crate::media;
use crate::platform::Platform;
use crate::resolve::{ResolvedImage, parse_config, parse_manifest, pick_platform};
use crate::types::{Descriptor, ImageIndex};

/// Resolves `reference` for each wanted platform. Indexes, manifests and configs
/// are verified into `store`, with the same validation as local inputs; layer
/// blobs are not fetched here. A tag is resolved with a manifest HEAD; what the
/// store already holds is not fetched again, so a warm resolve makes that one
/// request (see [`Client::resolve_manifest`]).
pub fn resolve_registry(
    store: &Store,
    client: &Client,
    reference: &Reference,
    platforms: &[Platform],
) -> Result<Vec<ResolvedImage>> {
    let repo = reference.repository();
    let top = client.resolve_manifest(repo, &reference.target(), store)?;
    store.put_bytes(&top.bytes)?;
    let name = Some(reference.to_string());
    if media::is_index(&top.media_type) {
        let index: ImageIndex = json("image index", &top.bytes)?;
        platforms
            .iter()
            .map(|want| {
                let d = pick_platform(&index, want)?;
                let bytes = fetch_manifest(store, client, repo, d)?;
                finish(store, client, repo, d, &bytes, want, name.clone())
            })
            .collect()
    } else {
        let d = Descriptor::new(&top.media_type, top.digest.clone(), top.bytes.len() as u64);
        platforms
            .iter()
            .map(|want| finish(store, client, repo, &d, &top.bytes, want, name.clone()))
            .collect()
    }
}

/// An index entry's manifest, by digest, verified into the store: from the store
/// when it is already there, else from the registry (which checks the digest).
/// The entry must be an image manifest, and its size must match the descriptor;
/// both are checked before any request.
pub fn fetch_manifest(store: &Store, client: &Client, repo: &str, d: &Descriptor) -> Result<Vec<u8>> {
    if !media::is_manifest(&d.media_type) {
        return Err(OciError::UnsupportedMediaType(d.media_type.clone()));
    }
    if d.size > MAX_METADATA_BLOB {
        return Err(StoreError::TooLarge {
            digest: d.digest.clone(),
            size: d.size,
            max: MAX_METADATA_BLOB,
        }
        .into());
    }
    let bytes = if store.has_blob(&d.digest) {
        store.read_metadata(&d.digest)?
    } else {
        client.get_manifest(repo, &d.digest.to_string())?.bytes
    };
    if bytes.len() as u64 != d.size {
        return Err(StoreError::SizeMismatch {
            digest: d.digest.clone(),
            expected: d.size,
            actual: bytes.len() as u64,
        }
        .into());
    }
    store.put_bytes(&bytes)?;
    Ok(bytes)
}

fn finish(
    store: &Store,
    client: &Client,
    repo: &str,
    d: &Descriptor,
    bytes: &[u8],
    want: &Platform,
    ref_name: Option<String>,
) -> Result<ResolvedImage> {
    let manifest = parse_manifest(d, bytes)?;
    client.fetch_blob(repo, &manifest.config.digest, manifest.config.size, store)?;
    let config = parse_config(&manifest, &store.read_metadata(&manifest.config.digest)?, want)?;
    Ok(ResolvedImage {
        platform: config.platform(),
        manifest_digest: d.digest.clone(),
        config_digest: manifest.config.digest.clone(),
        manifest,
        config,
        ref_name,
    })
}
