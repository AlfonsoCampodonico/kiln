//! Committing converted images: manifest or multi-arch index, then the ref (§6.5 step 3).

use std::path::Path;

use kiln_oci::media::{OCI_INDEX, OCI_MANIFEST};
use kiln_oci::{Descriptor, ImageIndex, LocalSource, Platform, ResolvedImage, canonical_json, resolve_local};
use kiln_store::{Digest, Store};

use crate::convert::{ConvertOptions, Converted, convert_image};
use crate::error::Result;
use crate::types::KILN_ARTIFACT;

/// The committed result: a manifest for one platform, an index for several.
#[derive(Debug, Clone)]
pub struct Output {
    pub digest: Digest,
    pub media_type: &'static str,
    pub images: Vec<Converted>,
}

/// Converts every resolved platform and writes the top-level blob. Requested
/// platforms that select the same source manifest (`linux/arm64` and
/// `linux/arm64/v8`) are converted once.
pub fn convert_resolved(
    store: &Store,
    resolved: &[ResolvedImage],
    reference: Option<&str>,
    opts: &ConvertOptions,
) -> Result<Output> {
    let mut seen = std::collections::HashSet::new();
    let mut images = resolved
        .iter()
        .filter(|r| seen.insert(r.manifest_digest.clone()))
        .map(|r| convert_image(store, r, reference, opts))
        .collect::<Result<Vec<_>>>()?;
    if let [one] = images.as_slice() {
        return Ok(Output {
            digest: one.manifest_digest.clone(),
            media_type: OCI_MANIFEST,
            images,
        });
    }
    images.sort_by(|a, b| a.platform.cmp(&b.platform));
    let index = ImageIndex {
        schema_version: 2,
        media_type: Some(OCI_INDEX.to_string()),
        artifact_type: Some(KILN_ARTIFACT.to_string()),
        manifests: images
            .iter()
            .map(|c| {
                let mut d = Descriptor::new(OCI_MANIFEST, c.manifest_digest.clone(), c.manifest_size);
                d.artifact_type = Some(KILN_ARTIFACT.to_string());
                d.platform = Some(c.platform.clone());
                d
            })
            .collect(),
        annotations: None,
    };
    let digest = store.put_bytes(&canonical_json(&index))?;
    Ok(Output {
        digest,
        media_type: OCI_INDEX,
        images,
    })
}

/// What to convert from a local OCI layout or `docker save` archive.
#[derive(Debug, Clone, Default)]
pub struct LocalRequest<'a> {
    /// `org.opencontainers.image.ref.name` (or containerd name) to pick in the source.
    pub source_ref: Option<&'a str>,
    /// Platforms to convert; empty means the host's.
    pub platforms: &'a [Platform],
    /// Name to record in `refs.json`.
    pub tag: Option<&'a str>,
}

/// Resolve, verify, convert and commit under the store's shared lock.
pub fn convert_local(store: &Store, path: &Path, req: &LocalRequest, opts: &ConvertOptions) -> Result<Output> {
    let _lock = store.lock_shared()?;
    let source = LocalSource::detect(path)?;
    let host = [Platform::host()];
    let platforms = if req.platforms.is_empty() {
        &host[..]
    } else {
        req.platforms
    };
    let resolved = resolve_local(store, &source, req.source_ref, platforms)?;
    let out = convert_resolved(store, &resolved, None, opts)?;
    if let Some(tag) = req.tag {
        store.set_ref(tag, &out.digest)?;
    }
    Ok(out)
}
