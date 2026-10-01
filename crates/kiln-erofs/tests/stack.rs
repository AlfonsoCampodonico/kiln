mod common;

use common::{convert_stack, p, squash_all, walk};
use kiln_erofs::testtar::{Opts, TarBuilder};

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
