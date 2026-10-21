//! Push and pull against the in-process test registry.
mod common;

use std::fs;

use common::*;
use kiln_oci::media::OCI_MANIFEST;
use kiln_oci::{Descriptor, ImageManifest, canonical_json};
use kiln_registry::testregistry::{Config, TestRegistry};
use kiln_registry::{Client, DockerConfig, Reference};
use kiln_snapshot::{
    ChunkedFile, Fetch, SNAPSHOT_ARTIFACT, SNAPSHOT_CHUNK, SNAPSHOT_CONFIG, SnapshotConfig, SnapshotError, assemble,
    chunk_bundle, fetch_chunks, load, pull, push, tag, write_lazy,
};
use kiln_store::{Digest, Store, StoreError};

fn client(host: &str) -> Client {
    Client::new(host, DockerConfig::anonymous()).unwrap()
}

fn reference(host: &str, path: &str) -> Reference {
    Reference::parse(&format!("{host}/{path}")).unwrap()
}

fn files() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("vmstate", pattern(3000, 1)),
        (
            "memory",
            [pattern(K, 2), zeros(2 * K), pattern(K, 3), pattern(K / 4, 4)].concat(),
        ),
        ("disk-0", [pattern(K, 2), zeros(K)].concat()),
    ]
}

/// Chunks `files` into `store` and tags it `name`.
fn stored(store: &Store, files: &[(&str, Vec<u8>)], name: &str) -> Digest {
    let dir = tempfile::tempdir().unwrap();
    bundle(dir.path(), files);
    let d = chunk_bundle(store, dir.path(), K).unwrap().manifest_digest;
    tag(store, name, &d).unwrap();
    d
}

fn is_digest_mismatch(e: &SnapshotError) -> bool {
    matches!(
        e,
        SnapshotError::Store(StoreError::DigestMismatch { .. })
            | SnapshotError::Registry(kiln_registry::RegistryError::Store(StoreError::DigestMismatch { .. }))
    )
}

#[test]
fn push_then_pull_round_trips() {
    let reg = TestRegistry::start(Config::default());
    let c = client(&reg.host());
    let r = reference(&reg.host(), "team/vm:1");
    let ((_h1, here), (_h2, there), (_h3, lazy)) = (store(), store(), store());
    let d = stored(&here, &files(), "vm");
    let pushed = push(&here, &c, "vm", &r).unwrap();
    assert_eq!(pushed.digest, d);
    assert_eq!(pushed.blobs, 5, "4 distinct chunks and the config");
    assert_eq!(pushed.skipped, 0);

    // Eager: every chunk is fetched, verified, and the bundle is whole.
    let pulled = pull(&there, &c, &r, "copy", Fetch::Eager).unwrap();
    assert_eq!(pulled.loaded.digest, d);
    assert_eq!(pulled.report.blobs, 5);
    assert_eq!(there.get_ref("copy").unwrap(), Some(d.clone()));
    let out = tempfile::tempdir().unwrap();
    assemble(&there, &pulled.loaded, out.path()).unwrap();
    for (name, bytes) in files() {
        assert_eq!(fs::read(out.path().join(name)).unwrap(), bytes, "{name}");
    }
    // Pulling again downloads nothing.
    let again = pull(&there, &c, &r, "copy", Fetch::Eager).unwrap();
    assert_eq!((again.report.blobs, again.report.skipped), (0, 5));

    // Lazy: only the manifest and the config, and the snapshot is tagged.
    let gets = reg.blob_requests("GET");
    let pulled = pull(&lazy, &c, &r, "vm", Fetch::Lazy).unwrap();
    assert_eq!(reg.blob_requests("GET") - gets, 1, "the config only");
    assert_eq!(lazy.list_blobs().unwrap().len(), 2);
    assert_eq!(lazy.get_ref("vm").unwrap(), Some(d.clone()));
    let out = tempfile::tempdir().unwrap();
    write_lazy(&pulled.loaded, out.path()).unwrap();
    assert!(out.path().join("lazy.json").is_file());
    // The chunks can follow later, verified.
    let report = fetch_chunks(&lazy, &c, r.repository(), &pulled.loaded).unwrap();
    assert_eq!(report.blobs, 4);
    assert_eq!(lazy.list_blobs().unwrap().len(), 6);
    // A tagged snapshot keeps its chunks through GC.
    lazy.gc().unwrap();
    assert_eq!(lazy.list_blobs().unwrap().len(), 6);
}

