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

#[test]
fn a_descriptor_size_is_checked_even_when_the_blob_is_already_stored() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path().join("store")).unwrap();
    let layer = TestLayer::tar(layer_tar("a"));
    let mut b = LayoutBuilder::new(&tmp.path().join("one"));
    let m = b.image(&arm(), std::slice::from_ref(&layer), cfg());
    let dir = b.add(m, None).finish();
    let img = resolve_local(&store, &LocalSource::Layout(dir), None, &[arm()])
        .unwrap()
        .remove(0);

    // A second layout whose manifest names the same layer digest with an inflated size.
    let mut manifest = img.manifest.clone();
    manifest.layers[0].size += 1;
    let bytes = kiln_oci::canonical_json(&manifest);
    let mut b2 = LayoutBuilder::new(&tmp.path().join("two"));
    b2.blob(&store.read_metadata(&img.config_digest).unwrap());
    b2.blob(&layer.blob);
    let d = kiln_oci::Descriptor::new(media::OCI_MANIFEST, b2.blob(&bytes), bytes.len() as u64);
    let dir2 = b2.add(d, None).finish();
    let err = resolve_local(&store, &LocalSource::Layout(dir2), None, &[arm()]).unwrap_err();
    assert!(
        matches!(err, OciError::Store(kiln_store::StoreError::SizeMismatch { .. })),
        "{err}"
    );
}

#[test]
fn oversized_metadata_is_a_typed_error() {
    let tmp = tempfile::tempdir().unwrap();
    let mut b = LayoutBuilder::new(&tmp.path().join("layout"));
    let m = b.image(&arm(), &[TestLayer::tar(layer_tar("a"))], cfg());
    let dir = b.add(m.clone(), None).finish();
    let store = Store::open(tmp.path().join("store")).unwrap();

    // A manifest descriptor claiming more than the metadata limit.
    let mut huge = m.clone();
    huge.size = kiln_store::MAX_METADATA_BLOB + 1;
    let mut b2 = LayoutBuilder::new(&tmp.path().join("layout2"));
    let dir2 = b2.add(huge, None).finish();
    assert!(matches!(
        resolve_local(&store, &LocalSource::Layout(dir2), None, &[arm()]),
        Err(OciError::Store(kiln_store::StoreError::TooLarge { .. }))
    ));

    // An index.json larger than the limit.
    fs::write(
        dir.join("index.json"),
        vec![b' '; kiln_store::MAX_METADATA_BLOB as usize + 1],
    )
    .unwrap();
    assert!(matches!(
        resolve_local(&store, &LocalSource::Layout(dir), None, &[arm()]),
        Err(OciError::MetadataTooLarge { .. })
    ));
}
