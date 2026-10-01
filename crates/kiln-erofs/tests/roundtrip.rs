mod common;

use std::io::Cursor;

use common::{convert, p, walk};
use kiln_erofs::ondisk::{LAYOUT_FLAT_INLINE, LAYOUT_FLAT_PLAIN};
use kiln_erofs::testtar::{Opts, TarBuilder};
use kiln_erofs::{Error, Image};

fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 31 % 251) as u8).collect()
}

#[test]
fn every_kind_round_trips() {
    let tar = TarBuilder::new()
        .dir("etc", &Opts::default().mode(0o755))
        .file("etc/small", b"hello", &Opts::default())
        .file("etc/exact", &pattern(4096), &Opts::default())
        .file("etc/big", &pattern(10_000), &Opts::default())
        .symlink("etc/link", "../usr/bin/php", &Opts::default())
        .hardlink("etc/hard", "etc/small")
        .chardev("dev/null", 1, 3, &Opts::default().mode(0o666))
        .fifo("run/fifo", &Opts::default().mode(0o600))
        .finish();
    let w = walk(&convert(&tar).0);
    assert_eq!(w[&p("etc/small")].data, b"hello");
    assert_eq!(w[&p("etc/small")].layout, LAYOUT_FLAT_INLINE);
    assert_eq!(w[&p("etc/exact")].data, pattern(4096));
    assert_eq!(w[&p("etc/exact")].layout, LAYOUT_FLAT_PLAIN);
    assert_eq!(w[&p("etc/big")].data, pattern(10_000));
    assert_eq!(w[&p("etc/big")].layout, LAYOUT_FLAT_INLINE);
    assert_eq!(
        (w[&p("etc/link")].kind, w[&p("etc/link")].data.as_slice()),
        ('l', &b"../usr/bin/php"[..])
    );
    assert_eq!(w[&p("etc/hard")].nid, w[&p("etc/small")].nid);
    assert_eq!(w[&p("etc/small")].nlink, 2);
    assert_eq!(
        (w[&p("dev/null")].kind, w[&p("dev/null")].rdev, w[&p("dev/null")].mode),
        ('c', (1, 3), 0o666)
    );
    assert_eq!(w[&p("run/fifo")].kind, 'p');
    assert_eq!((w[&p("")].kind, w[&p("")].mode), ('d', 0o755));
    assert_eq!(w[&p("dev")].mode, 0o755);
    assert_eq!(w[&p("etc")].nlink, 2);
    assert_eq!(w[&p("")].nlink, 5, "root has etc, dev, run");
}

#[test]
fn large_tail_with_xattrs_falls_back_to_plain() {
    let tar = TarBuilder::new()
        .file("f", &pattern(4090), &Opts::default().xattr("user.k", &[9u8; 100]))
        .finish();
    let w = walk(&convert(&tar).0);
    assert_eq!(w[&p("f")].layout, LAYOUT_FLAT_PLAIN);
    assert_eq!(w[&p("f")].data, pattern(4090));
    assert_eq!(w[&p("f")].xattrs[&p("user.k")], vec![9u8; 100]);
}

#[test]
fn many_entries_span_directory_blocks() {
    let mut b = TarBuilder::new();
    for i in 0..600 {
        b.file(&format!("d/f{i:04}"), b"", &Opts::default());
    }
    let (img, _) = convert(&b.finish());
    let mut image = Image::open(Cursor::new(img.as_slice())).unwrap();
    let d = image.lookup(b"d").unwrap().unwrap();
    let names: Vec<Vec<u8>> = image.read_dir(d).unwrap().into_iter().map(|e| e.name).collect();
    assert_eq!(names.len(), 600);
    assert!(names.windows(2).all(|w| w[0] < w[1]));
    assert!(image.lookup(b"d/f0000").unwrap().is_some());
    assert!(image.lookup(b"d/f0599").unwrap().is_some());
    assert!(image.lookup(b"d/f0600").unwrap().is_none());
}

#[test]
fn symlink_targets_inline_and_plain() {
    let mid = "m".repeat(3000);
    let full = "x".repeat(4096);
    let tar = TarBuilder::new()
        .symlink("a", &mid, &Opts::default())
        .symlink("b", &full, &Opts::default())
        .finish();
    let w = walk(&convert(&tar).0);
    assert_eq!(
        (w[&p("a")].data.clone(), w[&p("a")].layout),
        (mid.into_bytes(), LAYOUT_FLAT_INLINE)
    );
    assert_eq!(
        (w[&p("b")].data.clone(), w[&p("b")].layout),
        (full.into_bytes(), LAYOUT_FLAT_PLAIN)
    );
}

