#![cfg(target_os = "linux")]

mod common;

use std::path::Path;
use std::process::Command;

use common::{Mount, convert, convert_stack, fixtures, groups, is_root, squash_all, view, walk, walk_fs};
use kiln_erofs::testtar::{Opts, TarBuilder};

fn enabled() -> bool {
    std::env::var_os("KILN_KERNEL_TESTS").is_some()
}

fn stack() -> Vec<Vec<u8>> {
    let l0 = TarBuilder::new()
        .dir("etc", &Opts::default())
        .file("etc/a", b"a", &Opts::default())
        .file("etc/b", b"b", &Opts::default())
        .dir("var", &Opts::default())
        .file("var/x", b"x", &Opts::default())
        .file("h1", b"h", &Opts::default())
        .hardlink("h2", "h1")
        .dir("tmp", &Opts::default().mode(0o1777).xattr("user.k", b"v"))
        .finish();
    let l1 = TarBuilder::new()
        .whiteout("etc/a")
        .dir("var", &Opts::default())
        .opaque("var")
        .file("var/y", b"y", &Opts::default())
        .file("h1", b"new", &Opts::default())
        .file("tmp/cache/z", b"z", &Opts::default())
        .finish();
    convert_stack(&[l0, l1])
}

fn write_tmp(dir: &Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, bytes).unwrap();
    p
}

#[test]
fn fsck_accepts_every_image() {
    if !enabled() {
        eprintln!("skipping: set KILN_KERNEL_TESTS=1");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut images: Vec<(String, Vec<u8>)> = fixtures()
        .into_iter()
        .map(|(n, t)| (n.to_string(), convert(&t).0))
        .collect();
    let layers = stack();
    images.push(("squash".into(), squash_all(&layers)));
    for (i, l) in layers.into_iter().enumerate() {
        images.push((format!("layer{i}"), l));
    }
    for (name, img) in images {
        let path = write_tmp(dir.path(), &format!("{name}.erofs"), &img);
        let out = Command::new("fsck.erofs")
            .arg(&path)
            .output()
            .expect("fsck.erofs is installed (erofs-utils)");
        assert!(
            out.status.success(),
            "fsck.erofs {name}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn kernel_mount_matches_reader() {
    if !enabled() || !is_root() {
        eprintln!("skipping: needs KILN_KERNEL_TESTS=1 and root");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    for (name, tar) in fixtures() {
        let img = convert(&tar).0;
        let path = write_tmp(dir.path(), &format!("{name}.erofs"), &img);
        let mnt = dir.path().join(format!("{name}.mnt"));
        let _m = Mount::erofs(&path, &mnt);
        let kernel = walk_fs(&mnt);
        let ours = walk(&img);
        assert_eq!(
            view(&kernel, false),
            view(&ours, false),
            "fixture {name}: kernel view differs from kiln's reader"
        );
        assert_eq!(groups(&kernel), groups(&ours), "fixture {name}: hardlink groups differ");
        for (p, s) in &ours {
            assert_eq!(
                kernel[p].nlink,
                s.nlink,
                "fixture {name}: nlink of {:?}",
                String::from_utf8_lossy(p)
            );
        }
    }
}

#[test]
fn overlay_over_layers_equals_squash() {
    if !enabled() || !is_root() {
        eprintln!("skipping: needs KILN_KERNEL_TESTS=1 and root");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let layers = stack();
    let p0 = write_tmp(dir.path(), "l0.erofs", &layers[0]);
    let p1 = write_tmp(dir.path(), "l1.erofs", &layers[1]);
    let m0 = dir.path().join("m0");
    let m1 = dir.path().join("m1");
    let merged = dir.path().join("merged");
    let _l0 = Mount::erofs(&p0, &m0);
    let _l1 = Mount::erofs(&p1, &m1);
    let _ov = Mount::overlay(&[&m1, &m0], &merged);
    let overlay = walk_fs(&merged);
    let squashed = walk(&squash_all(&layers));
    assert!(!overlay.contains_key(b"etc/a".as_slice()));
    assert!(!overlay.contains_key(b"var/x".as_slice()));
    assert_eq!(overlay[&b"tmp".to_vec()].mode, 0o1777, "implicit tmp inherited 1777");
    assert_eq!(view(&overlay, false), view(&squashed, false));
    assert_eq!(groups(&overlay), groups(&squashed));
}
