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
