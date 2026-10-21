//! Loading a stored snapshot, assembling it and writing its lazy index.
mod common;

use std::collections::BTreeMap;
use std::fs;

use common::*;
use kiln_oci::{Descriptor, ImageManifest, canonical_json};
use kiln_snapshot::{
    LazyIndex, SNAPSHOT_ARTIFACT, SNAPSHOT_CONFIG, SnapshotError, assemble, chunk_bundle, is_snapshot, load, resolve,
    tag, write_lazy,
};
use kiln_store::{Digest, Store};

fn files() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("vmstate", pattern(1234, 1)),
        ("memory", [pattern(K, 2), zeros(3 * K), pattern(K / 2, 3)].concat()),
        ("disk-0", [zeros(K), pattern(K, 2)].concat()),
        ("empty", vec![]),
    ]
}

fn built(store: &Store) -> Digest {
    let dir = tempfile::tempdir().unwrap();
    bundle(dir.path(), &files());
    chunk_bundle(store, dir.path(), K).unwrap().manifest_digest
}

#[test]
fn assemble_round_trips_a_bundle_byte_for_byte() {
    let (_h, store) = store();
    let loaded = load(&store, &built(&store)).unwrap();
    let out = tempfile::tempdir().unwrap();
    let dest = out.path().join("bundle");
    assemble(&store, &loaded, &dest).unwrap();
    for (name, bytes) in files() {
        assert_eq!(fs::read(dest.join(name)).unwrap(), bytes, "{name}");
    }
    let written = fs::read(dest.join("bundle.json")).unwrap();
    let v: serde_json::Value = serde_json::from_slice(&written).unwrap();
    assert_eq!(v, bundle_json());
    let text = String::from_utf8(written).unwrap();
    assert!(
        text.contains("\n  \"arch\": \"aarch64\",\n  \"disks\""),
        "pretty and sorted: {text}"
    );
    assert_eq!(fs::read_dir(&dest).unwrap().count(), files().len() + 1);
}

/// APFS allocates small files densely whatever their holes, so the file is
/// large enough (128 MiB) for every filesystem kiln runs on to keep them.
#[cfg(unix)]
#[test]
fn assemble_leaves_zero_chunks_as_holes() {
    use std::io::{Seek, SeekFrom, Write};
    use std::os::unix::fs::MetadataExt;
    let (_h, store) = store();
    let src = tempfile::tempdir().unwrap();
    let size: u64 = 128 << 20;
    bundle(src.path(), &[]);
    let mut f = fs::File::create(src.path().join("memory")).unwrap();
    f.set_len(size).unwrap();
    f.seek(SeekFrom::Start(size - 10)).unwrap();
    f.write_all(&pattern(10, 1)).unwrap();
    drop(f);
    let built = chunk_bundle(&store, src.path(), 16 << 20).unwrap();
    assert_eq!(built.manifest.layers.len(), 1);
    let loaded = load(&store, &built.manifest_digest).unwrap();
    let out = tempfile::tempdir().unwrap();
    assemble(&store, &loaded, out.path()).unwrap();
    let m = fs::metadata(out.path().join("memory")).unwrap();
    assert_eq!(m.len(), size);
    assert!(m.blocks() * 512 <= 32 << 20, "{} blocks for {size} bytes", m.blocks());
    let mut tail = vec![0u8; 1 << 20];
    let mut f = fs::File::open(out.path().join("memory")).unwrap();
    f.seek(SeekFrom::Start(size - tail.len() as u64)).unwrap();
    std::io::Read::read_exact(&mut f, &mut tail).unwrap();
    assert!(tail[..tail.len() - 10].iter().all(|&b| b == 0));
    assert_eq!(&tail[tail.len() - 10..], &pattern(10, 1)[..]);
}

#[test]
fn assemble_refuses_a_non_empty_dest_and_a_missing_chunk() {
    let (_h, store) = store();
    let loaded = load(&store, &built(&store)).unwrap();
    let out = tempfile::tempdir().unwrap();
    fs::write(out.path().join("x"), b"x").unwrap();
    assert!(matches!(
        assemble(&store, &loaded, out.path()),
        Err(SnapshotError::DestNotEmpty(_))
    ));
    assert!(matches!(
        write_lazy(&loaded, out.path()),
        Err(SnapshotError::DestNotEmpty(_))
    ));
    // A lazily pulled snapshot has no chunks yet: assemble says so.
    let chunk = &loaded.manifest.layers[0].digest;
    fs::remove_file(store.blob_path(chunk)).unwrap();
    let empty = tempfile::tempdir().unwrap();
    assert!(assemble(&store, &loaded, empty.path()).is_err());
    assert!(
        !empty.path().join("bundle.json").exists(),
        "bundle.json is written last, so a partial bundle never looks whole"
    );
}

