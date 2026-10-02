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
            return Err(OciError::MetadataTooLarge {
                path: path.to_string(),
                max,
            });
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
                // The effective size: a PAX `size` record overrides the ustar field.
                let size = e.size();
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

    #[test]
    fn archive_indexes_the_effective_pax_size() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("pax.tar");
        let mut b = tar::Builder::new(Vec::new());
        b.append_pax_extensions([("size", &b"5"[..])]).unwrap();
        // The ustar field says 3 bytes, but the PAX record says 5.
        let mut h = tar::Header::new_ustar();
        h.set_path("f").unwrap();
        h.set_size(3);
        h.set_mode(0o644);
        h.set_cksum();
        b.append(&h, &b"ABCDE"[..]).unwrap();
        let bytes = b.into_inner().unwrap();
        fs::write(&p, &bytes).unwrap();
        let mut want = String::new();
        let mut reference = tar::Archive::new(&bytes[..]);
        for e in reference.entries().unwrap() {
            let mut e = e.unwrap();
            if e.path().unwrap().to_str() == Some("f") {
                e.read_to_string(&mut want).unwrap();
            }
        }
        assert_eq!(want, "ABCDE", "the tar crate reads the PAX size");
        let a = TarArchive::open(&p).unwrap();
        let mut got = String::new();
        a.open_path("f").unwrap().read_to_string(&mut got).unwrap();
        assert_eq!(got, want);
    }

    #[test]
    fn oversized_metadata_is_a_typed_error() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("oci-layout"), b"{}").unwrap();
        fs::write(dir.path().join("index.json"), vec![b' '; 11]).unwrap();
        let l = DirLayout::open(dir.path()).unwrap();
        assert!(matches!(
            l.read_small("index.json", 10),
            Err(OciError::MetadataTooLarge { max: 10, .. })
        ));
        assert_eq!(l.read_small("index.json", 11).unwrap().len(), 11);
    }
}
