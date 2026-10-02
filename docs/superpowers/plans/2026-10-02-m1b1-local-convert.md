# M1b-1: Local convert Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `kiln convert` turns a local OCI image layout or `docker save` archive into a verified, cached, deterministic kiln image, natively on macOS. It comes with `import`, `inspect`, `ls`, `gc` and `bench`.

**Architecture:**
- **`kiln-store`** is the content-addressed store from spec §5.2:
  - blobs that enter only by verified digest;
  - three conversion caches;
  - `refs.json`;
  - a shared/exclusive store lock;
  - mark-from-refs GC.
- **`kiln-oci`** reads OCI layouts and both `docker save` formats. It hashes every blob into the store and never trusts file names or index files.
- **`kiln-image`** runs the pipeline from spec §6.1–§6.4:
  - Phase A (parallel): bounded decompression and `diff_id` verification into M1a's `LayerWriter`.
  - Phase B (bottom-up): inherited-parent resolution and commit.
  - Then squash, and the provisional kiln manifest. It also loads and imports images.
- **`kiln`** is the CLI. Every image-supplied string it prints goes through one sanitiser.

**Tech Stack:**
- Rust 2024 edition, in the existing Cargo workspace.
- New runtime dependencies:
  - `serde` + `serde_json` (without `preserve_order`), `sha2`, `thiserror`, `tempfile`
  - `tar` (archive inputs), `flate2`, `zstd`
  - `clap` (derive) and `anyhow` (CLI only)
- New dev dependencies: `assert_cmd`, `predicates`.

**Spec:** `docs/superpowers/specs/2026-09-30-kiln-design.md` (rev 2.1). This plan implements the **local-input half of M1b**:
- §5.1 (provisional manifest without kernel and init layers);
- §5.2;
- §6.1–§6.4 and §6.6 for local inputs;
- §7.6's per-image and ratio limits;
- the local items of §11.5;
- T1, T3 and T8 for those paths;
- the CLI commands `convert`, `import`, `inspect`, `ls`, `gc` and `bench`.

Plan **M1b-2** (separate) adds `kiln-registry`, `pull`/`push`, registry inputs to `convert`, T2, and the network half of the hostile-registry tests. M1a's carry-forward items are each either done here (Tasks 5, 6, 8, 10) or ruled into M1b-2 (see Design decisions).

## Global Constraints

- **Repository:** `/Users/alfonso/Github/Personal/playground/kiln` (GitHub `AlfonsoCampodonico/kiln`, private), Apache-2.0. Work on a branch, never on `main`.
- **Crates:** `crates/kiln-store`, `crates/kiln-oci`, `crates/kiln-image` (libraries) and `crates/kiln` (the `kiln` binary). Every crate root has `#![forbid(unsafe_code)]`.
- **Store layout (spec §5.2), exactly:**
  - `$KILN_HOME` (default `~/.local/share/kiln`) contains `lock`, `refs.lock`, `refs.json`, `blobs/sha256/<hex>`, `tmp/`, and the three caches below.
  - `cache/layers/<src>@<fmt>` holds `erofs <digest>` or `parents <json>`.
  - `cache/layers-ctx/<src>@<fmt>@<ctx>` and `cache/squash/<hex>@<fmt>` each hold a bare digest.
  - `:` in a key becomes `_` in the file name.
- **Media types:**
  - Artifact type: `application/vnd.kiln.image.v1`.
  - Config: `application/vnd.kiln.image.config.v1+json`.
  - App layers: `application/vnd.kiln.layer.v1.erofs`.
  - Reserved for M3: `application/vnd.kiln.kernel.v1` and `application/vnd.kiln.init.v1.erofs`.
  - Annotations: `dev.kiln.source.digests` and `dev.kiln.inherits`.
- **Default limits:**

  | Limit | Default |
  |---|---|
  | Uncompressed bytes per layer / per image | 16 GiB / 64 GiB |
  | Expansion ratio | 200 (1 MiB floor for tiny compressed blobs) |
  | Entries per layer | 2,000,000 |
  | PAX record, long name or link, single xattr value | 1 MiB |
  | Path length / depth | 4096 bytes / 256 components |
  | Metadata blob (manifest, index, config) read into memory | 4 MiB |
  | `--max-layers` | 10 |

- **T1:**
  - A blob enters the store only after its full content hashes to the expected digest.
  - A cache entry is written only after its blob is committed, and only for layers whose decompressed digest matched `diff_ids[i]` and whose remainder after the tar end was zero padding.
  - Annotations never populate caches.
  - Imported or pulled erofs layers never enter the caches and are never squashed.
- **T3:** every limit above is enforced while streaming. Exceeding one is a typed error naming the limit.
- **T8:** every image-supplied string the CLI prints passes through `sanitize::clean` or `clean_line`. JSON output relies on serde escaping.
- **Locking:** `convert_local` and `import_image` hold the shared store lock from the first blob write until the ref is written. `gc` takes the exclusive lock.
- **Determinism (spec §6.6):**
  - JSON is serialised with sorted keys and no whitespace, through `serde_json::Value`. No crate may enable serde_json's `preserve_order`.
  - Local inputs omit `source.reference`.
  - Output must not depend on `--jobs` or on the store.
- **Portability:** everything here builds and passes its tests natively on macOS and Linux, without root. Kernel-level checks (Task 5's re-run, Task 10's script) use Docker or CI.
- **Formatting:** `rustfmt.toml` sets `max_width = 120`. The plan's code is already `cargo fmt`-clean; run `cargo fmt --all` before every commit anyway. CI runs `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test --all` on ubuntu-24.04 and macos-15.
- **Validation:** this plan's code was built and tested before the plan was written. Every task was replayed in order on a fresh clone of `main`, with fmt, clippy (`-D warnings`) and the full workspace tests passing after each one. The test counts in "Expected" lines come from that replay.
- **Commits:** end each commit message with these trailer lines (the commit steps pass them as a second `-m`):
  ```
  Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01P7rdQAESQYNEo2qFobUNxt
  ```

## Review Focus

These are inputs that no task's main tests target but that real users will hit. Each has a pinned test in the task named.

1. **`docker save` from Docker 25+** writes an attestation manifest next to the image in `index.json`. Without care, every export looks like two images and fails with "choose one with --ref". It must resolve to the one image. Pinned in Task 4 (`skips_docker_attestation_manifests_beside_the_image`).
2. **Tars with blank numeric header fields** (written by some non-Go tools) are accepted by containerd. They must convert, with the fields read as 0. Pinned in Task 5 (`empty_numeric_fields_read_as_zero_like_go`).
3. **`kiln gc` while a convert is running in another terminal** must not delete committed-but-untagged blobs. GC waits for the shared lock. Pinned in Task 2 (`gc_waits_for_shared_lock_holders`).
4. **The same image converted twice at once** (two terminals, or CI jobs sharing a store): both must succeed with identical digests, and no `refs.json` update may be lost. Pinned in Task 6 (`concurrent_converts_into_one_store_agree`).
5. **One bad layer among good ones**, for example a corrupted download in a multi-layer image: the convert must fail with no cache entry and no ref, even for the good layers that converted in parallel. A retry after fixing the input must start clean. Pinned in Task 6 (`a_failing_layer_leaves_no_entry_for_its_good_siblings_either`).

## Design decisions (rulings made while planning)

- **CLI diagnostics use `anyhow`, not `miette`** (spec §13 names `miette`). Every printed string must go through one sanitiser, and miette renders sources, labels and snippets itself. One line, `kiln: error: <chain>`, sanitised, is enforced in a single place. Revisit in M3 if remediation hints need richer rendering.
- **Trailing data after the tar end-of-archive marker** must be zero padding (spec §11.5 "trailing junk"). containerd ignores it. This is documented as format.md divergence 5.
- **A `parents` cache hit with a `layers-ctx` miss** (the layer is cached but its base changed) streams that layer sequentially in phase B. Fresh layers still stream in parallel in phase A. This keeps the warm path free of speculative work.
- **GC frees source OCI blobs**, because kiln images do not reference them. A re-convert re-verifies them from the source and then hits the layer cache.
- **Defaults:** `--platform` defaults to the host's (`linux/arm64` on Apple Silicon). `--tag` defaults to `<file stem>:latest`.
- **Carry-forward items ruled into M1b-2:**
  - The STAR-trailer 131-byte prefix and ambiguous GNU prefix tar differentials. They need hostile-tar fixtures, which M1b-2's hostile-registry suite builds.
  - The nightly Docker Hub job.

## File Structure

```
crates/kiln-store/            content-addressed store (Tasks 1–2)
  src/lib.rs                  Store: open, locks, blobs, verified ingest
  src/digest.rs               Digest, Hasher, HashingReader
  src/error.rs                StoreError
  src/cache.rs                CacheKind, cache_get/put/get_blob
  src/refs.rs                 refs.json, check_ref_name
  src/gc.rs                   references, live_blobs, gc
crates/kiln-oci/              OCI types and local inputs (Tasks 3–4)
  src/types.rs                Descriptor, ImageIndex, ImageManifest, ImageConfig, canonical_json
  src/media.rs                media types, Compression, layer policy
  src/platform.rs             Platform
  src/source.rs               BlobSource, DirLayout, TarArchive
  src/resolve.rs              LocalSource, ResolvedImage, resolve_local
  src/testlayout.rs           (doc-hidden) fixture builders
  tests/resolve.rs
crates/kiln-erofs/            M1a crate; Task 5 edits merge.rs and tarstream.rs
  tests/inherit_scale.rs
crates/kiln-image/            the pipeline (Tasks 6–7)
  src/types.rs                KilnConfig, Process, media-type constants
  src/decompress.rs           Bounded readers, Budget, layer_limit
  src/ctx.rs                  parents entries, ctx hash
  src/convert.rs              ConvertOptions, convert_image (phases A/B, squash)
  src/pipeline.rs             convert_resolved, convert_local, LocalRequest, Output
  src/load.rs                 load, load_manifest, resolve_name
  src/import.rs               import_image
  tests/common/mod.rs, tests/convert.rs, tests/hostile.rs, tests/import.rs
crates/kiln/                  the CLI (Tasks 8–9)
  src/main.rs                 clap commands
  src/sanitize.rs             T8
  src/bench.rs                kiln bench
  tests/cli.rs
docs/format.md                + kiln image schema 1, store keys, divergences (Task 10)
README.md, scripts/compare-export.sh, scripts/compare_tree.py (Task 10)
```

---


### Task 1: `kiln-store`: digests and the verified blob store

**Files:**
- Create: `crates/kiln-store/Cargo.toml`, `crates/kiln-store/src/error.rs`, `crates/kiln-store/src/digest.rs`, `crates/kiln-store/src/lib.rs`

**Interfaces:**
- Consumes: nothing (new crate).
- Produces:
  - `kiln_store::{Digest, Hasher, HashingReader, Store, StoreLock, TmpBlob, StoreError, Result, MAX_METADATA_BLOB}`.
  - `Digest`: `parse(&str)` (only `sha256:` + 64 lowercase hex), `of(&[u8])`, `hex() -> &str`; `Display`/serde as the `sha256:<hex>` string; `Ord`, `Hash`, `Clone`.
  - `Hasher` (`new`, `update`, `finish -> Digest`, `impl Write`); `HashingReader<R>` (`new`, `count`, `finish_to_eof() -> io::Result<(Digest, u64)>`, `impl Read`).
  - `Store::open(root)` (creates `blobs/sha256`, `cache/{layers,layers-ctx,squash}`, `tmp`), `open_read_only(root)` (creates nothing), `default_root()` (`$KILN_HOME`, else `~/.local/share/kiln`), `root()`, `tmp_dir()`, `lock_shared()`, `lock_exclusive()`.
  - Blobs: `blob_path`, `has_blob` (regular file only), `open_blob`, `blob_size`, `read_metadata` (refuses > 4 MiB), `tmp_blob() -> TmpBlob` (`reopen()` gives a read+write `File`; `file_mut()`), `commit(TmpBlob) -> Digest`, `put_bytes`, `put_reader -> (Digest, u64)`, `put_verified(r, &expected, Option<size>) -> u64`, `list_blobs`.
  - `StoreError` variants: `Io`, `BadDigest`, `DigestMismatch { expected, actual }`, `SizeMismatch { digest, expected, actual }`, `NotFound`, `TooLarge { digest, size, max }`, `Invalid { what, value }`, `Corrupt { path, reason }`.

- [ ] **Step 1: Create the crate manifest**

The workspace `Cargo.toml` already lists `crates/*`, so a new directory is picked up automatically.

`crates/kiln-store/Cargo.toml`:
```toml
[package]
name = "kiln-store"
version = "0.1.0"
edition.workspace = true
license.workspace = true
description = "Content-addressed blob store, caches and refs for kiln"

[dependencies]
serde = { version = "1.0.229", features = ["derive"] }
serde_json = "1.0.151"
sha2 = "0.11.0"
tempfile = "3.27.0"
thiserror = "2.0.21"
```

- [ ] **Step 2: Write the error type**

`crates/kiln-store/src/error.rs`:
```rust
use thiserror::Error;

use crate::Digest;

/// Errors from the kiln store.
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid digest {0:?} (expected sha256:<64 lowercase hex>)")]
    BadDigest(String),
    #[error("digest mismatch: expected {expected}, got {actual}")]
    DigestMismatch { expected: Digest, actual: Digest },
    #[error("size mismatch for {digest}: expected {expected} bytes, got {actual}")]
    SizeMismatch { digest: Digest, expected: u64, actual: u64 },
    #[error("blob {0} is not in the store")]
    NotFound(Digest),
    #[error("blob {digest} is {size} bytes, more than the {max}-byte limit for metadata")]
    TooLarge { digest: Digest, size: u64, max: u64 },
    #[error("invalid {what} {value:?}")]
    Invalid { what: &'static str, value: String },
    #[error("corrupt store file {path}: {reason}")]
    Corrupt { path: String, reason: String },
}

pub type Result<T> = std::result::Result<T, StoreError>;
```

- [ ] **Step 3: Write digests and hashing readers, with their unit tests**

`finish_to_eof` reads the source to its end before returning, so trailing bytes are always hashed (spec §6.1 step 3).

`crates/kiln-store/src/digest.rs`:
```rust
//! SHA-256 content digests (`sha256:<hex>`), the only algorithm kiln accepts.

use std::fmt;
use std::io::{self, Read, Write};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest as _, Sha256};

use crate::error::{Result, StoreError};

/// A SHA-256 digest, displayed and parsed as `sha256:<64 lowercase hex>`.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Digest {
    hex: String,
}

impl Digest {
    pub fn parse(s: &str) -> Result<Self> {
        let hex = s
            .strip_prefix("sha256:")
            .ok_or_else(|| StoreError::BadDigest(s.to_string()))?;
        if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            return Err(StoreError::BadDigest(s.to_string()));
        }
        Ok(Self { hex: hex.to_string() })
    }

    /// The digest of `bytes`.
    pub fn of(bytes: &[u8]) -> Self {
        let mut h = Hasher::new();
        h.update(bytes);
        h.finish()
    }

    pub fn hex(&self) -> &str {
        &self.hex
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sha256:{}", self.hex)
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sha256:{}", self.hex)
    }
}

impl Serialize for Digest {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Digest::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// Incremental SHA-256.
#[derive(Clone, Default)]
pub struct Hasher(Sha256);

impl Hasher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    pub fn finish(self) -> Digest {
        let out = self.0.finalize();
        Digest {
            hex: out.iter().map(|b| format!("{b:02x}")).collect(),
        }
    }
}

impl Write for Hasher {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A reader that hashes and counts every byte it passes through.
pub struct HashingReader<R> {
    inner: R,
    hasher: Hasher,
    count: u64,
}

impl<R: Read> HashingReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            hasher: Hasher::new(),
            count: 0,
        }
    }

    /// Bytes read so far.
    pub fn count(&self) -> u64 {
        self.count
    }

    /// Reads the rest of the stream (so trailing bytes are hashed too) and
    /// returns the digest and total byte count.
    pub fn finish_to_eof(mut self) -> io::Result<(Digest, u64)> {
        io::copy(&mut self, &mut io::sink())?;
        Ok((self.hasher.finish(), self.count))
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        self.count += n as u64;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMPTY: &str = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    #[test]
    fn digest_of_empty_matches_sha256() {
        assert_eq!(Digest::of(b"").to_string(), EMPTY);
    }

    #[test]
    fn parse_round_trips_and_rejects_bad_input() {
        assert_eq!(Digest::parse(EMPTY).unwrap().to_string(), EMPTY);
        for bad in [
            "",
            "sha256:",
            "sha512:00",
            "sha256:E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855",
            &format!("{EMPTY}0"),
            "sha256:../../etc/passwd",
        ] {
            assert!(Digest::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn hashing_reader_hashes_trailing_bytes() {
        let mut r = HashingReader::new(&b"hello world"[..]);
        let mut first = [0u8; 5];
        r.read_exact(&mut first).unwrap();
        let (d, n) = r.finish_to_eof().unwrap();
        assert_eq!((d, n), (Digest::of(b"hello world"), 11));
    }

    #[test]
    fn serde_uses_the_string_form() {
        let d = Digest::of(b"x");
        let json = serde_json::to_string(&d).unwrap();
        assert_eq!(json, format!("\"{d}\""));
        assert_eq!(serde_json::from_str::<Digest>(&json).unwrap(), d);
        assert!(serde_json::from_str::<Digest>("\"sha256:zz\"").is_err());
    }
}
```

- [ ] **Step 4: Write the blob store, with its unit tests**

Every write stages a file in `tmp/`, fsyncs it, then renames it into place. `put_verified` is the only way untrusted bytes enter the store: it hashes to EOF and commits only on a digest (and size) match, leaving nothing behind otherwise (T1).

`crates/kiln-store/src/lib.rs`:
```rust
//! The kiln local store (spec §5.2): content-addressed blobs, conversion caches,
//! the refs index, a store-wide lock and garbage collection.
#![forbid(unsafe_code)]

mod digest;
mod error;

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub use digest::{Digest, Hasher, HashingReader};
pub use error::{Result, StoreError};

/// Largest metadata blob (manifest, index, config) the store will load into memory.
pub const MAX_METADATA_BLOB: u64 = 4 << 20;

/// A store rooted at a directory (`$KILN_HOME`).
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

/// Holds the store-wide lock until dropped.
#[derive(Debug)]
pub struct StoreLock {
    _file: File,
}

/// A blob being written in the store's staging directory.
pub struct TmpBlob {
    file: tempfile::NamedTempFile,
}

impl TmpBlob {
    /// A second read+write handle to the staged file (for writers that take ownership).
    pub fn reopen(&self) -> Result<File> {
        Ok(self.file.reopen()?)
    }

    /// The staged file.
    pub fn file_mut(&mut self) -> &mut File {
        self.file.as_file_mut()
    }
}

impl Store {
    /// Opens (creating if needed) the store at `root`.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        for dir in [
            "blobs/sha256",
            "cache/layers",
            "cache/layers-ctx",
            "cache/squash",
            "tmp",
        ] {
            fs::create_dir_all(root.join(dir))?;
        }
        Ok(Self { root })
    }

    /// Opens an existing store without creating or locking anything (a read-only
    /// mount, for `kiln import --from-store`). Only read methods may be used.
    pub fn open_read_only(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        if !root.join("blobs/sha256").is_dir() {
            return Err(StoreError::Invalid {
                what: "store",
                value: format!("{} has no blobs/sha256 directory", root.display()),
            });
        }
        Ok(Self { root })
    }

    /// `$KILN_HOME`, else `~/.local/share/kiln`.
    pub fn default_root() -> Result<PathBuf> {
        if let Some(home) = std::env::var_os("KILN_HOME") {
            return Ok(PathBuf::from(home));
        }
        let home = std::env::var_os("HOME").ok_or_else(|| StoreError::Invalid {
            what: "environment",
            value: "neither KILN_HOME nor HOME is set".into(),
        })?;
        Ok(PathBuf::from(home).join(".local/share/kiln"))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Staging directory on the same filesystem as the blobs.
    pub fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }

    fn lock_file(&self, name: &str) -> Result<File> {
        Ok(OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.root.join(name))?)
    }

    /// Shared lock: held by convert, import and other writers that GC must not race.
    pub fn lock_shared(&self) -> Result<StoreLock> {
        let file = self.lock_file("lock")?;
        file.lock_shared()?;
        Ok(StoreLock { _file: file })
    }

    /// Exclusive lock: held by GC.
    pub fn lock_exclusive(&self) -> Result<StoreLock> {
        let file = self.lock_file("lock")?;
        file.lock()?;
        Ok(StoreLock { _file: file })
    }

    pub fn blob_path(&self, d: &Digest) -> PathBuf {
        self.root.join("blobs/sha256").join(d.hex())
    }

    /// Whether the blob exists as a regular file.
    pub fn has_blob(&self, d: &Digest) -> bool {
        fs::symlink_metadata(self.blob_path(d)).is_ok_and(|m| m.is_file())
    }

    pub fn open_blob(&self, d: &Digest) -> Result<File> {
        if !self.has_blob(d) {
            return Err(StoreError::NotFound(d.clone()));
        }
        Ok(File::open(self.blob_path(d))?)
    }

    pub fn blob_size(&self, d: &Digest) -> Result<u64> {
        Ok(self.open_blob(d)?.metadata()?.len())
    }

    /// Reads a metadata blob, refusing anything over [`MAX_METADATA_BLOB`].
    pub fn read_metadata(&self, d: &Digest) -> Result<Vec<u8>> {
        let size = self.blob_size(d)?;
        if size > MAX_METADATA_BLOB {
            return Err(StoreError::TooLarge {
                digest: d.clone(),
                size,
                max: MAX_METADATA_BLOB,
            });
        }
        Ok(fs::read(self.blob_path(d))?)
    }

    /// A new staged blob.
    pub fn tmp_blob(&self) -> Result<TmpBlob> {
        Ok(TmpBlob {
            file: tempfile::Builder::new().prefix("blob-").tempfile_in(self.tmp_dir())?,
        })
    }

    /// Hashes a staged blob and moves it into place; returns its digest.
    pub fn commit(&self, tmp: TmpBlob) -> Result<Digest> {
        let mut file = tmp.file;
        file.as_file_mut().flush()?;
        file.as_file_mut().sync_all()?;
        let mut reader = file.reopen()?;
        reader.seek(SeekFrom::Start(0))?;
        let (digest, _) = HashingReader::new(reader).finish_to_eof()?;
        file.persist(self.blob_path(&digest))
            .map_err(|e| StoreError::Io(e.error))?;
        Ok(digest)
    }

    /// Stores `bytes`; returns their digest.
    pub fn put_bytes(&self, bytes: &[u8]) -> Result<Digest> {
        let d = Digest::of(bytes);
        if self.has_blob(&d) {
            return Ok(d);
        }
        let mut tmp = self.tmp_blob()?;
        tmp.file_mut().write_all(bytes)?;
        self.commit(tmp)
    }

    /// Streams `r` to its end into the store; returns its digest and size.
    pub fn put_reader(&self, r: &mut dyn Read) -> Result<(Digest, u64)> {
        let mut tmp = self.tmp_blob()?;
        let size = io::copy(r, tmp.file_mut())?;
        Ok((self.commit(tmp)?, size))
    }

    /// Streams `r` to its end into the store, committing only if its content hashes
    /// to `expected` (and has `expected_size` bytes, when given). Spec §6.1 (T1).
    pub fn put_verified(&self, r: &mut dyn Read, expected: &Digest, expected_size: Option<u64>) -> Result<u64> {
        if self.has_blob(expected) {
            return self.blob_size(expected);
        }
        let mut tmp = self.tmp_blob()?;
        let mut hashing = HashingReader::new(r);
        io::copy(&mut hashing, tmp.file_mut())?;
        let (actual, size) = hashing.finish_to_eof()?;
        if &actual != expected {
            return Err(StoreError::DigestMismatch {
                expected: expected.clone(),
                actual,
            });
        }
        if let Some(want) = expected_size.filter(|&s| s != size) {
            return Err(StoreError::SizeMismatch {
                digest: expected.clone(),
                expected: want,
                actual: size,
            });
        }
        let committed = self.commit(tmp)?;
        debug_assert_eq!(&committed, expected);
        Ok(size)
    }

    /// Every blob digest in the store.
    pub fn list_blobs(&self) -> Result<Vec<Digest>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(self.root.join("blobs/sha256"))? {
            let name = entry?.file_name();
            if let Some(d) = name.to_str().and_then(|n| Digest::parse(&format!("sha256:{n}")).ok()) {
                out.push(d);
            }
        }
        out.sort();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_read_only_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Store::open_read_only(dir.path()).is_err());
        let d = Store::open(dir.path()).unwrap().put_bytes(b"x").unwrap();
        fs::remove_dir(dir.path().join("tmp")).unwrap();
        let ro = Store::open_read_only(dir.path()).unwrap();
        assert_eq!(ro.read_metadata(&d).unwrap(), b"x");
        assert!(!dir.path().join("tmp").exists());
    }

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path().join("home")).unwrap();
        (dir, s)
    }

    #[test]
    fn put_bytes_round_trips() {
        let (_d, s) = store();
        let d = s.put_bytes(b"hello").unwrap();
        assert_eq!(d, Digest::of(b"hello"));
        assert!(s.has_blob(&d));
        assert_eq!(s.read_metadata(&d).unwrap(), b"hello");
        assert_eq!(s.blob_size(&d).unwrap(), 5);
        assert_eq!(s.list_blobs().unwrap(), vec![d]);
    }

    #[test]
    fn put_verified_rejects_wrong_content_and_size_and_leaves_nothing() {
        let (_d, s) = store();
        let want = Digest::of(b"good");
        assert!(matches!(
            s.put_verified(&mut &b"evil"[..], &want, None),
            Err(StoreError::DigestMismatch { .. })
        ));
        assert!(matches!(
            s.put_verified(&mut &b"good"[..], &want, Some(5)),
            Err(StoreError::SizeMismatch { .. })
        ));
        assert!(!s.has_blob(&want));
        assert_eq!(
            fs::read_dir(s.tmp_dir()).unwrap().count(),
            0,
            "staged files are cleaned up"
        );
        assert_eq!(s.put_verified(&mut &b"good"[..], &want, Some(4)).unwrap(), 4);
        assert!(s.has_blob(&want));
    }

    #[test]
    fn put_verified_hashes_bytes_after_a_partial_read() {
        let (_d, s) = store();
        // The whole stream must match: trailing bytes are part of the content.
        let want = Digest::of(b"layer");
        assert!(s.put_verified(&mut &b"layer+trailing junk"[..], &want, None).is_err());
    }

    #[test]
    fn tmp_blob_commit_hashes_what_was_written_through_reopen() {
        let (_d, s) = store();
        let tmp = s.tmp_blob().unwrap();
        let mut f = tmp.reopen().unwrap();
        f.write_all(b"erofs bytes").unwrap();
        drop(f);
        assert_eq!(s.commit(tmp).unwrap(), Digest::of(b"erofs bytes"));
    }

    #[test]
    fn metadata_blobs_are_size_capped() {
        let (_d, s) = store();
        let big = vec![b'{'; MAX_METADATA_BLOB as usize + 1];
        let d = s.put_bytes(&big).unwrap();
        assert!(matches!(s.read_metadata(&d), Err(StoreError::TooLarge { .. })));
    }

    #[test]
    fn missing_blob_is_not_found() {
        let (_d, s) = store();
        assert!(matches!(
            s.open_blob(&Digest::of(b"nope")),
            Err(StoreError::NotFound(_))
        ));
    }

    #[test]
    fn exclusive_lock_excludes_shared() {
        let (_d, s) = store();
        let shared = s.lock_shared().unwrap();
        let f = s.lock_file("lock").unwrap();
        assert!(f.try_lock().is_err(), "exclusive must wait while a shared lock is held");
        drop(shared);
        assert!(f.try_lock().is_ok());
    }
}
```

