//! A chunk source for vmkit's lazy restore (design §6.1): chunks come from the store, which
//! is the local chunk cache, and a chunk the store lacks is fetched from the registry first,
//! verified on its way in (`put_verified`, T1). vmkit verifies every chunk again before use.

use std::io::Read;
use std::time::Duration;

use kiln_registry::Client;
use kiln_store::{Digest, Store, StoreError};

use crate::error::Result;

/// How many times a registry fetch is tried before the chunk counts as unavailable.
const ATTEMPTS: u32 = 3;

/// Serves a snapshot's chunks from `store`, fetching missing ones from `repo` on the
/// client's registry. Without a client only stored chunks are served.
pub struct StoreSource {
    store: Store,
    remote: Option<(Client, String)>,
}

impl StoreSource {
    pub fn new(store: Store, remote: Option<(Client, String)>) -> Self {
        Self { store, remote }
    }

    /// The bytes of chunk `digest`, which must be `size` bytes long.
    ///
    /// Holds the store's shared lock throughout, so a `kiln gc` cannot remove the chunk (or
    /// the staged download) between fetching and reading it. A lazy bundle directory keeps
    /// its chunks in the store only while its snapshot is tagged.
    pub fn chunk(&self, digest: &Digest, size: u64) -> Result<Vec<u8>> {
        let _lock = self.store.lock_shared()?;
        if !self.store.has_blob(digest)
            && let Some((client, repo)) = &self.remote
        {
            let mut delay = Duration::from_millis(200);
            for attempt in 1..=ATTEMPTS {
                match client.fetch_blob(repo, digest, size, &self.store) {
                    Ok(_) => break,
                    Err(e) if attempt == ATTEMPTS => return Err(e.into()),
                    // A verification failure is not transient, but the registry may answer
                    // differently next time; it never enters the store either way.
                    Err(_) => {
                        std::thread::sleep(delay);
                        delay *= 4;
                    }
                }
            }
        }
        let blob = self.store.open_blob(digest)?;
        let actual = blob.metadata()?.len();
        if actual != size {
            return Err(StoreError::SizeMismatch {
                digest: digest.clone(),
                expected: size,
                actual,
            }
            .into());
        }
        let mut bytes = Vec::with_capacity(size as usize);
        blob.take(size).read_to_end(&mut bytes)?;
        Ok(bytes)
    }
}

#[cfg(target_os = "linux")]
impl vmkit::lazy::ChunkSource for StoreSource {
    fn fetch(&self, digest: &str, size: u64) -> std::io::Result<Vec<u8>> {
        let d = Digest::parse(digest).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        self.chunk(&d, size).map_err(std::io::Error::other)
    }
}
