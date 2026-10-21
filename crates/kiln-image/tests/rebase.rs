//! `convert --base` and `rebase` (spec §16).
mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use common::{arm, mode_of, req};
use kiln_erofs::testtar::{Opts, TarBuilder};
use kiln_image::rebase::gates::Refusal;
use kiln_image::{
    BaseImage, ConvertOptions, ImageError, Output, RebaseRequest, convert_local, load, rebase, resolve_name,
};
use kiln_oci::testlayout::{LayoutBuilder, TestLayer};
use kiln_oci::{ContainerConfig, Platform};
use kiln_store::{Digest, Store};

const FPM: &str = "usr/local/etc/php-fpm.d/laravel-cloud-fpm.conf";

/// What a base build varies.
#[derive(Clone)]
struct Base {
    nginx: &'static [u8],
    var_mode: u32,
    fpm_conf: &'static [u8],
    os_release: &'static str,
    php: &'static str,
    env: Vec<&'static str>,
    cmd: &'static str,
}

impl Base {
    fn old() -> Self {
        Self {
            nginx: b"nginx 1.30.4",
            var_mode: 0o755,
            fpm_conf: b"request_terminate_timeout = ${PHP_FPM_REQUEST_TERMINATE_TIMEOUT}\n",
            os_release: "ID=debian\nVERSION_ID=\"12\"\nVERSION_CODENAME=bookworm\n",
            php: "8.4.12",
            env: vec![
                "PATH=/usr/local/bin:/usr/bin:/bin",
                "PHP_FPM_REQUEST_TERMINATE_TIMEOUT=60",
            ],
            cmd: "php-fpm",
        }
    }

    /// An apt upgrade: a new nginx, a new PHP patch release, `/var` tightened.
    fn patched() -> Self {
        Self {
            nginx: b"nginx 1.30.6",
            var_mode: 0o750,
            php: "8.4.14",
            ..Self::old()
        }
    }

    fn layers(&self) -> Vec<Vec<u8>> {
        vec![
            TarBuilder::new()
                .dir("etc", &Opts::default().mode(0o755))
                .file("usr/lib/os-release", self.os_release.as_bytes(), &Opts::default())
                .symlink("etc/os-release", "../usr/lib/os-release", &Opts::default())
                .dir("tmp", &Opts::default().mode(0o1777))
                .dir("var", &Opts::default().mode(self.var_mode))
                .finish(),
            TarBuilder::new()
                .file("usr/sbin/nginx", self.nginx, &Opts::default().mode(0o755))
                .file(FPM, self.fpm_conf, &Opts::default())
                .finish(),
        ]
    }

    fn env(&self) -> Vec<String> {
        let php = format!("PHP_VERSION={}", self.php);
        self.env.iter().map(|e| e.to_string()).chain([php]).collect()
    }

    fn config(&self) -> ContainerConfig {
        ContainerConfig {
            entrypoint: Some(vec!["docker-php-entrypoint".into()]),
            cmd: Some(vec![self.cmd.into()]),
            env: Some(self.env()),
            labels: Some(BTreeMap::from([
                ("php.version".into(), self.php.into()),
                ("vendor".into(), "laravel".into()),
            ])),
            ..Default::default()
        }
    }
}

/// `var/www/index.php` with no `var/` header: it inherits `/var` from the base.
fn app_code() -> Vec<u8> {
    TarBuilder::new()
        .file("var/www/index.php", b"<?php echo 'hi';", &Opts::default())
        .finish()
}

fn app_bin() -> Vec<u8> {
    TarBuilder::new()
        .dir("opt", &Opts::default().mode(0o755))
        .file("opt/app", b"#!/bin/sh\necho app\n", &Opts::default().mode(0o755))
        .finish()
}

/// What the Dockerfile template's `sed -i` does: copies the base's config up.
fn copied_up_fpm() -> Vec<u8> {
    TarBuilder::new()
        .file(FPM, b"request_terminate_timeout = 30\n", &Opts::default())
        .finish()
}

