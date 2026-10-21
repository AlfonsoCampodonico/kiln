mod common;

use common::{convert_stack, p, squash_all, walk};
use kiln_erofs::testtar::{Opts, TarBuilder};
use std::io::Cursor;

fn t(b: &mut TarBuilder) -> Vec<u8> {
    b.finish()
}

#[test]
fn implicit_dir_inherits_from_lower() {
    let l0 = t(TarBuilder::new().dir("tmp", &Opts::default().mode(0o1777).mtime(1000).xattr("user.k", b"v")));
    let l1 = t(TarBuilder::new().file("tmp/cache/x", b"", &Opts::default().mtime(2000)));
    let layers = convert_stack(&[l0, l1]);
    let w = walk(&layers[1]);
    assert_eq!(w[&p("tmp")].mode, 0o1777);
    assert_eq!(w[&p("tmp")].mtime, (1000, 0));
    assert_eq!(w[&p("tmp")].xattrs[&p("user.k")], b"v");
    assert_eq!((w[&p("tmp/cache")].mode, w[&p("tmp/cache")].mtime), (0o755, (2000, 0)));
}

#[test]
fn inheritance_respects_whiteouts_and_opaque() {
    let l0 = t(TarBuilder::new()
        .dir("a", &Opts::default().mode(0o700))
        .dir("o", &Opts::default())
        .dir("o/p", &Opts::default().mode(0o711)));
    let l1 = t(TarBuilder::new().whiteout("a").dir("o", &Opts::default()).opaque("o"));
    let l2 = t(TarBuilder::new()
        .file("a/f", b"", &Opts::default())
        .file("o/p/f", b"", &Opts::default()));
    let layers = convert_stack(&[l0, l1, l2]);
    let w = walk(&layers[2]);
    assert_eq!(w[&p("a")].mode, 0o755, "a is whited out below");
    assert_eq!(w[&p("o/p")].mode, 0o755, "o/p is hidden by the opaque o");
}

#[test]
fn inheritance_through_lower_non_dir_uses_defaults() {
    let l0 = t(TarBuilder::new().file("x", b"", &Opts::default().mode(0o600)));
    let l1 = t(TarBuilder::new().file("x/y", b"", &Opts::default()));
    let layers = convert_stack(&[l0, l1]);
    assert_eq!(walk(&layers[1])[&p("x")].mode, 0o755);
}

#[test]
fn inherited_opaque_is_not_copied() {
    let l0 = t(TarBuilder::new().dir("d", &Opts::default().mode(0o750)).opaque("d"));
    let l1 = t(TarBuilder::new().file("d/f", b"", &Opts::default()));
    let layers = convert_stack(&[l0, l1]);
    let w = walk(&layers[1]);
    assert_eq!(w[&p("d")].mode, 0o750);
    assert!(!w[&p("d")].xattrs.contains_key(&p("trusted.overlay.opaque")));
}

#[test]
fn squash_applies_whiteouts_and_drops_markers() {
    let l0 = t(TarBuilder::new()
        .dir("etc", &Opts::default())
        .file("etc/a", b"a", &Opts::default())
        .file("etc/b", b"b", &Opts::default())
        .dir("var", &Opts::default())
        .file("var/x", b"x", &Opts::default()));
    let l1 = t(TarBuilder::new()
        .whiteout("etc/a")
        .dir("var", &Opts::default())
        .opaque("var")
        .file("var/y", b"y", &Opts::default()));
    let w = walk(&squash_all(&convert_stack(&[l0, l1])));
    assert!(w.contains_key(&p("etc/b")));
    assert!(!w.contains_key(&p("etc/a")));
    assert!(!w.contains_key(&p("var/x")));
    assert_eq!(w[&p("var/y")].data, b"y");
    assert!(
        w.values().all(|s| !(s.kind == 'c' && s.rdev == (0, 0))),
        "no whiteouts survive"
    );
    assert!(w.values().all(|s| !s.xattrs.contains_key(&p("trusted.overlay.opaque"))));
}

