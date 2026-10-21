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
        .args(["convert", "--boot-layers", "none", "--platform", "linux/arm64"])
        .arg(&path)
        .assert()
        .success()
        .stdout(predicate::str::contains("app:latest → sha256:"))
        // A provisional image (no network: the pinned fetch has its own test), and
        // what `kiln run` needs for it.
        .stderr(predicate::str::contains(
            "note: --boot-layers none: a provisional image without kernel or kiln-init layers; \
             `kiln run` needs --kernel PATH --allow-custom-kernel for it (kiln-init comes from the pin, or --init)",
        ));
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
                .and(predicate::str::contains("unverified provenance"))
                .and(predicate::str::contains(
                    "boot:       none (a provisional image: run it with --kernel and --init)",
                )),
        );
    let json = kiln(home.path())
        .args(["inspect", "--json", "app:latest"])
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(v["images"][0]["config"]["process"]["cmd"][0], "php");
    // GC frees the source layer but keeps the kiln image, and the source manifest
    // and config that `kiln rebase` reads (spec §5.2).
    kiln(home.path())
        .arg("gc")
        .assert()
        .success()
        .stdout(predicate::str::contains("removed 1 blobs"));
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
            .args([
                "convert",
                "--boot-layers",
                "none",
                "--json",
                "--platform",
                "linux/arm64",
                "--tag",
                "x:1",
            ])
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
        .args(["convert", "--boot-layers", "none", "--platform", "linux/arm64"])
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
        .args(["convert", "--boot-layers", "none", "--platform", "linux/arm64"])
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
        .args(["convert", "--boot-layers", "none", "--platform", "linux/amd64"])
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
        .args([
            "bench",
            "--boot-layers",
            "none",
            "--platform",
            "linux/arm64",
            "--changed-top-bytes",
            "100000",
        ])
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
        .args(["convert", "--boot-layers", "none", "--platform", "linux/arm64"])
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
        .args([
            "convert",
            "--boot-layers",
            "none",
            "--platform",
            "linux/arm64",
            "--tag",
            "bad tag",
        ])
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
            "--boot-layers",
            "none",
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
        .args([
            "bench",
            "--boot-layers",
            "none",
            "--platform",
            "linux/arm64",
            "--platform",
            "linux/amd64",
        ])
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
        .args(["convert", "--boot-layers", "none", "--platform", "linux/arm64", &source])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("downloaded 1 layer (")
                .and(predicate::str::contains("1 layer (0 cached)"))
                .and(predicate::str::contains(format!("{source} → sha256:"))),
        );
    let warm = stdout_json(
        kiln(home.path())
            .args([
                "convert",
                "--boot-layers",
                "none",
                "--json",
                "--platform",
                "linux/arm64",
                &source,
            ])
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
        .args(["convert", "--boot-layers", "none", "--platform", "linux/arm64", &source])
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
        .args(["convert", "--boot-layers", "none", "--platform", "linux/arm64", &source])
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
    // Neither the failed nor the successful run printed the password.
    assert!(!stderr.contains("hunter2"));
    let plain: &[&str] = &[
        "convert",
        "--boot-layers",
        "none",
        "--platform",
        "linux/arm64",
        source.as_str(),
    ];
    let json: &[&str] = &[
        "convert",
        "--boot-layers",
        "none",
        "--json",
        "--platform",
        "linux/arm64",
        source.as_str(),
    ];
    for args in [plain, json] {
        let out = kiln(home.path())
            .env("DOCKER_CONFIG", config.path())
            .env(
                "PATH",
                format!("{}:{}", bin.path().display(), std::env::var("PATH").unwrap_or_default()),
            )
            .args(args)
            .output()
            .unwrap();
        let all = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.status.success() && !all.contains("hunter2"), "{all}");
    }
}