fn write_layout(dir: &Path, platforms: &[Platform], tars: &[Vec<u8>], cfg: ContainerConfig) -> PathBuf {
    let mut b = LayoutBuilder::new(dir);
    let layers: Vec<TestLayer> = tars.iter().cloned().map(TestLayer::tar).collect();
    let descs: Vec<_> = platforms.iter().map(|p| b.image(p, &layers, cfg.clone())).collect();
    let top = if let [one] = descs.as_slice() {
        one.clone()
    } else {
        b.multiarch(descs)
    };
    b.add(top, None).finish()
}

struct Fixture {
    _src: tempfile::TempDir,
    _home: tempfile::TempDir,
    store: Store,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let src = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let store = Store::open(home.path()).unwrap();
        let root = src.path().to_path_buf();
        Self {
            _src: src,
            _home: home,
            store,
            root,
        }
    }

    fn convert(&self, name: &str, tars: &[Vec<u8>], cfg: ContainerConfig, opts: &ConvertOptions) -> Output {
        self.convert_for(name, &[arm()], tars, cfg, opts).unwrap()
    }

    fn convert_for(
        &self,
        name: &str,
        platforms: &[Platform],
        tars: &[Vec<u8>],
        cfg: ContainerConfig,
        opts: &ConvertOptions,
    ) -> kiln_image::Result<Output> {
        let dir = self.root.join(name.replace(':', "-"));
        let path = write_layout(&dir, platforms, tars, cfg);
        convert_local(&self.store, &path, &req(platforms, name), opts)
    }

    fn base(&self, name: &str, b: &Base) -> Output {
        self.convert(name, &b.layers(), b.config(), &ConvertOptions::default())
    }

    fn on_base(&self, base: &str) -> ConvertOptions {
        ConvertOptions {
            base: Some(BaseImage {
                name: base.into(),
                image: load(&self.store, &resolve_name(&self.store, base).unwrap()).unwrap(),
            }),
            ..Default::default()
        }
    }

    /// An app on `base` (built from `b`) with `extra` app layers.
    fn app(&self, name: &str, base: &str, b: &Base, extra: &[Vec<u8>]) -> kiln_image::Result<Output> {
        let mut tars = b.layers();
        tars.extend_from_slice(extra);
        let mut cfg = b.config();
        cfg.env.as_mut().unwrap().push("APP_ENV=production".into());
        cfg.labels.as_mut().unwrap().insert("app".into(), "demo".into());
        self.convert_for(name, &[arm()], &tars, cfg, &self.on_base(base))
    }

    fn rebase(&self, old: &str, new: &str, image: &str, tag: &str) -> kiln_image::Result<kiln_image::Rebased> {
        let req = RebaseRequest {
            old,
            new,
            image,
            tag: Some(tag),
        };
        rebase(&self.store, &req, &ConvertOptions::default())
    }

    fn manifest(&self, name: &str) -> kiln_image::KilnManifest {
        let loaded = load(&self.store, &resolve_name(&self.store, name).unwrap()).unwrap();
        loaded.entries[0].1.clone()
    }

    fn layers(&self, name: &str) -> Vec<Digest> {
        self.manifest(name)
            .manifest
            .layers
            .iter()
            .map(|l| l.digest.clone())
            .collect()
    }
}

fn refusals(r: kiln_image::Result<kiln_image::Rebased>) -> Vec<Refusal> {
    match r {
        Err(ImageError::RebaseRefused(rs)) => rs,
        Err(e) => panic!("expected a refusal, got {e}"),
        Ok(_) => panic!("expected a refusal, got a rebased image"),
    }
}

fn only(rs: &[Refusal], gate: &str) -> String {
    assert_eq!(rs.len(), 1, "{rs:?}");
    assert_eq!(rs[0].gate, gate, "{rs:?}");
    rs[0].reason.clone()
}

#[test]
fn convert_with_base_records_the_boundary() {
    let f = Fixture::new();
    f.base("base:1", &Base::old());
    let out = f
        .app("app:1", "base:1", &Base::old(), &[app_code(), app_bin()])
        .unwrap();
    let m = f.manifest("app:1");
    let record = m.config.base.as_ref().expect("base recorded");
    assert_eq!(
        (record.name.as_str(), record.layers, record.source_layers),
        ("base:1", 2, 2)
    );
    assert_eq!(record.manifest_digest, f.manifest("base:1").digest);
    assert_eq!(f.layers("app:1")[..2], f.layers("base:1")[..]);
    assert_eq!(out.images[0].layers.len(), 4);
    assert_eq!(m.config.labels["php.version"], "8.4.12");
    assert_eq!(m.config.labels["app"], "demo");
}

