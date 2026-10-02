mod common;

use common::*;
use kiln_erofs::Image;
use kiln_erofs::testtar::{Opts, TarBuilder};
use kiln_image::types::{ANN_INHERITS, ANN_SOURCE_DIGESTS, KILN_ARTIFACT};
use kiln_image::{ConvertOptions, ImageError, convert_local, load, resolve_name};
use kiln_oci::testlayout::TestLayer;
use kiln_oci::{Platform, media};
use kiln_store::{Digest, Store};

#[test]
fn converts_a_layout_with_inherited_parents() {
    let src = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    let path = layout(src.path(), &[arm()], &[gz(&base(0o1777)), zst(&top())]);

    let out = convert_local(&store, &path, &req(&[arm()], "app:1"), &ConvertOptions::default()).unwrap();
    assert_eq!(out.media_type, media::OCI_MANIFEST);
    assert_eq!(store.get_ref("app:1").unwrap(), Some(out.digest.clone()));
    let c = &out.images[0];
    assert_eq!(c.layers.len(), 2);
    assert!(c.layers.iter().all(|l| !l.cached));
    assert!(!c.layers[0].inherits && c.layers[1].inherits);
    // overlayfs shows the upper dir's attributes, so the top layer must carry the base's 1777.
    assert_eq!(mode_of(&store, &c.layers[1].erofs, b"tmp"), 0o1777);

    let loaded = load(&store, &out.digest).unwrap();
    let (platform, m) = &loaded.entries[0];
    assert_eq!(platform.architecture, "arm64");
    assert_eq!(m.manifest.artifact_type.as_deref(), Some(KILN_ARTIFACT));
    assert_eq!(m.config.process.cmd, vec!["php", "-v"]);
    assert_eq!(m.config.process.working_dir.as_deref(), Some("/app"));
    assert_eq!(m.config.source.reference, None);
    assert_eq!(m.manifest.layers[1].annotation(ANN_INHERITS), Some("true"));
    assert_eq!(m.manifest.layers[0].annotation(ANN_INHERITS), None);
    let src_layers = &kiln_oci::resolve_local(&store, &kiln_oci::LocalSource::detect(&path).unwrap(), None, &[arm()])
        .unwrap()[0]
        .manifest
        .layers;
    assert_eq!(
        m.manifest.layers[1].annotation(ANN_SOURCE_DIGESTS),
        Some(src_layers[1].digest.to_string().as_str())
    );
}

#[test]
fn second_convert_is_fully_cached_and_identical() {
    let src = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    let path = layout(src.path(), &[arm()], &[gz(&base(0o1777)), gz(&top())]);
    let a = convert_local(&store, &path, &req(&[arm()], "a"), &ConvertOptions::default()).unwrap();
    let b = convert_local(&store, &path, &req(&[arm()], "b"), &ConvertOptions::default()).unwrap();
    assert_eq!(a.digest, b.digest);
    assert!(b.images[0].layers.iter().all(|l| l.cached));
}

