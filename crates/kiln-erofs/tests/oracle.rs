#![cfg(target_os = "linux")]

mod common;
mod model;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::{Mount, fsck, groups, is_root, skip, squash_all, try_convert_stack, view, walk_fs};
use kiln_erofs::testtar::{Opts, TarBuilder};
use proptest::strategy::{Strategy, ValueTree};
use proptest::test_runner::TestRunner;

fn oracle_bin() -> Option<PathBuf> {
    std::env::var_os("KILN_ORACLE").map(PathBuf::from)
}

fn handcrafted() -> Vec<Vec<Vec<u8>>> {
    let o = Opts::default;
    vec![
        vec![
            TarBuilder::new()
                .dir("etc", &o().mode(0o755))
                .file("etc/f", b"f", &o())
                .symlink("etc/l", "f", &o())
                .hardlink("etc/h", "etc/f")
                .whiteout("gone")
                .dir("op", &o())
                .opaque("op")
                .finish(),
        ],
        vec![
            TarBuilder::new()
                .dir("tmp", &o().mode(0o1777).uid(5).xattr("user.k", b"v"))
                .finish(),
            TarBuilder::new().file("tmp/cache/x", b"x", &o()).finish(),
        ],
        vec![
            TarBuilder::new()
                .dir("d", &o())
                .file("d/a", b"a", &o())
                .file("d/b", b"b", &o())
                .finish(),
            TarBuilder::new()
                .whiteout("d/a")
                .file("d", b"now a file", &o())
                .finish(),
            TarBuilder::new()
                .dir("d", &o().mode(0o700))
                .file("d/c", b"c", &o())
                .finish(),
        ],
        vec![
            TarBuilder::new().file("a", b"old", &o()).hardlink("b", "a").finish(),
            TarBuilder::new()
                .file("a", b"new", &o())
                .dir("x", &o().xattr("user.a", b"1"))
                .finish(),
            TarBuilder::new()
                .dir("x", &o().xattr("user.b", b"2"))
                .opaque("")
                .file("y", b"y", &o())
                .finish(),
        ],
    ]
}

fn sampled(n: usize) -> Vec<Vec<Vec<u8>>> {
    let mut runner = TestRunner::deterministic();
    let strategy = model::layers_strategy();
    let mut out = Vec::new();
    while out.len() < n {
        let layers = strategy.new_tree(&mut runner).unwrap().current();
        let comparable = !model::hits_deferred_dir_times(&layers) && !model::ambiguous_inheritance(&layers);
        if model::model_final(&layers).is_some() && comparable {
            out.push(layers.iter().map(|ops| model::to_tar(ops)).collect());
        }
    }
    out
}

/// Unmounts in reverse order of mounting, on success and on panic alike.
struct Mounts(Vec<Mount>);

impl Drop for Mounts {
    fn drop(&mut self) {
        while let Some(m) = self.0.pop() {
            drop(m);
        }
    }
}

fn compare_stack(bin: &Path, tars: &[Vec<u8>], case: usize) {
    let images = try_convert_stack(tars)
        .unwrap_or_else(|e| panic!("case {case}: kiln rejected a stack the oracle expects to be valid: {e}"));
    let dir = tempfile::tempdir().unwrap();
    let mut c_dirs = Vec::new();
    let mut k_dirs = Vec::new();
    let mut mounts = Mounts(Vec::new());
    for (i, (tar, img)) in tars.iter().zip(&images).enumerate() {
        let tar_path = dir.path().join(format!("l{i}.tar"));
        std::fs::write(&tar_path, tar).unwrap();
        let c_dir = dir.path().join(format!("c{i}"));
        let mut cmd = Command::new(bin);
        cmd.arg(&c_dir).arg(&tar_path);
        for parent in c_dirs.iter().rev() {
            cmd.arg(parent);
        }
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "case {case}: containerd rejected layer {i} that kiln accepted: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        c_dirs.push(c_dir);
        let img_path = dir.path().join(format!("l{i}.erofs"));
        std::fs::write(&img_path, img).unwrap();
        fsck(&img_path, &format!("case {case} layer {i}"));
        let k_dir = dir.path().join(format!("k{i}"));
        mounts.0.push(Mount::erofs(&img_path, &k_dir));
        k_dirs.push(k_dir);
    }
    let (a, b) = if tars.len() == 1 {
        (walk_fs(&c_dirs[0]), walk_fs(&k_dirs[0]))
    } else {
        let c_lowers: Vec<&Path> = c_dirs.iter().rev().map(PathBuf::as_path).collect();
        let k_lowers: Vec<&Path> = k_dirs.iter().rev().map(PathBuf::as_path).collect();
        let c_merged = dir.path().join("c-merged");
        let k_merged = dir.path().join("k-merged");
        mounts.0.push(Mount::overlay(&c_lowers, &c_merged));
        mounts.0.push(Mount::overlay(&k_lowers, &k_merged));
        (walk_fs(&c_merged), walk_fs(&k_merged))
    };
    if tars.len() == 1 {
        // containerd creates whiteout devices and parent directories "now", so their
        // mtimes are not comparable; everything else is.
        let keep_markers = |m: &std::collections::BTreeMap<Vec<u8>, common::Seen>| {
            m.iter()
                .map(|(p, s)| {
                    let whiteout = s.kind == 'c' && s.rdev == (0, 0);
                    let mtime = if s.kind == 'd' || whiteout { None } else { Some(s.mtime) };
                    (
                        p.clone(),
                        (
                            s.kind,
                            s.mode,
                            s.uid,
                            s.gid,
                            mtime,
                            s.xattrs.clone(),
                            s.data.clone(),
                            s.rdev,
                        ),
                    )
                })
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        assert_eq!(
            keep_markers(&a),
            keep_markers(&b),
            "case {case}: single layer differs from containerd"
        );
    } else {
        assert_eq!(
            view(&a, true),
            view(&b, true),
            "case {case}: merged view differs from containerd"
        );
        // kiln's squash must equal the kernel's overlay of kiln's own layers.
        let squash_path = dir.path().join("squash.erofs");
        std::fs::write(&squash_path, squash_all(&images)).unwrap();
        fsck(&squash_path, &format!("case {case} squash"));
        let squash_dir = dir.path().join("squash");
        mounts.0.push(Mount::erofs(&squash_path, &squash_dir));
        let squashed = walk_fs(&squash_dir);
        assert_eq!(
            view(&squashed, true),
            view(&b, true),
            "case {case}: squash differs from the kernel overlay of kiln's layers"
        );
        assert_eq!(
            groups(&squashed),
            groups(&b),
            "case {case}: squash hardlink groups differ from the kernel overlay"
        );
    }
    assert_eq!(
        groups(&a),
        groups(&b),
        "case {case}: hardlink groups differ from containerd"
    );
}

#[test]
fn kiln_matches_containerd() {
    let Some(bin) = oracle_bin() else {
        skip("set KILN_ORACLE to the oracle binary");
        return;
    };
    if !is_root() {
        skip("needs root");
        return;
    }
    let mut stacks = handcrafted();
    stacks.extend(sampled(200));
    assert_eq!(stacks.len(), 4 + 200, "handcrafted + sampled stacks");
    let mut compared = 0;
    for (case, tars) in stacks.iter().enumerate() {
        compare_stack(&bin, tars, case);
        compared += 1;
    }
    assert_eq!(compared, stacks.len(), "every stack must be compared");
}