- [ ] **Step 5: Run the crate's tests**

Run: `cargo test -q -p kiln-store`
Expected: all pass (12 tests).

- [ ] **Step 6: Format, lint and run the whole workspace**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace -q`
Expected: no clippy warnings; every test passes.

- [ ] **Step 7: Commit**

```bash
git add crates/kiln-store Cargo.lock
git commit -m "feat(kiln-store): content-addressed blob store with verified ingest" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P7rdQAESQYNEo2qFobUNxt"
```


### Task 2: `kiln-store`: caches, refs and GC

**Files:**
- Create: `crates/kiln-store/src/cache.rs`, `crates/kiln-store/src/refs.rs`, `crates/kiln-store/src/gc.rs`
- Modify: `crates/kiln-store/src/lib.rs`

**Interfaces:**
- Consumes: Task 1's `Store`, `Digest`, `StoreError`.
- Produces:
  - `CacheKind { Layers, LayersCtx, Squash }`; `Store::cache_get(kind, key) -> Option<String>`, `cache_put(kind, key, value)` (atomic), `cache_get_blob(kind, key) -> Option<Digest>` (a missing blob is a miss). Keys are `[A-Za-z0-9@:._-]`, at most 200 bytes; `:` is stored as `_` in file names.
  - `refs.json` as `{"refs": {name: digest}}`: `Store::refs() -> BTreeMap<String, Digest>`, `get_ref`, `set_ref`, `remove_ref -> bool`, each read-modify-write under `refs.lock`. `check_ref_name(&str)` allows `[A-Za-z0-9._/:@-]`, at most 255 bytes.
  - `references(&serde_json::Value) -> Vec<Digest>` (the `manifests[]`, `config` and `layers[]` digests of an index or manifest); `Store::live_blobs()`; `Store::gc() -> GcReport { blobs_removed, bytes_freed, cache_entries_removed }`.
  - `pub(crate) fn write_atomic(dir, path, bytes)`.

- [ ] **Step 1: Write the caches**

Cache values are `erofs <digest>` or `parents <json>` (layers) and a bare digest (layers-ctx, squash), exactly as spec §5.2 lays out.

`crates/kiln-store/src/cache.rs`:
```rust
//! Conversion caches (spec §5.2): small text entries keyed by source digests.

use std::fs;
use std::path::PathBuf;

use crate::error::{Result, StoreError};
use crate::{Digest, Store, write_atomic};

/// Which cache an entry lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheKind {
    /// `<src-digest>@<fmt>` → `erofs <digest>` or `parents <json>`.
    Layers,
    /// `<src-digest>@<fmt>@<ctx>` → erofs digest of a layer with inherited parents.
    LayersCtx,
    /// `<sha256 of ordered erofs digests>@<fmt>` → squashed erofs digest.
    Squash,
}

impl CacheKind {
    fn dir(self) -> &'static str {
        match self {
            CacheKind::Layers => "cache/layers",
            CacheKind::LayersCtx => "cache/layers-ctx",
            CacheKind::Squash => "cache/squash",
        }
    }

    pub(crate) const ALL: [CacheKind; 3] = [CacheKind::Layers, CacheKind::LayersCtx, CacheKind::Squash];
}

fn check_key(key: &str) -> Result<()> {
    let ok =
        !key.is_empty() && key.len() <= 200 && key.bytes().all(|b| b.is_ascii_alphanumeric() || b"@:._-".contains(&b));
    if ok {
        Ok(())
    } else {
        Err(StoreError::Invalid {
            what: "cache key",
            value: key.to_string(),
        })
    }
}

impl Store {
    fn cache_path(&self, kind: CacheKind, key: &str) -> Result<PathBuf> {
        check_key(key)?;
        Ok(self.root.join(kind.dir()).join(key.replace(':', "_")))
    }

    pub fn cache_get(&self, kind: CacheKind, key: &str) -> Result<Option<String>> {
        match fs::read_to_string(self.cache_path(kind, key)?) {
            Ok(v) => Ok(Some(v)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Writes an entry atomically. Callers commit the blob it names first.
    pub fn cache_put(&self, kind: CacheKind, key: &str, value: &str) -> Result<()> {
        let path = self.cache_path(kind, key)?;
        write_atomic(&self.tmp_dir(), &path, value.as_bytes())
    }

    /// An entry whose value is a digest; a missing blob counts as a miss.
    pub fn cache_get_blob(&self, kind: CacheKind, key: &str) -> Result<Option<Digest>> {
        let Some(v) = self.cache_get(kind, key)? else {
            return Ok(None);
        };
        let d = Digest::parse(v.trim())?;
        Ok(self.has_blob(&d).then_some(d))
    }

    /// All `(file name, value)` pairs of one cache (for GC).
    pub(crate) fn cache_entries(&self, kind: CacheKind) -> Result<Vec<(PathBuf, String)>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(self.root.join(kind.dir()))? {
            let path = entry?.path();
            if let Ok(v) = fs::read_to_string(&path) {
                out.push((path, v));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get_and_blob_hits() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let src = Digest::of(b"src");
        let key = format!("{src}@1");
        assert_eq!(s.cache_get(CacheKind::Layers, &key).unwrap(), None);
        s.cache_put(CacheKind::Layers, &key, "parents []").unwrap();
        assert_eq!(
            s.cache_get(CacheKind::Layers, &key).unwrap().as_deref(),
            Some("parents []")
        );

        let out = s.put_bytes(b"erofs").unwrap();
        s.cache_put(CacheKind::Squash, &key, &out.to_string()).unwrap();
        assert_eq!(s.cache_get_blob(CacheKind::Squash, &key).unwrap(), Some(out.clone()));
        fs::remove_file(s.blob_path(&out)).unwrap();
        assert_eq!(
            s.cache_get_blob(CacheKind::Squash, &key).unwrap(),
            None,
            "missing blob is a miss"
        );
    }

    #[test]
    fn keys_cannot_escape_the_cache_dir() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        for bad in ["../refs.json", "a/b", "", "x y"] {
            assert!(s.cache_put(CacheKind::Layers, bad, "v").is_err(), "{bad}");
        }
    }
}
```

- [ ] **Step 2: Write the refs index**

A single `refs.json` avoids one-file-per-tag collisions on case-insensitive macOS filesystems (spec §5.2).

`crates/kiln-store/src/refs.rs`:
```rust
//! The refs index (`refs.json`): tag → digest, one file replaced atomically.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};

use serde::{Deserialize, Serialize};

use crate::error::{Result, StoreError};
use crate::{Digest, Store, write_atomic};

#[derive(Default, Serialize, Deserialize)]
struct RefsFile {
    refs: BTreeMap<String, Digest>,
}

/// Validates a tag: printable ASCII from `[A-Za-z0-9._/:@-]`, at most 255 bytes.
pub fn check_ref_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 255
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._/:@-".contains(&b));
    if ok {
        Ok(())
    } else {
        Err(StoreError::Invalid {
            what: "ref name",
            value: name.to_string(),
        })
    }
}

impl Store {
    fn refs_path(&self) -> std::path::PathBuf {
        self.root.join("refs.json")
    }

    pub fn refs(&self) -> Result<BTreeMap<String, Digest>> {
        match fs::read(self.refs_path()) {
            Ok(bytes) => {
                let f: RefsFile = serde_json::from_slice(&bytes).map_err(|e| StoreError::Corrupt {
                    path: self.refs_path().display().to_string(),
                    reason: e.to_string(),
                })?;
                Ok(f.refs)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn get_ref(&self, name: &str) -> Result<Option<Digest>> {
        Ok(self.refs()?.remove(name))
    }

    /// Read-modify-write of `refs.json` under its own exclusive lock, so
    /// concurrent writers never lose an update.
    fn update_refs(&self, f: impl FnOnce(&mut BTreeMap<String, Digest>) -> bool) -> Result<bool> {
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.root.join("refs.lock"))?;
        lock.lock()?;
        let mut refs = self.refs()?;
        let changed = f(&mut refs);
        if changed {
            let bytes = serde_json::to_vec(&RefsFile { refs }).expect("refs serialize");
            write_atomic(&self.tmp_dir(), &self.refs_path(), &bytes)?;
        }
        Ok(changed)
    }

    pub fn set_ref(&self, name: &str, d: &Digest) -> Result<()> {
        check_ref_name(name)?;
        self.update_refs(|r| {
            r.insert(name.to_string(), d.clone());
            true
        })?;
        Ok(())
    }

    /// Returns whether the ref existed.
    pub fn remove_ref(&self, name: &str) -> Result<bool> {
        self.update_refs(|r| r.remove(name).is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_remove_persist() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let d = Digest::of(b"m");
        s.set_ref("app:RC1", &d).unwrap();
        s.set_ref("app:rc1", &Digest::of(b"other")).unwrap();
        let reopened = Store::open(dir.path()).unwrap();
        assert_eq!(
            reopened.get_ref("app:RC1").unwrap(),
            Some(d),
            "case-distinct tags do not collide"
        );
        assert_eq!(reopened.refs().unwrap().len(), 2);
        assert!(reopened.remove_ref("app:RC1").unwrap());
        assert!(!reopened.remove_ref("app:RC1").unwrap());
    }

    #[test]
    fn rejects_bad_names() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        for bad in ["", "a b", "x\u{1b}[31m", &"a".repeat(256)] {
            assert!(s.set_ref(bad, &Digest::of(b"m")).is_err());
        }
    }

    #[test]
    fn concurrent_writers_lose_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        std::thread::scope(|scope| {
            for t in 0..8 {
                let s = s.clone();
                scope.spawn(move || {
                    for i in 0..10 {
                        s.set_ref(&format!("t{t}:{i}"), &Digest::of(format!("{t}-{i}").as_bytes()))
                            .unwrap();
                    }
                });
            }
        });
        assert_eq!(s.refs().unwrap().len(), 80);
    }

    #[test]
    fn corrupt_refs_file_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        fs::write(dir.path().join("refs.json"), b"{not json").unwrap();
        assert!(matches!(s.refs(), Err(StoreError::Corrupt { .. })));
    }
}
```

- [ ] **Step 3: Write GC**

GC takes the exclusive lock, marks from `refs.json` through metadata blobs (read with the 4 MiB cap), deletes dead cache entries **before** blobs, keeps `parents [...]` entries (they name no blob), deletes unreferenced blobs and empties `tmp/`. `gc_waits_for_shared_lock_holders` pins that a running convert (which holds the shared lock) blocks GC.

`crates/kiln-store/src/gc.rs`:
```rust
//! Garbage collection (spec §5.2): mark from refs, drop dead cache entries, then blobs.

use std::collections::BTreeSet;
use std::fs;

use crate::cache::CacheKind;
use crate::error::Result;
use crate::{Digest, Store};

/// What a GC run removed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GcReport {
    pub blobs_removed: u64,
    pub bytes_freed: u64,
    pub cache_entries_removed: u64,
}

/// Digests a manifest or index references: `manifests[]`, `config`, `layers[]`.
pub fn references(json: &serde_json::Value) -> Vec<Digest> {
    let mut out = Vec::new();
    let mut push = |v: &serde_json::Value| {
        if let Some(d) = v
            .get("digest")
            .and_then(|d| d.as_str())
            .and_then(|s| Digest::parse(s).ok())
        {
            out.push(d);
        }
    };
    for key in ["manifests", "layers"] {
        if let Some(arr) = json.get(key).and_then(|a| a.as_array()) {
            arr.iter().for_each(&mut push);
        }
    }
    if let Some(c) = json.get("config") {
        push(c);
    }
    out
}

impl Store {
    /// Blobs reachable from refs.
    pub fn live_blobs(&self) -> Result<BTreeSet<Digest>> {
        let mut live = BTreeSet::new();
        let mut stack: Vec<Digest> = self.refs()?.into_values().collect();
        while let Some(d) = stack.pop() {
            if !live.insert(d.clone()) || !self.has_blob(&d) {
                continue;
            }
            if let Ok(bytes) = self.read_metadata(&d)
                && let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes)
            {
                stack.extend(references(&json));
            }
        }
        Ok(live)
    }

    /// Takes the exclusive lock, then removes unreferenced cache entries and blobs.
    pub fn gc(&self) -> Result<GcReport> {
        let _lock = self.lock_exclusive()?;
        let live = self.live_blobs()?;
        let mut report = GcReport::default();
        for kind in CacheKind::ALL {
            for (path, value) in self.cache_entries(kind)? {
                let target = value.strip_prefix("erofs ").unwrap_or(&value).trim();
                let dead = match Digest::parse(target) {
                    Ok(d) => !live.contains(&d),
                    // `parents [...]` entries name no blob; keep them.
                    Err(_) => false,
                };
                if dead {
                    fs::remove_file(path)?;
                    report.cache_entries_removed += 1;
                }
            }
        }
        for d in self.list_blobs()? {
            if !live.contains(&d) {
                let path = self.blob_path(&d);
                report.bytes_freed += fs::metadata(&path)?.len();
                fs::remove_file(path)?;
                report.blobs_removed += 1;
            }
        }
        for entry in fs::read_dir(self.tmp_dir())? {
            let _ = fs::remove_file(entry?.path());
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gc_waits_for_shared_lock_holders() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let root = dir.path().to_path_buf();
        let holder = std::thread::spawn(move || {
            let s = Store::open(root).unwrap();
            let _lock = s.lock_shared().unwrap();
            tx.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(400));
        });
        rx.recv().unwrap();
        let start = std::time::Instant::now();
        s.gc().unwrap();
        assert!(
            start.elapsed() >= std::time::Duration::from_millis(300),
            "gc ran while a convert held the lock"
        );
        holder.join().unwrap();
    }

    #[test]
    fn keeps_the_reachable_chain_and_removes_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let layer = s.put_bytes(b"layer").unwrap();
        let config = s.put_bytes(b"{}").unwrap();
        let manifest =
            serde_json::json!({"config": {"digest": config.to_string()}, "layers": [{"digest": layer.to_string()}]});
        let m = s.put_bytes(serde_json::to_vec(&manifest).unwrap().as_slice()).unwrap();
        let index = serde_json::json!({"manifests": [{"digest": m.to_string()}]});
        let i = s.put_bytes(serde_json::to_vec(&index).unwrap().as_slice()).unwrap();
        s.set_ref("app:1", &i).unwrap();
        let garbage = s.put_bytes(b"garbage").unwrap();
        s.cache_put(CacheKind::Layers, "k1@1", &format!("erofs {garbage}"))
            .unwrap();
        s.cache_put(CacheKind::Layers, "k2@1", &format!("erofs {layer}"))
            .unwrap();
        s.cache_put(CacheKind::Layers, "k3@1", "parents [\"61\"]").unwrap();

        let report = s.gc().unwrap();
        assert_eq!(report.blobs_removed, 1);
        assert_eq!(report.bytes_freed, 7);
        assert_eq!(report.cache_entries_removed, 1);
        for d in [&layer, &config, &m, &i] {
            assert!(s.has_blob(d));
        }
        assert!(!s.has_blob(&garbage));
        assert!(s.cache_get(CacheKind::Layers, "k1@1").unwrap().is_none());
        assert!(s.cache_get(CacheKind::Layers, "k2@1").unwrap().is_some());
        assert!(s.cache_get(CacheKind::Layers, "k3@1").unwrap().is_some());
    }

    #[test]
    fn gc_waits_for_shared_holders() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let shared = s.lock_shared().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let s2 = s.clone();
        let t = std::thread::spawn(move || {
            s2.gc().unwrap();
            tx.send(()).unwrap();
        });
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(rx.try_recv().is_err(), "gc must block while a shared lock is held");
        drop(shared);
        rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        t.join().unwrap();
    }
}
```

- [ ] **Step 4: Wire the modules into `lib.rs`**

In `crates/kiln-store/src/lib.rs`, replace:
```rust
mod digest;
mod error;
```
with:
```rust
mod cache;
mod digest;
mod error;
mod gc;
mod refs;
```

In `crates/kiln-store/src/lib.rs`, replace:
```rust
pub use digest::{Digest, Hasher, HashingReader};
pub use error::{Result, StoreError};
```
with:
```rust
pub use cache::CacheKind;
pub use digest::{Digest, Hasher, HashingReader};
pub use error::{Result, StoreError};
pub use gc::{GcReport, references};
pub use refs::check_ref_name;
```

In `crates/kiln-store/src/lib.rs`, replace:
```rust
#[cfg(test)]
```
with:
```rust
/// Writes `bytes` to `path` atomically (temp file in `dir`, fsync, rename).
pub(crate) fn write_atomic(dir: &Path, path: &Path, bytes: &[u8]) -> Result<()> {
    let mut tmp = tempfile::Builder::new().prefix("meta-").tempfile_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| StoreError::Io(e.error))?;
    Ok(())
}

#[cfg(test)]
```

- [ ] **Step 5: Run the crate's tests**

Run: `cargo test -q -p kiln-store`
Expected: all pass (21 tests).

- [ ] **Step 6: Format, lint and run the whole workspace**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace -q`
Expected: no clippy warnings; every test passes.

- [ ] **Step 7: Commit**

```bash
git add crates/kiln-store Cargo.lock
git commit -m "feat(kiln-store): layer caches, refs index and GC" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P7rdQAESQYNEo2qFobUNxt"
```


### Task 3: `kiln-oci`: OCI types, media types and platforms

**Files:**
- Create: `crates/kiln-oci/Cargo.toml`, `crates/kiln-oci/src/error.rs`, `crates/kiln-oci/src/media.rs`, `crates/kiln-oci/src/platform.rs`, `crates/kiln-oci/src/types.rs`, `crates/kiln-oci/src/lib.rs`

