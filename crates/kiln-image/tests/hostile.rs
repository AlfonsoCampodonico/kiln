//! Spec §11.5: each hostile input is rejected and leaves no cache entry or ref,
//! both from a local layout and over the network from a registry.
mod common;

use std::net::TcpListener;

use common::*;
use kiln_erofs::testtar::{Opts, TarBuilder};
use kiln_image::{ConvertOptions, ImageError, convert_local, convert_registry};
use kiln_oci::testlayout::{LayoutBuilder, TestLayer};
use kiln_oci::{ContainerConfig, Descriptor, ImageConfig, ImageManifest, RootFs, canonical_json, media};
use kiln_registry::RegistryError;
use kiln_registry::testregistry::{Config, TestRegistry};
use kiln_store::{Digest, Store, StoreError};

/// The registry error behind a convert failure (from resolve or from a layer fetch).
fn registry_error(e: &ImageError) -> Option<&RegistryError> {
    match e {
        ImageError::Registry(r) | ImageError::Oci(kiln_oci::OciError::Registry(r)) => Some(r),
        _ => None,
    }
}

/// Converts from `reg` into a fresh store, expecting a failure with no cache entry and no ref.
fn rejects_remote(reg: &TestRegistry, opts: &ConvertOptions) -> ImageError {
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    let r = reference(&reg.host(), "hostile/app:app");
    let err = convert_registry(&store, &client(&reg.host()), &r, &registry_req(&[arm()], "h"), opts).unwrap_err();
    assert_eq!(cache_entries(&store), 0, "no cache entry after {err}");
    assert_eq!(store.get_ref("h").unwrap(), None);
    err
}

