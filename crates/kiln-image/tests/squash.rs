//! The bottom squash (spec §6.4) as callers that budget layers rely on it (potter's composite
//! base): which layers it merges, how it is keyed, and that it never changes the layers above.

mod common;

use common::*;
use kiln_erofs::testtar::{Opts, TarBuilder};
use kiln_image::{ConvertOptions, Converted, convert_local};
use kiln_oci::testlayout::TestLayer;
use kiln_store::{Digest, Store};

/// Three "base" layers and three "step" layers. `tmp/y` in step 1 has no `tmp/` header, so its
/// layer inherits `tmp/`'s 1777 from base layer 0 (an implicit parent).
fn stack(base_note: &[u8], top_note: &[u8]) -> Vec<TestLayer> {
    let b0 = base(0o1777);
    let b1 = TarBuilder::new()
        .dir("usr", &Opts::default().mode(0o755))
        .file("usr/note", base_note, &Opts::default())
        .finish();
    let b2 = TarBuilder::new()
        .file("etc/os-release", b"ID=test\nVERSION=2\n", &Opts::default())
        .finish();
    let s0 = TarBuilder::new()
        .dir("app", &Opts::default().mode(0o755))
        .file("app/a", b"a", &Opts::default())
        .finish();
    let s1 = TarBuilder::new().file("tmp/y", b"y", &Opts::default()).finish();
    let s2 = TarBuilder::new().file("app/top", top_note, &Opts::default()).finish();
    [b0, b1, b2, s0, s1, s2].iter().map(|t| gz(t)).collect()
}

fn convert(store: &Store, layers: &[TestLayer], max_layers: usize) -> Converted {
    let src = tempfile::tempdir().unwrap();
    let path = layout(src.path(), &[arm()], layers);
    let opts = ConvertOptions {
        max_layers,
        ..Default::default()
    };
    convert_local(store, &path, &req(&[arm()], "t"), &opts)
        .unwrap()
        .images
        .remove(0)
}

fn erofs(c: &Converted) -> Vec<Digest> {
    c.layers.iter().map(|l| l.erofs.clone()).collect()
}

#[test]
fn squashes_exactly_the_bottom_len_minus_max_plus_one() {
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    let layers = stack(b"n", b"t");
    for max in 1..=8 {
        let c = convert(&store, &layers, max);
        let expect = if 6 > max { 6 - max + 1 } else { 0 };
        assert_eq!(c.squashed, expect, "max_layers {max}");
        assert_eq!(c.layers.len(), 6.min(max), "max_layers {max}");
        if expect > 0 {
            assert_eq!(c.layers[0].sources.len(), expect, "the merged layer lists its sources");
        }
    }
}

#[test]
fn the_layers_above_the_squash_are_byte_identical_to_an_unsquashed_convert() {
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    let layers = stack(b"n", b"t");
    let flat = convert(&store, &layers, 100);
    let squashed = convert(&store, &layers, 4);
    assert_eq!(squashed.squashed, 3);
    // Implicit parents are resolved against the unsquashed lowers before any squash, so the
    // inheriting layer (tmp/y) is the same blob either way.
    assert!(flat.layers[4].inherits);
    assert_eq!(erofs(&squashed)[1..], erofs(&flat)[3..]);
    assert_eq!(mode_of(&store, &squashed.layers[2].erofs, b"tmp"), 0o1777);
}

#[test]
fn the_squashed_layer_is_reused_when_only_layers_above_it_change() {
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    let a = convert(&store, &stack(b"n", b"t"), 4);
    let b = convert(&store, &stack(b"n", b"changed"), 4);
    assert_eq!(b.layers[0].erofs, a.layers[0].erofs, "same bottom layers, same squash");
    assert!(b.layers[0].cached, "a cache hit: not merged again");
    assert!(b.layers[1].cached && b.layers[2].cached);
    assert!(!b.layers[3].cached && b.layers[3].erofs != a.layers[3].erofs);
}

#[test]
fn a_change_inside_the_squash_rebuilds_it_and_nothing_above() {
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    let a = convert(&store, &stack(b"n", b"t"), 4);
    let c = convert(&store, &stack(b"other", b"t"), 4);
    assert_ne!(c.layers[0].erofs, a.layers[0].erofs);
    assert!(!c.layers[0].cached, "merged again");
    assert_eq!(erofs(&c)[1..], erofs(&a)[1..]);
    assert!(
        c.layers[1..].iter().all(|l| l.cached),
        "the layers above are cache hits"
    );
}

#[test]
fn the_squashed_layer_is_the_same_blob_in_every_store() {
    let layers = stack(b"n", b"t");
    let digests: Vec<Vec<Digest>> = (0..2)
        .map(|_| {
            let home = tempfile::tempdir().unwrap();
            let store = Store::open(home.path()).unwrap();
            erofs(&convert(&store, &layers, 4))
        })
        .collect();
    assert_eq!(digests[0], digests[1]);
}