#[test]
fn push_skips_chunks_the_registry_has() {
    let reg = TestRegistry::start(Config::default());
    let c = client(&reg.host());
    let (_h, s) = store();
    stored(&s, &files(), "a");
    push(&s, &c, "a", &reference(&reg.host(), "team/vm:a")).unwrap();
    // A minute later: the same memory and disk, a new vmstate.
    let mut later = files();
    later[0].1 = pattern(3000, 9);
    stored(&s, &later, "b");
    let puts = reg
        .log()
        .iter()
        .filter(|l| l.method == "PUT" && l.path.contains("/blobs/"))
        .count();
    let report = push(&s, &c, "b", &reference(&reg.host(), "team/vm:b")).unwrap();
    assert_eq!(report.skipped, 3, "the shared chunks");
    assert_eq!(report.blobs, 2, "the new vmstate chunk and the new config");
    let puts_after = reg
        .log()
        .iter()
        .filter(|l| l.method == "PUT" && l.path.contains("/blobs/"))
        .count();
    assert_eq!(puts_after - puts, 2);
}

#[test]
fn push_refuses_a_snapshot_whose_chunks_are_not_here() {
    let reg = TestRegistry::start(Config::default());
    let c = client(&reg.host());
    let r = reference(&reg.host(), "team/vm:1");
    let ((_h1, here), (_h2, lazy)) = (store(), store());
    stored(&here, &files(), "vm");
    push(&here, &c, "vm", &r).unwrap();
    pull(&lazy, &c, &r, "vm", Fetch::Lazy).unwrap();
    let other = TestRegistry::start(Config::default());
    let e = push(&lazy, &client(&other.host()), "vm", &reference(&other.host(), "x/y:1")).unwrap_err();
    assert!(matches!(e, SnapshotError::Store(StoreError::NotFound(_))), "{e}");
    assert_eq!(other.blob_requests("HEAD"), 0, "nothing is uploaded first");
}

#[test]
fn a_tampered_chunk_is_refused_and_nothing_is_tagged() {
    let (_h1, here) = store();
    let d = stored(&here, &files(), "vm");
    let chunk = load(&here, &d).unwrap().manifest.layers[2].digest.clone();
    let reg = TestRegistry::start(Config {
        corrupt: Some(chunk.clone()),
        ..Config::default()
    });
    let c = client(&reg.host());
    let r = reference(&reg.host(), "team/vm:1");
    push(&here, &c, "vm", &r).unwrap();
    let (_h2, there) = store();
    let e = pull(&there, &c, &r, "vm", Fetch::Eager).unwrap_err();
    assert!(is_digest_mismatch(&e), "{e}");
    assert!(there.refs().unwrap().is_empty(), "nothing is tagged");
    assert!(!there.has_blob(&chunk));
}

/// A config with too many files, and a valid chunk, pushed by hand.
fn hostile(store: &Store, files: usize) -> (Digest, Vec<u8>) {
    let chunk = pattern(K, 1);
    let cd = store.put_bytes(&chunk).unwrap();
    let config = SnapshotConfig {
        schema_version: 1,
        chunk_size: K,
        bundle: bundle_json(),
        files: (0..files)
            .map(|i| ChunkedFile {
                name: format!("f{i}"),
                size: K,
                chunks: vec![Some(cd.clone())],
            })
            .collect(),
    };
    let config = canonical_json(&config);
    let manifest = ImageManifest {
        schema_version: 2,
        media_type: Some(OCI_MANIFEST.into()),
        artifact_type: Some(SNAPSHOT_ARTIFACT.into()),
        config: Descriptor::new(SNAPSHOT_CONFIG, store.put_bytes(&config).unwrap(), config.len() as u64),
        layers: vec![Descriptor::new(SNAPSHOT_CHUNK, cd, K)],
        annotations: None,
    };
    let bytes = canonical_json(&manifest);
    (store.put_bytes(&bytes).unwrap(), bytes)
}