/// The fixture is rejected from a local layout and, with the same error, from a registry.
fn rejects(layers: &[TestLayer], opts: ConvertOptions) -> ImageError {
    let src = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    let path = layout(src.path(), &[arm()], layers);
    let err = convert_local(&store, &path, &req(&[arm()], "h"), &opts).unwrap_err();
    assert_eq!(cache_entries(&store), 0, "no cache entry after {err}");
    assert_eq!(store.get_ref("h").unwrap(), None);
    let remote = rejects_remote(&serve(&path), &opts);
    assert_eq!(
        remote.to_string(),
        err.to_string(),
        "the network path rejects it the same way"
    );
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
fn zstd_decompression_bomb() {
    let tar = TarBuilder::new()
        .file("zeros", &vec![0u8; 8 << 20], &Opts::default())
        .finish();
    let err = rejects(&[zst(&tar)], ConvertOptions::default());
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
fn uncompressed_layer_over_the_per_layer_limit() {
    let tar = TarBuilder::new()
        .file("data", &vec![7u8; 64 << 10], &Opts::default())
        .finish();
    let mut opts = ConvertOptions::default();
    opts.limits.max_layer_bytes = 16 << 10;
    let err = rejects(&[TestLayer::tar(tar)], opts);
    assert!(
        matches!(
            err,
            ImageError::LimitExceeded {
                what: "uncompressed bytes per layer",
                ..
            }
        ),
        "{err}"
    );
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

#[test]
fn warm_store_still_rejects_a_wrong_diff_id() {
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    let (s1, s2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let good = gz(&base(0o755));
    let mut wrong = good.clone();
    wrong.diff_id = Digest::of(b"something else");
    let p1 = layout(s1.path(), &[arm()], &[good]);
    convert_local(&store, &p1, &req(&[arm()], "good"), &ConvertOptions::default()).unwrap();
    // Same layer blob, now cached, but the second config lists a different diff_id.
    let p2 = layout(s2.path(), &[arm()], &[wrong]);
    let err = convert_local(&store, &p2, &req(&[arm()], "bad"), &ConvertOptions::default()).unwrap_err();
    assert!(matches!(err, ImageError::DiffIdMismatch { layer: 0, .. }), "{err}");
    assert_eq!(store.get_ref("bad").unwrap(), None);
}

#[test]
fn wrong_content_for_a_digest_over_the_network() {
    let src = tempfile::tempdir().unwrap();
    let layer = gz(&base(0o755));
    let digest = Digest::of(&layer.blob);
    let path = layout(src.path(), &[arm()], &[layer]);
    let reg = TestRegistry::serve_layout(
        &path,
        Config {
            corrupt: Some(digest),
            ..Default::default()
        },
    );
    let err = rejects_remote(&reg, &ConvertOptions::default());
    assert!(
        matches!(
            err,
            ImageError::Registry(RegistryError::Store(StoreError::DigestMismatch { .. }))
        ),
        "{err}"
    );
}

/// A layout whose only layer descriptor carries `urls`.
fn layout_with_urls(dir: &std::path::Path, tar: &[u8], urls: Vec<String>) -> (std::path::PathBuf, Digest) {
    let mut b = LayoutBuilder::new(dir);
    let cfg = ImageConfig {
        architecture: "arm64".into(),
        os: "linux".into(),
        variant: None,
        config: Some(ContainerConfig::default()),
        rootfs: RootFs {
            fs_type: "layers".into(),
            diff_ids: vec![Digest::of(tar)],
        },
    };
    let cfg_bytes = canonical_json(&cfg);
    let mut layer = Descriptor::new(media::OCI_LAYER_TAR, b.blob(tar), tar.len() as u64);
    layer.urls = Some(urls);
    let manifest = ImageManifest {
        schema_version: 2,
        media_type: Some(media::OCI_MANIFEST.into()),
        artifact_type: None,
        config: Descriptor::new(media::OCI_CONFIG, b.blob(&cfg_bytes), cfg_bytes.len() as u64),
        layers: vec![layer.clone()],
        annotations: None,
    };
    let bytes = canonical_json(&manifest);
    let d = Descriptor::new(media::OCI_MANIFEST, b.blob(&bytes), bytes.len() as u64);
    (b.add(d, Some("app")).finish(), layer.digest)
}

#[test]
fn descriptor_urls_are_never_fetched() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let target = format!("http://{}/layer.tar", listener.local_addr().unwrap());
    let src = tempfile::tempdir().unwrap();
    let (path, layer) = layout_with_urls(src.path(), &base(0o755), vec![target]);
    // The registry does not have the layer (as for a foreign layer): the convert fails.
    let reg = serve(&path);
    reg.remove_blob(&layer);
    let err = rejects_remote(&reg, &ConvertOptions::default());
    assert!(
        matches!(err, ImageError::Registry(RegistryError::NotFound { what: "blob", .. })),
        "{err}"
    );
    // When the registry has it, the convert uses the registry's copy.
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    let reg = serve(&path);
    let r = reference(&reg.host(), "x:app");
    convert_registry(
        &store,
        &client(&reg.host()),
        &r,
        &registry_req(&[arm()], "ok"),
        &ConvertOptions::default(),
    )
    .unwrap();
    assert!(listener.accept().is_err(), "nothing connected to the urls target");
}

#[test]
fn blob_redirects_to_link_local_or_private_addresses() {
    for target in [
        "https://169.254.169.254/latest/meta-data/",
        "http://169.254.169.254/latest/meta-data/",
        "https://10.0.0.1/",
        "https://172.16.0.1/",
        "https://192.168.0.1/",
    ] {
        let src = tempfile::tempdir().unwrap();
        let path = layout(src.path(), &[arm()], &[gz(&base(0o755))]);
        let reg = TestRegistry::serve_layout(
            &path,
            Config {
                redirect_blobs: Some(target.into()),
                ..Default::default()
            },
        );
        let err = rejects_remote(&reg, &ConvertOptions::default());
        assert!(
            matches!(registry_error(&err), Some(RegistryError::Refused { .. })),
            "{target}: {err}"
        );
    }
}
