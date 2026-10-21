//! Chunking a vmkit bundle directory into the store (design §5.5, push side).

use std::fs::{self, File};
use std::io::Read;
use std::path::Path;

use kiln_oci::media::OCI_MANIFEST;
use kiln_oci::{Descriptor, ImageManifest, canonical_json};
use kiln_store::{Digest, MAX_METADATA_BLOB, Store};

use crate::error::{Result, SnapshotError};
use crate::format::{
    BUNDLE_JSON, ChunkedFile, Limits, SCHEMA_VERSION, SNAPSHOT_ARTIFACT, SNAPSHOT_CHUNK, SNAPSHOT_CONFIG,
    SnapshotConfig, check_bundle, check_name,
};

/// The largest `bundle.json` read. vmkit writes a few hundred bytes.
pub const MAX_BUNDLE_JSON: u64 = 64 << 10;

/// A snapshot written to the store (untagged).
#[derive(Debug, Clone)]
pub struct Built {
    pub manifest_digest: Digest,
    pub config: SnapshotConfig,
    pub manifest: ImageManifest,
}

/// Opens `path` without following a symlink and requires a regular file. The
/// bundle's files were written by a VMM, which may plant links to host files
/// (design §4.2), and whatever is read here may be pushed.
pub(crate) fn open_regular(path: &Path) -> Result<(File, u64)> {
    let bundle_err = |reason: String| SnapshotError::Bundle {
        path: path.to_path_buf(),
        reason,
    };
    let file = open_nofollow(path).map_err(|e| bundle_err(e.to_string()))?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(bundle_err("not a regular file".into()));
    }
    Ok((file, meta.len()))
}

/// `O_NONBLOCK` so that opening a planted FIFO cannot hang; it is then refused
/// as not a regular file.
#[cfg(unix)]
fn open_nofollow(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

#[cfg(not(unix))]
fn open_nofollow(path: &Path) -> std::io::Result<File> {
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(std::io::Error::other("is a symlink"));
    }
    File::open(path)
}

/// Reads and parses `bundle.json`: at most [`MAX_BUNDLE_JSON`] bytes, `version` 1.
fn read_bundle_json(dir: &Path) -> Result<serde_json::Value> {
    let (file, size) = open_regular(&dir.join(BUNDLE_JSON))?;
    if size > MAX_BUNDLE_JSON {
        return Err(SnapshotError::LimitExceeded {
            what: "bundle.json size",
            max: MAX_BUNDLE_JSON,
        });
    }
    let mut bytes = Vec::new();
    file.take(MAX_BUNDLE_JSON + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BUNDLE_JSON {
        return Err(SnapshotError::LimitExceeded {
            what: "bundle.json size",
            max: MAX_BUNDLE_JSON,
        });
    }
    let bundle: serde_json::Value = serde_json::from_slice(&bytes).map_err(|source| SnapshotError::Json {
        what: "bundle.json",
        source,
    })?;
    check_bundle(&bundle)?;
    Ok(bundle)
}

/// The bundle's files but `bundle.json`, sorted by name. Each must be a regular
/// file with a valid name; anything else (a link, a directory) is an error.
fn list_files(dir: &Path, limits: &Limits) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| SnapshotError::BadName(name.to_string_lossy().into_owned()))?;
        if name == BUNDLE_JSON {
            continue;
        }
        check_name(name)?;
        if !entry.file_type()?.is_file() {
            return Err(SnapshotError::Bundle {
                path: entry.path(),
                reason: "not a regular file".into(),
            });
        }
        names.push(name.to_string());
        if names.len() > limits.max_files {
            return Err(SnapshotError::LimitExceeded {
                what: "files in a snapshot",
                max: limits.max_files as u64,
            });
        }
    }
    names.sort();
    Ok(names)
}

/// The fewest config bytes a chunk takes (`null,`).
const MIN_ENTRY_BYTES: u64 = 5;
/// The most distinct chunks whose layer descriptors fit a manifest a reader accepts (each
/// descriptor takes about 150 bytes of canonical JSON).
const MAX_LAYERS: usize = (MAX_METADATA_BLOB / 160) as usize;

