use std::path::{Path, PathBuf};

use assert_cmd::Command;
use kiln_erofs::testtar::{Opts, TarBuilder};
use kiln_oci::testlayout::{LayoutBuilder, TestLayer};
use kiln_oci::{ContainerConfig, Platform};
use predicates::prelude::*;

fn layout(dir: &Path, cmd: &str, layers: Vec<Vec<u8>>) -> PathBuf {
    let cfg = ContainerConfig {
        cmd: Some(vec![cmd.into()]),
        env: Some(vec!["A=1".into()]),
        ..Default::default()
    };
    layout_with(dir, cfg, layers)
}

fn layout_with(dir: &Path, cfg: ContainerConfig, layers: Vec<Vec<u8>>) -> PathBuf {
    let mut b = LayoutBuilder::new(dir);
    let layers: Vec<TestLayer> = layers.into_iter().map(TestLayer::tar).collect();
    let d = b.image(&Platform::parse("linux/arm64").unwrap(), &layers, cfg);
    b.add(d, None).finish()
}

fn simple() -> Vec<Vec<u8>> {
    vec![
        TarBuilder::new()
            .dir("tmp", &Opts::default().mode(0o1777))
            .file("hello", b"hi", &Opts::default())
            .finish(),
    ]
}

fn kiln(store: &Path) -> Command {
    let mut c = Command::cargo_bin("kiln").unwrap();
    c.arg("--store").arg(store);
    c
}

