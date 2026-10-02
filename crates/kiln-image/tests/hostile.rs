//! Spec §11.5: each hostile input is rejected and leaves no cache entry or ref.
mod common;

use common::*;
use kiln_erofs::testtar::{Opts, TarBuilder};
use kiln_image::{ConvertOptions, ImageError, convert_local};
use kiln_oci::testlayout::TestLayer;
use kiln_store::{Digest, Store};

fn rejects(layers: &[TestLayer], opts: ConvertOptions) -> ImageError {
    let src = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    let path = layout(src.path(), &[arm()], layers);
    let err = convert_local(&store, &path, &req(&[arm()], "h"), &opts).unwrap_err();
    assert_eq!(cache_entries(&store), 0, "no cache entry after {err}");
    assert_eq!(store.get_ref("h").unwrap(), None);
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