#[test]
fn lazy_json_lists_every_file() {
    let (_h, store) = store();
    let loaded = load(&store, &built(&store)).unwrap();
    let out = tempfile::tempdir().unwrap();
    write_lazy(&loaded, out.path()).unwrap();
    let mut names: Vec<_> = fs::read_dir(out.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["bundle.json", "lazy.json"]);
    let bytes = fs::read(out.path().join("lazy.json")).unwrap();
    let index: LazyIndex = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(bytes, canonical_json(&index), "written canonically");
    assert_eq!(index.version, 1);
    assert_eq!(index.chunk_size, K);
    let want: BTreeMap<_, _> = loaded
        .config
        .files
        .iter()
        .map(|f| (f.name.clone(), (f.size, f.chunks.clone())))
        .collect();
    let got: BTreeMap<_, _> = index
        .files
        .iter()
        .map(|(n, f)| (n.clone(), (f.size, f.chunks.clone())))
        .collect();
    assert_eq!(got, want);
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(v["files"]["memory"]["chunks"][1].is_null());
    assert_eq!(v["files"]["memory"]["size"], 4 * K + K / 2);
    let b: serde_json::Value = serde_json::from_slice(&fs::read(out.path().join("bundle.json")).unwrap()).unwrap();
    assert_eq!(b, bundle_json());
}

#[test]
fn load_refuses_an_image_manifest() {
    let (_h, store) = store();
    let config = store.put_bytes(b"{}").unwrap();
    let manifest = ImageManifest {
        schema_version: 2,
        media_type: Some(kiln_registry::OCI_MANIFEST.into()),
        artifact_type: Some("application/vnd.kiln.image.v1".into()),
        config: Descriptor::new("application/vnd.kiln.image.config.v1+json", config, 2),
        layers: vec![],
        annotations: None,
    };
    let d = store.put_bytes(&canonical_json(&manifest)).unwrap();
    assert!(matches!(load(&store, &d), Err(SnapshotError::NotASnapshot(_))));
    assert!(!is_snapshot(&store, &d));
    assert!(matches!(tag(&store, "x", &d), Err(SnapshotError::NotASnapshot(_))));
    assert!(store.refs().unwrap().is_empty());
    // A snapshot artifactType with another config type is no snapshot either.
    let mut m = manifest.clone();
    m.artifact_type = Some(SNAPSHOT_ARTIFACT.into());
    let d = store.put_bytes(&canonical_json(&m)).unwrap();
    assert!(matches!(load(&store, &d), Err(SnapshotError::NotASnapshot(_))));
}

#[test]
fn load_checks_the_stored_config() {
    let (_h, store) = store();
    let good = load(&store, &built(&store)).unwrap();
    let mut config = good.config.clone();
    config.files[0].name = "../escape".into();
    let bytes = canonical_json(&config);
    let mut m = good.manifest.clone();
    m.config = Descriptor::new(SNAPSHOT_CONFIG, store.put_bytes(&bytes).unwrap(), bytes.len() as u64);
    let d = store.put_bytes(&canonical_json(&m)).unwrap();
    assert!(matches!(load(&store, &d), Err(SnapshotError::BadName(_))));
    // A config descriptor whose size disagrees with the blob.
    let mut m = good.manifest.clone();
    m.config.size += 1;
    let d = store.put_bytes(&canonical_json(&m)).unwrap();
    assert!(load(&store, &d).is_err());
}

#[test]
fn tag_and_resolve() {
    let (_h, store) = store();
    let d = built(&store);
    assert!(is_snapshot(&store, &d));
    tag(&store, "127.0.0.1:5000/team/vm:1", &d).unwrap();
    assert_eq!(resolve(&store, "127.0.0.1:5000/team/vm:1").unwrap(), d);
    assert_eq!(resolve(&store, &d.to_string()).unwrap(), d);
    assert!(matches!(resolve(&store, "nope"), Err(SnapshotError::RefNotFound(_))));
    assert!(tag(&store, "bad name", &d).is_err());
    let loaded = load(&store, &d).unwrap();
    assert_eq!(loaded.digest, d);
    assert_eq!(
        loaded.logical_size(),
        files().iter().map(|(_, b)| b.len() as u64).sum::<u64>()
    );
    // vmstate (1), memory (2 distinct: the K-chunk and the short one; disk-0 shares one).
    assert_eq!(loaded.manifest.layers.len(), 3);
    assert_eq!(loaded.stored_size(), 1234 + K + K / 2);
    // GC keeps a tagged snapshot's chunks.
    let blobs = store.list_blobs().unwrap().len();
    store.gc().unwrap();
    assert_eq!(store.list_blobs().unwrap().len(), blobs);
}
