//! Chunking a bundle directory into the store.
mod common;

use std::fs;

use common::*;
use kiln_oci::ImageManifest;
use kiln_snapshot::{SNAPSHOT_ARTIFACT, SNAPSHOT_CHUNK, SNAPSHOT_CONFIG, SnapshotError, chunk_bundle};
use kiln_store::Digest;

#[test]
fn zero_chunks_are_not_stored() {
    let dir = tempfile::tempdir().unwrap();
    let (_h, store) = store();
    bundle(
        dir.path(),
        &[("memory", [pattern(K, 1), zeros(K), pattern(K, 2)].concat())],
    );
    let built = chunk_bundle(&store, dir.path(), K).unwrap();
    let f = &built.config.files[0];
    assert!(f.chunks[0].is_some() && f.chunks[1].is_none() && f.chunks[2].is_some());
    assert_eq!(built.manifest.layers.len(), 2);
    assert!(
        !store.has_blob(&Digest::of(&zeros(K))),
        "the zero chunk is never stored"
    );
    // Two chunks, the config and the manifest.
    assert_eq!(store.list_blobs().unwrap().len(), 4);
}

#[test]
fn identical_chunks_are_stored_once() {
    let dir = tempfile::tempdir().unwrap();
    let (_h, store) = store();
    let a = pattern(K, 7);
    bundle(
        dir.path(),
        &[("memory", [a.clone(), a.clone()].concat()), ("disk-0", a.clone())],
    );
    let built = chunk_bundle(&store, dir.path(), K).unwrap();
    assert_eq!(built.manifest.layers.len(), 1);
    assert_eq!(built.manifest.layers[0].digest, Digest::of(&a));
    let all: Vec<_> = built.config.files.iter().flat_map(|f| f.chunks.clone()).collect();
    assert_eq!(all, vec![Some(Digest::of(&a)); 3]);
}

#[test]
fn the_same_bundle_gives_the_same_digest() {
    let (d1, d2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let files = [
        ("vmstate", pattern(1000, 3)),
        ("memory", [pattern(K, 1), zeros(2 * K)].concat()),
        ("disk-0", pattern(3 * K + 5, 9)),
    ];
    bundle(d1.path(), &files);
    // Written in another order: the result does not depend on directory order.
    let mut reversed = files.clone();
    reversed.reverse();
    bundle(d2.path(), &reversed);
    let ((_h1, s1), (_h2, s2)) = (store(), store());
    let a = chunk_bundle(&s1, d1.path(), K).unwrap();
    let b = chunk_bundle(&s2, d2.path(), K).unwrap();
    assert_eq!(a.manifest_digest, b.manifest_digest);
    assert_eq!(
        chunk_bundle(&s1, d1.path(), K).unwrap().manifest_digest,
        a.manifest_digest
    );
    let names: Vec<_> = a.config.files.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["disk-0", "memory", "vmstate"], "files are sorted by name");
    // A different chunk size is a different snapshot.
    assert_ne!(
        chunk_bundle(&s1, d1.path(), 2 * K).unwrap().manifest_digest,
        a.manifest_digest
    );
}