**Interfaces:**
- Consumes: `kiln_store::{Digest, StoreError}`.
- Produces:
  - `OciError` variants: `Store`, `Io`, `Json { what, source }`, `NotAnImage(String)`, `MissingFile(String)`, `AmbiguousRef { available }`, `RefNotFound { wanted, available }`, `MissingPlatform { wanted, available }`, `UnsupportedMediaType(String)`, `ForeignLayer(String)`, `DiffIdCount { layers, diff_ids }`, `BadPlatform(String)`, `BadArchive(String)`. (Task 4 adds `pub(crate) fn json<T>(what, bytes)`.)
  - `media`: the OCI and Docker media-type constants (`OCI_INDEX`, `OCI_MANIFEST`, `OCI_CONFIG`, `OCI_LAYER_TAR`, `OCI_LAYER_GZIP`, `OCI_LAYER_ZSTD`, Docker equivalents), `Compression { None, Gzip, Zstd }`, `is_index`, `is_manifest`, `is_config`, `layer_compression(media_type) -> Result<Compression>` (refuses foreign and non-distributable layers), `sniff_compression(&[u8])`, `oci_layer_media_type(Compression)`.
  - `Platform { architecture, os, variant }` with `parse("os/arch[/variant]")`, `host()`, `matches(&candidate)` (`arm64` with no variant matches `v8`), `Display`, `Ord`.
  - `Descriptor { media_type, digest, size, urls, annotations, platform, artifact_type }` (`new`, `annotation`), `ImageIndex`, `ImageManifest`, `ImageConfig { architecture, os, variant, config, rootfs }` (`platform()`), `ContainerConfig` (Docker's PascalCase `User`, `Env`, `Entrypoint`, `Cmd`, `WorkingDir`, `StopSignal`), `RootFs { fs_type, diff_ids }`, `canonical_json<T: Serialize>(&T) -> Vec<u8>` (sorted keys, no whitespace).

- [ ] **Step 1: Create the crate manifest**

`crates/kiln-oci/Cargo.toml`:
```toml
[package]
name = "kiln-oci"
version = "0.1.0"
edition.workspace = true
license.workspace = true
description = "OCI types and verified local image inputs (layouts, docker archives) for kiln"

[dependencies]
kiln-store = { path = "../kiln-store" }
serde = { version = "1.0.229", features = ["derive"] }
serde_json = "1.0.151"
tar = "0.4.46"
thiserror = "2.0.21"

[dev-dependencies]
flate2 = "1.1.10"
tar = "0.4.46"
tempfile = "3.27.0"
```

- [ ] **Step 2: Write `error.rs`**

`crates/kiln-oci/src/error.rs`:
```rust
use thiserror::Error;

/// Errors from reading OCI inputs.
#[derive(Debug, Error)]
pub enum OciError {
    #[error(transparent)]
    Store(#[from] kiln_store::StoreError),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid {what}: {source}")]
    Json { what: String, source: serde_json::Error },
    #[error("{0} is not an OCI image layout or docker archive")]
    NotAnImage(String),
    #[error("missing {0} in the image source")]
    MissingFile(String),
    #[error("the source holds several images; choose one with --ref (available: {})", .available.join(", "))]
    AmbiguousRef { available: Vec<String> },
    #[error("no image named {wanted:?} in the source (available: {})", .available.join(", "))]
    RefNotFound { wanted: String, available: Vec<String> },
    #[error("no image for platform {wanted} (available: {})", .available.join(", "))]
    MissingPlatform { wanted: String, available: Vec<String> },
    #[error("unsupported media type {0:?}")]
    UnsupportedMediaType(String),
    #[error("foreign or non-distributable layer {0:?} is not supported")]
    ForeignLayer(String),
    #[error("image config lists {diff_ids} diff_ids for {layers} layers")]
    DiffIdCount { layers: usize, diff_ids: usize },
    #[error("invalid platform {0:?} (expected os/arch[/variant])")]
    BadPlatform(String),
    #[error("malformed archive: {0}")]
    BadArchive(String),
}

pub type Result<T> = std::result::Result<T, OciError>;
```

- [ ] **Step 3: Write `media.rs`, with its unit tests**

Docker schema v1 and foreign (`.foreign.`/`nondistributable`) layers are rejected here, before anything is fetched (spec §6.1 step 2).

`crates/kiln-oci/src/media.rs`:
```rust
//! Media types kiln reads, and the layer policy of spec §6.1.

use crate::error::{OciError, Result};

pub const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
pub const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
pub const OCI_CONFIG: &str = "application/vnd.oci.image.config.v1+json";
pub const OCI_LAYER_TAR: &str = "application/vnd.oci.image.layer.v1.tar";
pub const OCI_LAYER_GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
pub const OCI_LAYER_ZSTD: &str = "application/vnd.oci.image.layer.v1.tar+zstd";
pub const DOCKER_MANIFEST_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
pub const DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";
pub const DOCKER_CONFIG: &str = "application/vnd.docker.container.image.v1+json";
pub const DOCKER_LAYER_GZIP: &str = "application/vnd.docker.image.rootfs.diff.tar.gzip";
pub const DOCKER_LAYER_TAR: &str = "application/vnd.docker.image.rootfs.diff.tar";

/// How a layer blob is compressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    None,
    Gzip,
    Zstd,
}

pub fn is_index(media_type: &str) -> bool {
    media_type == OCI_INDEX || media_type == DOCKER_MANIFEST_LIST
}

pub fn is_manifest(media_type: &str) -> bool {
    media_type == OCI_MANIFEST || media_type == DOCKER_MANIFEST
}

pub fn is_config(media_type: &str) -> bool {
    media_type == OCI_CONFIG || media_type == DOCKER_CONFIG
}

/// The compression of a supported layer; rejects foreign, non-distributable and
/// unknown layer types before anything is fetched.
pub fn layer_compression(media_type: &str) -> Result<Compression> {
    match media_type {
        OCI_LAYER_TAR | DOCKER_LAYER_TAR => Ok(Compression::None),
        OCI_LAYER_GZIP | DOCKER_LAYER_GZIP => Ok(Compression::Gzip),
        OCI_LAYER_ZSTD => Ok(Compression::Zstd),
        m if m.contains("foreign") || m.contains("nondistributable") => Err(OciError::ForeignLayer(m.to_string())),
        m => Err(OciError::UnsupportedMediaType(m.to_string())),
    }
}

/// Sniffs compression from a blob's first bytes (docker archives carry no media types).
pub fn sniff_compression(head: &[u8]) -> Compression {
    if head.starts_with(&[0x1f, 0x8b]) {
        Compression::Gzip
    } else if head.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        Compression::Zstd
    } else {
        Compression::None
    }
}

pub fn oci_layer_media_type(c: Compression) -> &'static str {
    match c {
        Compression::None => OCI_LAYER_TAR,
        Compression::Gzip => OCI_LAYER_GZIP,
        Compression::Zstd => OCI_LAYER_ZSTD,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layer_policy() {
        assert_eq!(layer_compression(OCI_LAYER_GZIP).unwrap(), Compression::Gzip);
        assert_eq!(layer_compression(DOCKER_LAYER_GZIP).unwrap(), Compression::Gzip);
        assert_eq!(layer_compression(OCI_LAYER_ZSTD).unwrap(), Compression::Zstd);
        assert_eq!(layer_compression(OCI_LAYER_TAR).unwrap(), Compression::None);
        for foreign in [
            "application/vnd.docker.image.rootfs.foreign.diff.tar.gzip",
            "application/vnd.oci.image.layer.nondistributable.v1.tar+gzip",
        ] {
            assert!(matches!(layer_compression(foreign), Err(OciError::ForeignLayer(_))));
        }
        assert!(matches!(
            layer_compression("text/plain"),
            Err(OciError::UnsupportedMediaType(_))
        ));
    }

    #[test]
    fn sniffing() {
        assert_eq!(sniff_compression(&[0x1f, 0x8b, 8, 0]), Compression::Gzip);
        assert_eq!(sniff_compression(&[0x28, 0xb5, 0x2f, 0xfd]), Compression::Zstd);
        assert_eq!(sniff_compression(b"usr/"), Compression::None);
    }
}
```

- [ ] **Step 4: Write `platform.rs`, with its unit tests**

`crates/kiln-oci/src/platform.rs`:
```rust
//! Platforms (`os/arch[/variant]`) and matching.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::{OciError, Result};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Platform {
    pub architecture: String,
    pub os: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

impl Platform {
    pub fn parse(s: &str) -> Result<Self> {
        let parts: Vec<&str> = s.split('/').collect();
        let ok = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.');
        match parts.as_slice() {
            [os, arch] if ok(os) && ok(arch) => Ok(Self {
                os: os.to_string(),
                architecture: arch.to_string(),
                variant: None,
            }),
            [os, arch, v] if ok(os) && ok(arch) && ok(v) => Ok(Self {
                os: os.to_string(),
                architecture: arch.to_string(),
                variant: Some(v.to_string()),
            }),
            _ => Err(OciError::BadPlatform(s.to_string())),
        }
    }

    /// `linux/<host arch>`: the platform a native build targets by default.
    pub fn host() -> Self {
        let arch = match std::env::consts::ARCH {
            "aarch64" => "arm64",
            "x86_64" => "amd64",
            other => other,
        };
        Self {
            os: "linux".into(),
            architecture: arch.into(),
            variant: None,
        }
    }

    /// Whether `candidate` satisfies this wanted platform. A wanted platform without a
    /// variant accepts any variant; arm64 treats a missing variant as `v8`.
    pub fn matches(&self, candidate: &Platform) -> bool {
        let norm = |p: &Platform| match (p.architecture.as_str(), p.variant.as_deref()) {
            ("arm64", None) => Some("v8".to_string()),
            (_, v) => v.map(str::to_string),
        };
        self.os == candidate.os
            && self.architecture == candidate.architecture
            && (self.variant.is_none() || norm(self) == norm(candidate))
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.os, self.architecture)?;
        if let Some(v) = &self.variant {
            write!(f, "/{v}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_display_match() {
        let p = Platform::parse("linux/arm64/v8").unwrap();
        assert_eq!(p.to_string(), "linux/arm64/v8");
        assert!(Platform::parse("linux/arm64").unwrap().matches(&p));
        assert!(
            p.matches(&Platform::parse("linux/arm64").unwrap()),
            "arm64 without variant is v8"
        );
        assert!(!Platform::parse("linux/amd64").unwrap().matches(&p));
        assert!(
            !Platform::parse("linux/arm/v7")
                .unwrap()
                .matches(&Platform::parse("linux/arm/v6").unwrap())
        );
        for bad in ["linux", "linux/", "/arm64", "linux/arm64/v8/x", "linux/ar m64"] {
            assert!(Platform::parse(bad).is_err(), "{bad}");
        }
    }
}
```

- [ ] **Step 5: Write `types.rs`, with its unit tests**

Unknown JSON fields are ignored on read. `canonical_json` goes through `serde_json::Value`, whose map is a `BTreeMap` as long as no crate in the workspace enables serde_json's `preserve_order` feature; that is what keeps the output sorted (spec §6.6).

`crates/kiln-oci/src/types.rs`:
```rust
//! The OCI image-spec structures kiln reads and writes (unknown fields are ignored).

use std::collections::BTreeMap;

use kiln_store::Digest;
use serde::{Deserialize, Serialize};

use crate::platform::Platform;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Descriptor {
    pub media_type: String,
    pub digest: Digest,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub urls: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<Platform>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_type: Option<String>,
}

impl Descriptor {
    pub fn new(media_type: &str, digest: Digest, size: u64) -> Self {
        Self {
            media_type: media_type.to_string(),
            digest,
            size,
            urls: None,
            annotations: None,
            platform: None,
            artifact_type: None,
        }
    }

    pub fn annotation(&self, key: &str) -> Option<&str> {
        self.annotations.as_ref()?.get(key).map(String::as_str)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageIndex {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_type: Option<String>,
    pub manifests: Vec<Descriptor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageManifest {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_type: Option<String>,
    pub config: Descriptor,
    pub layers: Vec<Descriptor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<BTreeMap<String, String>>,
}

/// The parts of an OCI image config kiln uses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageConfig {
    pub architecture: String,
    pub os: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<ContainerConfig>,
    pub rootfs: RootFs,
}

impl ImageConfig {
    pub fn platform(&self) -> Platform {
        Platform {
            os: self.os.clone(),
            architecture: self.architecture.clone(),
            variant: self.variant.clone(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ContainerConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cmd: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_signal: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootFs {
    #[serde(rename = "type")]
    pub fs_type: String,
    pub diff_ids: Vec<Digest>,
}

/// JSON with sorted keys and no insignificant whitespace (spec §6.6).
pub fn canonical_json<T: Serialize>(value: &T) -> Vec<u8> {
    let v = serde_json::to_value(value).expect("serializable value");
    serde_json::to_vec(&v).expect("json value serializes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_json_sorts_keys_and_is_compact() {
        let d = Descriptor::new("x", Digest::of(b"a"), 3);
        let s = String::from_utf8(canonical_json(&d)).unwrap();
        assert!(s.starts_with("{\"digest\":"), "{s}");
        assert!(!s.contains(' '));
    }

    #[test]
    fn parses_docker_style_config() {
        let json = br#"{"architecture":"arm64","os":"linux","config":{"Env":["PATH=/bin"],"Cmd":["sh"],"WorkingDir":"/w","User":"1000:1000","StopSignal":"SIGQUIT","Labels":{"a":"b"}},"rootfs":{"type":"layers","diff_ids":[]},"history":[]}"#;
        let c: ImageConfig = serde_json::from_slice(json).unwrap();
        let cc = c.config.unwrap();
        assert_eq!(cc.cmd.unwrap(), vec!["sh"]);
        assert_eq!(cc.stop_signal.as_deref(), Some("SIGQUIT"));
        assert_eq!(cc.working_dir.as_deref(), Some("/w"));
    }
}
```

- [ ] **Step 6: Write `lib.rs`**

`crates/kiln-oci/src/lib.rs`:
```rust
//! OCI image types and verified local inputs for kiln (spec §6.1): OCI image
//! layouts and `docker save` archives. Every blob is hashed into the store;
//! file names, `index.json` and `manifest.json` are never trusted.
#![forbid(unsafe_code)]

mod error;
pub mod media;
mod platform;
mod types;

pub use error::{OciError, Result};
pub use platform::Platform;
pub use types::{ContainerConfig, Descriptor, ImageConfig, ImageIndex, ImageManifest, RootFs, canonical_json};
```

- [ ] **Step 7: Run the crate's tests**

Run: `cargo test -q -p kiln-oci`
Expected: all pass (5 tests).

- [ ] **Step 8: Format, lint and run the whole workspace**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace -q`
Expected: no clippy warnings; every test passes.

- [ ] **Step 9: Commit**

```bash
git add crates/kiln-oci Cargo.lock
git commit -m "feat(kiln-oci): OCI image types, media types and platforms" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P7rdQAESQYNEo2qFobUNxt"
```


### Task 4: `kiln-oci`: verified local inputs (OCI layouts and `docker save` archives)

**Files:**
- Create: `crates/kiln-oci/src/source.rs`, `crates/kiln-oci/src/resolve.rs`, `crates/kiln-oci/src/testlayout.rs`, `crates/kiln-oci/tests/resolve.rs`
- Modify: `crates/kiln-oci/src/lib.rs`

**Interfaces:**
- Consumes: Task 1–2 `Store` (`put_verified`, `put_bytes`, `put_reader`, `read_metadata`, `MAX_METADATA_BLOB`); Task 3 types.
- Produces:
  - `BlobSource` trait (`open_path`, `has_path`, `describe`, `open_blob(&Digest)`, `read_small(path, max)`), `DirLayout::open(dir)`, `TarArchive::open(file)`.
  - `LocalSource { Layout(PathBuf), Archive(PathBuf) }` with `detect(&Path)`; `REF_NAME` (`org.opencontainers.image.ref.name`), `CONTAINERD_NAME` (`io.containerd.image.name`).
  - `ResolvedImage { platform, manifest_digest, manifest: ImageManifest, config_digest, config: ImageConfig, ref_name: Option<String> }`.
  - `resolve_local(&Store, &LocalSource, ref_name: Option<&str>, platforms: &[Platform]) -> Result<Vec<ResolvedImage>>`: on return, the manifest, config and every layer blob are in the store, verified by digest and size. Legacy `docker save` archives get an OCI manifest synthesised from content digests.
  - `#[doc(hidden)] pub mod testlayout`: `TestLayer { media_type, blob, diff_id }` (`tar(Vec<u8>)`), `LayoutBuilder { new, blob, image(platform, layers, config) -> Descriptor, add(desc, ref_name), multiarch(manifests) -> Descriptor, finish() -> PathBuf }`, `docker_legacy_archive(path, platform, layer_tars, config, tag)`. Later tasks' tests build their fixtures with it.

- [ ] **Step 1: Write the failing integration tests**

These cover single-platform and multi-arch layouts, ref selection, tampered blobs, foreign layers (rejected before any fetch), `diff_ids` count mismatches, both `docker save` formats, and the Docker 25+ attestation manifest that sits beside the image in `index.json` (Review Focus 1).

`crates/kiln-oci/tests/resolve.rs`:
```rust
use std::fs;
use std::io::Write;

use flate2::Compression;
use flate2::write::GzEncoder;
use kiln_oci::testlayout::{LayoutBuilder, TestLayer, docker_legacy_archive};
use kiln_oci::{ContainerConfig, LocalSource, OciError, Platform, TarArchive, media, resolve_local};
use kiln_store::{Digest, Store};

fn layer_tar(name: &str) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    let mut h = tar::Header::new_ustar();
    h.set_path(name).unwrap();
    h.set_size(3);
    h.set_mode(0o644);
    h.set_cksum();
    b.append(&h, &b"abc"[..]).unwrap();
    b.into_inner().unwrap()
}

fn gz(bytes: &[u8]) -> TestLayer {
    let mut e = GzEncoder::new(Vec::new(), Compression::default());
    e.write_all(bytes).unwrap();
    TestLayer {
        media_type: media::OCI_LAYER_GZIP.into(),
        blob: e.finish().unwrap(),
        diff_id: Digest::of(bytes),
    }
}

fn arm() -> Platform {
    Platform::parse("linux/arm64").unwrap()
}

fn amd() -> Platform {
    Platform::parse("linux/amd64").unwrap()
}

fn cfg() -> ContainerConfig {
    ContainerConfig {
        cmd: Some(vec!["sh".into()]),
        ..Default::default()
    }
}

#[test]
fn resolves_a_single_platform_layout_into_the_store() {
    let tmp = tempfile::tempdir().unwrap();
    let mut b = LayoutBuilder::new(&tmp.path().join("layout"));
    let m = b.image(&arm(), &[gz(&layer_tar("a")), TestLayer::tar(layer_tar("b"))], cfg());
    let dir = b.add(m.clone(), Some("app:1")).finish();
    let store = Store::open(tmp.path().join("store")).unwrap();
    let imgs = resolve_local(&store, &LocalSource::detect(&dir).unwrap(), None, &[arm()]).unwrap();
    assert_eq!(imgs.len(), 1);
    let img = &imgs[0];
    assert_eq!(img.manifest_digest, m.digest);
    assert_eq!(img.ref_name.as_deref(), Some("app:1"));
    assert_eq!(img.manifest.layers.len(), 2);
    for d in std::iter::once(&img.manifest_digest)
        .chain([&img.config_digest])
        .chain(img.manifest.layers.iter().map(|l| &l.digest))
    {
        assert!(store.has_blob(d), "{d} ingested");
    }
}

#[test]
fn selects_platforms_from_a_multiarch_index_and_reports_missing_ones() {
    let tmp = tempfile::tempdir().unwrap();
    let mut b = LayoutBuilder::new(&tmp.path().join("layout"));
    let ma = b.image(&arm(), &[TestLayer::tar(layer_tar("arm"))], cfg());
    let mx = b.image(&amd(), &[TestLayer::tar(layer_tar("amd"))], cfg());
    let idx = b.multiarch(vec![ma.clone(), mx.clone()]);
    let dir = b.add(idx, None).finish();
    let store = Store::open(tmp.path().join("store")).unwrap();
    let src = LocalSource::Layout(dir);
    let imgs = resolve_local(&store, &src, None, &[amd(), arm()]).unwrap();
    assert_eq!(
        imgs.iter().map(|i| i.manifest_digest.clone()).collect::<Vec<_>>(),
        vec![mx.digest, ma.digest]
    );
    let err = resolve_local(&store, &src, None, &[Platform::parse("linux/riscv64").unwrap()]).unwrap_err();
    match err {
        OciError::MissingPlatform { available, .. } => assert_eq!(available, vec!["linux/arm64", "linux/amd64"]),
        e => panic!("{e}"),
    }
}

#[test]
fn several_images_need_a_ref_and_unknown_refs_list_names() {
    let tmp = tempfile::tempdir().unwrap();
    let mut b = LayoutBuilder::new(&tmp.path().join("layout"));
    let one = b.image(&arm(), &[TestLayer::tar(layer_tar("1"))], cfg());
    let two = b.image(&arm(), &[TestLayer::tar(layer_tar("2"))], cfg());
    let dir = b.add(one, Some("one")).add(two.clone(), Some("two")).finish();
    let store = Store::open(tmp.path().join("store")).unwrap();
    let src = LocalSource::Layout(dir);
    assert!(matches!(
        resolve_local(&store, &src, None, &[arm()]),
        Err(OciError::AmbiguousRef { .. })
    ));
    assert!(matches!(
        resolve_local(&store, &src, Some("three"), &[arm()]),
        Err(OciError::RefNotFound { .. })
    ));
    assert_eq!(
        resolve_local(&store, &src, Some("two"), &[arm()]).unwrap()[0].manifest_digest,
        two.digest
    );
}

#[test]
fn a_tampered_layer_blob_is_rejected_and_not_stored() {
    let tmp = tempfile::tempdir().unwrap();
    let mut b = LayoutBuilder::new(&tmp.path().join("layout"));
    let m = b.image(&arm(), &[TestLayer::tar(layer_tar("a"))], cfg());
    let dir = b.add(m, None).finish();
    let store = Store::open(tmp.path().join("store")).unwrap();
    let layer = resolve_local(
        &Store::open(tmp.path().join("probe")).unwrap(),
        &LocalSource::Layout(dir.clone()),
        None,
        &[arm()],
    )
    .unwrap()[0]
        .manifest
        .layers[0]
        .digest
        .clone();
    fs::write(dir.join("blobs/sha256").join(layer.hex()), layer_tar("evil")).unwrap();
    let err = resolve_local(&store, &LocalSource::Layout(dir), None, &[arm()]).unwrap_err();
    assert!(
        matches!(err, OciError::Store(kiln_store::StoreError::DigestMismatch { .. })),
        "{err}"
    );
    assert!(!store.has_blob(&layer));
}

#[test]
fn foreign_layers_are_rejected_before_any_layer_is_read() {
    let tmp = tempfile::tempdir().unwrap();
    let mut b = LayoutBuilder::new(&tmp.path().join("layout"));
    let mut l = TestLayer::tar(layer_tar("a"));
    l.media_type = "application/vnd.oci.image.layer.nondistributable.v1.tar".into();
    let m = b.image(&arm(), &[l.clone()], cfg());
    let dir = b.add(m, None).finish();
    // Remove the layer blob: the policy check must fail first, not the fetch.
    fs::remove_file(dir.join("blobs/sha256").join(Digest::of(&l.blob).hex())).unwrap();
    let store = Store::open(tmp.path().join("store")).unwrap();
    assert!(matches!(
        resolve_local(&store, &LocalSource::Layout(dir), None, &[arm()]),
        Err(OciError::ForeignLayer(_))
    ));
}

#[test]
fn diff_id_count_must_match_layers() {
    let tmp = tempfile::tempdir().unwrap();
    let mut b = LayoutBuilder::new(&tmp.path().join("layout"));
    let mut l = TestLayer::tar(layer_tar("a"));
    let m = b.image(
        &arm(),
        &[l.clone(), {
            l.blob = layer_tar("b");
            l.diff_id = Digest::of(&l.blob);
            l
        }],
        cfg(),
    );
    // Rewrite the manifest with one layer dropped but the config untouched.
    let store0 = Store::open(tmp.path().join("probe")).unwrap();
    let dir = b.add(m, None).finish();
    let img = resolve_local(&store0, &LocalSource::Layout(dir.clone()), None, &[arm()])
        .unwrap()
        .remove(0);
    let mut manifest = img.manifest.clone();
    manifest.layers.pop();
    let bytes = kiln_oci::canonical_json(&manifest);
    let mut b2 = LayoutBuilder::new(&tmp.path().join("layout2"));
    for blob in [img.config_digest.clone(), img.manifest.layers[0].digest.clone()] {
        b2.blob(
            &store0
                .read_metadata(&blob)
                .unwrap_or_else(|_| fs::read(store0.blob_path(&blob)).unwrap()),
        );
    }
    let d = kiln_oci::Descriptor::new(media::OCI_MANIFEST, b2.blob(&bytes), bytes.len() as u64);
    let dir2 = b2.add(d, None).finish();
    let store = Store::open(tmp.path().join("store")).unwrap();
    assert!(matches!(
        resolve_local(&store, &LocalSource::Layout(dir2), None, &[arm()]),
        Err(OciError::DiffIdCount { layers: 1, diff_ids: 2 })
    ));
}

#[test]
fn legacy_docker_archive_is_resolved_by_hashing_contents() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("img.tar");
    let mut gzl = GzEncoder::new(Vec::new(), Compression::default());
    gzl.write_all(&layer_tar("b")).unwrap();
    docker_legacy_archive(&path, &arm(), &[layer_tar("a"), layer_tar("b")], cfg(), "app:latest");
    let store = Store::open(tmp.path().join("store")).unwrap();
    let imgs = resolve_local(
        &store,
        &LocalSource::detect(&path).unwrap(),
        Some("app:latest"),
        &[arm()],
    )
    .unwrap();
    let img = &imgs[0];
    assert_eq!(img.ref_name.as_deref(), Some("app:latest"));
    assert_eq!(img.manifest.layers[0].digest, Digest::of(&layer_tar("a")));
    assert_eq!(img.manifest.layers[0].media_type, media::OCI_LAYER_TAR);
    assert!(store.has_blob(&img.manifest_digest));
    // Wrong platform is reported.
    assert!(matches!(
        resolve_local(&store, &LocalSource::Archive(path), None, &[amd()]),
        Err(OciError::MissingPlatform { .. })
    ));
}

#[test]
fn oci_layout_inside_a_tar_is_supported() {
    let tmp = tempfile::tempdir().unwrap();
    let mut b = LayoutBuilder::new(&tmp.path().join("layout"));
    let m = b.image(&arm(), &[gz(&layer_tar("a"))], cfg());
    let dir = b.add(m.clone(), None).finish();
    let tar_path = tmp.path().join("oci.tar");
    let mut tb = tar::Builder::new(fs::File::create(&tar_path).unwrap());
    tb.append_dir_all(".", &dir).unwrap();
    tb.into_inner().unwrap();
    assert!(TarArchive::open(&tar_path).unwrap().is_oci_layout());
    let store = Store::open(tmp.path().join("store")).unwrap();
    assert_eq!(
        resolve_local(&store, &LocalSource::Archive(tar_path), None, &[arm()]).unwrap()[0].manifest_digest,
        m.digest
    );
}

#[test]
fn not_an_image() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(matches!(LocalSource::detect(tmp.path()), Err(OciError::NotAnImage(_))));
}

#[test]
fn skips_docker_attestation_manifests_beside_the_image() {
    let src = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    let mut b = LayoutBuilder::new(src.path());
    let img = b.image(&arm(), &[gz(&layer_tar("a"))], cfg());
    let att_bytes = br#"{"schemaVersion":2,"layers":[]}"#;
    let mut att = kiln_oci::Descriptor::new(media::OCI_MANIFEST, b.blob(att_bytes), att_bytes.len() as u64);
    att.annotations = Some([("io.containerd.manifest.subject".to_string(), img.digest.to_string())].into());
    let img_digest = img.digest.clone();
    let path = b.add(img, Some("8.4-cli")).add(att, None).finish();
    let r = resolve_local(&store, &LocalSource::detect(&path).unwrap(), None, &[arm()]).unwrap();
    assert_eq!(r[0].manifest_digest, img_digest);
}
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -q -p kiln-oci --test resolve`
Expected: compile errors: `testlayout`, `resolve_local`, `LocalSource` not found.

- [ ] **Step 3: Write the blob sources**

File names, `index.json` and `manifest.json` are never trusted: `DirLayout` refuses symlinks and non-regular files; `TarArchive` indexes regular entries by offset, rejects `..` components, duplicates and links that escape, resolves symlink and hardlink aliases one level, and rejects gzip-compressed archives. Every blob is then hashed on ingest.

`crates/kiln-oci/src/source.rs`:
```rust
//! Byte sources for local images: an OCI layout directory or a `docker save` tar.
//! Nothing read here is trusted; callers verify every blob against its digest.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use kiln_store::Digest;

use crate::error::{OciError, Result};

/// Random access to named files inside an image source.
pub trait BlobSource {
    /// Opens a file by its path inside the source (e.g. `index.json`).
    fn open_path(&self, path: &str) -> Result<Box<dyn Read + '_>>;
    /// Whether a regular file exists at `path`.
    fn has_path(&self, path: &str) -> bool;
    /// A human-readable name for error messages.
    fn describe(&self) -> String;

    /// Opens an OCI layout blob by digest.
    fn open_blob(&self, d: &Digest) -> Result<Box<dyn Read + '_>> {
        self.open_path(&format!("blobs/sha256/{}", d.hex()))
    }

    /// Reads a small file, refusing more than `max` bytes.
    fn read_small(&self, path: &str, max: u64) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        self.open_path(path)?.take(max + 1).read_to_end(&mut buf)?;
        if buf.len() as u64 > max {
            return Err(OciError::BadArchive(format!("{path} is larger than {max} bytes")));
        }
        Ok(buf)
    }
}

/// Normalizes a path inside a source: strips `./` and leading `/`, rejects `..`.
pub(crate) fn clean_path(p: &str) -> Option<String> {
    let mut parts = Vec::new();
    for c in p.split('/') {
        match c {
            "" | "." => {}
            ".." => return None,
            c => parts.push(c),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// Resolves a link target relative to `dir` inside the source; `None` if it escapes.
pub(crate) fn join_within(dir: &str, target: &str) -> Option<String> {
    let mut parts: Vec<&str> = if target.starts_with('/') {
        Vec::new()
    } else {
        dir.split('/').filter(|c| !c.is_empty()).collect()
    };
    for c in target.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            c => parts.push(c),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// An OCI image layout directory.
pub struct DirLayout {
    root: PathBuf,
}

impl DirLayout {
    pub fn open(root: &Path) -> Result<Self> {
        if !root.join("oci-layout").is_file() {
            return Err(OciError::NotAnImage(root.display().to_string()));
        }
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    fn file_path(&self, path: &str) -> Result<PathBuf> {
        let clean = clean_path(path).ok_or_else(|| OciError::BadArchive(format!("invalid path {path:?}")))?;
        let full = self.root.join(&clean);
        // Refuse symlinks and anything that is not a regular file.
        match fs::symlink_metadata(&full) {
            Ok(m) if m.is_file() => Ok(full),
            Ok(_) => Err(OciError::BadArchive(format!("{clean} is not a regular file"))),
            Err(_) => Err(OciError::MissingFile(clean)),
        }
    }
}

impl BlobSource for DirLayout {
    fn open_path(&self, path: &str) -> Result<Box<dyn Read + '_>> {
        Ok(Box::new(File::open(self.file_path(path)?)?))
    }

    fn has_path(&self, path: &str) -> bool {
        self.file_path(path).is_ok()
    }

    fn describe(&self) -> String {
        self.root.display().to_string()
    }
}

/// A tar archive (`docker save`, legacy or OCI-in-tar), indexed once by offset.
pub struct TarArchive {
    path: PathBuf,
    entries: BTreeMap<String, (u64, u64)>,
}

impl TarArchive {
    pub fn open(path: &Path) -> Result<Self> {
        let mut head = [0u8; 2];
        let mut f = File::open(path)?;
        if f.read(&mut head)? == 2 && head == [0x1f, 0x8b] {
            return Err(OciError::BadArchive(
                "compressed archive: decompress it first (e.g. gunzip)".into(),
            ));
        }
        f.seek(SeekFrom::Start(0))?;
        let mut regular: BTreeMap<String, (u64, u64)> = BTreeMap::new();
        let mut links: Vec<(String, String)> = Vec::new();
        let mut ar = tar::Archive::new(f);
        for entry in ar.entries().map_err(|e| OciError::BadArchive(e.to_string()))? {
            let e = entry.map_err(|e| OciError::BadArchive(e.to_string()))?;
            let raw = String::from_utf8_lossy(&e.path_bytes()).into_owned();
            if raw.split('/').any(|c| c == "..") {
                return Err(OciError::BadArchive(format!("unsafe path {raw:?} in archive")));
            }
            // The archive root (`./`) cleans to nothing; skip it.
            let Some(name) = clean_path(&raw) else { continue };
            let kind = e.header().entry_type();
            if kind.is_file() {
                let size = e
                    .header()
                    .entry_size()
                    .map_err(|e| OciError::BadArchive(e.to_string()))?;
                if regular.insert(name.clone(), (e.raw_file_position(), size)).is_some() {
                    return Err(OciError::BadArchive(format!("duplicate entry {name:?}")));
                }
            } else if kind.is_symlink() || kind.is_hard_link() {
                let target = e
                    .link_name_bytes()
                    .map(|t| String::from_utf8_lossy(&t).into_owned())
                    .unwrap_or_default();
                let resolved = if kind.is_symlink() {
                    join_within(name.rsplit_once('/').map_or("", |(d, _)| d), &target)
                } else {
                    join_within("", &target)
                };
                let resolved =
                    resolved.ok_or_else(|| OciError::BadArchive(format!("link {name:?} escapes the archive")))?;
                links.push((name, resolved));
            }
        }
        // Old `docker save` links duplicate layers to an earlier copy; resolve one level.
        for (name, target) in links {
            if let Some(&loc) = regular.get(&target) {
                regular.entry(name).or_insert(loc);
            }
        }
        Ok(Self {
            path: path.to_path_buf(),
            entries: regular,
        })
    }

    /// Whether this archive contains an OCI layout (newer `docker save`).
    pub fn is_oci_layout(&self) -> bool {
        self.entries.contains_key("oci-layout") && self.entries.contains_key("index.json")
    }
}

impl BlobSource for TarArchive {
    fn open_path(&self, path: &str) -> Result<Box<dyn Read + '_>> {
        let clean = clean_path(path).ok_or_else(|| OciError::BadArchive(format!("invalid path {path:?}")))?;
        let &(offset, size) = self.entries.get(&clean).ok_or(OciError::MissingFile(clean))?;
        let mut f = File::open(&self.path)?;
        f.seek(SeekFrom::Start(offset))?;
        Ok(Box::new(f.take(size)))
    }

    fn has_path(&self, path: &str) -> bool {
        clean_path(path).is_some_and(|c| self.entries.contains_key(&c))
    }

    fn describe(&self) -> String {
        self.path.display().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_paths() {
        assert_eq!(clean_path("./blobs//sha256/x").as_deref(), Some("blobs/sha256/x"));
        assert_eq!(clean_path("/abs").as_deref(), Some("abs"));
        assert_eq!(clean_path("a/../b"), None);
        assert_eq!(clean_path("./"), None);
    }

    #[test]
    fn link_targets_resolve_within_the_source() {
        assert_eq!(join_within("b", "../a/layer.tar").as_deref(), Some("a/layer.tar"));
        assert_eq!(join_within("b", "/a/x").as_deref(), Some("a/x"));
        assert_eq!(join_within("b", "../../etc/passwd"), None);
        assert_eq!(join_within("", ".."), None);
    }

    #[test]
    fn layout_refuses_symlinked_blobs() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("oci-layout"), br#"{"imageLayoutVersion":"1.0.0"}"#).unwrap();
        fs::create_dir_all(dir.path().join("blobs/sha256")).unwrap();
        fs::write(dir.path().join("secret"), b"x").unwrap();
        std::os::unix::fs::symlink(dir.path().join("secret"), dir.path().join("blobs/sha256/abc")).unwrap();
        let l = DirLayout::open(dir.path()).unwrap();
        assert!(matches!(l.open_path("blobs/sha256/abc"), Err(OciError::BadArchive(_))));
        assert!(matches!(l.open_path("../secret"), Err(OciError::BadArchive(_))));
    }

    fn tar_with(entries: &[(&str, u8, &[u8], &str)]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (name, kind, data, link) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_entry_type(tar::EntryType::new(*kind));
            h.set_mode(0o644);
            {
                let old = h.as_old_mut();
                old.name[..name.len()].copy_from_slice(name.as_bytes());
                old.linkname[..link.len()].copy_from_slice(link.as_bytes());
            }
            h.set_cksum();
            b.append(&h, *data).unwrap();
        }
        b.into_inner().unwrap()
    }

    #[test]
    fn archive_indexes_files_and_resolves_layer_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.tar");
        fs::write(
            &p,
            tar_with(&[
                ("a/layer.tar", b'0', b"LAYER", ""),
                ("b/layer.tar", b'2', b"", "../a/layer.tar"),
            ]),
        )
        .unwrap();
        let a = TarArchive::open(&p).unwrap();
        let mut s = String::new();
        a.open_path("b/layer.tar").unwrap().read_to_string(&mut s).unwrap();
        assert_eq!(s, "LAYER");
        assert!(!a.is_oci_layout());
    }

    #[test]
    fn archive_rejects_traversal_duplicates_and_escaping_links() {
        let dir = tempfile::tempdir().unwrap();
        for (i, entries) in [
            vec![("../evil", b'0', &b"x"[..], "")],
            vec![("x", b'0', &b"1"[..], ""), ("./x", b'0', &b"2"[..], "")],
            vec![("a/l", b'2', &b""[..], "../../etc/passwd")],
        ]
        .into_iter()
        .enumerate()
        {
            let p = dir.path().join(format!("{i}.tar"));
            fs::write(&p, tar_with(&entries)).unwrap();
            assert!(matches!(TarArchive::open(&p), Err(OciError::BadArchive(_))), "case {i}");
        }
    }

    #[test]
    fn archive_rejects_gzip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.tar.gz");
        fs::write(&p, [0x1f, 0x8b, 8, 0]).unwrap();
        assert!(matches!(TarArchive::open(&p), Err(OciError::BadArchive(_))));
    }
}
```

- [ ] **Step 4: Write resolution**

Order matters for T1 and T3: descriptors are validated (media types, foreign layers, `diff_ids` count) **before** any large blob is read, metadata blobs are capped at 4 MiB, and descriptor `urls` are never used.

`crates/kiln-oci/src/resolve.rs`:
```rust
//! Resolving a local image source to verified, per-platform images (spec §6.1).

use std::io::Read;
use std::path::{Path, PathBuf};

use kiln_store::{Digest, MAX_METADATA_BLOB, Store};
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

/// One platform's image, with its manifest, config and layers verified into the store.
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
    if !store.has_blob(&d.digest) {
        let mut r = src.open_blob(&d.digest)?;
        store.put_verified(&mut r, &d.digest, Some(d.size))?;
    }
    Ok(())
}

fn ingest_metadata(store: &Store, src: &dyn BlobSource, d: &Descriptor) -> Result<Vec<u8>> {
    if d.size > MAX_METADATA_BLOB {
        return Err(OciError::BadArchive(format!(
            "{} is {} bytes, more than the metadata limit",
            d.digest, d.size
        )));
    }
    ingest(store, src, d)?;
    Ok(store.read_metadata(&d.digest)?)
}

fn names(d: &Descriptor) -> Vec<String> {
    [REF_NAME, CONTAINERD_NAME]
        .iter()
        .filter_map(|k| d.annotation(k).map(str::to_string))
        .collect()
}

/// Attestation manifests that Docker 25+ (`docker save`) and BuildKit list beside
/// images. They are never images, so selection skips them.
fn is_attestation(d: &Descriptor) -> bool {
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
            .find(|d| names(d).iter().any(|n| n == want))
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
    let image_name = names(top).into_iter().next();
    let bytes = ingest_metadata(store, src, top)?;
    if media::is_index(&top.media_type) {
        let inner: ImageIndex = json("image index", &bytes)?;
        platforms
            .iter()
            .map(|want| {
                let d = inner
                    .manifests
                    .iter()
                    .find(|m| m.platform.as_ref().is_some_and(|p| want.matches(p)))
                    .ok_or_else(|| OciError::MissingPlatform {
                        wanted: want.to_string(),
                        available: inner
                            .manifests
                            .iter()
                            .filter_map(|m| m.platform.as_ref())
                            .filter(|p| p.os != "unknown")
                            .map(|p| p.to_string())
                            .collect(),
                    })?;
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

/// Validates a manifest, then verifies its config and layers into the store.
fn finish_manifest(
    store: &Store,
    src: &dyn BlobSource,
    d: &Descriptor,
    bytes: &[u8],
    want: &Platform,
    ref_name: Option<String>,
) -> Result<ResolvedImage> {
    if !media::is_manifest(&d.media_type) {
        return Err(OciError::UnsupportedMediaType(d.media_type.clone()));
    }
    let manifest: ImageManifest = json("image manifest", bytes)?;
    if !media::is_config(&manifest.config.media_type) {
        return Err(OciError::UnsupportedMediaType(manifest.config.media_type.clone()));
    }
    // Reject unsupported layers before fetching anything large.
    for l in &manifest.layers {
        media::layer_compression(&l.media_type)?;
    }
    let config_bytes = ingest_metadata(store, src, &manifest.config)?;
    let config: ImageConfig = json("image config", &config_bytes)?;
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
    for l in &manifest.layers {
        ingest(store, src, l)?;
    }
    Ok(ResolvedImage {
        platform,
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
            .find(|e| tags(e).iter().any(|t| t == want))
            .ok_or_else(|| OciError::RefNotFound {
                wanted: want.to_string(),
                available: entries.iter().flat_map(tags).collect(),
            })?,
        None => match entries.as_slice() {
            [one] => one,
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
    let platform = config.platform();
    platforms
        .iter()
        .map(|want| {
            if !want.matches(&platform) {
                return Err(OciError::MissingPlatform {
                    wanted: want.to_string(),
                    available: vec![platform.to_string()],
                });
            }
            Ok(ResolvedImage {
                platform: platform.clone(),
                manifest_digest: manifest_digest.clone(),
                manifest: manifest.clone(),
                config_digest: config_digest.clone(),
                config: config.clone(),
                ref_name: tags(entry).into_iter().next(),
            })
        })
        .collect()
}
```

- [ ] **Step 5: Write the test layout builder**

`crates/kiln-oci/src/testlayout.rs`:
```rust
//! Builds OCI image layouts and `docker save` archives for tests and fixtures.
//! Not for production use. Callers supply already-compressed layer blobs.

use std::fs;
use std::path::{Path, PathBuf};

use kiln_store::Digest;

use crate::media::{DOCKER_MANIFEST, OCI_CONFIG, OCI_INDEX, OCI_MANIFEST};
use crate::platform::Platform;
use crate::resolve::REF_NAME;
use crate::types::{ContainerConfig, Descriptor, ImageConfig, ImageIndex, ImageManifest, RootFs, canonical_json};

/// One layer: its media type, blob bytes (as stored) and uncompressed tar digest.
#[derive(Debug, Clone)]
pub struct TestLayer {
    pub media_type: String,
    pub blob: Vec<u8>,
    pub diff_id: Digest,
}

impl TestLayer {
    /// An uncompressed tar layer.
    pub fn tar(tar: Vec<u8>) -> Self {
        Self {
            media_type: crate::media::OCI_LAYER_TAR.into(),
            diff_id: Digest::of(&tar),
            blob: tar,
        }
    }
}

/// Writes an OCI image layout directory.
pub struct LayoutBuilder {
    dir: PathBuf,
    index: Vec<Descriptor>,
}

impl LayoutBuilder {
    pub fn new(dir: &Path) -> Self {
        fs::create_dir_all(dir.join("blobs/sha256")).unwrap();
        fs::write(dir.join("oci-layout"), br#"{"imageLayoutVersion":"1.0.0"}"#).unwrap();
        Self {
            dir: dir.to_path_buf(),
            index: Vec::new(),
        }
    }

    /// Writes a blob and returns its digest.
    pub fn blob(&self, bytes: &[u8]) -> Digest {
        let d = Digest::of(bytes);
        fs::write(self.dir.join("blobs/sha256").join(d.hex()), bytes).unwrap();
        d
    }

    /// Writes config and manifest for one platform; returns the manifest descriptor.
    pub fn image(&self, platform: &Platform, layers: &[TestLayer], config: ContainerConfig) -> Descriptor {
        let cfg = ImageConfig {
            architecture: platform.architecture.clone(),
            os: platform.os.clone(),
            variant: platform.variant.clone(),
            config: Some(config),
            rootfs: RootFs {
                fs_type: "layers".into(),
                diff_ids: layers.iter().map(|l| l.diff_id.clone()).collect(),
            },
        };
        let cfg_bytes = canonical_json(&cfg);
        let config_desc = Descriptor::new(OCI_CONFIG, self.blob(&cfg_bytes), cfg_bytes.len() as u64);
        let layer_descs = layers
            .iter()
            .map(|l| Descriptor::new(&l.media_type, self.blob(&l.blob), l.blob.len() as u64))
            .collect();
        let manifest = ImageManifest {
            schema_version: 2,
            media_type: Some(OCI_MANIFEST.into()),
            artifact_type: None,
            config: config_desc,
            layers: layer_descs,
            annotations: None,
        };
        let bytes = canonical_json(&manifest);
        let mut d = Descriptor::new(OCI_MANIFEST, self.blob(&bytes), bytes.len() as u64);
        d.platform = Some(platform.clone());
        d
    }

    /// Adds a top-level `index.json` entry, optionally named.
    pub fn add(&mut self, mut desc: Descriptor, ref_name: Option<&str>) -> &mut Self {
        if let Some(n) = ref_name {
            desc.annotations = Some([(REF_NAME.to_string(), n.to_string())].into());
        }
        self.index.push(desc);
        self
    }

    /// Writes a nested multi-platform index; returns its descriptor.
    pub fn multiarch(&self, manifests: Vec<Descriptor>) -> Descriptor {
        let idx = ImageIndex {
            schema_version: 2,
            media_type: Some(OCI_INDEX.into()),
            artifact_type: None,
            manifests,
            annotations: None,
        };
        let bytes = canonical_json(&idx);
        Descriptor::new(OCI_INDEX, self.blob(&bytes), bytes.len() as u64)
    }

    /// Writes `index.json`; returns the layout directory.
    pub fn finish(&self) -> PathBuf {
        let idx = ImageIndex {
            schema_version: 2,
            media_type: Some(OCI_INDEX.into()),
            artifact_type: None,
            manifests: self.index.clone(),
            annotations: None,
        };
        fs::write(self.dir.join("index.json"), canonical_json(&idx)).unwrap();
        self.dir.clone()
    }
}

/// Writes a legacy `docker save` archive (`manifest.json`, config file, `<id>/layer.tar`).
pub fn docker_legacy_archive(
    path: &Path,
    platform: &Platform,
    layer_tars: &[Vec<u8>],
    config: ContainerConfig,
    tag: &str,
) {
    let cfg = ImageConfig {
        architecture: platform.architecture.clone(),
        os: platform.os.clone(),
        variant: None,
        config: Some(config),
        rootfs: RootFs {
            fs_type: "layers".into(),
            diff_ids: layer_tars.iter().map(|t| Digest::of(t)).collect(),
        },
    };
    let cfg_bytes = canonical_json(&cfg);
    let cfg_name = format!("{}.json", Digest::of(&cfg_bytes).hex());
    let mut b = tar::Builder::new(Vec::new());
    let mut add = |name: &str, data: &[u8]| {
        let mut h = tar::Header::new_ustar();
        h.set_path(name).unwrap();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        b.append(&h, data).unwrap();
    };
    add(&cfg_name, &cfg_bytes);
    let mut layers = Vec::new();
    for (i, t) in layer_tars.iter().enumerate() {
        let name = format!("{i:064}/layer.tar");
        add(&name, t);
        layers.push(name);
    }
    let manifest = serde_json::json!([{ "Config": cfg_name, "RepoTags": [tag], "Layers": layers }]);
    add("manifest.json", &serde_json::to_vec(&manifest).unwrap());
    fs::write(path, b.into_inner().unwrap()).unwrap();
    let _ = DOCKER_MANIFEST;
}
```

- [ ] **Step 6: Wire the modules into `lib.rs` and add the JSON helper resolution uses**

In `crates/kiln-oci/src/lib.rs`, replace:
```rust
mod platform;
mod types;
```
with:
```rust
mod platform;
mod resolve;
mod source;
#[doc(hidden)]
pub mod testlayout;
mod types;
```

In `crates/kiln-oci/src/lib.rs`, replace:
```rust
pub use platform::Platform;
pub use types::{ContainerConfig, Descriptor, ImageConfig, ImageIndex, ImageManifest, RootFs, canonical_json};
```
with:
```rust
pub use platform::Platform;
pub use resolve::{CONTAINERD_NAME, LocalSource, REF_NAME, ResolvedImage, resolve_local};
pub use source::{BlobSource, DirLayout, TarArchive};
pub use types::{ContainerConfig, Descriptor, ImageConfig, ImageIndex, ImageManifest, RootFs, canonical_json};
```

In `crates/kiln-oci/src/error.rs`, replace:
```rust
pub type Result<T> = std::result::Result<T, OciError>;
```
with:
```rust
pub type Result<T> = std::result::Result<T, OciError>;

pub(crate) fn json<T: serde::de::DeserializeOwned>(what: &str, bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|source| OciError::Json {
        what: what.to_string(),
        source,
    })
}
```

- [ ] **Step 7: Run the tests**

Run: `cargo test -q -p kiln-oci`
Expected: all pass (21 tests).

- [ ] **Step 8: Format, lint and run the whole workspace**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace -q`
Expected: no clippy warnings; every test passes.

- [ ] **Step 9: Commit**

```bash
git add crates/kiln-oci Cargo.lock
git commit -m "feat(kiln-oci): verified OCI layout and docker-save inputs" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P7rdQAESQYNEo2qFobUNxt"
```


### Task 5: `kiln-erofs` carry-forward: inheritance at scale, blank numeric tar fields

**Files:**
- Create: `crates/kiln-erofs/tests/inherit_scale.rs`
- Modify: `crates/kiln-erofs/src/merge.rs`, `crates/kiln-erofs/src/tarstream.rs`

**Interfaces:**
- Consumes: `kiln_erofs::{LayerWriter, Image, resolve_inherited, Limits}` and `testtar` from M1a.
- Produces: no API change.
  - `resolve_inherited` reads each lower directory once per call, through a `(layer, nid)` cache, instead of once per implicit path.
  - Tar header numeric fields (mode, uid, gid, mtime, device major and minor) that hold only NULs and spaces read as 0, matching Go's `archive/tar`, which containerd uses.

- [ ] **Step 1: Write the failing scale test**

M1a measured 3.3 ms per implicit path against a 100K-entry lower root: the lookup re-read the whole directory for every path. A layer that touches files under many sibling directories of a large lower directory (`node_modules`, `/usr/share/...`) would take seconds to finalise.

`crates/kiln-erofs/tests/inherit_scale.rs`:
```rust
//! `resolve_inherited` reads each lower directory once, however many implicit
//! paths are resolved under it (M1a carry-forward: it re-read them per path).

use std::collections::BTreeMap;
use std::io::Cursor;
use std::time::{Duration, Instant};

use kiln_erofs::testtar::{Opts, TarBuilder};
use kiln_erofs::{Image, LayerWriter, Limits, resolve_inherited};

fn image(tar: &[u8]) -> Image<Cursor<Vec<u8>>> {
    let spill = tempfile::tempdir().unwrap();
    let mut w = LayerWriter::new(Cursor::new(Vec::new()), spill.path(), Limits::default()).unwrap();
    w.append_tar(tar).unwrap();
    let (out, _) = w.finish(&BTreeMap::new()).unwrap();
    Image::open(out).unwrap()
}

#[test]
fn many_paths_under_a_large_lower_directory() {
    let mut lower = TarBuilder::new();
    lower.dir("big", &Opts::default().mode(0o755));
    for i in 0..20_000 {
        lower.dir(&format!("big/d{i}"), &Opts::default().mode(0o700).uid(i));
    }
    let mut lowers = vec![image(&lower.finish())];
    let paths: Vec<Vec<u8>> = (0..20_000)
        .step_by(10)
        .map(|i| format!("big/d{i}").into_bytes())
        .collect();

    let start = Instant::now();
    let got = resolve_inherited(&mut lowers, &paths).unwrap();
    let took = start.elapsed();

    assert_eq!(got.len(), paths.len());
    assert_eq!(got[b"big/d10".as_slice()].meta.uid, 10);
    assert_eq!(got[b"big/d10".as_slice()].meta.mode, 0o700);
    // Re-reading `big` per path took about 10 s in a debug build; one read takes milliseconds.
    assert!(took < Duration::from_secs(2), "took {took:?}");
}
```

- [ ] **Step 2: Run it to see it fail**

Run: `cargo test -q -p kiln-erofs --test inherit_scale`
Expected: FAIL: `took 8.8s` or similar (the assertion allows 2 s).

- [ ] **Step 3: Cache parsed directories in the merged lookup**

In `crates/kiln-erofs/src/merge.rs`, replace:
```rust
use std::collections::{BTreeMap, HashMap, VecDeque};
```
with:
```rust
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, VecDeque};
```

In `crates/kiln-erofs/src/merge.rs`, replace:
```rust
    let mut out = BTreeMap::new();
    for path in paths {
        let Some((layer, nid)) = merged_lookup(lowers, path)? else {
            continue;
```
with:
```rust
    let mut out = BTreeMap::new();
    let mut dirs = DirCache::new();
    for path in paths {
        let Some((layer, nid)) = merged_lookup(lowers, &mut dirs, path)? else {
            continue;
```

In `crates/kiln-erofs/src/merge.rs`, replace:
```rust
/// The topmost `(layer, nid)` providing `path` in the overlay of `layers`.
fn merged_lookup<R: Read + Seek>(layers: &mut [Image<R>], path: &[u8]) -> Result<Option<(usize, u64)>> {
    let comps: Vec<&[u8]> = components(path).collect();
```
with:
```rust
/// Parsed directories by `(layer, nid)`, so resolving many paths reads each directory once.
type DirCache = HashMap<(usize, u64), HashMap<Vec<u8>, u64>>;

fn child_nid<R: Read + Seek>(
    img: &mut Image<R>,
    dirs: &mut DirCache,
    layer: usize,
    dir: u64,
    name: &[u8],
) -> Result<Option<u64>> {
    let entries = match dirs.entry((layer, dir)) {
        Entry::Occupied(e) => e.into_mut(),
        Entry::Vacant(e) => e.insert(img.read_dir(dir)?.into_iter().map(|e| (e.name, e.nid)).collect()),
    };
    Ok(entries.get(name).copied())
}

/// The topmost `(layer, nid)` providing `path` in the overlay of `layers`.
fn merged_lookup<R: Read + Seek>(
    layers: &mut [Image<R>],
    dirs: &mut DirCache,
    path: &[u8],
) -> Result<Option<(usize, u64)>> {
    let comps: Vec<&[u8]> = components(path).collect();
```

In `crates/kiln-erofs/src/merge.rs`, replace:
```rust
        for (i, c) in comps.iter().enumerate() {
            let Some(entry) = img.read_dir(cur)?.into_iter().find(|e| e.name == *c) else {
                break;
            };
            let info = img.inode(entry.nid)?;
            if info.is_whiteout() {
```
with:
```rust
        for (i, c) in comps.iter().enumerate() {
            let Some(nid) = child_nid(img, dirs, layer, cur, c)? else {
                break;
            };
            let info = img.inode(nid)?;
            if info.is_whiteout() {
```

In `crates/kiln-erofs/src/merge.rs`, replace:
```rust
            if i + 1 == comps.len() {
                return Ok(Some((layer, entry.nid)));
            }
```
with:
```rust
            if i + 1 == comps.len() {
                return Ok(Some((layer, nid)));
            }
```

In `crates/kiln-erofs/src/merge.rs`, replace:
```rust
            }
            if is_opaque(&img.xattrs(entry.nid)?) {
                opaque_above = true;
            }
            cur = entry.nid;
        }
```
with:
```rust
            }
            if is_opaque(&img.xattrs(nid)?) {
                opaque_above = true;
            }
            cur = nid;
        }
```

- [ ] **Step 4: Run the scale test**

Run: `cargo test -q -p kiln-erofs --test inherit_scale`
Expected: PASS in well under a second.

- [ ] **Step 5: Read blank numeric fields as 0, like Go**

Go's `archive/tar` trims NULs and spaces from octal fields and reads an empty result as 0; the `tar` crate errors. Some non-Go tools write blank `uid`/`gid`/`mtime` fields, so kiln rejected images containerd accepts. The size field is parsed by the `tar` crate's iterator itself and stays strict; an unparseable non-blank field is still `MalformedTar` (`malformed_numeric_fields_are_still_rejected`). The edits add the helper, use it for each field, and add two unit tests.

In `crates/kiln-erofs/src/tarstream.rs`, replace:
```rust
fn to_u32(v: u64, what: &'static str, path: &[u8]) -> Result<u32> {
```
with:
```rust
/// A numeric header field at `range`. Go's archive/tar (containerd) reads a field of
/// only NULs and spaces as 0, where the tar crate fails; match Go.
fn numeric<T: Default>(header: &Header, range: std::ops::Range<usize>, parsed: io::Result<T>) -> Result<T> {
    if header.as_bytes()[range].iter().all(|&b| b == 0 || b == b' ') {
        return Ok(T::default());
    }
    parsed.map_err(tar_err)
}

fn to_u32(v: u64, what: &'static str, path: &[u8]) -> Result<u32> {
```

In `crates/kiln-erofs/src/tarstream.rs`, replace:
```rust
    }
    let uid = to_u32(pax.uid.map_or_else(|| header.uid(), Ok).map_err(tar_err)?, "uid", &path)?;
    let gid = to_u32(pax.gid.map_or_else(|| header.gid(), Ok).map_err(tar_err)?, "gid", &path)?;
    let mtime = match pax.mtime {
```
with:
```rust
    }
    let uid = match pax.uid {
        Some(v) => v,
        None => numeric(header, 108..116, header.uid())?,
    };
    let gid = match pax.gid {
        Some(v) => v,
        None => numeric(header, 116..124, header.gid())?,
    };
    let (uid, gid) = (to_u32(uid, "uid", &path)?, to_u32(gid, "gid", &path)?);
    let mtime = match pax.mtime {
```

In `crates/kiln-erofs/src/tarstream.rs`, replace:
```rust
        None => Timestamp {
            sec: header.mtime().map_err(tar_err)? as i64,
            nsec: 0,
```
with:
```rust
        None => Timestamp {
            sec: numeric(header, 136..148, header.mtime())? as i64,
            nsec: 0,
```

In `crates/kiln-erofs/src/tarstream.rs`, replace:
```rust
    let meta = Meta {
        mode: header.mode().map_err(tar_err)? & 0o7777,
        uid,
```
with:
```rust
    let meta = Meta {
        mode: numeric(header, 100..108, header.mode())? & 0o7777,
        uid,
```

In `crates/kiln-erofs/src/tarstream.rs`, replace:
```rust
    let device = || -> Result<(u32, u32)> {
        let major = header.device_major().map_err(tar_err)?.unwrap_or(0);
        let minor = header.device_minor().map_err(tar_err)?.unwrap_or(0);
        if major > 0xfff || minor > 0xf_ffff {
```
with:
```rust
    let device = || -> Result<(u32, u32)> {
        let major = numeric(header, 329..337, header.device_major())?.unwrap_or(0);
        let minor = numeric(header, 337..345, header.device_minor())?.unwrap_or(0);
        if major > 0xfff || minor > 0xf_ffff {
```

In `crates/kiln-erofs/src/tarstream.rs`, replace:
```rust
        Ok((out, warnings, n))
    }
```
with:
```rust
        Ok((out, warnings, n))
    }

    #[test]
    fn empty_numeric_fields_read_as_zero_like_go() {
        let mut h = tar::Header::new_ustar();
        h.set_path("dev/x").unwrap();
        h.set_size(0);
        h.set_entry_type(tar::EntryType::Char);
        for r in [100..108, 108..116, 116..124, 136..148, 329..337, 337..345] {
            h.as_mut_bytes()[r].fill(0);
        }
        h.as_mut_bytes()[108..116].copy_from_slice(b"        ");
        h.set_cksum();
        let mut tar = h.as_bytes().to_vec();
        tar.resize(tar.len() + 1024, 0);
        let (es, _, _) = collect(&tar, &Limits::default()).unwrap();
        let m = &es[0].0.meta;
        assert_eq!((m.mode, m.uid, m.gid, m.mtime.sec), (0, 0, 0, 0));
        assert!(matches!(es[0].0.kind, EntryKind::CharDev { major: 0, minor: 0 }));
    }

    #[test]
    fn malformed_numeric_fields_are_still_rejected() {
        let mut h = tar::Header::new_ustar();
        h.set_path("f").unwrap();
        h.set_size(0);
        h.as_mut_bytes()[108..116].copy_from_slice(b"12x4567\0");
        h.set_cksum();
        let mut tar = h.as_bytes().to_vec();
        tar.resize(tar.len() + 1024, 0);
        assert!(matches!(collect(&tar, &Limits::default()), Err(Error::MalformedTar(_))));
    }
```

- [ ] **Step 6: Run the crate's tests**

Run: `cargo test -q -p kiln-erofs`
Expected: all pass, including `empty_numeric_fields_read_as_zero_like_go` and the existing golden digests (output bytes do not change).

- [ ] **Step 7: Re-run the Linux kernel and oracle tests**

These need Linux and root. On macOS with Docker Desktop:

```bash
docker run --rm -v "$PWD":/src -w /src/tools/oracle golang:latest go build -buildvcs=false -o /src/target/oracle-linux .
docker run --rm --privileged --tmpfs /tmp:exec -v "$PWD":/src -w /src -e CARGO_TARGET_DIR=/tmp/t \
  -e KILN_KERNEL_TESTS=1 -e KILN_ORACLE=/src/target/oracle-linux -e KILN_REQUIRE_LINUX_TESTS=1 rust:latest bash -c '
  for i in $(seq 0 63); do [ -e /dev/loop$i ] || mknod /dev/loop$i b 7 $i; done
  apt-get -qq update >/dev/null && DEBIAN_FRONTEND=noninteractive apt-get -qq install -y erofs-utils >/dev/null 2>&1
  cargo test -q -p kiln-erofs --test kernel --test oracle -- --test-threads=1'
```
Expected: both test binaries pass. CI's `linux-kernel` job runs the same tests on every push.

- [ ] **Step 8: Format, lint and run the whole workspace**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace -q`
Expected: no clippy warnings; every test passes.

- [ ] **Step 9: Commit**

```bash
git add crates/kiln-erofs Cargo.lock
git commit -m "fix(kiln-erofs): resolve inherited parents in one pass per directory; read blank tar numeric fields as 0" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P7rdQAESQYNEo2qFobUNxt"
```


### Task 6: `kiln-image`: the verified conversion pipeline

**Files:**
- Create: `crates/kiln-image/Cargo.toml`, `crates/kiln-image/src/lib.rs`, `crates/kiln-image/src/error.rs`, `crates/kiln-image/src/types.rs`, `crates/kiln-image/src/decompress.rs`, `crates/kiln-image/src/ctx.rs`, `crates/kiln-image/src/convert.rs`, `crates/kiln-image/src/pipeline.rs`, `crates/kiln-image/src/load.rs`, `crates/kiln-image/tests/common/mod.rs`, `crates/kiln-image/tests/convert.rs`, `crates/kiln-image/tests/hostile.rs`

**Interfaces:**
- Consumes: `kiln_store` (Tasks 1–2), `kiln_oci::{resolve_local, LocalSource, ResolvedImage, Descriptor, ImageIndex, ImageManifest, Platform, canonical_json, media}` and `testlayout` (Tasks 3–4), `kiln_erofs::{LayerWriter, Image, resolve_inherited, squash, Limits, DirAttrs, FORMAT_VERSION}`.
- Produces:
  - `types`: `KilnConfig { schema_version, architecture, process: Process, kernel: Option<KernelRef>, init: Option<InitRef>, source: SourceRef, erofs_format_version }`, `Process { entrypoint, cmd, env, working_dir, user, stop_signal }` (`from_oci`), `SourceRef { manifest_digest, reference }`, and the constants `KILN_ARTIFACT`, `KILN_CONFIG`, `KILN_LAYER`, `KILN_KERNEL`, `KILN_INIT`, `ANN_SOURCE_DIGESTS`, `ANN_INHERITS`, `SCHEMA_VERSION`.
  - `ImageError` variants: `Store`, `Oci`, `Erofs`, `Io`, `Json { what, source }`, `DiffIdMismatch { layer, expected, actual }`, `LimitExceeded { what, max }`, `UnsupportedPlatform(String)`, `TrailingData { layer }`, `NotAKilnImage(Digest)`, `UnknownSchema(u32)`, `RefNotFound(String)`, `BadOption(String)`.
  - `ConvertOptions { max_layers: 10, limits: Limits::default(), max_image_bytes: 64 GiB, max_expansion_ratio: 200, jobs: available CPUs }`.
  - `convert_image(&Store, &ResolvedImage, reference: Option<&str>, &ConvertOptions) -> Result<Converted>`; `Converted { platform, manifest_digest, manifest_size, layers: Vec<LayerReport>, squashed }`; `LayerReport { sources, erofs, size, cached, inherits, warnings }`.
  - `convert_resolved(&Store, &[ResolvedImage], reference, &ConvertOptions) -> Result<Output>`; `convert_local(&Store, &Path, &LocalRequest, &ConvertOptions) -> Result<Output>` (holds the shared store lock; tags with `LocalRequest::tag`); `LocalRequest { source_ref, platforms, tag }`; `Output { digest, media_type, images }`.
  - `load(&Store, &Digest) -> Result<Loaded>`, `load_manifest`, `resolve_name(&Store, name) -> Result<Digest>`; `Loaded { digest, is_index, entries: Vec<(Platform, KilnManifest)> }`; `KilnManifest { digest, manifest, config }`.

- [ ] **Step 1: Create the crate manifest**

`crates/kiln-image/Cargo.toml`:
```toml
[package]
name = "kiln-image"
version = "0.1.0"
edition.workspace = true
license.workspace = true
description = "kiln image format and the verified OCI-to-erofs conversion pipeline"

[dependencies]
flate2 = "1.1.10"
kiln-erofs = { path = "../kiln-erofs" }
kiln-oci = { path = "../kiln-oci" }
kiln-store = { path = "../kiln-store" }
serde = { version = "1.0.229", features = ["derive"] }
serde_json = "1.0.151"
thiserror = "2.0.21"
zstd = "0.14.0"

[dev-dependencies]
tempfile = "3.27.0"
```

- [ ] **Step 2: Write the failing integration tests**

`tests/common/mod.rs` builds layouts with a base layer (`tmp/` mode 1777, `etc/os-release`) and a top layer that writes `tmp/x` with no `tmp/` header, so the top layer must inherit 1777 from below. `convert.rs` covers inheritance, warm caching, a changed base, concurrency, determinism across stores and job counts, squash, multi-arch and platform checks. `hostile.rs` is spec §11.5's local half: each input must be rejected **with no cache entry and no ref**.

`crates/kiln-image/tests/common/mod.rs`:
```rust
//! Shared helpers: build OCI layouts and inspect results.
#![allow(dead_code)]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use flate2::write::GzEncoder;
use kiln_erofs::Image;
use kiln_erofs::testtar::{Opts, TarBuilder};
use kiln_image::LocalRequest;
use kiln_oci::testlayout::{LayoutBuilder, TestLayer};
use kiln_oci::{ContainerConfig, Platform, media};
use kiln_store::{Digest, Store};

pub fn gz(tar: &[u8]) -> TestLayer {
    let mut e = GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(tar).unwrap();
    TestLayer {
        media_type: media::OCI_LAYER_GZIP.into(),
        blob: e.finish().unwrap(),
        diff_id: Digest::of(tar),
    }
}

pub fn zst(tar: &[u8]) -> TestLayer {
    TestLayer {
        media_type: media::OCI_LAYER_ZSTD.into(),
        blob: zstd::encode_all(tar, 3).unwrap(),
        diff_id: Digest::of(tar),
    }
}

pub fn arm() -> Platform {
    Platform::parse("linux/arm64").unwrap()
}

/// Base: `tmp/` 1777 owned by 0, `etc/os-release`. Top: `tmp/x` with no `tmp/` header.
pub fn base(tmp_mode: u32) -> Vec<u8> {
    TarBuilder::new()
        .dir("tmp", &Opts::default().mode(tmp_mode))
        .dir("etc", &Opts::default().mode(0o755))
        .file("etc/os-release", b"ID=test\n", &Opts::default())
        .finish()
}

pub fn top() -> Vec<u8> {
    TarBuilder::new().file("tmp/x", b"x", &Opts::default()).finish()
}

pub fn layout(dir: &Path, platforms: &[Platform], layers: &[TestLayer]) -> PathBuf {
    let mut b = LayoutBuilder::new(dir);
    let cfg = ContainerConfig {
        cmd: Some(vec!["php".into(), "-v".into()]),
        working_dir: Some("/app".into()),
        ..Default::default()
    };
    let descs: Vec<_> = platforms.iter().map(|p| b.image(p, layers, cfg.clone())).collect();
    let top = if let [one] = descs.as_slice() {
        one.clone()
    } else {
        b.multiarch(descs)
    };
    b.add(top, Some("app")).finish()
}

pub fn req<'a>(platforms: &'a [Platform], tag: &'a str) -> LocalRequest<'a> {
    LocalRequest {
        source_ref: None,
        platforms,
        tag: Some(tag),
    }
}

pub fn cache_entries(store: &Store) -> usize {
    ["cache/layers", "cache/layers-ctx", "cache/squash"]
        .iter()
        .map(|d| fs::read_dir(store.root().join(d)).unwrap().count())
        .sum()
}

pub fn mode_of(store: &Store, erofs: &Digest, path: &[u8]) -> u32 {
    let mut img = Image::open(store.open_blob(erofs).unwrap()).unwrap();
    let nid = img.lookup(path).unwrap().expect("path present");
    img.inode(nid).unwrap().mode & 0o7777
}
```

`crates/kiln-image/tests/convert.rs`:
```rust
mod common;

use common::*;
use kiln_erofs::Image;
use kiln_erofs::testtar::{Opts, TarBuilder};
use kiln_image::types::{ANN_INHERITS, ANN_SOURCE_DIGESTS, KILN_ARTIFACT};
use kiln_image::{ConvertOptions, ImageError, convert_local, load, resolve_name};
use kiln_oci::testlayout::TestLayer;
use kiln_oci::{Platform, media};
use kiln_store::{Digest, Store};

#[test]
fn converts_a_layout_with_inherited_parents() {
    let src = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    let path = layout(src.path(), &[arm()], &[gz(&base(0o1777)), zst(&top())]);

    let out = convert_local(&store, &path, &req(&[arm()], "app:1"), &ConvertOptions::default()).unwrap();
    assert_eq!(out.media_type, media::OCI_MANIFEST);
    assert_eq!(store.get_ref("app:1").unwrap(), Some(out.digest.clone()));
    let c = &out.images[0];
    assert_eq!(c.layers.len(), 2);
    assert!(c.layers.iter().all(|l| !l.cached));
    assert!(!c.layers[0].inherits && c.layers[1].inherits);
    // overlayfs shows the upper dir's attributes, so the top layer must carry the base's 1777.
    assert_eq!(mode_of(&store, &c.layers[1].erofs, b"tmp"), 0o1777);

    let loaded = load(&store, &out.digest).unwrap();
    let (platform, m) = &loaded.entries[0];
    assert_eq!(platform.architecture, "arm64");
    assert_eq!(m.manifest.artifact_type.as_deref(), Some(KILN_ARTIFACT));
    assert_eq!(m.config.process.cmd, vec!["php", "-v"]);
    assert_eq!(m.config.process.working_dir.as_deref(), Some("/app"));
    assert_eq!(m.config.source.reference, None);
    assert_eq!(m.manifest.layers[1].annotation(ANN_INHERITS), Some("true"));
    assert_eq!(m.manifest.layers[0].annotation(ANN_INHERITS), None);
    let src_layers = &kiln_oci::resolve_local(&store, &kiln_oci::LocalSource::detect(&path).unwrap(), None, &[arm()])
        .unwrap()[0]
        .manifest
        .layers;
    assert_eq!(
        m.manifest.layers[1].annotation(ANN_SOURCE_DIGESTS),
        Some(src_layers[1].digest.to_string().as_str())
    );
}

#[test]
fn second_convert_is_fully_cached_and_identical() {
    let src = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    let path = layout(src.path(), &[arm()], &[gz(&base(0o1777)), gz(&top())]);
    let a = convert_local(&store, &path, &req(&[arm()], "a"), &ConvertOptions::default()).unwrap();
    let b = convert_local(&store, &path, &req(&[arm()], "b"), &ConvertOptions::default()).unwrap();
    assert_eq!(a.digest, b.digest);
    assert!(b.images[0].layers.iter().all(|l| l.cached));
}

#[test]
fn a_changed_base_reconverts_the_inheriting_layer() {
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    let (s1, s2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let p1 = layout(s1.path(), &[arm()], &[gz(&base(0o1777)), gz(&top())]);
    let p2 = layout(s2.path(), &[arm()], &[gz(&base(0o755)), gz(&top())]);
    let a = convert_local(&store, &p1, &req(&[arm()], "a"), &ConvertOptions::default()).unwrap();
    let b = convert_local(&store, &p2, &req(&[arm()], "b"), &ConvertOptions::default()).unwrap();
    let (la, lb) = (&a.images[0].layers, &b.images[0].layers);
    assert_eq!(la[1].sources, lb[1].sources, "same top layer blob");
    assert_ne!(la[1].erofs, lb[1].erofs, "different inherited context");
    assert!(!lb[1].cached);
    assert_eq!(mode_of(&store, &lb[1].erofs, b"tmp"), 0o755);
    // Both contexts stay cached.
    let again = convert_local(&store, &p1, &req(&[arm()], "a"), &ConvertOptions::default()).unwrap();
    assert!(again.images[0].layers.iter().all(|l| l.cached));
    assert_eq!(again.digest, a.digest);
}

#[test]
fn concurrent_converts_into_one_store_agree() {
    let src = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let path = layout(src.path(), &[arm()], &[gz(&base(0o1777)), gz(&top())]);
    let digests: Vec<Digest> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..4)
            .map(|i| {
                let (path, root) = (&path, home.path());
                s.spawn(move || {
                    let store = Store::open(root).unwrap();
                    let tag = format!("t{i}");
                    convert_local(&store, path, &req(&[arm()], &tag), &ConvertOptions::default())
                        .unwrap()
                        .digest
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert!(digests.windows(2).all(|w| w[0] == w[1]));
    let store = Store::open(home.path()).unwrap();
    assert_eq!(store.refs().unwrap().len(), 4, "no lost refs.json update");
}

#[test]
fn output_is_deterministic_across_stores_and_job_counts() {
    let src = tempfile::tempdir().unwrap();
    let layers: Vec<TestLayer> = (0..6)
        .map(|i| {
            gz(&TarBuilder::new()
                .file(&format!("d{i}/f"), &[i as u8; 3000], &Opts::default())
                .finish())
        })
        .collect();
    let path = layout(src.path(), &[arm()], &layers);
    let mut digests = Vec::new();
    for jobs in [1, 8] {
        let home = tempfile::tempdir().unwrap();
        let store = Store::open(home.path()).unwrap();
        let opts = ConvertOptions {
            jobs,
            ..Default::default()
        };
        digests.push(convert_local(&store, &path, &req(&[arm()], "x"), &opts).unwrap().digest);
    }
    assert_eq!(digests[0], digests[1]);
}

#[test]
fn squashes_bottom_layers_above_max_layers() {
    let src = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    let l0 = TarBuilder::new()
        .file("a", b"a", &Opts::default())
        .file("gone", b"g", &Opts::default())
        .finish();
    let l1 = TarBuilder::new().whiteout("gone").finish();
    let l2 = TarBuilder::new().file("b", b"b", &Opts::default()).finish();
    let l3 = TarBuilder::new().file("c", b"c", &Opts::default()).finish();
    let path = layout(src.path(), &[arm()], &[gz(&l0), gz(&l1), gz(&l2), gz(&l3)]);
    let opts = ConvertOptions {
        max_layers: 2,
        ..Default::default()
    };
    let out = convert_local(&store, &path, &req(&[arm()], "s"), &opts).unwrap();
    let c = &out.images[0];
    assert_eq!((c.layers.len(), c.squashed), (2, 3));
    assert_eq!(c.layers[0].sources.len(), 3);
    let mut img = Image::open(store.open_blob(&c.layers[0].erofs).unwrap()).unwrap();
    assert!(img.lookup(b"a").unwrap().is_some() && img.lookup(b"b").unwrap().is_some());
    assert!(img.lookup(b"gone").unwrap().is_none(), "whiteout applied");
    assert!(img.lookup(b".wh.gone").unwrap().is_none(), "and removed");
    let again = convert_local(&store, &path, &req(&[arm()], "s"), &opts).unwrap();
    assert!(again.images[0].layers[0].cached);
    assert_eq!(again.digest, out.digest);
}

#[test]
fn multiarch_produces_a_sorted_index() {
    let src = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    let amd = Platform::parse("linux/amd64").unwrap();
    let path = layout(src.path(), &[arm(), amd.clone()], &[gz(&base(0o1777))]);
    let out = convert_local(&store, &path, &req(&[arm(), amd], "m"), &ConvertOptions::default()).unwrap();
    assert_eq!(out.media_type, media::OCI_INDEX);
    let loaded = load(&store, &out.digest).unwrap();
    assert!(loaded.is_index);
    let archs: Vec<_> = loaded.entries.iter().map(|(p, _)| p.architecture.as_str()).collect();
    assert_eq!(archs, ["amd64", "arm64"]);
    assert_eq!(resolve_name(&store, "m").unwrap(), out.digest);
    assert_eq!(resolve_name(&store, &out.digest.to_string()).unwrap(), out.digest);
    assert!(matches!(resolve_name(&store, "nope"), Err(ImageError::RefNotFound(_))));
}

#[test]
fn rejects_unsupported_platforms() {
    let src = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    let rv = Platform::parse("linux/riscv64").unwrap();
    let path = layout(src.path(), std::slice::from_ref(&rv), &[gz(&base(0o755))]);
    let err = convert_local(&store, &path, &req(&[rv], "r"), &ConvertOptions::default()).unwrap_err();
    assert!(matches!(err, ImageError::UnsupportedPlatform(_)), "{err}");
}
```

`crates/kiln-image/tests/hostile.rs`:
```rust
//! Spec §11.5: each hostile input is rejected and leaves no cache entry or ref.
mod common;

use common::*;
use kiln_erofs::testtar::{Opts, TarBuilder};
use kiln_image::{ConvertOptions, ImageError, convert_local};
use kiln_oci::testlayout::TestLayer;
use kiln_store::{Digest, Store};

fn rejects(layers: &[TestLayer], opts: ConvertOptions) -> ImageError {
    let src = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    let path = layout(src.path(), &[arm()], layers);
    let err = convert_local(&store, &path, &req(&[arm()], "h"), &opts).unwrap_err();
    assert_eq!(cache_entries(&store), 0, "no cache entry after {err}");
    assert_eq!(store.get_ref("h").unwrap(), None);
    err
}

#[test]
fn diff_id_mismatch() {
    let mut l = gz(&base(0o755));
    l.diff_id = Digest::of(b"something else");
    assert!(matches!(
        rejects(&[l], ConvertOptions::default()),
        ImageError::DiffIdMismatch { layer: 0, .. }
    ));
}

#[test]
fn trailing_junk_after_the_tar_end() {
    let mut tar = base(0o755);
    tar.extend_from_slice(b"junk");
    // The diff_id covers the junk, so only the trailing-data rule catches it.
    let err = rejects(&[gz(&tar)], ConvertOptions::default());
    assert!(matches!(err, ImageError::TrailingData { layer: 0 }), "{err}");
}

#[test]
fn zero_padding_after_the_tar_end_is_fine() {
    let mut tar = base(0o755);
    tar.resize(tar.len() + 8192, 0);
    let src = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    let path = layout(src.path(), &[arm()], &[gz(&tar)]);
    convert_local(&store, &path, &req(&[arm()], "ok"), &ConvertOptions::default()).unwrap();
}

#[test]
fn decompression_bomb() {
    let tar = TarBuilder::new()
        .file("zeros", &vec![0u8; 8 << 20], &Opts::default())
        .finish();
    let err = rejects(&[gz(&tar)], ConvertOptions::default());
    assert!(
        matches!(
            err,
            ImageError::LimitExceeded {
                what: "expansion ratio",
                ..
            }
        ),
        "{err}"
    );
}

#[test]
fn per_image_byte_limit() {
    let l = |n: &str| gz(&TarBuilder::new().file(n, &[7u8; 40_000], &Opts::default()).finish());
    let opts = ConvertOptions {
        max_image_bytes: 60_000,
        ..Default::default()
    };
    let err = rejects(&[l("a"), l("b")], opts);
    assert!(
        matches!(
            err,
            ImageError::LimitExceeded {
                what: "uncompressed bytes per image",
                ..
            }
        ),
        "{err}"
    );
}

#[test]
fn too_many_entries() {
    let mut b = TarBuilder::new();
    for i in 0..50 {
        b.file(&format!("f{i}"), b"", &Opts::default());
    }
    let mut opts = ConvertOptions::default();
    opts.limits.max_entries = 20;
    let err = rejects(&[gz(&b.finish())], opts);
    assert!(
        matches!(err, ImageError::Erofs(kiln_erofs::Error::LimitExceeded { .. })),
        "{err}"
    );
}

#[test]
fn oversized_pax_record() {
    let tar = TarBuilder::new()
        .file("f", b"", &Opts::default().pax("comment", &vec![b'a'; 4096]))
        .finish();
    let mut opts = ConvertOptions::default();
    opts.limits.max_header_record = 1024;
    let err = rejects(&[gz(&tar)], opts);
    assert!(
        matches!(err, ImageError::Erofs(kiln_erofs::Error::LimitExceeded { .. })),
        "{err}"
    );
}

#[test]
fn hardlink_cycle() {
    let tar = TarBuilder::new().hardlink("a", "b").hardlink("b", "a").finish();
    let err = rejects(&[gz(&tar)], ConvertOptions::default());
    assert!(matches!(err, ImageError::Erofs(_)), "{err}");
}

#[test]
fn a_failing_layer_leaves_no_entry_for_its_good_siblings_either() {
    let good = gz(&base(0o755));
    let mut bad = gz(&top());
    bad.diff_id = Digest::of(b"nope");
    // The good layer converts in parallel, but nothing is committed before phase B.
    rejects(&[good, bad], ConvertOptions::default());
}
```

- [ ] **Step 3: Run them to see them fail**

Run: `cargo test -q -p kiln-image`
Expected: compile errors: crate `kiln_image` has no `convert_local`, `load`, ....

- [ ] **Step 4: Write the error type and the image format types**

`types.rs` is spec §5.1 as data. `kernel` and `init` stay `None` until M3, and are omitted from the JSON, so M1b output stays deterministic and M3 can add them without a schema bump.

`crates/kiln-image/src/error.rs`:
```rust
use kiln_store::Digest;
use thiserror::Error;

/// Errors from converting, inspecting or importing kiln images.
#[derive(Debug, Error)]
pub enum ImageError {
    #[error(transparent)]
    Store(#[from] kiln_store::StoreError),
    #[error(transparent)]
    Oci(#[from] kiln_oci::OciError),
    #[error("erofs: {0}")]
    Erofs(#[from] kiln_erofs::Error),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid {what}: {source}")]
    Json {
        what: &'static str,
        source: serde_json::Error,
    },
    #[error("layer {layer}: uncompressed content is {actual}, but the image config says {expected}")]
    DiffIdMismatch {
        layer: usize,
        expected: Digest,
        actual: Digest,
    },
    #[error("limit exceeded: {what} (max {max})")]
    LimitExceeded { what: &'static str, max: u64 },
    #[error("unsupported platform {0} (kiln converts linux/amd64 and linux/arm64)")]
    UnsupportedPlatform(String),
    #[error("layer {layer}: non-zero data after the tar end-of-archive marker")]
    TrailingData { layer: usize },
    #[error("{0} is not a kiln image")]
    NotAKilnImage(Digest),
    #[error("unsupported kiln image schema version {0}")]
    UnknownSchema(u32),
    #[error("no image named {0:?}")]
    RefNotFound(String),
    #[error("invalid option: {0}")]
    BadOption(String),
}

pub type Result<T> = std::result::Result<T, ImageError>;

pub(crate) fn json<T: serde::de::DeserializeOwned>(what: &'static str, bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|source| ImageError::Json { what, source })
}
```

`crates/kiln-image/src/types.rs`:
```rust
//! The kiln image format (spec §5.1).

use kiln_oci::ContainerConfig;
use kiln_store::Digest;
use serde::{Deserialize, Serialize};

pub const KILN_ARTIFACT: &str = "application/vnd.kiln.image.v1";
pub const KILN_CONFIG: &str = "application/vnd.kiln.image.config.v1+json";
pub const KILN_LAYER: &str = "application/vnd.kiln.layer.v1.erofs";
pub const KILN_KERNEL: &str = "application/vnd.kiln.kernel.v1";
pub const KILN_INIT: &str = "application/vnd.kiln.init.v1.erofs";

/// Comma-separated OCI layer digests an erofs layer was built from (informational).
pub const ANN_SOURCE_DIGESTS: &str = "dev.kiln.source.digests";
/// `"true"` on layers converted with inherited parent attributes (informational).
pub const ANN_INHERITS: &str = "dev.kiln.inherits";

pub const SCHEMA_VERSION: u32 = 1;

/// The `kiln` image config blob.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KilnConfig {
    pub schema_version: u32,
    pub architecture: String,
    pub process: Process,
    /// Set by milestone M3, when kernel layers are added.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel: Option<KernelRef>,
    /// Set by milestone M3, when init layers are added.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub init: Option<InitRef>,
    pub source: SourceRef,
    pub erofs_format_version: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Process {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entrypoint: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cmd: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_signal: Option<String>,
}

impl Process {
    pub fn from_oci(c: Option<&ContainerConfig>) -> Self {
        let Some(c) = c else { return Self::default() };
        Self {
            entrypoint: c.entrypoint.clone().unwrap_or_default(),
            cmd: c.cmd.clone().unwrap_or_default(),
            env: c.env.clone().unwrap_or_default(),
            working_dir: c.working_dir.clone().filter(|w| !w.is_empty()),
            user: c.user.clone().filter(|u| !u.is_empty()),
            stop_signal: c.stop_signal.clone().filter(|s| !s.is_empty()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KernelRef {
    pub profile: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitRef {
    pub version: String,
}

/// Where an image came from. `reference` is set only for registry inputs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceRef {
    pub manifest_digest: Digest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
}

/// Lowercase hex of raw bytes (paths and xattrs are not always UTF-8).
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_from_oci_drops_empty_strings() {
        let c = ContainerConfig {
            cmd: Some(vec!["php".into()]),
            working_dir: Some(String::new()),
            user: Some("www-data".into()),
            ..Default::default()
        };
        let p = Process::from_oci(Some(&c));
        assert_eq!(p.cmd, vec!["php"]);
        assert_eq!(p.working_dir, None);
        assert_eq!(p.user.as_deref(), Some("www-data"));
        assert_eq!(Process::from_oci(None), Process::default());
    }

    #[test]
    fn hex_round_trips() {
        assert_eq!(hex(b"\x00\xffa"), "00ff61");
        assert_eq!(unhex("00ff61").unwrap(), b"\x00\xffa");
        assert!(unhex("0").is_none() && unhex("zz").is_none());
    }

    #[test]
    fn config_omits_unset_kernel_and_init() {
        let c = KilnConfig {
            schema_version: 1,
            architecture: "arm64".into(),
            process: Process::default(),
            kernel: None,
            init: None,
            source: SourceRef {
                manifest_digest: Digest::of(b"m"),
                reference: None,
            },
            erofs_format_version: 1,
        };
        let s = String::from_utf8(kiln_oci::canonical_json(&c)).unwrap();
        assert!(
            !s.contains("kernel") && !s.contains("init") && !s.contains("reference"),
            "{s}"
        );
        assert!(s.contains("\"erofsFormatVersion\":1"));
    }
}
```

- [ ] **Step 5: Write bounded decompression**

Per-layer limit: `min(max_layer_bytes, ratio × compressed size)`, with a 1 MiB floor for tiny compressed blobs; for uncompressed layers, the blob size itself. A shared atomic budget enforces the per-image limit across parallel layers. Each `Bounded` reader records which limit tripped, so the pipeline reports `LimitExceeded { what: "expansion ratio" | "uncompressed bytes per layer" | "uncompressed bytes per image" }` instead of the tar parser's I/O error. `MultiGzDecoder` and the `zstd` crate handle multi-member gzip and multi-frame zstd (`zstd:chunked`).

`crates/kiln-image/src/decompress.rs`:
```rust
//! Opening a stored layer as a bounded, hashed tar stream (spec §6.1, §7.6).

use std::io::{self, BufReader, Read};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use kiln_oci::media::Compression;

/// Shared per-image budget of decompressed bytes.
#[derive(Debug, Default)]
pub struct Budget {
    used: AtomicU64,
}

/// Which limit a [`Bounded`] reader tripped.
#[derive(Debug, Default)]
pub struct Tripped {
    layer: AtomicBool,
    image: AtomicBool,
}

impl Tripped {
    pub fn layer(&self) -> bool {
        self.layer.load(Ordering::SeqCst)
    }

    pub fn image(&self) -> bool {
        self.image.load(Ordering::SeqCst)
    }
}

/// Fails reads past `layer_max` bytes, or past the shared image budget.
pub struct Bounded<R> {
    inner: R,
    read: u64,
    layer_max: u64,
    budget: Arc<Budget>,
    image_max: u64,
    tripped: Arc<Tripped>,
}

impl<R: Read> Read for Bounded<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read += n as u64;
        if self.read > self.layer_max {
            self.tripped.layer.store(true, Ordering::SeqCst);
            return Err(io::Error::other("kiln: layer expansion limit exceeded"));
        }
        if self.budget.used.fetch_add(n as u64, Ordering::SeqCst) + n as u64 > self.image_max {
            self.tripped.image.store(true, Ordering::SeqCst);
            return Err(io::Error::other("kiln: image size limit exceeded"));
        }
        Ok(n)
    }
}

/// The most decompressed bytes a layer of `compressed` bytes may produce: the
/// expansion ratio (with a 1 MiB floor for tiny blobs), never above `layer_cap`.
pub fn layer_limit(compression: Compression, compressed: u64, ratio: u64, layer_cap: u64) -> u64 {
    match compression {
        Compression::None => compressed.min(layer_cap),
        _ => compressed.saturating_mul(ratio).max(1 << 20).min(layer_cap),
    }
}

/// Wraps a stored blob in its decompressor and the limits.
pub fn open_bounded<'a>(
    blob: impl Read + 'a,
    compression: Compression,
    layer_max: u64,
    budget: Arc<Budget>,
    image_max: u64,
    tripped: Arc<Tripped>,
) -> io::Result<Bounded<Box<dyn Read + 'a>>> {
    let inner: Box<dyn Read + 'a> = match compression {
        Compression::None => Box::new(blob),
        Compression::Gzip => Box::new(flate2::read::MultiGzDecoder::new(BufReader::new(blob))),
        Compression::Zstd => Box::new(zstd::stream::read::Decoder::new(blob)?),
    };
    Ok(Bounded {
        inner,
        read: 0,
        layer_max,
        budget,
        image_max,
        tripped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn drain(r: &mut dyn Read) -> io::Result<Vec<u8>> {
        let mut v = Vec::new();
        r.read_to_end(&mut v)?;
        Ok(v)
    }

    #[test]
    fn decompresses_gzip_multimember_and_zstd_multiframe() {
        let mut gz = Vec::new();
        for part in [&b"hello "[..], b"world"] {
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(part).unwrap();
            gz.extend(e.finish().unwrap());
        }
        let b = Arc::new(Budget::default());
        let t = Arc::new(Tripped::default());
        assert_eq!(
            drain(&mut open_bounded(&gz[..], Compression::Gzip, 100, b.clone(), 100, t.clone()).unwrap()).unwrap(),
            b"hello world"
        );
        let mut zs = zstd::encode_all(&b"one "[..], 3).unwrap();
        zs.extend(zstd::encode_all(&b"two"[..], 3).unwrap());
        assert_eq!(
            drain(&mut open_bounded(&zs[..], Compression::Zstd, 100, b, 1000, t).unwrap()).unwrap(),
            b"one two"
        );
    }

    #[test]
    fn trips_layer_and_image_limits() {
        let zeros = vec![0u8; 10_000];
        let gz = {
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
            e.write_all(&zeros).unwrap();
            e.finish().unwrap()
        };
        let t = Arc::new(Tripped::default());
        assert!(
            drain(
                &mut open_bounded(
                    &gz[..],
                    Compression::Gzip,
                    5000,
                    Arc::new(Budget::default()),
                    u64::MAX,
                    t.clone()
                )
                .unwrap()
            )
            .is_err()
        );
        assert!(t.layer() && !t.image());
        let t = Arc::new(Tripped::default());
        let b = Arc::new(Budget::default());
        assert!(
            drain(&mut open_bounded(&zeros[..], Compression::None, u64::MAX, b, 4000, t.clone()).unwrap()).is_err()
        );
        assert!(t.image());
    }

    #[test]
    fn layer_limit_rules() {
        assert_eq!(layer_limit(Compression::None, 10, 200, 1000), 10);
        assert_eq!(
            layer_limit(Compression::Gzip, 10, 200, u64::MAX),
            1 << 20,
            "1 MiB floor"
        );
        assert_eq!(layer_limit(Compression::Gzip, 1 << 20, 200, u64::MAX), 200 << 20);
        assert_eq!(
            layer_limit(Compression::Zstd, 1 << 30, 200, 16 << 30),
            16 << 30,
            "capped"
        );
    }
}
```

- [ ] **Step 6: Write the inheritance cache keys**

The `ctx` encoding is normative in `docs/format.md` (Task 10): the JSON list, in implicit-path order, of `[hex(path), null]` or `[hex(path), [mode, uid, gid, sec, nsec, [[index, hex(name), hex(value)], …]]]`, hashed with SHA-256. Hex keeps non-UTF-8 paths and xattrs exact.

`crates/kiln-image/src/ctx.rs`:
```rust
//! Cache keys for layers whose output depends on inherited parent attributes (§6.3).

use std::collections::BTreeMap;

use kiln_erofs::DirAttrs;
use kiln_store::Digest;
use serde_json::json;

use crate::types::{hex, unhex};

/// `parents <json array of hex paths>`: the layer-cache value for a layer with implicit dirs.
pub fn parents_entry(paths: &[Vec<u8>]) -> String {
    let hexes: Vec<String> = paths.iter().map(|p| hex(p)).collect();
    format!("parents {}", serde_json::to_string(&hexes).expect("strings serialize"))
}

/// Parses a `parents [...]` entry back to paths; `None` if malformed.
pub fn parse_parents(entry: &str) -> Option<Vec<Vec<u8>>> {
    let list: Vec<String> = serde_json::from_str(entry.strip_prefix("parents ")?).ok()?;
    list.iter().map(|h| unhex(h)).collect()
}

/// SHA-256 (hex) of the canonical encoding of each implicit path and what it inherited.
pub fn ctx_hash(paths: &[Vec<u8>], inherited: &BTreeMap<Vec<u8>, DirAttrs>) -> String {
    let entries: Vec<serde_json::Value> = paths
        .iter()
        .map(|p| match inherited.get(p) {
            None => json!([hex(p), null]),
            Some(a) => {
                let xattrs: Vec<serde_json::Value> = a
                    .xattrs
                    .iter()
                    .map(|(k, v)| json!([k.index, hex(&k.name), hex(v)]))
                    .collect();
                json!([
                    hex(p),
                    [
                        a.meta.mode,
                        a.meta.uid,
                        a.meta.gid,
                        a.meta.mtime.sec,
                        a.meta.mtime.nsec,
                        xattrs
                    ]
                ])
            }
        })
        .collect();
    Digest::of(&serde_json::to_vec(&entries).expect("json serializes"))
        .hex()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_erofs::{Meta, Timestamp, XattrKey, Xattrs};

    fn attrs(mode: u32) -> DirAttrs {
        let mut x = Xattrs::new();
        x.insert(
            XattrKey {
                index: 1,
                name: b"k".to_vec(),
            },
            b"v".to_vec(),
        );
        DirAttrs {
            meta: Meta {
                mode,
                uid: 0,
                gid: 0,
                mtime: Timestamp { sec: 5, nsec: 0 },
            },
            xattrs: x,
        }
    }

    #[test]
    fn parents_round_trip_with_non_utf8_paths() {
        let paths = vec![b"tmp".to_vec(), b"caf\xe9".to_vec()];
        assert_eq!(parse_parents(&parents_entry(&paths)).unwrap(), paths);
        assert!(parse_parents("erofs sha256:00").is_none());
    }

    #[test]
    fn ctx_changes_with_inherited_attributes_only() {
        let paths = vec![b"tmp".to_vec()];
        let a = BTreeMap::from([(b"tmp".to_vec(), attrs(0o1777))]);
        let b = BTreeMap::from([(b"tmp".to_vec(), attrs(0o755))]);
        assert_eq!(ctx_hash(&paths, &a), ctx_hash(&paths, &a.clone()));
        assert_ne!(ctx_hash(&paths, &a), ctx_hash(&paths, &b));
        assert_ne!(
            ctx_hash(&paths, &a),
            ctx_hash(&paths, &BTreeMap::new()),
            "absent differs from present"
        );
    }
}
```

- [ ] **Step 7: Write the conversion**

How `convert_image` runs (spec §6.2–§6.4):
1. **Plan** each layer from `cache/layers/<src>@<fmt>`: `erofs D` with the blob present → done; `parents [...]` → resolve later; anything else → convert.
2. **Phase A (parallel):** every layer to convert streams through the decompressor, the limits, a `HashingReader` and the `LayerWriter`, into a staged blob. Then the remainder is read to EOF. Only zero padding may follow the tar's end-of-archive marker (`TrailingData` otherwise), and the decompressed digest must equal `rootfs.diff_ids[i]`. Nothing is committed in this phase, so a failure anywhere leaves no cache entry (Review Focus 5).
3. **Phase B (sequential, bottom-up):** each pending layer resolves its implicit directories against the already-final lower layers, finishes, commits, and only then writes its cache entries. A `parents` hit computes `ctx` and checks `cache/layers-ctx`; a miss there streams that one layer again sequentially (a rare case: the layer is cached but its base changed).
4. **Squash** when there are more than `max_layers` layers: the bottom `N − max + 1` merge from their erofs blobs (never from tars, never from pulled kiln images), cached under the SHA-256 of their newline-joined digests.
5. **Commit** the canonical config and manifest blobs.

Layer output files are `TmpBlob::reopen()` handles, which are read+write and not in append mode: the `LayerWriter`/`squash` output contract from M1a.

`crates/kiln-image/src/convert.rs`:
```rust
//! OCI image → kiln image (spec §6.2–§6.4).

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use kiln_erofs::{DirAttrs, FORMAT_VERSION, Image, LayerWriter, Limits};
use kiln_oci::media::{self, Compression};
use kiln_oci::{Descriptor, ImageManifest, Platform, ResolvedImage, canonical_json};
use kiln_store::{CacheKind, Digest, HashingReader, Store, TmpBlob};

use crate::ctx::{ctx_hash, parents_entry, parse_parents};
use crate::decompress::{Budget, Tripped, layer_limit, open_bounded};
use crate::error::{ImageError, Result};
use crate::types::*;

/// Conversion settings (spec §6.4, §7.6).
#[derive(Debug, Clone)]
pub struct ConvertOptions {
    /// App layers above this count are squashed into the bottom layer.
    pub max_layers: usize,
    pub limits: Limits,
    /// Decompressed bytes across all layers of one image.
    pub max_image_bytes: u64,
    /// Decompressed / compressed bytes for one layer.
    pub max_expansion_ratio: u64,
    /// Layers converted in parallel.
    pub jobs: usize,
}

impl Default for ConvertOptions {
    fn default() -> Self {
        Self {
            max_layers: 10,
            limits: Limits::default(),
            max_image_bytes: 64 << 30,
            max_expansion_ratio: 200,
            jobs: std::thread::available_parallelism().map_or(4, |n| n.get()),
        }
    }
}

/// One erofs layer of a converted image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerReport {
    /// The OCI layers it was built from (several when squashed).
    pub sources: Vec<Digest>,
    pub erofs: Digest,
    pub size: u64,
    /// Taken from a cache instead of converted.
    pub cached: bool,
    pub inherits: bool,
    pub warnings: Vec<String>,
}

/// One converted platform.
#[derive(Debug, Clone)]
pub struct Converted {
    pub platform: Platform,
    pub manifest_digest: Digest,
    pub manifest_size: u64,
    pub layers: Vec<LayerReport>,
    /// How many source layers were merged into the bottom layer (0: no squash).
    pub squashed: usize,
}

/// A layer whose tar is consumed but whose metadata waits for its lowers.
struct Pending {
    tmp: TmpBlob,
    writer: LayerWriter<File>,
    implicit: Vec<Vec<u8>>,
    warnings: Vec<String>,
}

enum Plan {
    Done(Digest),
    Parents(Vec<Vec<u8>>),
    Convert,
}

fn layer_key(src: &Digest) -> String {
    format!("{src}@{FORMAT_VERSION}")
}

fn plan_layer(store: &Store, desc: &Descriptor) -> Result<Plan> {
    let Some(entry) = store.cache_get(CacheKind::Layers, &layer_key(&desc.digest))? else {
        return Ok(Plan::Convert);
    };
    if let Some(d) = entry.strip_prefix("erofs ").and_then(|d| Digest::parse(d.trim()).ok()) {
        return Ok(if store.has_blob(&d) {
            Plan::Done(d)
        } else {
            Plan::Convert
        });
    }
    Ok(parse_parents(&entry).map_or(Plan::Convert, Plan::Parents))
}

fn check_platform(p: &Platform) -> Result<()> {
    if p.os == "linux" && (p.architecture == "amd64" || p.architecture == "arm64") {
        Ok(())
    } else {
        Err(ImageError::UnsupportedPlatform(p.to_string()))
    }
}

/// Streams layer `i` through the decompressor, limits and tar reader, then checks
/// the decompressed digest against `rootfs.diff_ids[i]` (spec §6.1 step 3).
fn stream_layer(
    store: &Store,
    img: &ResolvedImage,
    i: usize,
    opts: &ConvertOptions,
    budget: &Arc<Budget>,
) -> Result<Pending> {
    let desc = &img.manifest.layers[i];
    let compression = media::layer_compression(&desc.media_type)?;
    let max = layer_limit(
        compression,
        desc.size,
        opts.max_expansion_ratio,
        opts.limits.max_layer_bytes,
    );
    let tripped = Arc::new(Tripped::default());
    let limit_error = |t: &Tripped| {
        if t.image() {
            Some(ImageError::LimitExceeded {
                what: "uncompressed bytes per image",
                max: opts.max_image_bytes,
            })
        } else if t.layer() {
            let what = if max < opts.limits.max_layer_bytes && compression != Compression::None {
                "expansion ratio"
            } else {
                "uncompressed bytes per layer"
            };
            Some(ImageError::LimitExceeded { what, max })
        } else {
            None
        }
    };
    let blob = store.open_blob(&desc.digest)?;
    let bounded = open_bounded(
        blob,
        compression,
        max,
        budget.clone(),
        opts.max_image_bytes,
        tripped.clone(),
    )?;
    let mut hashed = HashingReader::new(bounded);
    let tmp = store.tmp_blob()?;
    let mut writer = LayerWriter::new(tmp.reopen()?, &store.tmp_dir(), opts.limits.clone())?;
    if let Err(e) = writer.append_tar(&mut hashed) {
        return Err(limit_error(&tripped).unwrap_or(e.into()));
    }
    // Read to EOF: only zero padding may follow the end-of-archive marker.
    let mut buf = vec![0u8; 64 << 10];
    loop {
        let n = match hashed.read(&mut buf) {
            Ok(n) => n,
            Err(e) => return Err(limit_error(&tripped).unwrap_or(e.into())),
        };
        if n == 0 {
            break;
        }
        if buf[..n].iter().any(|&b| b != 0) {
            return Err(ImageError::TrailingData { layer: i });
        }
    }
    let (actual, _) = hashed.finish_to_eof()?;
    let expected = &img.config.rootfs.diff_ids[i];
    if &actual != expected {
        return Err(ImageError::DiffIdMismatch {
            layer: i,
            expected: expected.clone(),
            actual,
        });
    }
    let implicit = writer.implicit_dirs();
    Ok(Pending {
        tmp,
        writer,
        implicit,
        warnings: Vec::new(),
    })
}

/// Runs `stream_layer` for `todo` on up to `opts.jobs` threads; stops early on error.
fn stream_parallel(
    store: &Store,
    img: &ResolvedImage,
    todo: &[usize],
    opts: &ConvertOptions,
    budget: &Arc<Budget>,
) -> Result<BTreeMap<usize, Pending>> {
    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let results = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..opts.jobs.clamp(1, todo.len().max(1)) {
            s.spawn(|| {
                while !failed.load(Ordering::SeqCst) {
                    let Some(&i) = todo.get(next.fetch_add(1, Ordering::SeqCst)) else {
                        break;
                    };
                    let r = stream_layer(store, img, i, opts, budget);
                    if r.is_err() {
                        failed.store(true, Ordering::SeqCst);
                    }
                    results.lock().expect("no poisoning").push((i, r));
                }
            });
        }
    });
    let mut results = results.into_inner().expect("no poisoning");
    results.sort_by_key(|(i, _)| *i);
    results.into_iter().map(|(i, r)| r.map(|p| (i, p))).collect()
}

/// Finalises `p` against `lowers`, commits it, then writes its cache entries.
fn finish_layer(
    store: &Store,
    src: &Digest,
    p: Pending,
    lowers: &mut [Image<File>],
) -> Result<(Digest, bool, Vec<String>)> {
    let inherited: BTreeMap<Vec<u8>, DirAttrs> = if p.implicit.is_empty() {
        BTreeMap::new()
    } else {
        kiln_erofs::resolve_inherited(lowers, &p.implicit)?
    };
    let (_out, summary) = p.writer.finish(&inherited)?;
    let d = store.commit(p.tmp)?;
    let key = layer_key(src);
    if p.implicit.is_empty() {
        store.cache_put(CacheKind::Layers, &key, &format!("erofs {d}"))?;
    } else {
        let ctx = ctx_hash(&p.implicit, &inherited);
        store.cache_put(CacheKind::LayersCtx, &format!("{key}@{ctx}"), &d.to_string())?;
        store.cache_put(CacheKind::Layers, &key, &parents_entry(&p.implicit))?;
    }
    let mut warnings = p.warnings;
    warnings.extend(summary.warnings);
    Ok((d, !p.implicit.is_empty(), warnings))
}

fn squash_key(layers: &[LayerReport]) -> String {
    let joined: Vec<String> = layers.iter().map(|l| l.erofs.to_string()).collect();
    format!("{}@{FORMAT_VERSION}", Digest::of(joined.join("\n").as_bytes()).hex())
}

/// Merges the bottom `k` layers into one (spec §6.4).
fn squash_bottom(store: &Store, layers: Vec<LayerReport>, k: usize) -> Result<Vec<LayerReport>> {
    let (bottom, rest) = layers.split_at(k);
    let key = squash_key(bottom);
    let sources: Vec<Digest> = bottom.iter().flat_map(|l| l.sources.clone()).collect();
    let (erofs, cached) = match store.cache_get_blob(CacheKind::Squash, &key)? {
        Some(d) => (d, true),
        None => {
            let mut images = bottom
                .iter()
                .map(|l| Ok(Image::open(store.open_blob(&l.erofs)?)?))
                .collect::<Result<Vec<_>>>()?;
            let tmp = store.tmp_blob()?;
            kiln_erofs::squash(&mut images, tmp.reopen()?, &store.tmp_dir())?;
            let d = store.commit(tmp)?;
            store.cache_put(CacheKind::Squash, &key, &d.to_string())?;
            (d, false)
        }
    };
    let size = store.blob_size(&erofs)?;
    let mut out = vec![LayerReport {
        sources,
        erofs,
        size,
        cached,
        inherits: false,
        warnings: Vec::new(),
    }];
    out.extend_from_slice(rest);
    Ok(out)
}

/// Converts the app layers of one resolved image and commits its kiln manifest.
pub fn convert_image(
    store: &Store,
    img: &ResolvedImage,
    reference: Option<&str>,
    opts: &ConvertOptions,
) -> Result<Converted> {
    if opts.max_layers == 0 || opts.jobs == 0 {
        return Err(ImageError::BadOption("max-layers and jobs must be at least 1".into()));
    }
    let platform = img.config.platform();
    check_platform(&platform)?;
    let layers = &img.manifest.layers;
    let plans = layers
        .iter()
        .map(|d| plan_layer(store, d))
        .collect::<Result<Vec<_>>>()?;
    let todo: Vec<usize> = plans
        .iter()
        .enumerate()
        .filter(|(_, p)| matches!(p, Plan::Convert))
        .map(|(i, _)| i)
        .collect();
    let budget = Arc::new(Budget::default());
    let mut pending = stream_parallel(store, img, &todo, opts, &budget)?;

    // Bottom-up: each layer's lowers are final before it is.
    let mut reports: Vec<LayerReport> = Vec::new();
    let mut lowers: Vec<Image<File>> = Vec::new();
    for (i, plan) in plans.into_iter().enumerate() {
        let src = &layers[i].digest;
        let (erofs, cached, inherits, warnings) = match plan {
            Plan::Done(d) => (d, true, false, Vec::new()),
            Plan::Parents(paths) => {
                let inherited = kiln_erofs::resolve_inherited(&mut lowers, &paths)?;
                let key = format!("{}@{}", layer_key(src), ctx_hash(&paths, &inherited));
                match store.cache_get_blob(CacheKind::LayersCtx, &key)? {
                    Some(d) => (d, true, true, Vec::new()),
                    None => {
                        let p = stream_layer(store, img, i, opts, &budget)?;
                        let (d, inherits, w) = finish_layer(store, src, p, &mut lowers)?;
                        (d, false, inherits, w)
                    }
                }
            }
            Plan::Convert => {
                let p = pending.remove(&i).expect("streamed in phase A");
                let (d, inherits, w) = finish_layer(store, src, p, &mut lowers)?;
                (d, false, inherits, w)
            }
        };
        lowers.push(Image::open(store.open_blob(&erofs)?)?);
        let size = store.blob_size(&erofs)?;
        reports.push(LayerReport {
            sources: vec![src.clone()],
            erofs,
            size,
            cached,
            inherits,
            warnings,
        });
    }
    drop(lowers);

    let mut squashed = 0;
    if reports.len() > opts.max_layers {
        squashed = reports.len() - opts.max_layers + 1;
        reports = squash_bottom(store, reports, squashed)?;
    }

    let config = KilnConfig {
        schema_version: SCHEMA_VERSION,
        architecture: platform.architecture.clone(),
        process: Process::from_oci(img.config.config.as_ref()),
        kernel: None,
        init: None,
        source: SourceRef {
            manifest_digest: img.manifest_digest.clone(),
            reference: reference.map(str::to_string),
        },
        erofs_format_version: FORMAT_VERSION,
    };
    let config_bytes = canonical_json(&config);
    let config_digest = store.put_bytes(&config_bytes)?;
    let manifest = ImageManifest {
        schema_version: 2,
        media_type: Some(media::OCI_MANIFEST.to_string()),
        artifact_type: Some(KILN_ARTIFACT.to_string()),
        config: Descriptor::new(KILN_CONFIG, config_digest, config_bytes.len() as u64),
        layers: reports.iter().map(layer_descriptor).collect(),
        annotations: None,
    };
    let bytes = canonical_json(&manifest);
    let manifest_digest = store.put_bytes(&bytes)?;
    Ok(Converted {
        platform,
        manifest_digest,
        manifest_size: bytes.len() as u64,
        layers: reports,
        squashed,
    })
}

fn layer_descriptor(l: &LayerReport) -> Descriptor {
    let mut d = Descriptor::new(KILN_LAYER, l.erofs.clone(), l.size);
    let sources: Vec<String> = l.sources.iter().map(Digest::to_string).collect();
    let mut ann = BTreeMap::from([(ANN_SOURCE_DIGESTS.to_string(), sources.join(","))]);
    if l.inherits {
        ann.insert(ANN_INHERITS.to_string(), "true".to_string());
    }
    d.annotations = Some(ann);
    d
}
```

- [ ] **Step 8: Write the pipeline entry point and loading**

`convert_local` holds the shared store lock from resolution until the ref is written, so GC (exclusive) can never delete a blob that is committed but not yet referenced. With several platforms it writes an index, sorted by platform. `load` rejects anything without the kiln `artifactType`, with an unexpected config media type, or with an unknown `schemaVersion`.

`crates/kiln-image/src/pipeline.rs`:
```rust
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

/// Converts every resolved platform and writes the top-level blob.
pub fn convert_resolved(
    store: &Store,
    resolved: &[ResolvedImage],
    reference: Option<&str>,
    opts: &ConvertOptions,
) -> Result<Output> {
    let mut images = resolved
        .iter()
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
```

`crates/kiln-image/src/load.rs`:
```rust
//! Reading kiln images back from the store (`inspect`, `ls`, `import`).

use kiln_oci::media::OCI_INDEX;
use kiln_oci::{ImageIndex, ImageManifest, Platform};
use kiln_store::{Digest, Store};

use crate::error::{ImageError, Result, json};
use crate::types::{KILN_ARTIFACT, KILN_CONFIG, KilnConfig, SCHEMA_VERSION};

/// One platform's kiln manifest and config.
#[derive(Debug, Clone)]
pub struct KilnManifest {
    pub digest: Digest,
    pub manifest: ImageManifest,
    pub config: KilnConfig,
}

/// A stored kiln image: one manifest, or an index of per-platform manifests.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub digest: Digest,
    pub is_index: bool,
    pub entries: Vec<(Platform, KilnManifest)>,
}

/// A ref name, or a digest of a blob in the store.
pub fn resolve_name(store: &Store, name: &str) -> Result<Digest> {
    if let Ok(d) = Digest::parse(name)
        && store.has_blob(&d)
    {
        return Ok(d);
    }
    store
        .get_ref(name)?
        .ok_or_else(|| ImageError::RefNotFound(name.to_string()))
}

pub fn load_manifest(store: &Store, digest: &Digest) -> Result<KilnManifest> {
    let manifest: ImageManifest = json("kiln manifest", &store.read_metadata(digest)?)?;
    if manifest.artifact_type.as_deref() != Some(KILN_ARTIFACT) || manifest.config.media_type != KILN_CONFIG {
        return Err(ImageError::NotAKilnImage(digest.clone()));
    }
    let config: KilnConfig = json("kiln config", &store.read_metadata(&manifest.config.digest)?)?;
    if config.schema_version != SCHEMA_VERSION {
        return Err(ImageError::UnknownSchema(config.schema_version));
    }
    Ok(KilnManifest {
        digest: digest.clone(),
        manifest,
        config,
    })
}

pub fn load(store: &Store, digest: &Digest) -> Result<Loaded> {
    let bytes = store.read_metadata(digest)?;
    let probe: serde_json::Value = json("kiln image", &bytes)?;
    if probe.get("mediaType").and_then(|m| m.as_str()) != Some(OCI_INDEX) {
        let m = load_manifest(store, digest)?;
        let platform = Platform {
            os: "linux".into(),
            architecture: m.config.architecture.clone(),
            variant: None,
        };
        return Ok(Loaded {
            digest: digest.clone(),
            is_index: false,
            entries: vec![(platform, m)],
        });
    }
    let index: ImageIndex = json("kiln index", &bytes)?;
    if index.artifact_type.as_deref() != Some(KILN_ARTIFACT) {
        return Err(ImageError::NotAKilnImage(digest.clone()));
    }
    let entries = index
        .manifests
        .iter()
        .map(|d| {
            let platform = d
                .platform
                .clone()
                .ok_or_else(|| ImageError::NotAKilnImage(digest.clone()))?;
            Ok((platform, load_manifest(store, &d.digest)?))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Loaded {
        digest: digest.clone(),
        is_index: true,
        entries,
    })
}
```

`crates/kiln-image/src/lib.rs`:
```rust
//! kiln images: the OCI → kiln conversion pipeline, image format, inspect and import.
#![forbid(unsafe_code)]

mod convert;
mod ctx;
mod decompress;
mod error;
mod load;
mod pipeline;
pub mod types;

pub use convert::{ConvertOptions, Converted, LayerReport, convert_image};
pub use error::{ImageError, Result};
pub use load::{KilnManifest, Loaded, load, load_manifest, resolve_name};
pub use pipeline::{LocalRequest, Output, convert_local, convert_resolved};
```

- [ ] **Step 9: Run the tests**

Run: `cargo test -q -p kiln-image`
Expected: all pass (25 tests).

- [ ] **Step 10: Format, lint and run the whole workspace**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace -q`
Expected: no clippy warnings; every test passes.

- [ ] **Step 11: Commit**

```bash
git add crates/kiln-image Cargo.lock
git commit -m "feat(kiln-image): verified OCI to kiln conversion with caching, inheritance and squash" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P7rdQAESQYNEo2qFobUNxt"
```


### Task 7: `kiln-image`: import between stores

**Files:**
- Create: `crates/kiln-image/src/import.rs`, `crates/kiln-image/tests/import.rs`
- Modify: `crates/kiln-image/src/lib.rs`

**Interfaces:**
- Consumes: Task 1 `Store::open_read_only`, `put_verified`; Task 6 `load`, `convert_local` (in the test).
- Produces: `import_image(dst: &Store, src: &Store, name: &str, as_name: &str) -> Result<ImportReport>`; `ImportReport { digest, blobs_copied, bytes_copied }`. `name` is a ref or a digest in `src`.

- [ ] **Step 1: Write the failing test**

This is the Lima path from spec §10: the macOS store is mounted read-only, so the source store must be opened without creating `tmp/` or lock files. Every blob is verified into the destination; a tampered source blob fails with `DigestMismatch` and nothing is tagged. Imported erofs layers never enter the conversion caches (spec §6.2).

`crates/kiln-image/tests/import.rs`:
```rust
mod common;

use std::fs;

use common::*;
use kiln_image::{ConvertOptions, ImageError, convert_local, import_image, load};
use kiln_store::Store;

#[test]
fn imports_between_stores_verifying_blobs() {
    let src = tempfile::tempdir().unwrap();
    let (h1, h2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = Store::open(h1.path()).unwrap();
    let path = layout(src.path(), &[arm()], &[gz(&base(0o1777)), gz(&top())]);
    let out = convert_local(&a, &path, &req(&[arm()], "app:1"), &ConvertOptions::default()).unwrap();
    let ro = Store::open_read_only(h1.path()).unwrap();
    let b = Store::open(h2.path()).unwrap();
    let r = import_image(&b, &ro, "app:1", "app:1").unwrap();
    assert_eq!(r.digest, out.digest);
    assert_eq!(r.blobs_copied, 4, "manifest, config, two layers");
    assert_eq!(load(&b, &out.digest).unwrap().entries.len(), 1);
    assert_eq!(cache_entries(&b), 0, "imported layers never enter the caches");
    assert_eq!(import_image(&b, &ro, "app:1", "app:2").unwrap().blobs_copied, 0);

    // A tampered layer in the source store is refused and nothing is tagged.
    let c = Store::open(tempfile::tempdir().unwrap().keep()).unwrap();
    let layer = &out.images[0].layers[1].erofs;
    fs::write(a.blob_path(layer), b"tampered").unwrap();
    let err = import_image(&c, &ro, "app:1", "app:1").unwrap_err();
    assert!(
        matches!(err, ImageError::Store(kiln_store::StoreError::DigestMismatch { .. })),
        "{err}"
    );
    assert_eq!(c.get_ref("app:1").unwrap(), None);
}
```

- [ ] **Step 2: Run it to see it fail**

Run: `cargo test -q -p kiln-image --test import`
Expected: compile error: no `import_image` in `kiln_image`.

- [ ] **Step 3: Write the import**

Metadata is parsed only after it has been verified into `dst`. Blobs that are already present are skipped, so re-imports copy only changed layers.

`crates/kiln-image/src/import.rs`:
```rust
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
```

In `crates/kiln-image/src/lib.rs`, replace:
```rust
mod error;
mod load;
```
with:
```rust
mod error;
mod import;
mod load;
```

In `crates/kiln-image/src/lib.rs`, replace:
```rust
pub use error::{ImageError, Result};
pub use load::{KilnManifest, Loaded, load, load_manifest, resolve_name};
```
with:
```rust
pub use error::{ImageError, Result};
pub use import::{ImportReport, import_image};
pub use load::{KilnManifest, Loaded, load, load_manifest, resolve_name};
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -q -p kiln-image`
Expected: all pass (26 tests).

- [ ] **Step 5: Format, lint and run the whole workspace**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace -q`
Expected: no clippy warnings; every test passes.

- [ ] **Step 6: Commit**

```bash
git add crates/kiln-image Cargo.lock
git commit -m "feat(kiln-image): verified import from another store" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P7rdQAESQYNEo2qFobUNxt"
```


### Task 8: `kiln` CLI: convert, import, inspect, ls, gc

**Files:**
- Create: `crates/kiln/Cargo.toml`, `crates/kiln/src/main.rs`, `crates/kiln/src/sanitize.rs`, `crates/kiln/tests/cli.rs`

**Interfaces:**
- Consumes: `kiln_image::{convert_local, LocalRequest, ConvertOptions, Output, import_image, load, resolve_name, types::ANN_INHERITS}`, `kiln_store::{Store, check_ref_name}`, `kiln_oci::Platform`.
- Produces: the `kiln` binary. Global flag `--store DIR`; subcommands `convert PATH [--platform P]... [--ref NAME] [--tag NAME] [--max-layers N] [--jobs N] [--json]`, `import --from-store DIR NAME [--as NAME]`, `inspect NAME [--json]`, `ls [--json]`, `gc`. `ConvertArgs` (platforms, max_layers, jobs) is shared with Task 9's `bench`. `sanitize::{clean, clean_line, MAX_CHARS}`.

- [ ] **Step 1: Create the crate manifest**

`crates/kiln/Cargo.toml`:
```toml
[package]
name = "kiln"
version = "0.1.0"
edition.workspace = true
license.workspace = true
description = "Build microVM images from OCI images"

[[bin]]
name = "kiln"
path = "src/main.rs"

[dependencies]
anyhow = "1.0.104"
clap = { version = "4.6.7", features = ["derive"] }
kiln-image = { path = "../kiln-image" }
kiln-oci = { path = "../kiln-oci" }
kiln-store = { path = "../kiln-store" }
serde_json = "1.0.151"

[dev-dependencies]
tempfile = "3.27.0"
kiln-erofs = { path = "../kiln-erofs" }
assert_cmd = "2.2.2"
predicates = "3.1.4"
```

- [ ] **Step 2: Write the failing CLI tests**

`image_strings_are_sanitised_in_inspect_and_errors` puts terminal escapes in an image's `Cmd` and in a tar path that ends up in an error message; neither may reach the terminal (T8; this also closes M1a's `MalformedTar` carry-forward, since every error passes through `clean`). `missing_platform_lists_available_ones` is the common Mac mistake of converting an amd64-only image without `--platform`.

`crates/kiln/tests/cli.rs`:
```rust
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use kiln_erofs::testtar::{Opts, TarBuilder};
use kiln_oci::testlayout::{LayoutBuilder, TestLayer};
use kiln_oci::{ContainerConfig, Platform};
use predicates::prelude::*;

fn layout(dir: &Path, cmd: &str, layers: Vec<Vec<u8>>) -> PathBuf {
    let mut b = LayoutBuilder::new(dir);
    let cfg = ContainerConfig {
        cmd: Some(vec![cmd.into()]),
        env: Some(vec!["A=1".into()]),
        ..Default::default()
    };
    let layers: Vec<TestLayer> = layers.into_iter().map(TestLayer::tar).collect();
    let d = b.image(&Platform::parse("linux/arm64").unwrap(), &layers, cfg);
    b.add(d, None).finish()
}

fn simple() -> Vec<Vec<u8>> {
    vec![
        TarBuilder::new()
            .dir("tmp", &Opts::default().mode(0o1777))
            .file("hello", b"hi", &Opts::default())
            .finish(),
    ]
}

fn kiln(store: &Path) -> Command {
    let mut c = Command::cargo_bin("kiln").unwrap();
    c.arg("--store").arg(store);
    c
}

#[test]
fn convert_ls_inspect_gc_import() {
    let src = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let path = layout(&src.path().join("app"), "php", simple());
    kiln(home.path())
        .args(["convert", "--platform", "linux/arm64"])
        .arg(&path)
        .assert()
        .success()
        .stdout(predicate::str::contains("app:latest → sha256:"));
    kiln(home.path())
        .args(["ls"])
        .assert()
        .success()
        .stdout(predicate::str::contains("app:latest").and(predicate::str::contains("linux/arm64")));
    kiln(home.path())
        .args(["inspect", "app:latest"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("cmd:        php")
                .and(predicate::str::contains("env:        A=1"))
                .and(predicate::str::contains("unverified provenance")),
        );
    let json = kiln(home.path())
        .args(["inspect", "--json", "app:latest"])
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(v["images"][0]["config"]["process"]["cmd"][0], "php");
    // GC frees the source OCI blobs (layer, config, manifest) but keeps the kiln image.
    kiln(home.path())
        .arg("gc")
        .assert()
        .success()
        .stdout(predicate::str::contains("removed 3 blobs"));
    kiln(home.path()).args(["inspect", "app:latest"]).assert().success();
    kiln(home.path())
        .arg("gc")
        .assert()
        .success()
        .stdout(predicate::str::contains("removed 0 blobs"));

    let other = tempfile::tempdir().unwrap();
    kiln(other.path())
        .args(["import", "--from-store"])
        .arg(home.path())
        .args(["app:latest", "--as", "copy:1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("copy:1 → sha256:"));
    kiln(other.path()).args(["inspect", "copy:1"]).assert().success();
}

#[test]
fn json_convert_summary_reports_cache_hits() {
    let src = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let path = layout(&src.path().join("app"), "php", simple());
    let run = || {
        let out = kiln(home.path())
            .args(["convert", "--json", "--platform", "linux/arm64", "--tag", "x:1"])
            .arg(&path)
            .output()
            .unwrap();
        assert!(out.status.success());
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()
    };
    assert_eq!(run()["images"][0]["layers"][0]["cached"], false);
    assert_eq!(run()["images"][0]["layers"][0]["cached"], true);
}

#[test]
fn image_strings_are_sanitised_in_inspect_and_errors() {
    let src = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let path = layout(&src.path().join("evil"), "\x1b]0;pwned\x07\x1b[2J", simple());
    kiln(home.path())
        .args(["convert", "--platform", "linux/arm64"])
        .arg(&path)
        .assert()
        .success();
    kiln(home.path())
        .args(["inspect", "evil:latest"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("\x1b")
                .not()
                .and(predicate::str::contains("\x07").not()),
        );

    // A hostile tar path ends up in an error message.
    let bad = vec![TarBuilder::new().hardlink("x", "\x1b[31mmissing").finish()];
    let path = layout(&src.path().join("bad"), "sh", bad);
    kiln(home.path())
        .args(["convert", "--platform", "linux/arm64"])
        .arg(&path)
        .assert()
        .failure()
        .stderr(predicate::str::contains("kiln: error:").and(predicate::str::contains("\x1b").not()));
}

#[test]
fn missing_platform_lists_available_ones() {
    let src = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let path = layout(&src.path().join("app"), "php", simple());
    kiln(home.path())
        .args(["convert", "--platform", "linux/amd64"])
        .arg(&path)
        .assert()
        .failure()
        .stderr(predicate::str::contains("linux/arm64"));
}

#[test]
fn errors_print_each_cause_once() {
    let home = tempfile::tempdir().unwrap();
    let out = kiln(home.path()).args(["inspect", "nope:1"]).output().unwrap();
    assert!(!out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "kiln: error: no image named \"nope:1\"\n"
    );
}
```

- [ ] **Step 3: Run them to see them fail**

Run: `cargo test -q -p kiln`
Expected: build error: no `src/main.rs`.

- [ ] **Step 4: Write the sanitiser**

Spec §13: lossy UTF-8 (the callers already hold `String`s), C0 and C1 controls removed except `\n` and `\t` (so ESC and CSI go), length capped. `clean_line` also flattens newlines for one-line fields.

`crates/kiln/src/sanitize.rs`:
```rust
//! Sanitising untrusted text before printing it (spec §13, threat T8).

/// Longest sanitised string, in characters.
pub const MAX_CHARS: usize = 4096;

/// Removes control characters (C0, DEL, C1, so ESC too) except `\n` and `\t`,
/// and caps the length.
pub fn clean(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(MAX_CHARS));
    for (i, c) in s
        .chars()
        .filter(|&c| c == '\n' || c == '\t' || !c.is_control())
        .enumerate()
    {
        if i == MAX_CHARS {
            out.push('…');
            break;
        }
        out.push(c);
    }
    out
}

/// [`clean`] that also flattens newlines and tabs, for single-line fields.
pub fn clean_line(s: &str) -> String {
    clean(s).replace(['\n', '\t'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_escapes_and_controls_but_keeps_newlines_and_tabs() {
        assert_eq!(clean("a\x1b[31mred\x1b[0m\x07\u{9b}2J\n\tb\x7f"), "a[31mred[0m2J\n\tb");
        assert_eq!(clean_line("a\nb\tc"), "a b c");
    }

    #[test]
    fn caps_length() {
        let s = clean(&"x".repeat(MAX_CHARS + 10));
        assert_eq!(s.chars().count(), MAX_CHARS + 1);
        assert!(s.ends_with('…'));
    }
}
```

- [ ] **Step 5: Write the CLI**

Decisions this file encodes:
- **Errors** print as one line, `kiln: error: <chain>`, sanitised. `error_message` skips a cause whose text its parent already includes, since several `thiserror` variants embed their source.
- **`convert`** defaults to the host platform and tags `<file stem>:latest` unless `--tag` is given; it refuses a stem that is not a valid ref name and asks for `--tag`.
- **`gc`** frees the source OCI blobs too: kiln images do not reference them, and a later convert re-verifies them from the source (it hits the layer cache, so nothing is reconverted).
- **`inspect`** labels `source` as unverified provenance (spec §8.1). `--json` output is serde-escaped, so it is safe as is.

`crates/kiln/src/main.rs`:
```rust
//! The `kiln` CLI. Every string from an image is sanitised before printing (T8).
#![forbid(unsafe_code)]

mod sanitize;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use kiln_image::{ConvertOptions, LocalRequest, Output, convert_local, import_image, load, resolve_name};
use kiln_oci::Platform;
use kiln_store::Store;
use serde_json::json;

use crate::sanitize::{clean, clean_line};

#[derive(Parser)]
#[command(name = "kiln", version, about = "Build microVM images from OCI images")]
struct Cli {
    /// Store directory (default: $KILN_HOME, else ~/.local/share/kiln).
    #[arg(long, global = true, value_name = "DIR")]
    store: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Args, Clone)]
struct ConvertArgs {
    /// Platform to convert (os/arch[/variant]); repeatable. Default: the host's.
    #[arg(long = "platform", value_name = "PLATFORM")]
    platforms: Vec<String>,
    /// Squash the bottom layers when there are more app layers than this.
    #[arg(long, default_value_t = 10)]
    max_layers: usize,
    /// Layers converted in parallel (default: available CPUs).
    #[arg(long)]
    jobs: Option<usize>,
}

impl ConvertArgs {
    fn options(&self) -> ConvertOptions {
        let mut o = ConvertOptions {
            max_layers: self.max_layers,
            ..Default::default()
        };
        if let Some(j) = self.jobs {
            o.jobs = j;
        }
        o
    }

    fn platforms(&self) -> Result<Vec<Platform>> {
        self.platforms
            .iter()
            .map(|p| Platform::parse(p).with_context(|| format!("invalid --platform {}", clean_line(p))))
            .collect()
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Convert a local OCI layout directory or `docker save` archive.
    Convert {
        path: PathBuf,
        /// Image to pick when the source holds several (its ref name).
        #[arg(long = "ref", value_name = "NAME")]
        source_ref: Option<String>,
        /// Name for the result (default: <file name>:latest).
        #[arg(long)]
        tag: Option<String>,
        #[command(flatten)]
        args: ConvertArgs,
        /// Print a JSON summary.
        #[arg(long)]
        json: bool,
    },
    /// Copy an image from another (possibly read-only) store, verifying every blob.
    Import {
        #[arg(long = "from-store", value_name = "DIR")]
        from_store: PathBuf,
        name: String,
        /// Local name (default: the same name).
        #[arg(long = "as", value_name = "NAME")]
        as_name: Option<String>,
    },
    /// Show an image's platforms, process and layers.
    Inspect {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// List tagged images.
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// Remove blobs and cache entries no tag reaches.
    Gc,
}

fn open_store(cli: &Cli) -> Result<Store> {
    let root = match &cli.store {
        Some(p) => p.clone(),
        None => Store::default_root()?,
    };
    Store::open(&root).with_context(|| format!("opening store {}", root.display()))
}

fn default_tag(path: &Path) -> Result<String> {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| kiln_store::check_ref_name(s).is_ok());
    match stem {
        Some(s) => Ok(format!("{s}:latest")),
        None => bail!(
            "cannot derive a name from {}; pass --tag",
            clean_line(&path.display().to_string())
        ),
    }
}

fn human_size(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

fn convert_summary(out: &Output, tag: &str) -> serde_json::Value {
    json!({
        "tag": tag,
        "digest": out.digest.to_string(),
        "mediaType": out.media_type,
        "images": out.images.iter().map(|c| json!({
            "platform": c.platform.to_string(),
            "manifest": c.manifest_digest.to_string(),
            "squashed": c.squashed,
            "layers": c.layers.iter().map(|l| json!({
                "erofs": l.erofs.to_string(),
                "size": l.size,
                "cached": l.cached,
                "inherits": l.inherits,
                "sources": l.sources.iter().map(ToString::to_string).collect::<Vec<_>>(),
                "warnings": l.warnings,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

fn run(cli: Cli) -> Result<()> {
    match &cli.cmd {
        Cmd::Convert {
            path,
            source_ref,
            tag,
            args,
            json,
        } => {
            let store = open_store(&cli)?;
            let tag = match tag {
                Some(t) => t.clone(),
                None => default_tag(path)?,
            };
            let platforms = args.platforms()?;
            let req = LocalRequest {
                source_ref: source_ref.as_deref(),
                platforms: &platforms,
                tag: Some(&tag),
            };
            let out = convert_local(&store, path, &req, &args.options())?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&convert_summary(&out, &tag))?);
                return Ok(());
            }
            for c in &out.images {
                let cached = c.layers.iter().filter(|l| l.cached).count();
                print!(
                    "{}  {}  {} layers ({cached} cached)",
                    clean_line(&c.platform.to_string()),
                    c.manifest_digest,
                    c.layers.len()
                );
                if c.squashed > 0 {
                    print!(", bottom {} squashed", c.squashed);
                }
                println!();
                for w in c.layers.iter().flat_map(|l| &l.warnings) {
                    eprintln!("warning: {}", clean_line(w));
                }
            }
            println!("{tag} → {}", out.digest);
        }
        Cmd::Import {
            from_store,
            name,
            as_name,
        } => {
            let store = open_store(&cli)?;
            let src = Store::open_read_only(from_store)?;
            let as_name = as_name.as_deref().unwrap_or(name);
            let r = import_image(&store, &src, name, as_name)?;
            println!(
                "{as_name} → {} ({} blobs, {} copied)",
                r.digest,
                r.blobs_copied,
                human_size(r.bytes_copied)
            );
        }
        Cmd::Inspect { name, json } => {
            let store = open_store(&cli)?;
            let loaded = load(&store, &resolve_name(&store, name)?)?;
            if *json {
                let entries: Vec<_> = loaded
                    .entries
                    .iter()
                    .map(|(p, m)| json!({"platform": p.to_string(), "digest": m.digest.to_string(), "manifest": m.manifest, "config": m.config}))
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &json!({"digest": loaded.digest.to_string(), "index": loaded.is_index, "images": entries})
                    )?
                );
                return Ok(());
            }
            println!("{}{}", loaded.digest, if loaded.is_index { " (index)" } else { "" });
            for (p, m) in &loaded.entries {
                let pr = &m.config.process;
                let list = |v: &[String]| v.iter().map(|s| clean_line(s)).collect::<Vec<_>>().join(" ");
                println!("\n{}  {}", clean_line(&p.to_string()), m.digest);
                println!("  entrypoint: {}", list(&pr.entrypoint));
                println!("  cmd:        {}", list(&pr.cmd));
                println!("  workdir:    {}", clean_line(pr.working_dir.as_deref().unwrap_or("/")));
                println!("  user:       {}", clean_line(pr.user.as_deref().unwrap_or("root")));
                for e in &pr.env {
                    println!("  env:        {}", clean_line(e));
                }
                let reference = m
                    .config
                    .source
                    .reference
                    .as_deref()
                    .map(|r| format!(" {}", clean_line(r)))
                    .unwrap_or_default();
                println!(
                    "  source:     {}{reference} (unverified provenance)",
                    m.config.source.manifest_digest
                );
                println!("  layers:");
                for l in &m.manifest.layers {
                    let inherits = if l.annotation(kiln_image::types::ANN_INHERITS).is_some() {
                        "  inherits"
                    } else {
                        ""
                    };
                    println!("    {}  {:>10}{inherits}", l.digest, human_size(l.size));
                }
            }
        }
        Cmd::Ls { json } => {
            let store = open_store(&cli)?;
            let mut rows = Vec::new();
            for (name, digest) in store.refs()? {
                let (platforms, size) = match load(&store, &digest) {
                    Ok(l) => {
                        let ps: Vec<String> = l.entries.iter().map(|(p, _)| clean_line(&p.to_string())).collect();
                        let size: u64 = l
                            .entries
                            .iter()
                            .flat_map(|(_, m)| m.manifest.layers.iter().map(|d| d.size))
                            .sum();
                        (ps.join(","), size)
                    }
                    Err(e) => (format!("<{}>", clean_line(&e.to_string())), 0),
                };
                rows.push((name, digest, platforms, size));
            }
            if *json {
                let v: Vec<_> = rows
                    .iter()
                    .map(|(n, d, p, s)| json!({"name": n, "digest": d.to_string(), "platforms": p, "size": s}))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            println!("{:<32} {:<19} {:<24} SIZE", "NAME", "DIGEST", "PLATFORMS");
            for (n, d, p, s) in rows {
                println!("{n:<32} {:<19} {p:<24} {}", &d.to_string()[..19], human_size(s));
            }
        }
        Cmd::Gc => {
            // Source OCI blobs are not referenced by kiln images, so GC frees them;
            // a later convert re-verifies them from the source.
            let r = open_store(&cli)?.gc()?;
            println!(
                "removed {} blobs ({}) and {} cache entries",
                r.blobs_removed,
                human_size(r.bytes_freed),
                r.cache_entries_removed
            );
        }
    }
    Ok(())
}

/// The error chain joined by `: `, skipping causes their parent already prints.
fn error_message(e: &anyhow::Error) -> String {
    let mut msg = String::new();
    for cause in e.chain() {
        let s = cause.to_string();
        if !msg.contains(&s) {
            if !msg.is_empty() {
                msg.push_str(": ");
            }
            msg.push_str(&s);
        }
    }
    msg
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kiln: error: {}", clean(&error_message(&e)));
            ExitCode::FAILURE
        }
    }
}
```

- [ ] **Step 6: Run the tests**

Run: `cargo test -q -p kiln`
Expected: all pass (7 tests).

- [ ] **Step 7: Format, lint and run the whole workspace**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace -q`
Expected: no clippy warnings; every test passes.

- [ ] **Step 8: Commit**

```bash
git add crates/kiln Cargo.lock
git commit -m "feat(kiln): convert, import, inspect, ls and gc commands" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P7rdQAESQYNEo2qFobUNxt"
```


### Task 9: `kiln bench`

**Files:**
- Create: `crates/kiln/src/bench.rs`
- Modify: `crates/kiln/Cargo.toml`, `crates/kiln/src/main.rs`, `crates/kiln/tests/cli.rs`

**Interfaces:**
- Consumes: Task 8's `ConvertArgs`; `kiln_image::convert_local`; `kiln_oci::resolve_local` and types.
- Produces: `kiln bench PATH [--platform P] [--max-layers N] [--jobs N]` printing `{platform, layers, cold_ms, warm_ms, changed_top_ms, changed_top_bytes, targets, met}`; hidden `--changed-top-bytes` (default 50 MiB) for tests; `bench::CHANGED_TOP_BYTES`.

- [ ] **Step 1: Write the failing test**

In `crates/kiln/tests/cli.rs`, replace:
```rust
    );
}
```
with:
```rust
    );
}

#[test]
fn bench_emits_json() {
    let src = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let path = layout(&src.path().join("app"), "php", simple());
    let out = kiln(home.path())
        .args(["bench", "--platform", "linux/arm64", "--changed-top-bytes", "100000"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["layers"], 1);
    for k in ["cold_ms", "warm_ms", "changed_top_ms"] {
        assert!(v[k].is_u64(), "{k}");
    }
}
```

- [ ] **Step 2: Run it to see it fail**

Run: `cargo test -q -p kiln --test cli bench_emits_json`
Expected: FAIL: `unrecognized subcommand 'bench'`.

- [ ] **Step 3: Write the benchmark**

It measures spec §1's three targets in a fresh temporary store: cold convert, warm re-convert, and a derived layout with one new ~50 MB incompressible gzip layer on top (`derive_changed_top`). The derived layout reuses the source's verified blobs. CI records the JSON; nothing fails on a missed target yet (spec §11.6).

`crates/kiln/src/bench.rs`:
```rust
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
```

In `crates/kiln/Cargo.toml`, replace:
```toml
clap = { version = "4.6.7", features = ["derive"] }
kiln-image = { path = "../kiln-image" }
```
with:
```toml
clap = { version = "4.6.7", features = ["derive"] }
flate2 = "1.1.10"
kiln-image = { path = "../kiln-image" }
```

In `crates/kiln/Cargo.toml`, replace:
```toml
serde_json = "1.0.151"

[dev-dependencies]
tempfile = "3.27.0"
kiln-erofs = { path = "../kiln-erofs" }
```
with:
```toml
serde_json = "1.0.151"
tar = "0.4.46"
tempfile = "3.27.0"

[dev-dependencies]
kiln-erofs = { path = "../kiln-erofs" }
```

In `crates/kiln/src/main.rs`, replace:
```rust
mod sanitize;
```
with:
```rust
mod bench;
mod sanitize;
```

In `crates/kiln/src/main.rs`, replace:
```rust
    Gc,
}
```
with:
```rust
    Gc,
    /// Measure convert performance on a local OCI layout (JSON on stdout).
    Bench {
        path: PathBuf,
        #[command(flatten)]
        args: ConvertArgs,
        /// Size of the synthetic changed top layer (for tests).
        #[arg(long, hide = true, default_value_t = bench::CHANGED_TOP_BYTES)]
        changed_top_bytes: usize,
    },
}
```

In `crates/kiln/src/main.rs`, replace:
```rust
            );
        }
    }
    Ok(())
```
with:
```rust
            );
        }
        Cmd::Bench {
            path,
            args,
            changed_top_bytes,
        } => {
            let report = bench::run(path, &args.platforms()?, &args.options(), *changed_top_bytes)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
    }
    Ok(())
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -q -p kiln`
Expected: all pass (9 tests).

- [ ] **Step 5: Measure a real image**

```bash
cargo build --release -p kiln
docker pull --platform linux/arm64 php:8.4-cli
mkdir -p /tmp/kiln-bench && docker save --platform linux/arm64 php:8.4-cli -o /tmp/kiln-bench/php.tar
mkdir -p /tmp/kiln-bench/php && tar -xf /tmp/kiln-bench/php.tar -C /tmp/kiln-bench/php
target/release/kiln bench --platform linux/arm64 /tmp/kiln-bench/php
```
Expected on an M-series Mac (measured during planning on an M4 Pro): `cold_ms` ≈ 1900, `warm_ms` ≈ 5, `changed_top_ms` ≈ 450, and all three `met` true. Note the numbers in the task report. The changed-top run has 11 layers, so it also exercises squash.

- [ ] **Step 6: Format, lint and run the whole workspace**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace -q`
Expected: no clippy warnings; every test passes.

- [ ] **Step 7: Commit**

```bash
git add crates/kiln Cargo.lock
git commit -m "feat(kiln): bench command for the convert performance targets" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P7rdQAESQYNEo2qFobUNxt"
```


### Task 10: Format docs, README and the real-image comparison

**Files:**
- Create: `README.md`, `scripts/compare-export.sh`, `scripts/compare_tree.py`
- Modify: `docs/format.md`

**Interfaces:**
- Consumes: the `kiln` binary from Tasks 8–9.
- Produces: `docs/format.md` gains the normative kiln image section (spec §5.1 as built, provisional until M3), the store's cache-key definitions, and divergences 4–5; `README.md`; `scripts/compare-export.sh IMAGE [PLATFORM] [MAX_LAYERS]`, which checks a kernel overlay mount of kiln's layers against `docker export`.

- [ ] **Step 1: Extend `docs/format.md`**

Divergence 4 is the M1a carry-forward item (kiln drops tar-supplied `trusted.overlay.*` xattrs). Divergence 5 documents the trailing-data rule from Task 6.

In `docs/format.md`, replace:
```markdown
This document is normative. It currently defines the **erofs profile** (format version 1). The image manifest (§5.1 of the design spec) and the control protocol (§9.5) are added by milestones M1b and M3.
```
with:
````markdown
This document is normative. It defines the **kiln image** (schema version 1, provisional until M3 adds kernel and init layers) and the **erofs profile** (format version 1). The control protocol (§9.5 of the design spec) is added by milestone M3.

## kiln image, schema version 1

A kiln image is an OCI 1.1 artifact. All JSON kiln writes is serialised with keys sorted and no insignificant whitespace, so identical inputs give identical digests.

### Index (multi-platform images)

An OCI image index (`mediaType` `application/vnd.oci.image.index.v1+json`, `schemaVersion` 2) with `artifactType` `application/vnd.kiln.image.v1`. Each entry in `manifests` is a kiln manifest descriptor with `artifactType` `application/vnd.kiln.image.v1` and a `platform` (`os`, `architecture`, optional `variant`). Entries are sorted by platform (architecture, then os, then variant). A single-platform image is a bare manifest with no index.

### Manifest

An OCI image manifest (`mediaType` `application/vnd.oci.image.manifest.v1+json`, `schemaVersion` 2) with:
- `artifactType`: `application/vnd.kiln.image.v1`. Readers reject manifests without it.
- `config`: a descriptor with `mediaType` `application/vnd.kiln.image.config.v1+json`.
- `layers`, in boot order:
  1. Kernel, `application/vnd.kiln.kernel.v1` (from M3; absent in schema-1 images built before M3).
  2. Init layer, `application/vnd.kiln.init.v1.erofs` (from M3; absent likewise).
  3. App layers, lowest first, `application/vnd.kiln.layer.v1.erofs`: erofs profile images (below), applied as overlayfs lower layers.
- App layer annotations, **informational only** (never used for caching or verification):
  - `dev.kiln.source.digests`: the comma-separated digests of the OCI layers the erofs layer was built from, in order. A squashed bottom layer lists all of them.
  - `dev.kiln.inherits`: `"true"` when the layer has implicit directories whose attributes came from the layers below it. Such a layer is only valid on top of exactly those layers.

### Config

```json
{
  "architecture": "arm64",
  "erofsFormatVersion": 1,
  "process": { "cmd": ["php", "-a"], "entrypoint": ["docker-php-entrypoint"], "env": ["PATH=/usr/local/bin:/usr/bin:/bin"], "stopSignal": "SIGQUIT", "user": "www-data", "workingDir": "/app" },
  "schemaVersion": 1,
  "source": { "manifestDigest": "sha256:…", "reference": "docker.io/library/php:8.4-cli" }
}
```
- `schemaVersion`: 1. Readers reject other values.
- `architecture`: `amd64` or `arm64`.
- `process`: copied from the source OCI config. Empty lists and strings are omitted.
- `kernel` (`{ "profile", "version" }`) and `init` (`{ "version" }`): set from M3, omitted before.
- `source.manifestDigest`: the source OCI manifest. `source.reference` is present only for registry inputs. This is unverified provenance.
- `erofsFormatVersion`: the erofs profile version of every app layer.
- There is no kernel command line field and no kiln version field.

### Local store (informative)

`$KILN_HOME` (default `~/.local/share/kiln`) holds `blobs/sha256/<hex>`, `refs.json` (`{"refs": {"<name>": "<digest>"}}`), and three caches of one-line text files, written only after the blob they name is committed:
- `cache/layers/<source layer digest>@<format version>`: `erofs <digest>` for a layer that depends only on its tar, or `parents <JSON list of hex-encoded implicit paths>`.
- `cache/layers-ctx/<source layer digest>@<format version>@<ctx>`: the erofs digest for a layer with implicit parents. `ctx` is the hex SHA-256 of the JSON list, in implicit-path order, of `[hex(path), null]` (absent in the lowers, or not a directory) or `[hex(path), [mode, uid, gid, mtime_sec, mtime_nsec, [[xattr_index, hex(name), hex(value)], …]]]`.
- `cache/squash/<hex SHA-256 of the newline-joined erofs digests>@<format version>`: a squashed bottom layer.

`:` in a cache key is written as `_` in its file name.
````

In `docs/format.md`, replace:
```markdown
kiln is checked against containerd's overlayfs snapshotter (kiln-erofs Task 14). It deliberately differs in three cases that image builders do not produce:
1. containerd sets directory mtimes in a final pass. If a later entry in the same layer replaced a directory, or one of its parents, with a non-directory, that pass fails or re-times the replacement. kiln keeps each entry's own attributes.
```
with:
```markdown
kiln is checked against containerd's overlayfs snapshotter (kiln-erofs Task 14). It deliberately differs in these cases, which image builders do not produce:
1. containerd sets directory mtimes in a final pass. If a later entry in the same layer replaced a directory, or one of its parents, with a non-directory, that pass fails or re-times the replacement. kiln keeps each entry's own attributes.
```

In `docs/format.md`, replace:
```markdown
3. containerd follows symlinks in lower layers while resolving parents. kiln does not.
```
with:
```markdown
3. containerd follows symlinks in lower layers while resolving parents. kiln does not.
4. A tar may carry `trusted.overlay.*` xattrs in PAX records. containerd writes them to disk, where they can forge whiteouts, opaque directories or redirects. kiln drops them with a warning; overlay markers come only from `.wh.` entries.
5. containerd ignores everything after a tar's end-of-archive marker. kiln requires the decompressed remainder to be zero padding and rejects the layer otherwise, so the bytes covered by `diff_id` mean one thing.

Header numeric fields (mode, uid, gid, mtime, device numbers) that hold only NULs and spaces read as 0, as in Go's `archive/tar`.
```

- [ ] **Step 2: Write the README**

`README.md`:
````markdown
# kiln

kiln turns OCI container images into microVM images: one deterministic erofs filesystem per layer, stacked with overlayfs inside the guest. It runs natively on macOS and Linux, without root.

Status: milestone M1b. `kiln convert` works on local inputs. Registry pull and push come next, then `kiln run` (M3).

## Quickstart

```bash
cargo install --path crates/kiln

# A local OCI layout or `docker save` archive (Docker 25+ writes OCI-in-tar).
docker save --platform linux/arm64 php:8.4-cli -o php.tar
kiln convert --platform linux/arm64 --tag php:8.4-cli php.tar

kiln ls
kiln inspect php:8.4-cli
kiln gc
```

`convert` options:
- `--platform os/arch[/variant]`: repeatable; the default is the host's. Several platforms produce a multi-arch index.
- `--tag NAME`: the name in the store (default: `<file name>:latest`).
- `--ref NAME`: which image to take when the source holds several.
- `--max-layers N` (default 10): more app layers than this squashes the bottom ones into one.
- `--json`: a machine-readable summary.

A second convert of the same image is served from the layer cache. Changing a base layer reconverts only the layers that inherit attributes from it.

## Other commands

- `kiln import --from-store DIR NAME [--as NAME]` copies an image from another store (for example the macOS store mounted read-only into a Lima VM), verifying every blob.
- `kiln bench LAYOUT` measures cold, warm and changed-top-layer conversions and prints JSON.
- `--store DIR` (or `$KILN_HOME`) selects the store; the default is `~/.local/share/kiln`.

## Docs

- `docs/format.md`: the normative image format and erofs profile.
- `docs/superpowers/specs/2026-09-30-kiln-design.md`: the design.

kiln prints image-supplied strings (command lines, environment, error paths) with control characters removed.

License: Apache-2.0.
````

- [ ] **Step 3: Write the real-image comparison**

This is the oracle at image scale. It mounts kiln's erofs layers with the kernel's erofs and overlayfs, using kiln's mount options, and compares names, types, modes, owners, mtimes, content hashes, links, device numbers and xattrs against `docker export`. Two details matter. Docker Desktop's VM has only 8 loop devices, so the script makes more. The overlay mount must go through `mount(2)`: with tens of layers, util-linux's default `fsconfig` path rejects the long `lowerdir` string, even with plain directories.

`scripts/compare-export.sh`:
```bash
#!/usr/bin/env bash
# Converts a local Docker image with kiln and checks that the overlay of its erofs
# layers, mounted by the Linux kernel, matches `docker export` of the same image.
# Needs Docker (Docker Desktop works) and a built `target/release/kiln`.
# Usage: scripts/compare-export.sh IMAGE [PLATFORM] [MAX_LAYERS]
set -euo pipefail
image="$1"
platform="${2:-linux/arm64}"
max_layers="${3:-10}"
root=$(cd "$(dirname "$0")/.." && pwd)
kiln="$root/target/release/kiln"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

docker save --platform "$platform" "$image" -o "$work/image.tar"
"$kiln" --store "$work/store" convert --platform "$platform" --max-layers "$max_layers" --tag cmp:1 "$work/image.tar"
mkdir "$work/layers"
"$kiln" --store "$work/store" inspect --json cmp:1 \
  | python3 -c 'import json,sys; [print(l["digest"].split(":")[1]) for l in json.load(sys.stdin)["images"][0]["manifest"]["layers"]]' \
  | nl -v0 -nrz -w2 | while read -r i hex; do cp "$work/store/blobs/sha256/$hex" "$work/layers/$i.erofs"; done
cid=$(docker create --platform "$platform" "$image")
docker export "$cid" -o "$work/ref.tar"
docker rm "$cid" >/dev/null
cp "$root/scripts/compare_tree.py" "$work/"

docker run --rm --privileged --platform "$platform" -v "$work":/w debian:trixie-slim bash -euc '
  apt-get -qq update >/dev/null
  DEBIAN_FRONTEND=noninteractive apt-get -qq install -y python3 >/dev/null 2>&1
  for i in $(seq 0 63); do [ -e /dev/loop$i ] || mknod /dev/loop$i b 7 $i; done
  mkdir -p /ref /merged
  tar --numeric-owner --xattrs --xattrs-include="*" -xpf /w/ref.tar -C /ref
  lower=""
  for f in $(ls /w/layers | sort); do
    mkdir -p /l/$f && mount -t erofs -o ro,loop /w/layers/$f /l/$f
    lower="/l/$f${lower:+:$lower}"
  done
  # The fsconfig API rejects long lowerdir strings (many layers); use mount(2).
  LIBMOUNT_FORCE_MOUNT2=always mount -t overlay overlay -o "lowerdir=$lower,xino=on,redirect_dir=off,index=off,metacopy=off" /merged
  python3 /w/compare_tree.py /ref /merged
'
```

`scripts/compare_tree.py`:
```python
"""Compares two directory trees: types, modes, owners, mtimes, content, links, devices, xattrs.

Usage: compare_tree.py REFERENCE CANDIDATE. Exits 1 if they differ. Paths that
`docker export` adds for the container runtime are skipped.
"""
import hashlib
import os
import stat
import sys

SKIP_TOP = {"dev", "proc", "sys"}
SKIP = {".dockerenv", "etc/hosts", "etc/hostname", "etc/resolv.conf", "etc/mtab"}


def describe(path):
    st = os.lstat(path)
    m = st.st_mode
    e = [stat.S_IFMT(m), stat.S_IMODE(m), st.st_uid, st.st_gid]
    if not stat.S_ISDIR(m):
        e.append(int(st.st_mtime))
    if stat.S_ISREG(m):
        h = hashlib.sha256()
        with open(path, "rb") as f:
            for chunk in iter(lambda: f.read(1 << 20), b""):
                h.update(chunk)
        e += [st.st_size, h.hexdigest()]
    if stat.S_ISLNK(m):
        e.append(os.readlink(path))
    if stat.S_ISCHR(m) or stat.S_ISBLK(m):
        e.append(st.st_rdev)
    names = os.listxattr(path, follow_symlinks=False)
    e.append(sorted((k, os.getxattr(path, k, follow_symlinks=False)) for k in names))
    return e


def walk(root):
    out = {}
    for dirpath, dirs, files in os.walk(root):
        for name in dirs + files:
            path = os.path.join(dirpath, name)
            rel = os.path.relpath(path, root)
            if rel in SKIP or rel.split("/")[0] in SKIP_TOP:
                continue
            out[rel] = describe(path)
    return out


def main():
    ref, cand = walk(sys.argv[1]), walk(sys.argv[2])
    diff = [k for k in sorted(set(ref) | set(cand)) if ref.get(k) != cand.get(k)]
    print(f"reference {len(ref)} entries, kiln {len(cand)} entries, {len(diff)} differ")
    for k in diff[:20]:
        print(f"  {k}\n    reference: {ref.get(k)}\n    kiln:      {cand.get(k)}")
    sys.exit(1 if diff else 0)


if __name__ == "__main__":
    main()
```

```bash
chmod +x scripts/compare-export.sh
```

- [ ] **Step 4: Run it on real images**

```bash
cargo build --release -p kiln
scripts/compare-export.sh php:8.4-cli linux/arm64        # 10 layers, no squash
scripts/compare-export.sh php:8.4-cli linux/arm64 3      # bottom 8 squashed
```
Expected: each run ends with `reference 12321 entries, kiln 12321 entries, 0 differ`; the exact entry count changes with the image tag. During planning the same check passed for a 40-layer Laravel image (squashed to 10, and unsquashed), a 21-layer Ruby image and a 15-layer Node image. Run it on at least one more image you have locally and note the result in the task report.

- [ ] **Step 5: Format, lint and run the whole workspace**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace -q`
Expected: no clippy warnings; every test passes.

- [ ] **Step 6: Commit**

```bash
git add docs/format.md README.md scripts/compare-export.sh scripts/compare_tree.py Cargo.lock
git commit -m "docs: kiln image format, README and real-image comparison script" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P7rdQAESQYNEo2qFobUNxt"
```


## Spec coverage (M1b-1)

| Spec item | Task |
|---|---|
| §5.1 Image format (provisional: no kernel or init layers yet), annotations informational only | 6, 10 |
| §5.2 Store: layout, atomic writes, cache-hit stat, refs.json, lock, GC order | 1, 2, 6 |
| §6.1 Resolve, validate before fetch, verified fetch to EOF, `diff_id` check, local inputs hashed | 3, 4, 6 |
| §6.2 Convert layers: cache key, `erofs`/`parents` entries, parallel conversion, deferred finalisation, imported layers never cached | 6, 7 |
| §6.3 Implicit parents: ctx hash, `layers-ctx` cache, `dev.kiln.inherits` | 5, 6 |
| §6.4 Squash: `--max-layers` 10, bottom `N − max + 1` from erofs, squash cache | 6 |
| §6.6 Determinism: canonical JSON, no reference for local inputs, job-count independence | 3, 6 |
| §7.6 Per-image (64 GiB) and expansion-ratio (200) limits | 6 |
| §10 `kiln import --from-store` on a read-only store | 1, 7, 8 |
| §11.5 Hostile inputs, local half: wrong content for a digest, trailing junk, `diff_id` mismatch, decompression bomb, million-entry layer (scaled-down limit), oversized PAX, hardlink cycles | 4, 6 |
| §11.6 `kiln bench` | 9 |
| §11.2 Oracle at image scale (real images vs `docker export`) | 10 |
| §13 Error handling and T8 sanitising | 8 |
| T1, T3, T8 | 1, 4, 6, 7, 8 |
| §12 M1 CLI: `convert`, `import`, `inspect`, `ls`, `gc`, `bench` | 8, 9 |

**M1a carry-forward:**
- `resolve_inherited` cost: Task 5.
- Output contract: Task 6, which uses `TmpBlob::reopen`.
- Never squash pulled erofs: Tasks 6 and 7.
- T8 sanitising of `MalformedTar`: Task 8.
- Per-image limits: Task 6.
- The format.md `trusted.overlay` divergence: Task 10.
- The remaining tar differentials go to M1b-2.

**Left to plan M1b-2:**
- `kiln-registry`: auth, credential helpers, T2 redirect rules and redaction.
- `kiln pull`/`push`, and registry references in `convert`, with `source.reference` normalised.
- The network half of §11.5: descriptor `urls`, redirects to `169.254.169.254` and RFC 1918.
- The zot and distribution CI registries.