/// Fills `buf` from `r`; fewer bytes only at the end of the stream.
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// Chunks the bundle in `dir` into `store` and writes its config and manifest
/// (design §5.2). Zero chunks become `null` and are never stored; identical
/// chunks are stored once; the layers are the distinct chunks in first-use order.
/// The same bundle and chunk size always give the same manifest digest.
///
/// Nothing is tagged: a caller that tags the result holds `store.lock_shared()`
/// across both, so GC cannot remove the blobs in between.
pub fn chunk_bundle(store: &Store, dir: &Path, chunk_size: u64) -> Result<Built> {
    let limits = Limits::default();
    limits.check_chunk_size(chunk_size)?;
    let _lock = store.lock_shared()?;
    let bundle = read_bundle_json(dir)?;
    let names = list_files(dir, &limits)?;
    let mut buf = vec![0u8; chunk_size as usize];
    // Compared with `==`, a memcmp: fast even in debug builds.
    let zeros = vec![0u8; chunk_size as usize];
    // Every bound a reader applies is checked before anything is read or stored: the file
    // sizes, their total, and whether the config and manifest could fit at all.
    let mut opened = Vec::with_capacity(names.len());
    let (mut total, mut entries) = (0u64, 0u64);
    for name in names {
        let path = dir.join(&name);
        let (file, size) = open_regular(&path)?;
        if size > limits.max_file_size {
            return Err(SnapshotError::LimitExceeded {
                what: "snapshot file size",
                max: limits.max_file_size,
            });
        }
        total = total.saturating_add(size);
        entries = entries.saturating_add(size.div_ceil(chunk_size));
        opened.push((name, path, file, size));
    }
    if total > limits.max_total_size {
        return Err(SnapshotError::LimitExceeded {
            what: "snapshot total size",
            max: limits.max_total_size,
        });
    }
    // Each chunk is at least a `null` in the config.
    if entries.saturating_mul(MIN_ENTRY_BYTES) > limits.max_config {
        return Err(SnapshotError::LimitExceeded {
            what: "chunks per snapshot (use a larger --chunk-size)",
            max: limits.max_config / MIN_ENTRY_BYTES,
        });
    }
    let mut layers: Vec<Descriptor> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut files = Vec::with_capacity(opened.len());
    for (name, path, file, size) in opened {
        // Read the size fstat reported, and no more: a file that changes while
        // it is read is an error rather than a torn snapshot.
        let mut r = file.take(size);
        let mut chunks = Vec::with_capacity(size.div_ceil(chunk_size) as usize);
        let mut left = size;
        while left > 0 {
            let want = left.min(chunk_size) as usize;
            let got = read_full(&mut r, &mut buf[..want])?;
            if got != want {
                return Err(SnapshotError::Bundle {
                    path,
                    reason: "file shrank while it was read".into(),
                });
            }
            left -= want as u64;
            let bytes = &buf[..want];
            if bytes == &zeros[..want] {
                chunks.push(None);
                continue;
            }
            let d = Digest::of(bytes);
            if !seen.contains(&d) && layers.len() >= MAX_LAYERS {
                return Err(SnapshotError::LimitExceeded {
                    what: "distinct chunks per snapshot (use a larger --chunk-size)",
                    max: MAX_LAYERS as u64,
                });
            }
            store.put_bytes(bytes)?;
            if seen.insert(d.clone()) {
                layers.push(Descriptor::new(SNAPSHOT_CHUNK, d.clone(), want as u64));
            }
            chunks.push(Some(d));
        }
        files.push(ChunkedFile { name, size, chunks });
    }
    let config = SnapshotConfig {
        schema_version: SCHEMA_VERSION,
        chunk_size,
        bundle,
        files,
    };
    // What a reader would refuse is not written (the total size, for one).
    config.check(&layers, &limits)?;
    let config_bytes = canonical_json(&config);
    if config_bytes.len() as u64 > limits.max_config {
        return Err(SnapshotError::LimitExceeded {
            what: "snapshot config size (use a larger --chunk-size)",
            max: limits.max_config,
        });
    }
    let config_digest = store.put_bytes(&config_bytes)?;
    let manifest = ImageManifest {
        schema_version: 2,
        media_type: Some(OCI_MANIFEST.to_string()),
        artifact_type: Some(SNAPSHOT_ARTIFACT.to_string()),
        config: Descriptor::new(SNAPSHOT_CONFIG, config_digest, config_bytes.len() as u64),
        layers,
        annotations: None,
    };
    let manifest_bytes = canonical_json(&manifest);
    if manifest_bytes.len() as u64 > MAX_METADATA_BLOB {
        return Err(SnapshotError::LimitExceeded {
            what: "snapshot manifest size (use a larger --chunk-size)",
            max: MAX_METADATA_BLOB,
        });
    }
    let manifest_digest = store.put_bytes(&manifest_bytes)?;
    Ok(Built {
        manifest_digest,
        config,
        manifest,
    })
}