#[test]
fn a_source_that_is_neither_a_path_nor_a_reference_is_explained() {
    let home = tempfile::tempdir().unwrap();
    kiln(home.path())
        .args(["convert", "--boot-layers", "none", "Not A Reference!"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "Not A Reference! is not an existing path or a valid image reference",
        ));
    kiln(home.path())
        .args(["convert", "--boot-layers", "none", "--ref", "x", "localhost:1/app:v1"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--ref selects an image inside a local source"));
    // A rejected invocation creates nothing.
    assert!(!home.path().join("refs.json").exists());
}

#[test]
fn a_source_spelled_like_a_path_is_never_looked_up_in_a_registry() {
    let home = tempfile::tempdir().unwrap();
    let empty = tempfile::tempdir().unwrap();
    for src in [
        "app.tar",
        "build/app.tar",
        "app.tar.gz",
        "app.tgz",
        "./nope",
        "../nope",
        "/no/such/nope",
        "~/nope",
    ] {
        kiln(home.path())
            .env("DOCKER_CONFIG", empty.path())
            .args(["convert", "--boot-layers", "none", src])
            .assert()
            .failure()
            .stderr(predicate::str::contains(format!("{src}: no such file or directory")));
    }
    // Nothing was created: the store was never opened.
    assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 0);
}

#[test]
fn a_registry_convert_that_finds_nothing_says_there_was_no_local_path_either() {
    let src = tempfile::tempdir().unwrap();
    let reg = registry_with_image(src.path(), Config::default());
    let home = tempfile::tempdir().unwrap();
    let empty = tempfile::tempdir().unwrap();
    kiln(home.path())
        .env("DOCKER_CONFIG", empty.path())
        .args([
            "convert",
            "--boot-layers",
            "none",
            &format!("{}/app:missing", reg.host()),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not found").and(predicate::str::contains("(no such local path either)")));
}

#[test]
fn other_registry_convert_errors_do_not_mention_local_paths() {
    let src = tempfile::tempdir().unwrap();
    let mut b = LayoutBuilder::new(src.path());
    let layers: Vec<TestLayer> = simple().into_iter().map(TestLayer::tar).collect();
    let s390x = Platform::parse("linux/s390x").unwrap();
    let d = b.image(&s390x, &layers, ContainerConfig::default());
    b.add(d, Some("v1")).finish();
    let reg = TestRegistry::serve_layout(src.path(), Config::default());
    let home = tempfile::tempdir().unwrap();
    let empty = tempfile::tempdir().unwrap();
    let out = kiln(home.path())
        .env("DOCKER_CONFIG", empty.path())
        .args([
            "convert",
            "--boot-layers",
            "none",
            "--platform",
            "linux/s390x",
            &format!("{}/app:v1", reg.host()),
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unsupported platform") && !stderr.contains("local path"),
        "{stderr}"
    );
}

#[test]
fn converting_a_kiln_image_from_a_registry_suggests_kiln_pull() {
    let src = tempfile::tempdir().unwrap();
    let reg = registry_with_image(src.path(), Config::default());
    let home = tempfile::tempdir().unwrap();
    let source = format!("{}/team/app:v1", reg.host());
    let dest = format!("{}/team/kiln:v1", reg.host());
    kiln(home.path())
        .args(["convert", "--boot-layers", "none", "--platform", "linux/arm64", &source])
        .assert()
        .success();
    kiln(home.path()).args(["push", &source, &dest]).assert().success();
    kiln(home.path())
        .args(["convert", "--boot-layers", "none", "--platform", "linux/arm64", &dest])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("is a kiln image")
                .and(predicate::str::contains("kiln pull"))
                .and(predicate::str::contains("local path").not()),
        );
}

/// `kiln run` refuses bad flags before anything else, with Docker's 125.
#[test]
fn run_refuses_bad_flags_with_125() {
    let home = tempfile::tempdir().unwrap();
    kiln(home.path())
        .args(["run", "--kernel", "/k", "img"])
        .assert()
        .code(125)
        .stderr(predicate::str::contains("--allow-custom-kernel"));
    kiln(home.path())
        .args(["run", "--disk", "1000", "img"])
        .assert()
        .code(125)
        .stderr(predicate::str::contains("multiple of 4096"));
    kiln(home.path())
        .args(["run", "--persist-reset", "img"])
        .assert()
        .code(125)
        .stderr(predicate::str::contains("need --persist"));
    kiln(home.path())
        .args([
            "run",
            "--persist",
            "/d.img",
            "--persist-reset",
            "--persist-force",
            "img",
        ])
        .assert()
        .code(125)
        .stderr(predicate::str::contains("exclude each other"));
    for (args, needle) in [
        (&["run", "-p", "8080:80", "img"][..], "need --net"),
        (&["run", "--egress", "deny-all", "img"], "need --net"),
        (&["run", "--net", "--egress", "everything", "img"], "unknown --egress"),
        (
            &["run", "--net", "--egress", "open", "--egress", "deny-all", "img"],
            "two policies",
        ),
        (
            &["run", "--net", "--egress", "allow=10.0.0.0/40", "img"],
            "not an IPv4 address or CIDR",
        ),
        (
            &["run", "--net", "-p", "8080:80", "-p", "0.0.0.0:8080:81", "img"],
            "published twice",
        ),
    ] {
        kiln(home.path())
            .args(args)
            .assert()
            .code(125)
            .stderr(predicate::str::contains(needle));
    }
}

/// Usage errors of `kiln run` exit 125, as `docker run`'s do; help and version
/// exit 0, and other subcommands keep clap's 2.
#[test]
fn run_usage_errors_exit_125() {
    let home = tempfile::tempdir().unwrap();
    for (args, needle) in [
        (&["run", "--disk", "abc", "img"][..], "--disk"),
        (&["run", "--vmm", "qemu", "img"], "unknown VMM"),
        (&["run", "--cpus", "300", "img"], "--cpus"),
        (&["run"], "IMAGE"),
        (&["run", "--bogus", "img"], "--bogus"),
        (&["run", "-e"], "-e"),
        (&["run", "--net", "-p", "80", "img"], "[ADDRESS:]HOST:GUEST"),
    ] {
        kiln(home.path())
            .args(args)
            .assert()
            .code(125)
            .stderr(predicate::str::contains(needle));
    }
    kiln(home.path())
        .args(["run", "--help"])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("--allow-custom-kernel"));
    kiln(home.path()).arg("--version").assert().code(0);
    kiln(home.path()).args(["ls", "--bogus"]).assert().code(2);
}

/// Where VMs cannot run, `kiln run` says how to use a Lima VM (spec §9.7).
#[cfg(not(target_os = "linux"))]
#[test]
fn run_explains_the_lima_vm() {
    let home = tempfile::tempdir().unwrap();
    kiln(home.path()).args(["run", "img"]).assert().code(125).stderr(
        predicate::str::contains("Linux with KVM")
            .and(predicate::str::contains("Lima VM"))
            .and(predicate::str::contains("lima/kiln.yaml")),
    );
}

/// On Linux the image is resolved before a VMM is needed: a provisional image, run
/// without --kernel, is refused with 125 (no KVM needed).
#[cfg(target_os = "linux")]
#[test]
fn run_refuses_an_image_without_a_kernel_before_booting() {
    let src = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let mut b = LayoutBuilder::new(&src.path().join("app"));
    let d = b.image(
        &Platform::host(),
        &[TestLayer::tar(simple().remove(0))],
        ContainerConfig::default(),
    );
    let path = b.add(d, None).finish();
    kiln(home.path())
        .args(["convert", "--boot-layers", "none", "--tag", "app"])
        .arg(&path)
        .assert()
        .success();
    kiln(home.path())
        .args(["run", "app"])
        .assert()
        .code(125)
        .stderr(predicate::str::contains("has no kernel layer").and(predicate::str::contains("--allow-custom-kernel")));
    kiln(home.path()).args(["run", "missing"]).assert().code(125);
    // An egress exception for the host's outbound address is refused before boot
    // (pasta gives the VM's namespace that address).
    let outbound = {
        let s = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
        s.connect("192.0.2.1:9").unwrap();
        s.local_addr().unwrap().ip()
    };
    kiln(home.path())
        .args(["run", "--net", "--egress", &format!("allow={outbound}"), "app"])
        .assert()
        .code(125)
        .stderr(predicate::str::contains(format!(
            "is the host's own address {outbound}"
        )));
    // -t -i puts kiln's terminal in raw mode: without one on stdin, a clear error.
    kiln(home.path())
        .args(["run", "-t", "-i", "app"])
        .assert()
        .code(125)
        .stderr(predicate::str::contains("the input device is not a TTY"));
}

/// The real pinned fetch (spec §6.5, §8.1), from ghcr.io: only with
/// KILN_TEST_NETWORK=1 (one CI job sets it), so the default tests need no network.
/// The image gets the pinned kernel layer (no kiln-init is pinned yet), fetched
/// anonymously by digest, and `kiln gc` keeps the kernel blob.
#[test]
fn pinned_boot_layers_are_fetched_from_the_registry() {
    if std::env::var_os("KILN_TEST_NETWORK").is_none_or(|v| v != "1") {
        eprintln!("skipped: KILN_TEST_NETWORK=1 fetches the pinned kernel from ghcr.io");
        return;
    }
    let src = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let docker = tempfile::tempdir().unwrap();
    let path = layout(&src.path().join("app"), "php", simple());
    kiln(home.path())
        .env("DOCKER_CONFIG", docker.path())
        .args(["convert", "--platform", "linux/arm64"])
        .arg(&path)
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "note: this kiln pins no kiln-init for arm64 yet, so the image has no kiln-init layer; \
             `kiln run` needs --init for it",
        ));
    kiln(home.path())
        .args(["inspect", "app:latest"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "boot:       kernel base 6.18.54, no init layer (run it with --init)",
        ));
    let pin = kiln_image::artifacts::kernel_pin("base", "6.18.54", "arm64").unwrap();
    let blob = home
        .path()
        .join("blobs/sha256")
        .join(pin.digest.trim_start_matches("sha256:"));
    assert_eq!(std::fs::metadata(&blob).unwrap().len(), pin.size);
    kiln(home.path()).arg("gc").assert().success();
    assert!(blob.exists(), "gc removed the pinned kernel");
}

