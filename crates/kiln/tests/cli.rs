use std::path::{Path, PathBuf};

use assert_cmd::Command;
use kiln_erofs::testtar::{Opts, TarBuilder};
use kiln_oci::testlayout::{LayoutBuilder, TestLayer};
use kiln_oci::{ContainerConfig, Platform};
use kiln_registry::testregistry::{Config, TestRegistry};
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

/// A tagged layout (`v1`) served by an in-process registry.
fn registry_with_image(dir: &Path, config: Config) -> TestRegistry {
    let mut b = LayoutBuilder::new(dir);
    let layers: Vec<TestLayer> = simple().into_iter().map(TestLayer::tar).collect();
    let cfg = ContainerConfig {
        cmd: Some(vec!["php".into()]),
        ..Default::default()
    };
    let d = b.image(&Platform::parse("linux/arm64").unwrap(), &layers, cfg);
    b.add(d, Some("v1")).finish();
    TestRegistry::serve_layout(dir, config)
}

fn stdout_json(out: std::process::Output) -> serde_json::Value {
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn convert_push_and_pull_through_a_registry() {
    let src = tempfile::tempdir().unwrap();
    let reg = registry_with_image(src.path(), Config::default());
    let (home, other) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let source = format!("{}/team/app:v1", reg.host());
    kiln(home.path())
        .args(["convert", "--platform", "linux/arm64", &source])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("downloaded 1 layers")
                .and(predicate::str::contains(format!("{source} → sha256:"))),
        );
    let warm = stdout_json(
        kiln(home.path())
            .args(["convert", "--json", "--platform", "linux/arm64", &source])
            .output()
            .unwrap(),
    );
    assert_eq!(warm["layersDownloaded"], 0);
    assert_eq!(warm["tag"], source.as_str());
    kiln(home.path())
        .args(["inspect", &source])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!("{source} (unverified provenance)")));

    let dest = format!("{}/team/kiln:v1", reg.host());
    let pushed = stdout_json(
        kiln(home.path())
            .args(["push", "--json", &source, &dest])
            .output()
            .unwrap(),
    );
    assert_eq!(pushed["digest"], warm["digest"]);
    assert!(pushed["blobs"].as_u64().unwrap() >= 2);
    let pulled = stdout_json(
        kiln(other.path())
            .args(["pull", "--json", "--tag", "copy:1", &dest])
            .output()
            .unwrap(),
    );
    assert_eq!(pulled["digest"], warm["digest"]);
    kiln(other.path())
        .args(["pull", &dest])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!("{dest} → sha256:")));
    kiln(other.path())
        .args(["ls"])
        .assert()
        .success()
        .stdout(predicate::str::contains("copy:1").and(predicate::str::contains(&dest)));

    // Pulling something that is not a kiln image says what to do instead.
    kiln(other.path())
        .args(["pull", &source])
        .assert()
        .failure()
        .stderr(predicate::str::contains("use `kiln convert`"));
}

#[test]
fn registry_credentials_come_from_a_credential_helper_on_path() {
    use std::os::unix::fs::PermissionsExt;
    let src = tempfile::tempdir().unwrap();
    let reg = registry_with_image(
        src.path(),
        Config {
            basic: Some(("kiln".into(), "hunter2".into())),
            ..Default::default()
        },
    );
    let home = tempfile::tempdir().unwrap();
    let source = format!("{}/app:v1", reg.host());
    let empty = tempfile::tempdir().unwrap();
    let out = kiln(home.path())
        .env("DOCKER_CONFIG", empty.path())
        .args(["convert", "--platform", "linux/arm64", &source])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("authentication failed") && stderr.contains("no credentials"),
        "{stderr}"
    );

    let bin = tempfile::tempdir().unwrap();
    let helper = bin.path().join("docker-credential-kilntest");
    std::fs::write(
        &helper,
        "#!/bin/sh\nread server\nprintf '{\"Username\":\"kiln\",\"Secret\":\"hunter2\"}'\n",
    )
    .unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let config = tempfile::tempdir().unwrap();
    std::fs::write(
        config.path().join("config.json"),
        serde_json::to_vec(&serde_json::json!({"credHelpers": {reg.host(): "kilntest"}})).unwrap(),
    )
    .unwrap();
    let path = format!("{}:{}", bin.path().display(), std::env::var("PATH").unwrap_or_default());
    kiln(home.path())
        .env("DOCKER_CONFIG", config.path())
        .env("PATH", path)
        .args(["convert", "--platform", "linux/arm64", &source])
        .assert()
        .success();
    // The password never reaches the registry in the clear (basic auth is base64), nor any output.
    let basic = format!(
        "Basic {}",
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, "kiln:hunter2")
    );
    assert!(
        reg.log()
            .iter()
            .any(|l| l.authorization.as_deref() == Some(basic.as_str()))
    );
}

#[test]
fn a_source_that_is_neither_a_path_nor_a_reference_is_explained() {
    let home = tempfile::tempdir().unwrap();
    kiln(home.path())
        .args(["convert", "./no/such/Dir"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "./no/such/Dir is not an existing path or a valid image reference",
        ));
    kiln(home.path())
        .args(["convert", "--ref", "x", "localhost:1/app:v1"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--ref selects an image inside a local source"));
}