#[test]
fn a_squashed_base_is_matched_with_and_without_its_hint() {
    let f = Fixture::new();
    let mut tars = Base::old().layers();
    tars.push(TarBuilder::new().file("usr/share/doc", b"d", &Opts::default()).finish());
    let squash2 = ConvertOptions {
        max_layers: 2,
        ..Default::default()
    };
    f.convert("base:1", &tars, Base::old().config(), &squash2);
    assert_eq!(f.layers("base:1").len(), 2, "bottom two of three squashed");

    let mut app_tars = tars.clone();
    app_tars.push(app_code());
    let path = write_layout(&f.root.join("app"), &[arm()], &app_tars, Base::old().config());
    let mut opts = f.on_base("base:1");
    convert_local(&f.store, &path, &req(&[arm()], "app:1"), &opts).unwrap();
    let r = f.manifest("app:1").config.base.unwrap();
    assert_eq!((r.layers, r.source_layers), (2, 3));
    assert_eq!(f.layers("app:1")[..2], f.layers("base:1")[..]);

    // The source annotation is only a hint: without it every candidate is tried.
    let b = opts.base.as_mut().unwrap();
    b.image.entries[0].1.manifest.layers[0].annotations = None;
    convert_local(&f.store, &path, &req(&[arm()], "app:2"), &opts).unwrap();
    assert_eq!(f.layers("app:2"), f.layers("app:1"));
}

#[test]
fn app_layers_squash_above_the_boundary_only() {
    let f = Fixture::new();
    f.base("base:1", &Base::old());
    let extra: Vec<Vec<u8>> = (0..4)
        .map(|i| {
            TarBuilder::new()
                .file(&format!("srv/f{i}"), b"x", &Opts::default())
                .whiteout(if i == 0 { "usr/sbin/nginx" } else { "nothing" })
                .finish()
        })
        .collect();
    let mut tars = Base::old().layers();
    tars.extend(extra);
    let path = write_layout(&f.root.join("app"), &[arm()], &tars, Base::old().config());
    let opts = ConvertOptions {
        max_layers: 4,
        ..f.on_base("base:1")
    };
    let out = convert_local(&f.store, &path, &req(&[arm()], "app:1"), &opts).unwrap();
    assert_eq!(out.images[0].squashed, 3, "four app layers into a budget of two");
    let layers = f.layers("app:1");
    assert_eq!(layers.len(), 4);
    assert_eq!(layers[..2], f.layers("base:1")[..], "the base half is untouched");
    // The upper squash keeps the whiteout that hides the base's nginx.
    let mut img = kiln_erofs::Image::open(f.store.open_blob(&layers[2]).unwrap()).unwrap();
    let nid = img.lookup(b"usr/sbin/nginx").unwrap().expect("whiteout kept");
    assert!(img.inode(nid).unwrap().is_whiteout());
}

#[test]
fn convert_with_base_refuses_images_not_built_on_it() {
    let f = Fixture::new();
    f.base("base:1", &Base::old());
    let e = f.app("app:1", "base:1", &Base::patched(), &[app_code()]).unwrap_err();
    assert!(matches!(e, ImageError::NotOnBase(..)), "{e}");
    let e = f.app("app:2", "base:1", &Base::old(), &[]).unwrap_err();
    assert!(e.to_string().contains("no layers above the base"), "{e}");
    let mut tars = Base::old().layers();
    tars.push(app_code());
    let path = write_layout(&f.root.join("app3"), &[arm()], &tars, Base::old().config());
    let opts = ConvertOptions {
        max_layers: 2,
        ..f.on_base("base:1")
    };
    let e = convert_local(&f.store, &path, &req(&[arm()], "app:3"), &opts).unwrap_err();
    assert!(e.to_string().contains("--max-layers must be more than 2"), "{e}");
}

