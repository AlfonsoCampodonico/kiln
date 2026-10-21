//! Writing a loaded snapshot out as a bundle directory: complete (eager pull), or
//! `bundle.json` plus the chunk index `lazy.json` (lazy pull, design §6.2).

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use kiln_oci::canonical_json;
use kiln_store::{Digest, Store, StoreError};
use serde::{Deserialize, Serialize};

use crate::error::{Result, SnapshotError};
use crate::format::{BUNDLE_JSON, LAZY_JSON, check_name};
use crate::load::Loaded;

/// The vmkit lazy index format `lazy.json` carries.
pub const LAZY_VERSION: u32 = 1;

/// `lazy.json` (vmkit lazy index format 1): the chunks of every file the bundle
/// directory does not hold in full. Keys are snake_case, as in `bundle.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LazyIndex {
    pub chunk_size: u64,
    pub files: BTreeMap<String, LazyFile>,
    pub version: u32,
}

/// One lazily served file: its size and chunks (`None`: all zero).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LazyFile {
    pub chunks: Vec<Option<Digest>>,
    pub size: u64,
}

impl LazyIndex {
    /// The index of every file of `loaded`.
    pub fn of(loaded: &Loaded) -> Self {
        Self {
            chunk_size: loaded.config.chunk_size,
            files: loaded
                .config
                .files
                .iter()
                .map(|f| {
                    let file = LazyFile {
                        chunks: f.chunks.clone(),
                        size: f.size,
                    };
                    (f.name.clone(), file)
                })
                .collect(),
            version: LAZY_VERSION,
        }
    }
}

/// Creates `dest` if needed and requires it to be an empty directory, so a pull
/// never mixes its files with others.
fn empty_dest(dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)?;
    if fs::read_dir(dest)?.next().is_some() {
        return Err(SnapshotError::DestNotEmpty(dest.to_path_buf()));
    }
    Ok(())
}

/// A new file in `dir`; `create_new` (`O_EXCL`) never follows or reuses an entry.
fn create(dir: &Path, name: &str) -> Result<File> {
    // Checked again here: `Loaded`'s fields are public.
    if name != BUNDLE_JSON && name != LAZY_JSON {
        check_name(name)?;
    }
    Ok(OpenOptions::new().write(true).create_new(true).open(dir.join(name))?)
}

fn write_new(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let mut f = create(dir, name)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

/// `bundle.json` from the config, pretty-printed with sorted keys.
fn write_bundle_json(loaded: &Loaded, dest: &Path) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(&loaded.config.bundle).expect("json value serializes");
    bytes.push(b'\n');
    write_new(dest, BUNDLE_JSON, &bytes)
}

/// Writes the complete bundle into `dest` (created if missing, else it must be
/// empty). Zero chunks stay holes: each file is sized first and only its data
/// chunks are written. Every chunk must be in the store. `bundle.json` is
/// written last, so a bundle left by a failure is never taken for a whole one.
pub fn assemble(store: &Store, loaded: &Loaded, dest: &Path) -> Result<()> {
    empty_dest(dest)?;
    let chunk_size = loaded.config.chunk_size;
    for f in &loaded.config.files {
        let mut out = create(dest, &f.name)?;
        out.set_len(f.size)?;
        for (i, c) in f.chunks.iter().enumerate() {
            let Some(d) = c else { continue };
            let len = f.chunk_len(i, chunk_size);
            let blob = store.open_blob(d)?;
            let actual = blob.metadata()?.len();
            if actual != len {
                return Err(StoreError::SizeMismatch {
                    digest: d.clone(),
                    expected: len,
                    actual,
                }
                .into());
            }
            out.seek(SeekFrom::Start(i as u64 * chunk_size))?;
            io::copy(&mut blob.take(len), &mut out)?;
        }
        out.sync_all()?;
    }
    write_bundle_json(loaded, dest)
}

/// Writes `bundle.json` and `lazy.json` into `dest` (created if missing, else it
/// must be empty): a bundle whose files are all served on demand (design §6.2).
pub fn write_lazy(loaded: &Loaded, dest: &Path) -> Result<()> {
    empty_dest(dest)?;
    write_new(dest, LAZY_JSON, &canonical_json(&LazyIndex::of(loaded)))?;
    write_bundle_json(loaded, dest)
}