#[test]
fn squash_preserves_hardlinks_and_splits_replaced_ones() {
    let l0 = t(TarBuilder::new()
        .file("a", b"old", &Opts::default())
        .hardlink("b", "a")
        .file("c", b"c", &Opts::default())
        .hardlink("d", "c"));
    let l1 = t(TarBuilder::new().file("a", b"new", &Opts::default()));
    let w = walk(&squash_all(&convert_stack(&[l0, l1])));
    assert_eq!((w[&p("a")].data.as_slice(), w[&p("a")].nlink), (&b"new"[..], 1));
    assert_eq!((w[&p("b")].data.as_slice(), w[&p("b")].nlink), (&b"old"[..], 1));
    assert_eq!(w[&p("c")].nid, w[&p("d")].nid);
    assert_eq!(w[&p("c")].nlink, 2);
}

#[test]
fn squash_uses_top_directory_attrs() {
    let l0 = t(TarBuilder::new().dir("d", &Opts::default().mode(0o700).xattr("user.k", b"v")));
    let l1 = t(TarBuilder::new().dir("d", &Opts::default().mode(0o755)));
    let w = walk(&squash_all(&convert_stack(&[l0, l1])));
    assert_eq!(w[&p("d")].mode, 0o755);
    assert!(w[&p("d")].xattrs.is_empty());
}

#[test]
fn squash_copies_file_data_and_is_deterministic() {
    let big: Vec<u8> = (0..20_000).map(|i| (i % 253) as u8).collect();
    let l0 = t(TarBuilder::new()
        .file("big", &big, &Opts::default())
        .symlink("s", "big", &Opts::default()));
    let l1 = t(TarBuilder::new().file("small", b"s", &Opts::default()));
    let layers = convert_stack(&[l0, l1]);
    let a = squash_all(&layers);
    let w = walk(&a);
    assert_eq!(w[&p("big")].data, big);
    assert_eq!(w[&p("s")].data, b"big");
    assert_eq!(a, squash_all(&layers));
}

#[test]
fn squash_of_empty_stack_is_root_only() {
    let dir = tempfile::tempdir().unwrap();
    let mut imgs: Vec<kiln_erofs::Image<Cursor<&[u8]>>> = Vec::new();
    let (out, _) = kiln_erofs::squash(&mut imgs, Cursor::new(Vec::new()), dir.path()).unwrap();
    let img = out.into_inner();
    let w = walk(&img);
    assert_eq!(w.len(), 1);
    assert!(w.contains_key(&p("")));
    assert_eq!(w[&p("")].kind, 'd');
}

#[test]
fn squash_base_time_is_min_reachable_mtime() {
    let l0 = t(TarBuilder::new().file("old", b"", &Opts::default().mtime(100)).file(
        "keep",
        b"",
        &Opts::default().mtime(500),
    ));
    let l1 = t(TarBuilder::new().whiteout("old"));
    let layers = convert_stack(&[l0, l1]);
    let img = squash_all(&layers);
    let w = walk(&img);
    let sb = kiln_erofs::ondisk::SuperBlock::decode(&img[1024..]).unwrap();
    assert_eq!(sb.epoch, 500, "base time should be min of reachable mtimes");
    assert_eq!(sb.fixed_nsec, 0);
    assert!(w[&p("keep")].compact);
    assert!(!w.contains_key(&p("old")), "old should be whited out");
}

/// overlayfs ignores an opaque marker on a layer's root (verified against the kernel).
#[test]
fn root_opaque_marker_hides_nothing() {
    let l0 = t(TarBuilder::new().file("x", b"x", &Opts::default()));
    let l1 = t(TarBuilder::new().opaque("").file("y", b"y", &Opts::default()));
    let layers = convert_stack(&[l0, l1]);
    assert_eq!(
        walk(&layers[1])[&p("")].xattrs[&p("trusted.overlay.opaque")],
        b"y",
        "the layer still records the marker"
    );
    let w = walk(&squash_all(&layers));
    assert_eq!(w[&p("x")].data, b"x");
    assert_eq!(w[&p("y")].data, b"y");
}