#[test]
fn rebase_swaps_the_base_and_refinalises_inheriting_layers() {
    let f = Fixture::new();
    f.base("base:1", &Base::old());
    f.base("base:2", &Base::patched());
    f.app("app:1", "base:1", &Base::old(), &[app_code(), app_bin()])
        .unwrap();
    let before = f.layers("app:1");
    assert_eq!(mode_of(&f.store, &before[2], b"var"), 0o755);

    let out = f.rebase("base:1", "base:2", "app:1", "app:2").unwrap();
    let r = &out.images[0];
    assert_eq!(r.base_layers, 2);
    assert_eq!(r.reresolved, 1);
    assert!(r.app[0].changed(), "var/www inherits /var from the base");
    assert!(
        !r.app[1].changed(),
        "a layer without implicit parents is reused byte for byte"
    );
    let after = f.layers("app:2");
    assert_eq!(after[..2], f.layers("base:2")[..]);
    assert_eq!(after[3], before[3]);
    assert_eq!(mode_of(&f.store, &after[2], b"var"), 0o750);

    let m = f.manifest("app:2");
    let record = m.config.base.as_ref().unwrap();
    assert_eq!(
        (record.name.as_str(), &record.manifest_digest),
        ("base:2", &f.manifest("base:2").digest)
    );
    assert!(m.config.process.env.contains(&"PHP_VERSION=8.4.14".to_string()));
    assert!(m.config.process.env.contains(&"APP_ENV=production".to_string()));
    assert_eq!(m.config.labels["php.version"], "8.4.14");
    assert_eq!(m.config.labels["app"], "demo");
    assert_eq!(m.config.source, f.manifest("app:1").config.source);

    // Deterministic, and reversible.
    let again = f.rebase("base:1", "base:2", "app:1", "app:3").unwrap();
    assert_eq!(again.digest, out.digest);
    let back = f.rebase("base:2", "base:1", "app:2", "app:4").unwrap();
    assert_eq!(f.layers("app:4"), before);
    assert_eq!(back.digest, f.manifest("app:1").digest, "the same image again");
}

#[test]
fn refuses_a_changed_platform() {
    let f = Fixture::new();
    f.base("base:1", &Base::old());
    let amd = Platform::parse("linux/amd64").unwrap();
    f.convert_for(
        "base:2",
        &[amd],
        &Base::patched().layers(),
        Base::patched().config(),
        &ConvertOptions::default(),
    )
    .unwrap();
    f.app("app:1", "base:1", &Base::old(), &[app_code()]).unwrap();
    let reason = only(&refusals(f.rebase("base:1", "base:2", "app:1", "x")), "platform");
    assert!(reason.contains("new base has no linux/arm64"), "{reason}");
}

#[test]
fn refuses_without_a_record_or_on_the_wrong_old_base() {
    let f = Fixture::new();
    f.base("base:1", &Base::old());
    f.base("base:2", &Base::patched());
    let mut tars = Base::old().layers();
    tars.push(app_code());
    f.convert("plain:1", &tars, Base::old().config(), &ConvertOptions::default());
    let reason = only(&refusals(f.rebase("base:1", "base:2", "plain:1", "x")), "bottom");
    assert!(reason.contains("records no base"), "{reason}");

    f.app("app:1", "base:1", &Base::old(), &[app_code()]).unwrap();
    let reason = only(&refusals(f.rebase("base:2", "base:2", "app:1", "x")), "bottom");
    assert!(reason.contains("not the old base's"), "{reason}");
    assert!(f.store.get_ref("x").unwrap().is_none(), "nothing tagged");
}

#[test]
fn refuses_a_distro_upgrade() {
    let f = Fixture::new();
    f.base("base:1", &Base::old());
    let trixie = Base {
        os_release: "ID=debian\nVERSION_ID=\"13\"\nVERSION_CODENAME=trixie\n",
        ..Base::patched()
    };
    f.base("base:2", &trixie);
    f.app("app:1", "base:1", &Base::old(), &[app_code()]).unwrap();
    let reason = only(&refusals(f.rebase("base:1", "base:2", "app:1", "x")), "distro");
    assert!(reason.contains("bookworm") && reason.contains("trixie"), "{reason}");
}