#[test]
fn the_manifest_is_a_snapshot_artifact_without_annotations() {
    let dir = tempfile::tempdir().unwrap();
    let (_h, store) = store();
    bundle(dir.path(), &[("memory", pattern(2 * K, 1)), ("disk-0", pattern(K, 2))]);
    let built = chunk_bundle(&store, dir.path(), K).unwrap();
    let bytes = store.read_metadata(&built.manifest_digest).unwrap();
    let m: ImageManifest = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(m, built.manifest);
    assert_eq!(bytes, kiln_oci::canonical_json(&m), "stored canonically");
    assert_eq!(m.schema_version, 2);
    assert_eq!(m.media_type.as_deref(), Some(kiln_registry::OCI_MANIFEST));
    assert_eq!(m.artifact_type.as_deref(), Some(SNAPSHOT_ARTIFACT));
    assert_eq!(m.config.media_type, SNAPSHOT_CONFIG);
    assert!(m.annotations.is_none() && m.layers.iter().all(|l| l.annotations.is_none()));
    assert!(m.layers.iter().all(|l| l.media_type == SNAPSHOT_CHUNK));
    // Layers in first use: disk-0's chunk, then memory's two.
    let first_use: Vec<_> = ["disk-0", "memory"]
        .iter()
        .flat_map(|n| built.config.files.iter().find(|f| f.name == *n).unwrap().chunks.clone())
        .flatten()
        .collect();
    let layers: Vec<_> = m.layers.iter().map(|l| l.digest.clone()).collect();
    assert_eq!(layers, first_use);
    let config: serde_json::Value = serde_json::from_slice(&store.read_metadata(&m.config.digest).unwrap()).unwrap();
    assert_eq!(config["bundle"], bundle_json());
}

#[test]
fn the_last_chunk_may_be_short() {
    let dir = tempfile::tempdir().unwrap();
    let (_h, store) = store();
    bundle(dir.path(), &[("memory", pattern(2 * K + 100, 4)), ("empty", vec![])]);
    let built = chunk_bundle(&store, dir.path(), K).unwrap();
    let memory = built.config.files.iter().find(|f| f.name == "memory").unwrap();
    assert_eq!((memory.size, memory.chunks.len()), (2 * K + 100, 3));
    assert_eq!(built.manifest.layers[2].size, 100);
    let empty = built.config.files.iter().find(|f| f.name == "empty").unwrap();
    assert!(empty.chunks.is_empty());
}

#[cfg(unix)]
#[test]
fn a_symlink_or_directory_in_the_bundle_is_refused() {
    let (_h, store) = store();
    let host_secret = tempfile::NamedTempFile::new().unwrap();
    fs::write(host_secret.path(), b"secret").unwrap();

    let dir = tempfile::tempdir().unwrap();
    bundle(dir.path(), &[("memory", pattern(K, 1))]);
    std::os::unix::fs::symlink(host_secret.path(), dir.path().join("disk-0")).unwrap();
    assert!(matches!(
        chunk_bundle(&store, dir.path(), K),
        Err(SnapshotError::Bundle { .. })
    ));

    let dir = tempfile::tempdir().unwrap();
    bundle(dir.path(), &[("memory", pattern(K, 1))]);
    fs::create_dir(dir.path().join("disk-0")).unwrap();
    assert!(matches!(
        chunk_bundle(&store, dir.path(), K),
        Err(SnapshotError::Bundle { .. })
    ));

    // bundle.json itself is opened without following links.
    let dir = tempfile::tempdir().unwrap();
    bundle(dir.path(), &[("memory", pattern(K, 1))]);
    let outside = tempfile::tempdir().unwrap();
    let real = outside.path().join("bundle.json");
    fs::rename(dir.path().join("bundle.json"), &real).unwrap();
    std::os::unix::fs::symlink(&real, dir.path().join("bundle.json")).unwrap();
    assert!(matches!(
        chunk_bundle(&store, dir.path(), K),
        Err(SnapshotError::Bundle { .. })
    ));
    assert!(!store.list_blobs().unwrap().iter().any(|d| *d == Digest::of(b"secret")));
}

#[test]
fn bad_names_in_the_bundle_are_refused() {
    let (_h, store) = store();
    for bad in ["Disk-0", "lazy.json", ".hidden", "a b"] {
        let dir = tempfile::tempdir().unwrap();
        bundle(dir.path(), &[("memory", pattern(K, 1))]);
        fs::write(dir.path().join(bad), b"x").unwrap();
        assert!(
            matches!(chunk_bundle(&store, dir.path(), K), Err(SnapshotError::BadName(_))),
            "{bad}"
        );
    }
}

