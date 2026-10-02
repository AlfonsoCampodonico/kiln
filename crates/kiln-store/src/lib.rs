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