#[test]
fn a_changed_base_reconverts_the_inheriting_layer() {
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    let (s1, s2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let p1 = layout(s1.path(), &[arm()], &[gz(&base(0o1777)), gz(&top())]);
    let p2 = layout(s2.path(), &[arm()], &[gz(&base(0o755)), gz(&top())]);
    let a = convert_local(&store, &p1, &req(&[arm()], "a"), &ConvertOptions::default()).unwrap();
    let b = convert_local(&store, &p2, &req(&[arm()], "b"), &ConvertOptions::default()).unwrap();
    let (la, lb) = (&a.images[0].layers, &b.images[0].layers);
    assert_eq!(la[1].sources, lb[1].sources, "same top layer blob");
    assert_ne!(la[1].erofs, lb[1].erofs, "different inherited context");
    assert!(!lb[1].cached);
    assert_eq!(mode_of(&store, &lb[1].erofs, b"tmp"), 0o755);
    // Both contexts stay cached.
    let again = convert_local(&store, &p1, &req(&[arm()], "a"), &ConvertOptions::default()).unwrap();
    assert!(again.images[0].layers.iter().all(|l| l.cached));
    assert_eq!(again.digest, a.digest);
}

#[test]
fn concurrent_converts_into_one_store_agree() {
    let src = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let path = layout(src.path(), &[arm()], &[gz(&base(0o1777)), gz(&top())]);
    let digests: Vec<Digest> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..4)
            .map(|i| {
                let (path, root) = (&path, home.path());
                s.spawn(move || {
                    let store = Store::open(root).unwrap();
                    let tag = format!("t{i}");
                    convert_local(&store, path, &req(&[arm()], &tag), &ConvertOptions::default())
                        .unwrap()
                        .digest
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert!(digests.windows(2).all(|w| w[0] == w[1]));
    let store = Store::open(home.path()).unwrap();
    assert_eq!(store.refs().unwrap().len(), 4, "no lost refs.json update");
}

#[test]
fn output_is_deterministic_across_stores_and_job_counts() {
    let src = tempfile::tempdir().unwrap();
    let layers: Vec<TestLayer> = (0..6)
        .map(|i| {
            gz(&TarBuilder::new()
                .file(&format!("d{i}/f"), &[i as u8; 3000], &Opts::default())
                .finish())
        })
        .collect();
    let path = layout(src.path(), &[arm()], &layers);
    let mut digests = Vec::new();
    for jobs in [1, 8] {
        let home = tempfile::tempdir().unwrap();
        let store = Store::open(home.path()).unwrap();
        let opts = ConvertOptions {
            jobs,
            ..Default::default()
        };
        digests.push(convert_local(&store, &path, &req(&[arm()], "x"), &opts).unwrap().digest);
    }
    assert_eq!(digests[0], digests[1]);
}

#[test]
fn squashes_bottom_layers_above_max_layers() {
    let src = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    let l0 = TarBuilder::new()
        .file("a", b"a", &Opts::default())
        .file("gone", b"g", &Opts::default())
        .finish();
    let l1 = TarBuilder::new().whiteout("gone").finish();
    let l2 = TarBuilder::new().file("b", b"b", &Opts::default()).finish();
    let l3 = TarBuilder::new().file("c", b"c", &Opts::default()).finish();
    let path = layout(src.path(), &[arm()], &[gz(&l0), gz(&l1), gz(&l2), gz(&l3)]);
    let opts = ConvertOptions {
        max_layers: 2,
        ..Default::default()
    };
    let out = convert_local(&store, &path, &req(&[arm()], "s"), &opts).unwrap();
    let c = &out.images[0];
    assert_eq!((c.layers.len(), c.squashed), (2, 3));
    assert_eq!(c.layers[0].sources.len(), 3);
    let mut img = Image::open(store.open_blob(&c.layers[0].erofs).unwrap()).unwrap();
    assert!(img.lookup(b"a").unwrap().is_some() && img.lookup(b"b").unwrap().is_some());
    assert!(img.lookup(b"gone").unwrap().is_none(), "whiteout applied");
    assert!(img.lookup(b".wh.gone").unwrap().is_none(), "and removed");
    let again = convert_local(&store, &path, &req(&[arm()], "s"), &opts).unwrap();
    assert!(again.images[0].layers[0].cached);
    assert_eq!(again.digest, out.digest);
}

#[test]
fn multiarch_produces_a_sorted_index() {
    let src = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    let amd = Platform::parse("linux/amd64").unwrap();
    let path = layout(src.path(), &[arm(), amd.clone()], &[gz(&base(0o1777))]);
    let out = convert_local(&store, &path, &req(&[arm(), amd], "m"), &ConvertOptions::default()).unwrap();
    assert_eq!(out.media_type, media::OCI_INDEX);
    let loaded = load(&store, &out.digest).unwrap();
    assert!(loaded.is_index);
    let archs: Vec<_> = loaded.entries.iter().map(|(p, _)| p.architecture.as_str()).collect();
    assert_eq!(archs, ["amd64", "arm64"]);
    assert_eq!(resolve_name(&store, "m").unwrap(), out.digest);
    assert_eq!(resolve_name(&store, &out.digest.to_string()).unwrap(), out.digest);
    assert!(matches!(resolve_name(&store, "nope"), Err(ImageError::RefNotFound(_))));
}

#[test]
fn rejects_unsupported_platforms() {
    let src = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    let rv = Platform::parse("linux/riscv64").unwrap();
    let path = layout(src.path(), std::slice::from_ref(&rv), &[gz(&base(0o755))]);
    let err = convert_local(&store, &path, &req(&[rv], "r"), &ConvertOptions::default()).unwrap_err();
    assert!(matches!(err, ImageError::UnsupportedPlatform(_)), "{err}");
}