fn push_by_hand(store: &Store, c: &Client, repo: &str, tag: &str, bytes: &[u8]) {
    let m: ImageManifest = serde_json::from_slice(bytes).unwrap();
    for d in std::iter::once(&m.config).chain(&m.layers) {
        c.push_blob(repo, &d.digest, &store.blob_path(&d.digest)).unwrap();
    }
    c.put_manifest(repo, tag, OCI_MANIFEST, bytes).unwrap();
}

#[test]
fn a_config_over_the_limits_is_refused_before_any_chunk_is_fetched() {
    let reg = TestRegistry::start(Config::default());
    let c = client(&reg.host());
    let (_h1, here) = store();
    let (_, bytes) = hostile(&here, 65);
    push_by_hand(&here, &c, "team/vm", "1", &bytes);
    let m: ImageManifest = serde_json::from_slice(&bytes).unwrap();
    let r = reference(&reg.host(), "team/vm:1");
    for fetch in [Fetch::Eager, Fetch::Lazy] {
        let (_h2, there) = store();
        let gets = reg.blob_requests("GET");
        let e = pull(&there, &c, &r, "vm", fetch).unwrap_err();
        assert!(
            matches!(
                e,
                SnapshotError::LimitExceeded {
                    what: "files in a snapshot",
                    max: 64
                }
            ),
            "{e}"
        );
        assert_eq!(reg.blob_requests("GET") - gets, 1, "the config, and no chunk");
        assert!(!there.has_blob(&m.layers[0].digest));
        assert!(there.refs().unwrap().is_empty());
    }
    // A config descriptor over the size limit is refused before the config is fetched.
    let mut big = m.clone();
    big.config.size = (4 << 20) + 1;
    let big = canonical_json(&big);
    reg.put_manifest("big", OCI_MANIFEST, &big);
    let (_h3, there) = store();
    let gets = reg.blob_requests("GET");
    let e = pull(&there, &c, &reference(&reg.host(), "team/vm:big"), "vm", Fetch::Lazy).unwrap_err();
    assert!(
        matches!(
            e,
            SnapshotError::LimitExceeded {
                what: "snapshot config size",
                ..
            }
        ),
        "{e}"
    );
    assert_eq!(reg.blob_requests("GET"), gets);
}

#[test]
fn pull_refuses_what_is_not_a_snapshot() {
    let reg = TestRegistry::start(Config::default());
    let c = client(&reg.host());
    let image = ImageManifest {
        schema_version: 2,
        media_type: Some(OCI_MANIFEST.into()),
        artifact_type: Some("application/vnd.kiln.image.v1".into()),
        config: Descriptor::new("application/vnd.kiln.image.config.v1+json", Digest::of(b"{}"), 2),
        layers: vec![],
        annotations: None,
    };
    reg.put_manifest("img", OCI_MANIFEST, &canonical_json(&image));
    let (_h, there) = store();
    let e = pull(&there, &c, &reference(&reg.host(), "team/app:img"), "x", Fetch::Eager).unwrap_err();
    assert!(matches!(e, SnapshotError::NotASnapshot(_)), "{e}");
    assert_eq!(reg.blob_requests("GET"), 0);
    assert!(there.refs().unwrap().is_empty());
    // A client for another registry is refused before any request.
    let other = TestRegistry::start(Config::default());
    let e = pull(
        &there,
        &client(&other.host()),
        &reference(&reg.host(), "team/app:img"),
        "x",
        Fetch::Lazy,
    );
    assert!(matches!(e, Err(SnapshotError::BadOption(_))));
}