/// Only `.wh..wh..opq` makes a directory opaque; a tar cannot inject the xattr itself.
#[test]
fn tar_supplied_overlay_xattrs_are_dropped() {
    let l0 = t(TarBuilder::new().file("d/x", b"x", &Opts::default()));
    let l1 = t(TarBuilder::new()
        .dir("d", &Opts::default().xattr("trusted.overlay.opaque", b"y"))
        .file("d/y", b"y", &Opts::default()));
    let layers = convert_stack(&[l0, l1]);
    assert!(walk(&layers[1])[&p("d")].xattrs.is_empty());
    let w = walk(&squash_all(&layers));
    assert!(w.contains_key(&p("d/x")) && w.contains_key(&p("d/y")));
}

fn squash_upper_all(layers: &[Vec<u8>]) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let mut imgs: Vec<_> = layers
        .iter()
        .map(|l| kiln_erofs::Image::open(Cursor::new(l.as_slice())).unwrap())
        .collect();
    let (out, _) = kiln_erofs::squash_upper(&mut imgs, Cursor::new(Vec::new()), dir.path()).unwrap();
    out.into_inner()
}

const OPQ: &str = "trusted.overlay.opaque";

/// Merging upper layers must keep what hides the layers below the merge.
#[test]
fn upper_squash_keeps_whiteouts_and_opaque_markers() {
    let l0 = t(TarBuilder::new()
        .file("etc/a", b"a", &Opts::default())
        .dir("var", &Opts::default())
        .opaque("var")
        .file("var/y", b"y", &Opts::default()));
    let l1 = t(TarBuilder::new()
        .whiteout("etc/a")
        .whiteout("etc/base-only")
        .dir("var", &Opts::default().mode(0o750))
        .file("var/z", b"z", &Opts::default()));
    let w = walk(&squash_upper_all(&convert_stack(&[l0, l1])));
    for path in ["etc/a", "etc/base-only"] {
        assert_eq!(
            (w[&p(path)].kind, w[&p(path)].rdev),
            ('c', (0, 0)),
            "{path} stays a whiteout"
        );
    }
    assert_eq!(w[&p("var")].mode, 0o750);
    assert_eq!(w[&p("var")].xattrs[&p(OPQ)], b"y", "opaque survives a later plain dir");
    assert!(w.contains_key(&p("var/y")) && w.contains_key(&p("var/z")));
}

#[test]
fn upper_squash_makes_a_dir_over_a_non_dir_opaque() {
    let l0 = t(TarBuilder::new().file("x", b"", &Opts::default()).whiteout("w"));
    let l1 = t(TarBuilder::new()
        .dir("x", &Opts::default())
        .file("x/f", b"", &Opts::default())
        .dir("w", &Opts::default())
        .dir("n", &Opts::default()));
    let w = walk(&squash_upper_all(&convert_stack(&[l0, l1])));
    assert_eq!(w[&p("x")].xattrs[&p(OPQ)], b"y", "the base's x must not show through");
    assert_eq!(w[&p("w")].xattrs[&p(OPQ)], b"y", "nor what the whiteout hid");
    assert!(
        !w[&p("n")].xattrs.contains_key(&p(OPQ)),
        "a new dir still merges with the base"
    );
}