const FPM: &str = "usr/local/etc/php-fpm.d/laravel-cloud-fpm.conf";

/// A two-layer base: `var/` with `var_mode`, nginx, and the php-fpm config.
fn rebase_base(nginx: &[u8], var_mode: u32, fpm: &[u8]) -> Vec<Vec<u8>> {
    vec![
        TarBuilder::new()
            .file("etc/os-release", b"ID=debian\nVERSION_ID=12\n", &Opts::default())
            .dir("var", &Opts::default().mode(var_mode))
            .finish(),
        TarBuilder::new()
            .file("usr/sbin/nginx", nginx, &Opts::default().mode(0o755))
            .file(FPM, fpm, &Opts::default())
            .finish(),
    ]
}

fn rebase_cfg(php: &str) -> ContainerConfig {
    ContainerConfig {
        cmd: Some(vec!["php-fpm".into()]),
        env: Some(vec![
            format!("PHP_VERSION={php}"),
            "PHP_FPM_REQUEST_TERMINATE_TIMEOUT=60".into(),
        ]),
        labels: Some([("php.version".to_string(), php.to_string())].into()),
        ..Default::default()
    }
}

/// End to end: convert a base and an app on it, rebase onto a patched base, then
/// see the shadowed-path gate catch a copied-up config the next base changed.
#[test]
fn rebase_onto_a_patched_base_and_refuse_a_stale_copy() {
    let src = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let fpm = b"request_terminate_timeout = ${PHP_FPM_REQUEST_TERMINATE_TIMEOUT}\n";
    let old = rebase_base(b"nginx 1.30.4", 0o755, fpm);
    let patched = rebase_base(b"nginx 1.30.6", 0o750, fpm);
    let fpm_changed = rebase_base(b"nginx 1.30.6", 0o755, b"request_terminate_timeout = 30\n");
    let mut app = old.clone();
    // A code layer with implicit parents, and the template's `sed -i` copy-up.
    app.push(
        TarBuilder::new()
            .file("var/www/index.php", b"<?php echo 'hi';", &Opts::default())
            .finish(),
    );
    app.push(
        TarBuilder::new()
            .file(FPM, b"request_terminate_timeout = 120\n", &Opts::default())
            .finish(),
    );
    for (name, layers, php) in [
        ("base1", old, "8.4.12"),
        ("base2", patched, "8.4.14"),
        ("base3", fpm_changed, "8.4.14"),
        ("app", app, "8.4.12"),
    ] {
        let path = layout_with(&src.path().join(name), rebase_cfg(php), layers);
        let mut c = kiln(home.path());
        c.args([
            "convert",
            "--boot-layers",
            "none",
            "--platform",
            "linux/arm64",
            "--tag",
            &format!("{name}:1"),
        ]);
        if name == "app" {
            c.args(["--base", "base1:1"]);
        }
        c.arg(&path).assert().success();
    }
    kiln(home.path())
        .args(["inspect", "app:1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("base:       base1:1"));

    let out = kiln(home.path())
        .args(["rebase", "base1:1", "base2:1", "app:1", "--tag", "app:2", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let img = &v["images"][0];
    assert_eq!(img["baseLayers"], 2);
    assert_eq!(img["reresolved"], 2, "both app layers have implicit parents");
    assert_eq!(img["app"][0]["changed"], true, "var/www inherits the patched /var");
    assert_eq!(img["app"][1]["changed"], false, "the copy-up layer is reused as is");
    kiln(home.path()).args(["inspect", "app:2"]).assert().success().stdout(
        predicate::str::contains("base:       base2:1").and(predicate::str::contains("env:        PHP_VERSION=8.4.14")),
    );

    kiln(home.path())
        .args(["rebase", "base1:1", "base3:1", "app:1", "--tag", "app:3"])
        .assert()
        .failure()
        .code(1)
        .stderr(
            predicate::str::contains("kiln: rebase refused:")
                .and(predicate::str::contains("  - shadow: 1 path the app shadows differs"))
                .and(predicate::str::contains(format!("/{FPM}: content differs"))),
        );
    kiln(home.path()).args(["inspect", "app:3"]).assert().failure();
}

#[test]
fn rebase_text_output_and_a_missing_record() {
    let src = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let fpm = b"x\n";
    let base = rebase_base(b"nginx 1", 0o755, fpm);
    let mut app = base.clone();
    app.push(TarBuilder::new().file("srv/app", b"a", &Opts::default()).finish());
    for (name, layers) in [("base", base.clone()), ("app", app.clone())] {
        let path = layout_with(&src.path().join(name), rebase_cfg("8.4.1"), layers);
        kiln(home.path())
            .args([
                "convert",
                "--boot-layers",
                "none",
                "--platform",
                "linux/arm64",
                "--tag",
                &format!("{name}:1"),
            ])
            .arg(&path)
            .assert()
            .success();
    }
    kiln(home.path())
        .args(["rebase", "base:1", "base:1", "app:1", "--tag", "x:1"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("bottom: the image records no base"));
    // Usage errors are clap's 2; refusals and other errors are 1 (spec §16.6).
    kiln(home.path()).args(["rebase", "base:1", "base:1"]).assert().code(2);
    let path = layout_with(&src.path().join("app2"), rebase_cfg("8.4.1"), app);
    kiln(home.path())
        .args([
            "convert",
            "--boot-layers",
            "none",
            "--platform",
            "linux/arm64",
            "--tag",
            "app:2",
            "--base",
            "base:1",
        ])
        .arg(&path)
        .assert()
        .success()
        .stdout(predicate::str::contains("bottom 2 from base base:1"));
    kiln(home.path())
        .args(["rebase", "base:1", "base:1", "app:2", "--tag", "x:1"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("2 base layers, 1 app layer (0 changed, 1 re-resolved)")
                .and(predicate::str::contains("  reused   sha256:"))
                .and(predicate::str::contains("x:1 → sha256:")),
        );
}