#[test]
fn bundle_json_must_be_a_bundle_of_version_1() {
    let (_h, store) = store();
    let dir = tempfile::tempdir().unwrap();
    bundle(dir.path(), &[("memory", pattern(K, 1))]);
    fs::write(dir.path().join("bundle.json"), br#"{"version":2}"#).unwrap();
    assert!(matches!(
        chunk_bundle(&store, dir.path(), K),
        Err(SnapshotError::UnknownBundleVersion(_))
    ));
    fs::write(dir.path().join("bundle.json"), b"not json").unwrap();
    assert!(matches!(
        chunk_bundle(&store, dir.path(), K),
        Err(SnapshotError::Json { .. })
    ));
    let mut big = br#"{"version":1,"pad":""#.to_vec();
    big.extend(vec![b'x'; 64 << 10]);
    big.extend(br#""}"#);
    fs::write(dir.path().join("bundle.json"), big).unwrap();
    assert!(matches!(
        chunk_bundle(&store, dir.path(), K),
        Err(SnapshotError::LimitExceeded { .. })
    ));
    fs::remove_file(dir.path().join("bundle.json")).unwrap();
    assert!(chunk_bundle(&store, dir.path(), K).is_err());
}

#[test]
fn a_bad_chunk_size_is_refused_before_reading() {
    let (_h, store) = store();
    let dir = tempfile::tempdir().unwrap();
    bundle(dir.path(), &[("memory", pattern(K, 1))]);
    for size in [1000, 32 << 10, 32 << 20] {
        assert!(matches!(
            chunk_bundle(&store, dir.path(), size),
            Err(SnapshotError::BadChunkSize { .. })
        ));
    }
    assert!(store.list_blobs().unwrap().is_empty());
}

#[test]
fn the_built_snapshot_passes_the_readers_checks() {
    let dir = tempfile::tempdir().unwrap();
    let (_h, store) = store();
    bundle(
        dir.path(),
        &[
            ("memory", [pattern(K, 1), zeros(K)].concat()),
            ("vmstate", pattern(10, 2)),
        ],
    );
    let built = chunk_bundle(&store, dir.path(), K).unwrap();
    built
        .config
        .check(&built.manifest.layers, &kiln_snapshot::Limits::default())
        .unwrap();
}

/// A bundle whose `names` are sparse files of `size` bytes (holes only: nothing to read).
fn sparse_bundle(dir: &std::path::Path, names: &[&str], size: u64) {
    bundle(dir, &[]);
    for name in names {
        fs::File::create(dir.join(name)).unwrap().set_len(size).unwrap();
    }
}

#[test]
fn a_bundle_over_the_total_size_is_refused_before_anything_is_read() {
    let dir = tempfile::tempdir().unwrap();
    let (_h, store) = store();
    sparse_bundle(dir.path(), &["a", "b", "c", "d", "e"], 1 << 40);
    let start = std::time::Instant::now();
    match chunk_bundle(&store, dir.path(), 16 << 20) {
        Err(SnapshotError::LimitExceeded { what, .. }) => assert!(what.contains("total"), "{what}"),
        other => panic!("expected a limit error, got {other:?}"),
    }
    assert!(start.elapsed().as_secs() < 5, "5 TiB of holes were read");
    assert!(store.list_blobs().unwrap().is_empty());
}

#[test]
fn too_many_chunks_for_a_config_are_refused_before_anything_is_read() {
    let dir = tempfile::tempdir().unwrap();
    let (_h, store) = store();
    // 64 GiB at 64 KiB is a million chunks: more `null`s than a 4 MiB config holds.
    sparse_bundle(dir.path(), &["memory"], 64 << 30);
    match chunk_bundle(&store, dir.path(), K) {
        Err(SnapshotError::LimitExceeded { what, .. }) => assert!(what.contains("chunk-size"), "{what}"),
        other => panic!("expected a limit error, got {other:?}"),
    }
    assert!(store.list_blobs().unwrap().is_empty());
}