#[test]
fn xattrs_shared_and_inline() {
    let tar = TarBuilder::new()
        .file(
            "a",
            b"",
            &Opts::default()
                .xattr("user.common", b"1")
                .xattr("security.capability", &[1, 0, 0, 2]),
        )
        .file("b", b"", &Opts::default().xattr("user.common", b"1"))
        .file(
            "c",
            b"",
            &Opts::default()
                .xattr("user.solo", b"s")
                .xattr("system.posix_acl_access", &[2, 0, 0, 0]),
        )
        .finish();
    let (img, _) = convert(&tar);
    let w = walk(&img);
    assert_eq!(w[&p("a")].xattrs[&p("user.common")], b"1");
    assert_eq!(w[&p("a")].xattrs[&p("security.capability")], vec![1, 0, 0, 2]);
    assert_eq!(w[&p("b")].xattrs[&p("user.common")], b"1");
    assert_eq!(w[&p("c")].xattrs[&p("user.solo")], b"s");
    assert_eq!(w[&p("c")].xattrs[&p("system.posix_acl_access")], vec![2, 0, 0, 0]);
    let image = Image::open(Cursor::new(img.as_slice())).unwrap();
    assert_ne!(image.superblock().xattr_blkaddr, 0, "user.common is shared");
}

#[test]
fn extended_inodes_only_when_needed() {
    let tar = TarBuilder::new()
        .file("plain", b"", &Opts::default())
        .file("bigid", b"", &Opts::default().uid(70_000))
        .file("later", b"", &Opts::default().mtime(1_700_000_001))
        .file("nsec", b"", &Opts::default().pax("mtime", b"1700000000.5"))
        .finish();
    let w = walk(&convert(&tar).0);
    assert!(w[&p("plain")].compact);
    assert!(!w[&p("bigid")].compact);
    assert_eq!(w[&p("bigid")].uid, 70_000);
    assert!(!w[&p("later")].compact);
    assert_eq!(w[&p("later")].mtime, (1_700_000_001, 0));
    assert!(!w[&p("nsec")].compact);
    assert_eq!(w[&p("nsec")].mtime, (1_700_000_000, 500_000_000));
    assert_eq!(w[&p("plain")].mtime, (1_700_000_000, 0));
}

#[test]
fn explicit_root_entry_sets_root_attrs() {
    let tar = TarBuilder::new().dir("./", &Opts::default().mode(0o700)).finish();
    assert_eq!(walk(&convert(&tar).0)[&p("")].mode, 0o700);
}

#[test]
fn whiteouts_and_opaque_are_stored_as_overlay_markers() {
    let tar = TarBuilder::new()
        .whiteout("etc/gone")
        .dir("var", &Opts::default())
        .opaque("var")
        .finish();
    let w = walk(&convert(&tar).0);
    assert_eq!((w[&p("etc/gone")].kind, w[&p("etc/gone")].rdev), ('c', (0, 0)));
    assert_eq!(w[&p("var")].xattrs[&p("trusted.overlay.opaque")], b"y");
}

#[test]
fn non_utf8_names_round_trip() {
    let tar = TarBuilder::new()
        .entry(b"caf\xe9", b'0', b"latin1", b"", (0, 0), &Opts::default())
        .finish();
    let w = walk(&convert(&tar).0);
    assert_eq!(w[&b"caf\xe9".to_vec()].data, b"latin1");
}

#[test]
fn names_sorting_before_dot() {
    let mut b = TarBuilder::new();
    for name in ["a", "-", "+", "#a", "!"] {
        b.file(name, name.as_bytes(), &Opts::default());
    }
    let (img, _) = convert(&b.finish());
    let mut image = Image::open(Cursor::new(img.as_slice())).unwrap();
    let root = image.root_nid();
    let names: Vec<Vec<u8>> = image.read_dir(root).unwrap().into_iter().map(|e| e.name).collect();
    assert_eq!(names, vec![p("!"), p("#a"), p("+"), p("-"), p("a")]);
    for name in ["!", "#a", "+", "-", "a"] {
        let nid = image.lookup(name.as_bytes()).unwrap().unwrap();
        assert_eq!(image.read_data(nid).unwrap(), name.as_bytes());
    }
}

#[test]
fn negative_mtime_round_trips() {
    let tar = TarBuilder::new()
        .file("old", b"", &Opts::default().pax("mtime", b"-1.5"))
        .file("epoch", b"", &Opts::default().mtime(0))
        .finish();
    let w = walk(&convert(&tar).0);
    assert_eq!(w[&p("old")].mtime, (-2, 500_000_000));
    assert!(w[&p("old")].compact, "the minimum mtime is the base time");
    assert_eq!(w[&p("epoch")].mtime, (0, 0));
    assert!(!w[&p("epoch")].compact);
}

#[test]
fn open_rejects_truncated_and_foreign_images() {
    let (img, _) = convert(&TarBuilder::new().file("f", b"x", &Opts::default()).finish());
    assert!(matches!(Image::open(Cursor::new(&img[..4096])), Err(Error::Corrupt(_))));
    let mut bad = img.clone();
    bad[1024] ^= 0xff;
    assert!(matches!(
        Image::open(Cursor::new(bad.as_slice())),
        Err(Error::ProfileViolation(_))
    ));
}
