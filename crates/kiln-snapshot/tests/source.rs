//! The store and registry chunk source for lazy restore.
mod common;

use common::*;
use kiln_registry::testregistry::{Config, TestRegistry};
use kiln_registry::{Client, DockerConfig};
use kiln_snapshot::StoreSource;
use kiln_store::Digest;

fn client(host: &str) -> Client {
    Client::new(host, DockerConfig::anonymous()).unwrap()
}

/// A registry holding `bytes` as a blob of `repo`, pushed from a scratch store.
fn registry_with(bytes: &[u8]) -> (TestRegistry, Digest) {
    let reg = TestRegistry::start(Config::default());
    let (_dir, s) = store();
    let d = s.put_bytes(bytes).unwrap();
    client(&reg.host()).push_blob("team/vm", &d, &s.blob_path(&d)).unwrap();
    (reg, d)
}

#[test]
fn store_source_prefers_the_store() {
    let (_dir, s) = store();
    let d = s.put_bytes(b"chunk").unwrap();
    let src = StoreSource::new(s.clone(), None);
    assert_eq!(src.chunk(&d, 5).unwrap(), b"chunk");
    assert!(src.chunk(&d, 4).is_err(), "the size must match");
    assert!(src.chunk(&Digest::of(b"other"), 5).is_err(), "nothing to fetch it from");
}

#[test]
fn store_source_fetches_and_caches() {
    let (reg, d) = registry_with(b"chunk from the registry");
    let (_dir, s) = store();
    let src = StoreSource::new(s.clone(), Some((client(&reg.host()), "team/vm".into())));
    assert_eq!(src.chunk(&d, 23).unwrap(), b"chunk from the registry");
    assert!(s.has_blob(&d), "the store is the chunk cache");
    let gets = reg.blob_requests("GET");
    assert_eq!(src.chunk(&d, 23).unwrap(), b"chunk from the registry");
    assert_eq!(reg.blob_requests("GET"), gets, "a cached chunk is not fetched again");
}

#[test]
fn a_bad_registry_chunk_never_enters_the_store() {
    let (reg, _) = registry_with(b"the registry's bytes");
    let (_dir, s) = store();
    let src = StoreSource::new(s.clone(), Some((client(&reg.host()), "team/vm".into())));
    // A digest the registry's bytes do not hash to: it serves nothing that verifies.
    let want = Digest::of(b"what the snapshot names");
    assert!(src.chunk(&want, 20).is_err());
    assert!(!s.has_blob(&want));
}
