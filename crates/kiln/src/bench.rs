//! `kiln bench`: the spec §1 performance targets, measured from a local OCI layout.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use kiln_image::{ConvertOptions, LocalRequest, convert_local};
use kiln_oci::media::{OCI_INDEX, OCI_LAYER_GZIP, OCI_MANIFEST};
use kiln_oci::{Descriptor, ImageIndex, LocalSource, Platform, canonical_json, resolve_local};
use kiln_store::{Digest, Store};
use serde_json::json;

/// Default size of the synthetic changed top layer (spec §1: "~50 MB uncompressed").
pub const CHANGED_TOP_BYTES: usize = 50 << 20;

const TARGET_WARM_MS: u128 = 200;
const TARGET_CHANGED_MS: u128 = 2000;
const TARGET_COLD_MS: u128 = 5000;

/// Incompressible, deterministic bytes (xorshift64).
fn noise(len: usize) -> Vec<u8> {
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut v = Vec::with_capacity(len + 8);
    while v.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.extend_from_slice(&x.to_le_bytes());
    }
    v.truncate(len);
    v
}

fn write_blob(dir: &Path, bytes: &[u8]) -> Result<Digest> {
    let d = Digest::of(bytes);
    fs::write(dir.join("blobs/sha256").join(d.hex()), bytes)?;
    Ok(d)
}

/// A copy of the (first) resolved image with one new ~50 MB gzip layer on top.
fn derive_changed_top(
    store: &Store,
    src: &Path,
    platforms: &[Platform],
    top_bytes: usize,
    out: &Path,
) -> Result<PathBuf> {
    let img = resolve_local(store, &LocalSource::detect(src)?, None, platforms)?.remove(0);
    fs::create_dir_all(out.join("blobs/sha256"))?;
    fs::write(out.join("oci-layout"), br#"{"imageLayoutVersion":"1.0.0"}"#)?;
    for l in &img.manifest.layers {
        fs::copy(
            store.blob_path(&l.digest),
            out.join("blobs/sha256").join(l.digest.hex()),
        )?;
    }
    let mut tar = tar::Builder::new(Vec::new());
    let mut h = tar::Header::new_ustar();
    h.set_path("kiln-bench/data")?;
    h.set_size(top_bytes as u64);
    h.set_mode(0o644);
    h.set_uid(0);
    h.set_gid(0);
    h.set_mtime(1_700_000_000);
    h.set_cksum();
    tar.append(&h, noise(top_bytes).as_slice())?;
    let tar = tar.into_inner()?;
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(&tar)?;
    let blob = gz.finish()?;

    let mut config = img.config.clone();
    config.rootfs.diff_ids.push(Digest::of(&tar));
    let config_bytes = canonical_json(&config);
    let mut manifest = img.manifest.clone();
    manifest.config = Descriptor::new(
        &manifest.config.media_type,
        write_blob(out, &config_bytes)?,
        config_bytes.len() as u64,
    );
    manifest.layers.push(Descriptor::new(
        OCI_LAYER_GZIP,
        write_blob(out, &blob)?,
        blob.len() as u64,
    ));
    let manifest_bytes = canonical_json(&manifest);
    let mut top = Descriptor::new(
        OCI_MANIFEST,
        write_blob(out, &manifest_bytes)?,
        manifest_bytes.len() as u64,
    );
    top.platform = Some(img.platform.clone());
    let index = ImageIndex {
        schema_version: 2,
        media_type: Some(OCI_INDEX.into()),
        artifact_type: None,
        manifests: vec![top],
        annotations: None,
    };
    fs::write(out.join("index.json"), canonical_json(&index))?;
    Ok(out.to_path_buf())
}

fn timed<T>(f: impl FnOnce() -> Result<T>) -> Result<(T, u128)> {
    let start = Instant::now();
    let v = f()?;
    Ok((v, start.elapsed().as_millis()))
}

/// Cold, warm and changed-top-layer conversions into a fresh temporary store.
pub fn run(src: &Path, platforms: &[Platform], opts: &ConvertOptions, top_bytes: usize) -> Result<serde_json::Value> {
    let work = tempfile::tempdir()?;
    let store = Store::open(work.path().join("store"))?;
    let host = [Platform::host()];
    let platforms = if platforms.is_empty() { &host[..] } else { platforms };
    let platforms = &platforms[..1];
    let req = LocalRequest {
        source_ref: None,
        platforms,
        tag: Some("bench"),
    };
    let (out, cold) = timed(|| Ok(convert_local(&store, src, &req, opts)?)).context("cold convert")?;
    let ((), warm) = timed(|| Ok(convert_local(&store, src, &req, opts).map(drop)?)).context("warm convert")?;
    let derived = derive_changed_top(&store, src, platforms, top_bytes, &work.path().join("derived"))?;
    let ((), changed) =
        timed(|| Ok(convert_local(&store, &derived, &req, opts).map(drop)?)).context("changed-top convert")?;
    Ok(json!({
        "platform": platforms[0].to_string(),
        "layers": out.images[0].layers.len(),
        "cold_ms": cold,
        "warm_ms": warm,
        "changed_top_ms": changed,
        "changed_top_bytes": top_bytes,
        "targets": { "cold_ms": TARGET_COLD_MS, "warm_ms": TARGET_WARM_MS, "changed_top_ms": TARGET_CHANGED_MS },
        "met": { "cold": cold < TARGET_COLD_MS, "warm": warm < TARGET_WARM_MS, "changed_top": changed < TARGET_CHANGED_MS },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noise_is_deterministic_and_incompressible() {
        let a = noise(1 << 16);
        assert_eq!(a, noise(1 << 16));
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&a).unwrap();
        assert!(gz.finish().unwrap().len() > a.len() * 9 / 10);
    }
}