#[test]
fn convert_ls_inspect_gc_import() {
    let src = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let path = layout(&src.path().join("app"), "php", simple());
    kiln(home.path())
        .args(["convert", "--platform", "linux/arm64"])
        .arg(&path)
        .assert()
        .success()
        .stdout(predicate::str::contains("app:latest → sha256:"));
    kiln(home.path())
        .args(["ls"])
        .assert()
        .success()
        .stdout(predicate::str::contains("app:latest").and(predicate::str::contains("linux/arm64")));
    kiln(home.path())
        .args(["inspect", "app:latest"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("cmd:        php")
                .and(predicate::str::contains("env:        A=1"))
                .and(predicate::str::contains("unverified provenance")),
        );
    let json = kiln(home.path())
        .args(["inspect", "--json", "app:latest"])
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(v["images"][0]["config"]["process"]["cmd"][0], "php");
    // GC frees the source OCI blobs (layer, config, manifest) but keeps the kiln image.
    kiln(home.path())
        .arg("gc")
        .assert()
        .success()
        .stdout(predicate::str::contains("removed 3 blobs"));
    kiln(home.path()).args(["inspect", "app:latest"]).assert().success();
    kiln(home.path())
        .arg("gc")
        .assert()
        .success()
        .stdout(predicate::str::contains("removed 0 blobs"));

    let other = tempfile::tempdir().unwrap();
    kiln(other.path())
        .args(["import", "--from-store"])
        .arg(home.path())
        .args(["app:latest", "--as", "copy:1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("copy:1 → sha256:"));
    kiln(other.path()).args(["inspect", "copy:1"]).assert().success();
}

#[test]
fn json_convert_summary_reports_cache_hits() {
    let src = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let path = layout(&src.path().join("app"), "php", simple());
    let run = || {
        let out = kiln(home.path())
            .args(["convert", "--json", "--platform", "linux/arm64", "--tag", "x:1"])
            .arg(&path)
            .output()
            .unwrap();
        assert!(out.status.success());
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()
    };
    assert_eq!(run()["images"][0]["layers"][0]["cached"], false);
    assert_eq!(run()["images"][0]["layers"][0]["cached"], true);
}

#[test]
fn image_strings_are_sanitised_in_inspect_and_errors() {
    let src = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let cfg = ContainerConfig {
        cmd: Some(vec!["\x1b]0;pwned\x07\x1b[2J".into()]),
        entrypoint: Some(vec!["\x1b[2Jentry\x07".into()]),
        env: Some(vec!["X=\x1b[2Jv".into()]),
        working_dir: Some("/w\x1b[2Jd\x07".into()),
        user: Some("u\x1b[2Js\x07".into()),
        ..Default::default()
    };
    let path = layout_with(&src.path().join("evil"), cfg, simple());
    kiln(home.path())
        .args(["convert", "--platform", "linux/arm64"])
        .arg(&path)
        .assert()
        .success();
    kiln(home.path())
        .args(["inspect", "evil:latest"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("\x1b")
                .not()
                .and(predicate::str::contains("\x07").not())
                .and(predicate::str::contains("env:        X=[2Jv"))
                .and(predicate::str::contains("entrypoint: [2Jentry"))
                .and(predicate::str::contains("workdir:    /w[2Jd"))
                .and(predicate::str::contains("user:       u[2Js")),
        );

    // A hostile tar path ends up in an error message.
    let bad = vec![TarBuilder::new().hardlink("x", "\x1b[31mmissing").finish()];
    let path = layout(&src.path().join("bad"), "sh", bad);
    let out = kiln(home.path())
        .args(["convert", "--platform", "linux/arm64"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.starts_with("kiln: error:") && !stderr.contains('\x1b'));
    assert!(stderr.ends_with('\n'));
    assert_eq!(stderr.trim_end_matches('\n').lines().count(), 1);
}

#[test]
fn missing_platform_lists_available_ones() {
    let src = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let path = layout(&src.path().join("app"), "php", simple());
    kiln(home.path())
        .args(["convert", "--platform", "linux/amd64"])
        .arg(&path)
        .assert()
        .failure()
        .stderr(predicate::str::contains("linux/arm64"));
}

#[test]
fn errors_print_each_cause_once() {
    let home = tempfile::tempdir().unwrap();
    let out = kiln(home.path()).args(["inspect", "nope:1"]).output().unwrap();
    assert!(!out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "kiln: error: no image named \"nope:1\"\n"
    );
}

#[test]
fn bench_emits_json() {
    let src = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let path = layout(&src.path().join("app"), "php", simple());
    let out = kiln(home.path())
        .args(["bench", "--platform", "linux/arm64", "--changed-top-bytes", "100000"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["layers"], 1);
    for k in ["cold_ms", "warm_ms", "changed_top_ms"] {
        assert!(v[k].is_u64(), "{k}");
    }
}

/// 100 independent one-file layers: no directories, so none needs its lowers.
fn flat_layers(n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| {
            TarBuilder::new()
                .file(&format!("f{i}"), b"x", &Opts::default())
                .finish()
        })
        .collect()
}

/// `ulimit -n` lowers both limits of the child, so only bounded phase-A file use passes.
#[cfg(unix)]
#[test]
fn many_layers_convert_with_a_low_open_file_limit() {
    let src = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let path = layout(&src.path().join("app"), "php", flat_layers(100));
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c")
        .arg("ulimit -n 256 && exec \"$0\" \"$@\"")
        .arg(assert_cmd::cargo::cargo_bin("kiln"))
        .arg("--store")
        .arg(home.path())
        .args(["convert", "--platform", "linux/arm64"])
        .arg(&path);
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("bottom 91 squashed"));
}

#[test]
fn a_bad_tag_fails_before_any_conversion() {
    let src = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let path = layout(&src.path().join("app"), "php", simple());
    kiln(home.path())
        .args(["convert", "--platform", "linux/arm64", "--tag", "bad tag"])
        .arg(&path)
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid --tag bad tag"));
    let blobs = home.path().join("blobs/sha256");
    assert!(!blobs.exists() || std::fs::read_dir(blobs).unwrap().next().is_none());
    kiln(home.path())
        .args(["import", "--from-store"])
        .arg(home.path())
        .args(["x:1", "--as", "bad name"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid name bad name"));
}

#[test]
fn repeated_platform_flags_convert_once() {
    let src = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let path = layout(&src.path().join("app"), "php", simple());
    let out = kiln(home.path())
        .args([
            "convert",
            "--json",
            "--platform",
            "linux/arm64",
            "--platform",
            "linux/arm64",
        ])
        .arg(&path)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["mediaType"], "application/vnd.oci.image.manifest.v1+json");
    assert_eq!(v["images"].as_array().unwrap().len(), 1);
}

#[test]
fn bench_refuses_more_than_one_platform() {
    let src = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let path = layout(&src.path().join("app"), "php", simple());
    kiln(home.path())
        .args(["bench", "--platform", "linux/arm64", "--platform", "linux/amd64"])
        .arg(&path)
        .assert()
        .failure()
        .stderr(predicate::str::contains("at most one --platform"));
}
