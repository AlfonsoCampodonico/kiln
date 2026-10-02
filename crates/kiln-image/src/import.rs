//! `kiln import --from-store`: copy a kiln image between stores, verifying every blob.

use kiln_oci::{ImageIndex, ImageManifest};
use kiln_store::{Digest, Store};

use crate::error::{ImageError, Result, json};
use crate::load::load;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportReport {
    pub digest: Digest,
    pub blobs_copied: usize,
    pub bytes_copied: u64,
}

fn copy(dst: &Store, src: &Store, d: &Digest, size: Option<u64>, report: &mut ImportReport) -> Result<()> {
    if dst.has_blob(d) {
        return Ok(());
    }
    report.bytes_copied += dst.put_verified(&mut src.open_blob(d)?, d, size)?;
    report.blobs_copied += 1;
    Ok(())
}

/// Copies `name` (a ref or digest in `src`) into `dst` and tags it `as_name`.
/// Metadata is parsed only after it is verified into `dst`. Imported erofs layers
/// never enter the conversion caches (spec §6.2).
pub fn import_image(dst: &Store, src: &Store, name: &str, as_name: &str) -> Result<ImportReport> {
    let _lock = dst.lock_shared()?;
    let top = match Digest::parse(name) {
        Ok(d) => d,
        Err(_) => src
            .get_ref(name)?
            .ok_or_else(|| ImageError::RefNotFound(name.to_string()))?,
    };
    let mut report = ImportReport {
        digest: top.clone(),
        blobs_copied: 0,
        bytes_copied: 0,
    };
    copy(dst, src, &top, None, &mut report)?;
    let top_bytes = dst.read_metadata(&top)?;
    let manifests: Vec<Digest> = match serde_json::from_slice::<ImageIndex>(&top_bytes) {
        Ok(index) => {
            for m in &index.manifests {
                copy(dst, src, &m.digest, Some(m.size), &mut report)?;
            }
            index.manifests.into_iter().map(|m| m.digest).collect()
        }
        Err(_) => vec![top.clone()],
    };
    for d in &manifests {
        let m: ImageManifest = json("kiln manifest", &dst.read_metadata(d)?)?;
        copy(dst, src, &m.config.digest, Some(m.config.size), &mut report)?;
        for l in &m.layers {
            copy(dst, src, &l.digest, Some(l.size), &mut report)?;
        }
    }
    // Refuses to tag anything but a kiln image; copied blobs stay unreferenced for GC.
    load(dst, &top)?;
    dst.set_ref(as_name, &top)?;
    Ok(report)
}