#[test]
fn refuses_a_runtime_minor_upgrade() {
    let f = Fixture::new();
    f.base("base:1", &Base::old());
    f.base(
        "base:2",
        &Base {
            php: "8.5.0",
            ..Base::patched()
        },
    );
    f.app("app:1", "base:1", &Base::old(), &[app_code()]).unwrap();
    let reason = only(&refusals(f.rebase("base:1", "base:2", "app:1", "x")), "runtime");
    assert!(reason.contains("php.version changes from 8.4.12 to 8.5.0"), "{reason}");
}

#[test]
fn refuses_a_dropped_env_var() {
    let f = Fixture::new();
    f.base("base:1", &Base::old());
    f.base(
        "base:2",
        &Base {
            env: vec!["PATH=/usr/local/bin:/usr/bin:/bin"],
            ..Base::patched()
        },
    );
    f.app("app:1", "base:1", &Base::old(), &[app_code()]).unwrap();
    let reason = only(&refusals(f.rebase("base:1", "base:2", "app:1", "x")), "env");
    assert!(reason.contains("PHP_FPM_REQUEST_TERMINATE_TIMEOUT"), "{reason}");
}

#[test]
fn refuses_a_changed_start_contract() {
    let f = Fixture::new();
    f.base("base:1", &Base::old());
    f.base(
        "base:2",
        &Base {
            cmd: "php-fpm8.4",
            ..Base::patched()
        },
    );
    f.app("app:1", "base:1", &Base::old(), &[app_code()]).unwrap();
    let reason = only(&refusals(f.rebase("base:1", "base:2", "app:1", "x")), "process");
    assert!(reason.starts_with("cmd changes"), "{reason}");
}

/// The case that motivated the gate: the app copied the base's php-fpm config up,
/// and the new base changed it.
#[test]
fn refuses_when_a_copied_up_file_changed_in_the_new_base() {
    let f = Fixture::new();
    f.base("base:1", &Base::old());
    f.base(
        "base:2",
        &Base {
            fpm_conf: b"request_terminate_timeout = ${PHP_FPM_REQUEST_TERMINATE_TIMEOUT:-60}\n",
            ..Base::patched()
        },
    );
    f.app("app:1", "base:1", &Base::old(), &[app_code(), copied_up_fpm()])
        .unwrap();
    let reason = only(&refusals(f.rebase("base:1", "base:2", "app:1", "x")), "shadow");
    assert!(reason.contains(&format!("/{FPM}: content differs")), "{reason}");
    assert!(!reason.contains("usr/sbin/nginx"), "nginx is not shadowed: {reason}");
    // The same copy over an unchanged config rebases fine.
    f.base("base:3", &Base::patched());
    f.rebase("base:1", "base:3", "app:1", "x").unwrap();
}

#[test]
fn reports_every_failing_gate_at_once() {
    let f = Fixture::new();
    f.base("base:1", &Base::old());
    f.base(
        "base:2",
        &Base {
            php: "8.5.0",
            cmd: "frankenphp",
            fpm_conf: b"changed\n",
            ..Base::patched()
        },
    );
    f.app("app:1", "base:1", &Base::old(), &[copied_up_fpm()]).unwrap();
    let gates: Vec<&str> = refusals(f.rebase("base:1", "base:2", "app:1", "x"))
        .iter()
        .map(|r| r.gate)
        .collect();
    assert_eq!(gates, ["runtime", "process", "shadow"]);
}

#[test]
fn a_missing_source_tar_is_named() {
    let f = Fixture::new();
    f.base("base:1", &Base::old());
    f.base("base:2", &Base::patched());
    f.app("app:1", "base:1", &Base::old(), &[app_code()]).unwrap();
    let tar = Digest::of(&app_code());
    std::fs::remove_file(f.store.blob_path(&tar)).unwrap();
    let e = f.rebase("base:1", "base:2", "app:1", "x").unwrap_err();
    assert!(matches!(e, ImageError::SourceMissing(_)), "{e}");
    assert!(
        e.to_string().contains(&tar.to_string()) && e.to_string().contains("re-convert"),
        "{e}"
    );
}