#[test]
fn upper_squash_opaque_clears_lower_children_and_keeps_hardlinks() {
    let l0 = t(TarBuilder::new()
        .file("d/old", b"o", &Opts::default())
        .file("a", b"a", &Opts::default())
        .hardlink("b", "a"));
    let l1 = t(TarBuilder::new()
        .dir("d", &Opts::default())
        .opaque("d")
        .file("d/new", b"n", &Opts::default()));
    let w = walk(&squash_upper_all(&convert_stack(&[l0, l1])));
    assert!(!w.contains_key(&p("d/old")));
    assert_eq!(w[&p("d")].xattrs[&p(OPQ)], b"y");
    assert_eq!(w[&p("a")].nid, w[&p("b")].nid);
}

#[test]
fn merged_view_lists_and_hides_like_overlayfs() {
    let l0 = t(TarBuilder::new()
        .file("etc/a", b"a", &Opts::default())
        .file("etc/b", b"b", &Opts::default())
        .file("o/x", b"x", &Opts::default()));
    let l1 = t(TarBuilder::new()
        .whiteout("etc/a")
        .file("etc/c", b"c", &Opts::default())
        .dir("o", &Opts::default())
        .opaque("o")
        .file("o/y", b"y", &Opts::default()));
    let layers = convert_stack(&[l0, l1]);
    let imgs = layers
        .iter()
        .map(|l| kiln_erofs::Image::open(Cursor::new(l.as_slice())).unwrap())
        .collect();
    let mut m = kiln_erofs::Merged::new(imgs);
    assert_eq!(m.read_dir(b"etc").unwrap(), vec![p("b"), p("c")]);
    assert!(m.lookup(b"etc/a").unwrap().is_none());
    assert_eq!(m.lookup(b"etc/b").unwrap().map(|(l, _)| l), Some(0));
    assert_eq!(m.subtree(b"o").unwrap(), vec![p("o/y")]);
    assert_eq!(
        m.subtree(b"").unwrap(),
        [p("etc"), p("etc/b"), p("etc/c"), p("o"), p("o/y")]
    );
    let (layer, nid) = m.lookup(b"etc/c").unwrap().unwrap();
    assert_eq!(m.image(layer).read_data(nid).unwrap(), b"c");
}

#[test]
fn walk_lists_every_path_with_its_nid() {
    let l = t(TarBuilder::new().file("a/b", b"", &Opts::default()).whiteout("c"));
    let layers = convert_stack(&[l]);
    let mut img = kiln_erofs::Image::open(Cursor::new(layers[0].as_slice())).unwrap();
    let paths: Vec<Vec<u8>> = img.walk().unwrap().into_iter().map(|(p, _)| p).collect();
    assert_eq!(paths, [p("a"), p("c"), p("a/b")]);
}

/// Squashing the app half separately must not change what the full stack shows.
#[test]
fn base_plus_upper_squash_equals_full_squash() {
    let base = t(TarBuilder::new()
        .file("etc/a", b"a", &Opts::default())
        .file("etc/keep", b"k", &Opts::default())
        .file("x", b"x", &Opts::default())
        .file("o/old", b"o", &Opts::default())
        .file("w/deep", b"d", &Opts::default())
        .dir("m", &Opts::default().mode(0o700)));
    let u1 = t(TarBuilder::new()
        .whiteout("etc/a")
        .whiteout("w")
        .dir("o", &Opts::default())
        .opaque("o")
        .file("o/new", b"n", &Opts::default())
        .file("m/f", b"f", &Opts::default()));
    let u2 = t(TarBuilder::new()
        .dir("x", &Opts::default())
        .file("x/in", b"i", &Opts::default())
        .dir("w", &Opts::default())
        .file("w/fresh", b"f", &Opts::default())
        .file("etc/b", b"b", &Opts::default()));
    let layers = convert_stack(&[base.clone(), u1, u2]);
    let full = walk(&squash_all(&layers));
    let upper = squash_upper_all(&layers[1..]);
    let split = walk(&squash_all(&[layers[0].clone(), upper]));
    assert_eq!(common::view(&full, true), common::view(&split, true));
    assert!(!split.contains_key(&p("w/deep")) && !split.contains_key(&p("o/old")));
}
