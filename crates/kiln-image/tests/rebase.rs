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
use kiln_oci::{ContainerConfig, ImageConfig, ImageManifest, Platform, canonical_json};
use kiln_store::{Digest, Store};

const FPM: &str = "usr/local/etc/php-fpm.d/www.conf";

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
            fpm_conf: b"request_terminate_timeout = ${FPM_REQUEST_TERMINATE_TIMEOUT_SEC}\n",
            os_release: "ID=debian\nVERSION_ID=\"12\"\nVERSION_CODENAME=bookworm\n",
            php: "8.4.12",
            env: vec![
                "PATH=/usr/local/bin:/usr/bin:/bin",
                "FPM_REQUEST_TERMINATE_TIMEOUT_SEC=60",
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
                ("vendor".into(), "acme".into()),
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
            max_layers: None,
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
    assert_eq!(m.config.labels.as_ref().unwrap()["php.version"], "8.4.12");
    assert_eq!(m.config.labels.as_ref().unwrap()["app"], "demo");
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
}

/// A base that fills `--max-layers` leaves the app one layer instead of refusing.
#[test]
fn a_base_that_fills_max_layers_leaves_one_app_layer() {
    let f = Fixture::new();
    f.base("base:1", &Base::old());
    let mut tars = Base::old().layers();
    tars.extend([app_code(), app_bin()]);
    let path = write_layout(&f.root.join("app"), &[arm()], &tars, Base::old().config());
    let opts = ConvertOptions {
        max_layers: 2,
        ..f.on_base("base:1")
    };
    let out = convert_local(&f.store, &path, &req(&[arm()], "app:1"), &opts).unwrap();
    assert_eq!(out.images[0].layers.len(), 3);
    assert_eq!(out.images[0].squashed, 2);
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
    assert_eq!(m.config.labels.as_ref().unwrap()["php.version"], "8.4.14");
    assert_eq!(m.config.labels.as_ref().unwrap()["app"], "demo");
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
    assert!(reason.contains("FPM_REQUEST_TERMINATE_TIMEOUT_SEC"), "{reason}");
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
            fpm_conf: b"request_terminate_timeout = ${FPM_REQUEST_TERMINATE_TIMEOUT_SEC:-60}\n",
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

/// Images with a kernel layer: the boundary and the gates look at app layers only,
/// and the rebased image keeps its own kernel layer (spec §5.1, §16.5).
#[test]
fn boot_layers_are_not_base_layers() {
    use kiln_image::artifacts::{ArtifactKind, Pin};
    use kiln_image::types::{KILN_KERNEL, KILN_LAYER};
    use kiln_image::{Boot, BootLayers};

    let f = Fixture::new();
    // Already in the store, so nothing is fetched.
    let kernel = f.store.put_bytes(b"a kernel Image").unwrap();
    let pin = Pin {
        kind: ArtifactKind::Kernel,
        name: "base",
        version: "6.18.54",
        arch: "arm64",
        repository: "example.invalid/kernels/base",
        digest: Box::leak(kernel.to_string().into_boxed_str()),
        size: 14,
    };
    let boot = BootLayers::Explicit(Boot {
        kernel: Some(pin),
        init: None,
    });
    let with_kernel = ConvertOptions {
        boot: boot.clone(),
        ..Default::default()
    };
    // The old base has a kernel layer, the new one does not: neither counts.
    f.convert("base:1", &Base::old().layers(), Base::old().config(), &with_kernel);
    f.base("base:2", &Base::patched());
    let opts = ConvertOptions {
        boot,
        ..f.on_base("base:1")
    };
    let mut tars = Base::old().layers();
    tars.push(app_code());
    let path = write_layout(&f.root.join("app"), &[arm()], &tars, Base::old().config());
    convert_local(&f.store, &path, &req(&[arm()], "app:1"), &opts).unwrap();
    assert_eq!(f.manifest("app:1").config.base.unwrap().layers, 2);

    f.rebase("base:1", "base:2", "app:1", "app:2").unwrap();
    let m = f.manifest("app:2");
    let types: Vec<&str> = m.manifest.layers.iter().map(|l| l.media_type.as_str()).collect();
    assert_eq!(types, [KILN_KERNEL, KILN_LAYER, KILN_LAYER, KILN_LAYER]);
    assert_eq!(m.manifest.layers[0].digest, kernel);
    assert_eq!(
        m.manifest.layers[1..3]
            .iter()
            .map(|l| l.digest.clone())
            .collect::<Vec<_>>(),
        f.layers("base:2")
    );
    assert!(m.config.kernel.is_some(), "the image's own kernel stays");
}

/// Rewrites a tagged image's kiln config, as an older kiln or a hostile store might
/// have written it.
fn rewrite_config(f: &Fixture, name: &str, edit: impl FnOnce(&mut kiln_image::types::KilnConfig)) {
    let km = f.manifest(name);
    let mut config = km.config.clone();
    edit(&mut config);
    let bytes = canonical_json(&config);
    let mut manifest = km.manifest.clone();
    manifest.config.digest = f.store.put_bytes(&bytes).unwrap();
    manifest.config.size = bytes.len() as u64;
    let d = f.store.put_bytes(&canonical_json(&manifest)).unwrap();
    f.store.set_ref(name, &d).unwrap();
}

fn source_of(f: &Fixture, name: &str) -> (ImageManifest, ImageConfig) {
    let d = f.manifest(name).config.source.manifest_digest;
    let m: ImageManifest = serde_json::from_slice(&f.store.read_metadata(&d).unwrap()).unwrap();
    let c: ImageConfig = serde_json::from_slice(&f.store.read_metadata(&m.config.digest).unwrap()).unwrap();
    (m, c)
}

/// Stores a source manifest and config; returns the manifest's digest.
fn put_source(f: &Fixture, mut m: ImageManifest, c: &ImageConfig) -> Digest {
    let bytes = canonical_json(c);
    m.config.digest = f.store.put_bytes(&bytes).unwrap();
    m.config.size = bytes.len() as u64;
    f.store.put_bytes(&canonical_json(&m)).unwrap()
}

/// A base converted before kiln recorded labels: its runtime labels come from its
/// source config, and when that is gone too the rebase is refused, not waved through.
#[test]
fn labels_of_an_older_base_come_from_its_source_or_refuse() {
    let f = Fixture::new();
    f.base("base:1", &Base::old());
    rewrite_config(&f, "base:1", |c| c.labels = None);
    f.base(
        "base:2",
        &Base {
            php: "8.5.0",
            ..Base::patched()
        },
    );
    f.base("base:3", &Base::patched());
    f.app("app:1", "base:1", &Base::old(), &[app_code()]).unwrap();
    let reason = only(&refusals(f.rebase("base:1", "base:2", "app:1", "x")), "runtime");
    assert!(reason.contains("from 8.4.12 to 8.5.0"), "{reason}");
    f.rebase("base:1", "base:3", "app:1", "app:3").unwrap();
    assert_eq!(
        f.manifest("app:3").config.labels.as_ref().unwrap()["php.version"],
        "8.4.14",
        "labels follow the new base"
    );

    let src = f.manifest("base:1").config.source.manifest_digest;
    std::fs::remove_file(f.store.blob_path(&src)).unwrap();
    let reason = only(&refusals(f.rebase("base:1", "base:3", "app:1", "x")), "runtime");
    assert!(reason.contains("old base's labels are unknown"), "{reason}");
}

/// The recorded sources must rebuild the image's own app layers on the old base.
#[test]
fn a_source_that_does_not_rebuild_the_image_is_refused() {
    let f = Fixture::new();
    f.base("base:1", &Base::old());
    f.base("base:2", &Base::patched());
    f.app("app:1", "base:1", &Base::old(), &[app_code()]).unwrap();
    let (mut m, mut c) = source_of(&f, "app:1");
    let other = app_bin();
    m.layers[2] = kiln_oci::Descriptor::new(
        &m.layers[2].media_type,
        f.store.put_bytes(&other).unwrap(),
        other.len() as u64,
    );
    c.rootfs.diff_ids[2] = Digest::of(&other);
    let fake = put_source(&f, m.clone(), &c);
    rewrite_config(&f, "app:1", |k| k.source.manifest_digest = fake);
    let reason = only(&refusals(f.rebase("base:1", "base:2", "app:1", "x")), "source");
    assert!(reason.contains("do not rebuild its app layers"), "{reason}");

    // A descriptor whose size understates its blob fails as any input would (§6.1),
    // so the expansion-ratio limit cannot be dodged.
    let bigger = TarBuilder::new()
        .file("var/www/big", &vec![0u8; 1 << 20], &Opts::default())
        .finish();
    m.layers[2] = kiln_oci::Descriptor::new(&m.layers[2].media_type, f.store.put_bytes(&bigger).unwrap(), 1024);
    c.rootfs.diff_ids[2] = Digest::of(&bigger);
    let lying = put_source(&f, m, &c);
    rewrite_config(&f, "app:1", |k| k.source.manifest_digest = lying);
    let e = f.rebase("base:1", "base:2", "app:1", "x").unwrap_err();
    assert!(e.to_string().contains("size"), "{e}");
}

/// gc keeps what rebase needs: the source manifest and config, and the tars of
/// layers that inherit from the base.
#[test]
fn rebase_still_works_after_gc() {
    let f = Fixture::new();
    f.base("base:1", &Base::old());
    f.base("base:2", &Base::patched());
    f.app("app:1", "base:1", &Base::old(), &[app_code(), app_bin()])
        .unwrap();
    let removed = f.store.gc().unwrap().blobs_removed;
    assert!(removed > 0, "the bases' tars go");
    // The app half's tars stay, the base half's go, even one that inherits.
    assert!(f.store.has_blob(&Digest::of(&app_bin())) && f.store.has_blob(&Digest::of(&app_code())));
    let base_half = Base::old().layers();
    assert!(
        !f.store.has_blob(&Digest::of(&base_half[1])),
        "it has implicit parents, but is never re-derived"
    );
    let out = f.rebase("base:1", "base:2", "app:1", "app:2").unwrap();
    assert!(out.images[0].app[0].changed() && !out.images[0].app[1].changed());
}

/// Without its sources (a pulled image), an image whose app layers inherit nothing
/// is rebased with those layers as they are; one that inherits cannot be.
#[test]
fn without_sources_only_non_inheriting_layers_are_kept() {
    let f = Fixture::new();
    f.base("base:1", &Base::old());
    f.base("base:2", &Base::patched());
    f.app("app:1", "base:1", &Base::old(), &[app_bin()]).unwrap();
    f.app("app:2", "base:1", &Base::old(), &[app_code()]).unwrap();
    for name in ["app:1", "app:2"] {
        let src = f.manifest(name).config.source.manifest_digest;
        std::fs::remove_file(f.store.blob_path(&src)).unwrap();
    }
    let out = f.rebase("base:1", "base:2", "app:1", "x:1").unwrap();
    assert!(!out.images[0].app[0].changed());
    let e = f.rebase("base:1", "base:2", "app:2", "x:2").unwrap_err();
    assert!(matches!(e, ImageError::SourceMissing(_)), "{e}");
}

fn with_layer(b: &Base, extra: Vec<u8>) -> Vec<Vec<u8>> {
    let mut tars = b.layers();
    tars.push(extra);
    tars
}

/// An implicit parent the new base turns into a symlink would mask it.
#[test]
fn refuses_when_an_implicit_parent_becomes_a_non_directory() {
    let f = Fixture::new();
    let old = with_layer(
        &Base::old(),
        TarBuilder::new().dir("var/run", &Opts::default()).finish(),
    );
    let new = with_layer(
        &Base::patched(),
        TarBuilder::new().symlink("var/run", "/run", &Opts::default()).finish(),
    );
    f.convert("base:1", &old, Base::old().config(), &ConvertOptions::default());
    f.convert("base:2", &new, Base::patched().config(), &ConvertOptions::default());
    let mut tars = old.clone();
    tars.push(
        TarBuilder::new()
            .file("var/run/app.pid", b"1", &Opts::default())
            .finish(),
    );
    f.convert_for("app:1", &[arm()], &tars, Base::old().config(), &f.on_base("base:1"))
        .unwrap();
    let reason = only(&refusals(f.rebase("base:1", "base:2", "app:1", "x")), "shadow");
    assert!(
        reason.contains("/var/run: the new base has a non-directory"),
        "{reason}"
    );
}

/// An opaque directory without its own header still hides the base's subtree.
#[test]
fn an_implicit_opaque_dir_still_compares_what_it_hides() {
    let f = Fixture::new();
    let conf = |extra: bool| {
        let mut t = TarBuilder::new();
        t.file("etc/conf.d/a", b"a", &Opts::default());
        if extra {
            t.file("etc/conf.d/b", b"b", &Opts::default());
        }
        t.finish()
    };
    let old = with_layer(&Base::old(), conf(false));
    let new = with_layer(&Base::patched(), conf(true));
    f.convert("base:1", &old, Base::old().config(), &ConvertOptions::default());
    f.convert("base:2", &new, Base::patched().config(), &ConvertOptions::default());
    let mut tars = old.clone();
    tars.push(
        TarBuilder::new()
            .opaque("etc/conf.d")
            .file("etc/conf.d/mine", b"m", &Opts::default())
            .finish(),
    );
    f.convert_for("app:1", &[arm()], &tars, Base::old().config(), &f.on_base("base:1"))
        .unwrap();
    let reason = only(&refusals(f.rebase("base:1", "base:2", "app:1", "x")), "shadow");
    assert!(reason.contains("/etc/conf.d/b: the new base adds it"), "{reason}");
}

fn squash_entries(f: &Fixture) -> usize {
    std::fs::read_dir(f.store.root().join("cache/squash")).unwrap().count()
}

/// A one-layer (squashed) base is matched by its hint alone: an image not on it
/// costs no squash of every prefix.
#[test]
fn a_one_layer_base_tries_only_plausible_squashes() {
    let f = Fixture::new();
    let mut base = Base::old().layers();
    base.push(TarBuilder::new().file("usr/share/doc", b"d", &Opts::default()).finish());
    let one = ConvertOptions {
        max_layers: 1,
        ..Default::default()
    };
    f.convert("base:1", &base, Base::old().config(), &one);
    assert_eq!(f.layers("base:1").len(), 1);
    let before = squash_entries(&f);
    let unrelated: Vec<Vec<u8>> = (0..12)
        .map(|i| {
            TarBuilder::new()
                .file(&format!("l{i}"), b"x", &Opts::default())
                .finish()
        })
        .collect();
    let e = f
        .convert_for(
            "app:1",
            &[arm()],
            &unrelated,
            Base::old().config(),
            &f.on_base("base:1"),
        )
        .unwrap_err();
    assert!(matches!(e, ImageError::NotOnBase(..)), "{e}");
    assert_eq!(squash_entries(&f), before, "no squash was kept");
    let mut tars = base.clone();
    tars.push(app_code());
    f.convert_for("app:2", &[arm()], &tars, Base::old().config(), &f.on_base("base:1"))
        .unwrap();
    assert_eq!(f.manifest("app:2").config.base.unwrap().source_layers, 3);
}

/// A base that is itself built on a base, with its app half squashed, is matched
/// layered as it was: bottom squash below, upper squash above (spec §16.2).
#[test]
fn a_chained_base_is_matched() {
    let f = Fixture::new();
    f.base("base:0", &Base::old());
    let mid: Vec<Vec<u8>> = (0..3)
        .map(|i| {
            TarBuilder::new()
                .file(&format!("opt/mid/{i}"), b"m", &Opts::default())
                .finish()
        })
        .collect();
    let mut mid_tars = Base::old().layers();
    mid_tars.extend(mid.clone());
    let three = ConvertOptions {
        max_layers: 3,
        ..f.on_base("base:0")
    };
    f.convert_for("mid:1", &[arm()], &mid_tars, Base::old().config(), &three)
        .unwrap();
    assert_eq!(f.layers("mid:1").len(), 3, "base 2 + the mid app squashed into 1");
    let mut tars = mid_tars.clone();
    tars.push(app_code());
    f.convert_for("app:1", &[arm()], &tars, Base::old().config(), &f.on_base("mid:1"))
        .unwrap();
    let r = f.manifest("app:1").config.base.unwrap();
    assert_eq!((r.layers, r.source_layers), (3, 5));
    assert_eq!(f.layers("app:1")[..3], f.layers("mid:1")[..]);
}

/// Rebase keeps the image's app layering and reports reuse by digest.
#[test]
fn rebase_keeps_the_image_app_layering() {
    let f = Fixture::new();
    f.base("base:1", &Base::old());
    f.base("base:2", &Base::patched());
    let extra = TarBuilder::new().file("srv/x", b"x", &Opts::default()).finish();
    let mut tars = Base::old().layers();
    tars.extend([app_code(), extra, app_bin()]);
    let mut cfg = Base::old().config();
    cfg.labels.as_mut().unwrap().insert("app".into(), "demo".into());
    let opts = ConvertOptions {
        max_layers: 4,
        ..f.on_base("base:1")
    };
    f.convert_for("app:1", &[arm()], &tars, cfg, &opts).unwrap();
    let before = f.layers("app:1");
    assert_eq!(before.len(), 4);
    let out = f.rebase("base:1", "base:2", "app:1", "app:2").unwrap();
    let app = &out.images[0].app;
    assert_eq!(app.len(), 2, "two app layers in, two out");
    assert!(app[0].changed(), "the squash holds var/www, which inherits /var");
    assert!(!app[1].changed() && app[1].erofs == before[3]);
    let req = RebaseRequest {
        old: "base:1",
        new: "base:2",
        image: "app:1",
        tag: Some("app:3"),
        max_layers: Some(3),
    };
    let out = rebase(&f.store, &req, &ConvertOptions::default()).unwrap();
    assert_eq!(out.images[0].app.len(), 1, "--max-layers squashes further");
}

/// A base whose recorded source manifest has fewer layers than the base itself, or
/// whose own record claims an absurd source count, suggests widths that do not
/// fit: they are dropped, never computed into a panic.
#[test]
fn hostile_source_counts_do_not_overflow() {
    let f = Fixture::new();
    f.base("base:1", &Base::old());
    let empty = f
        .store
        .put_bytes(&canonical_json(&serde_json::json!({
            "schemaVersion": 2,
            "config": {"mediaType": "x", "digest": Digest::of(b"c").to_string(), "size": 1},
            "layers": [],
        })))
        .unwrap();
    rewrite_config(&f, "base:1", |c| c.source.manifest_digest = empty.clone());
    let unrelated = vec![TarBuilder::new().file("x", b"x", &Opts::default()).finish(); 3];
    let e = f
        .convert_for(
            "app:1",
            &[arm()],
            &unrelated,
            Base::old().config(),
            &f.on_base("base:1"),
        )
        .unwrap_err();
    assert!(matches!(e, ImageError::NotOnBase(..)), "{e}");

    // A chained base: its app half is tried with the record's source count.
    f.base("base:0", &Base::old());
    let mut mid = Base::old().layers();
    mid.extend((0..3).map(|i| {
        TarBuilder::new()
            .file(&format!("opt/{i}"), b"m", &Opts::default())
            .finish()
    }));
    let three = ConvertOptions {
        max_layers: 3,
        ..f.on_base("base:0")
    };
    f.convert_for("mid:1", &[arm()], &mid, Base::old().config(), &three)
        .unwrap();
    rewrite_config(&f, "mid:1", |c| {
        c.source.manifest_digest = empty.clone();
        c.base.as_mut().unwrap().source_layers = usize::MAX;
    });
    let mut tars = Base::old().layers();
    tars.extend(unrelated);
    let e = f
        .convert_for("app:2", &[arm()], &tars, Base::old().config(), &f.on_base("mid:1"))
        .unwrap_err();
    assert!(matches!(e, ImageError::NotOnBase(..)), "{e}");
}

/// A squashed app half, then gc: rebase still re-derives it, whether or not the
/// squash holds a layer that inherits from the base.
#[test]
fn a_squashed_app_half_survives_gc() {
    let f = Fixture::new();
    f.base("base:1", &Base::old());
    f.base("base:2", &Base::patched());
    let bins: Vec<Vec<u8>> = (0..3)
        .map(|i| {
            TarBuilder::new()
                .dir("opt", &Opts::default().mode(0o755))
                .file(&format!("opt/bin{i}"), b"b", &Opts::default())
                .finish()
        })
        .collect();
    let three = ConvertOptions {
        max_layers: 3,
        ..f.on_base("base:1")
    };
    let mut plain = Base::old().layers();
    plain.extend(bins.clone());
    f.convert_for("plain:1", &[arm()], &plain, Base::old().config(), &three)
        .unwrap();
    let mut mixed = Base::old().layers();
    mixed.push(app_code());
    mixed.extend(bins);
    f.convert_for("mixed:1", &[arm()], &mixed, Base::old().config(), &three)
        .unwrap();
    f.store.gc().unwrap();
    let out = f.rebase("base:1", "base:2", "plain:1", "plain:2").unwrap();
    assert!(
        !out.images[0].app[0].changed(),
        "nothing inherits: the squash is the same"
    );
    let out = f.rebase("base:1", "base:2", "mixed:1", "mixed:2").unwrap();
    assert!(out.images[0].app[0].changed(), "var/www follows the new /var");
}

fn labelled(labels: &[(&str, &str)]) -> ContainerConfig {
    ContainerConfig {
        cmd: Some(vec!["sh".into()]),
        labels: Some(labels.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()),
        ..Default::default()
    }
}

/// Configs record labels even when there are none, so a label-less base (most
/// official images) is not "unknown" once its source is gone, and a label that a
/// rebase dropped does not come back from the app's source.
#[test]
fn empty_labels_are_recorded() {
    let f = Fixture::new();
    let b = Base::old().layers();
    f.convert("b:1", &b, labelled(&[("vendor", "acme")]), &ConvertOptions::default());
    f.convert("b:2", &b, labelled(&[]), &ConvertOptions::default());
    f.convert("b:3", &b, labelled(&[]), &ConvertOptions::default());
    assert_eq!(f.manifest("b:2").config.labels, Some(BTreeMap::new()));
    let mut tars = b.clone();
    tars.push(app_code());
    f.convert_for(
        "app:1",
        &[arm()],
        &tars,
        labelled(&[("vendor", "acme")]),
        &f.on_base("b:1"),
    )
    .unwrap();
    // As if pulled: no source config to fall back on (b:2 and b:3 share one).
    let src = f.manifest("b:2").config.source.manifest_digest;
    std::fs::remove_file(f.store.blob_path(&src)).unwrap();
    f.rebase("b:1", "b:2", "app:1", "app:2").unwrap();
    assert_eq!(f.manifest("app:2").config.labels, Some(BTreeMap::new()));
    f.rebase("b:2", "b:3", "app:2", "app:3").unwrap();
    assert_eq!(
        f.manifest("app:3").config.labels,
        Some(BTreeMap::new()),
        "vendor stays dropped"
    );
}

/// gc keeps a chained base's own base, so matching survives its tag moving; and a
/// base whose own base is gone says so.
#[test]
fn a_chained_base_survives_gc_and_names_a_missing_inner_base() {
    let f = Fixture::new();
    f.base("a:1", &Base::old());
    let mut b = Base::old().layers();
    b.extend((0..3).map(|i| {
        TarBuilder::new()
            .file(&format!("opt/b{i}"), b"b", &Opts::default())
            .finish()
    }));
    let three = ConvertOptions {
        max_layers: 3,
        ..f.on_base("a:1")
    };
    f.convert_for("b:1", &[arm()], &b, Base::old().config(), &three)
        .unwrap();
    let inner = f.manifest("a:1").digest;
    f.base("a:1", &Base::patched());
    f.store.gc().unwrap();
    assert!(f.store.has_blob(&inner), "b:1's base is kept");
    let mut c = b.clone();
    c.push(app_code());
    f.convert_for("c:1", &[arm()], &c, Base::old().config(), &f.on_base("b:1"))
        .unwrap();

    std::fs::remove_file(f.store.blob_path(&inner)).unwrap();
    let e = f
        .convert_for("c:2", &[arm()], &c, Base::old().config(), &f.on_base("b:1"))
        .unwrap_err();
    assert!(matches!(e, ImageError::BaseMissing { .. }), "{e}");
    assert!(e.to_string().contains(&inner.to_string()), "{e}");
}

/// gc after a hostile image names another image's manifest as its source: the
/// other image keeps every blob.
#[test]
fn a_hostile_source_pointer_cannot_make_gc_delete_another_image() {
    let f = Fixture::new();
    f.base("base:1", &Base::old());
    f.base("base:2", &Base::patched());
    f.app("app:1", "base:1", &Base::old(), &[app_code()]).unwrap();
    let victim = f.manifest("app:1");
    f.store.set_ref("zz-evil:1", &f.manifest("base:2").digest).unwrap();
    rewrite_config(&f, "zz-evil:1", |c| c.source.manifest_digest = victim.digest.clone());
    f.store.gc().unwrap();
    for d in std::iter::once(&victim.manifest.config.digest).chain(victim.manifest.layers.iter().map(|l| &l.digest)) {
        assert!(f.store.has_blob(d), "{d} of app:1 was deleted");
    }
}

/// A base that records its own base but matches layer for layer needs nothing
/// else, so one pulled without its inner base still works with `--base`.
#[test]
fn a_chained_base_matching_exactly_needs_no_inner_base() {
    let f = Fixture::new();
    f.base("b1:1", &Base::old());
    let mut b2 = Base::old().layers();
    b2.push(TarBuilder::new().file("opt/b2", b"b", &Opts::default()).finish());
    f.convert_for("b2:1", &[arm()], &b2, Base::old().config(), &f.on_base("b1:1"))
        .unwrap();
    let inner = f.manifest("b1:1").digest;
    std::fs::remove_file(f.store.blob_path(&inner)).unwrap();
    let mut tars = b2.clone();
    tars.push(app_code());
    f.convert_for("app:1", &[arm()], &tars, Base::old().config(), &f.on_base("b2:1"))
        .unwrap();
    assert_eq!(f.manifest("app:1").config.base.unwrap().layers, 3);
}
