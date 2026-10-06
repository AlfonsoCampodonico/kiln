//! Choosing what boots (spec §8.1, T4, §11.5 "kernel and init swap"), without a VM.
//! This kiln pins no kernel or kiln-init yet, so every kernel and init is the user's.

use std::path::PathBuf;

use kiln_erofs::testtar::{Opts, TarBuilder};
use kiln_image::types::{InitRef, KILN_CONFIG, KILN_INIT, KILN_KERNEL, KernelRef};
use kiln_image::{ConvertOptions, LocalRequest, convert_local, load, resolve_name};
use kiln_oci::testlayout::{LayoutBuilder, TestLayer};
use kiln_oci::{Descriptor, Platform};
use kiln_run::RunOptions;
use kiln_run::prepare::prepare;
use kiln_store::Store;

const ARCH: &str = "arm64";

struct Setup {
    _dirs: Vec<tempfile::TempDir>,
    store: Store,
    files: PathBuf,
}

/// A store with `app`, a provisional arm64 image (no boot layers, as convert makes
/// them while kiln pins no kernel), and a kernel and an init binary in `files`.
fn setup() -> Setup {
    let (src, home, files) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    let layout = src.path().join("layout");
    let mut b = LayoutBuilder::new(&layout);
    let tar = TarBuilder::new().file("hello", b"hi", &Opts::default()).finish();
    let desc = b.image(
        &Platform::parse("linux/arm64").unwrap(),
        &[TestLayer::tar(tar)],
        Default::default(),
    );
    b.add(desc, Some("app"));
    b.finish();
    let store = Store::open(home.path()).unwrap();
    let platforms = [Platform::parse("linux/arm64").unwrap()];
    let req = LocalRequest {
        source_ref: None,
        platforms: &platforms,
        tag: Some("app"),
    };
    convert_local(&store, &layout, &req, &ConvertOptions::default()).unwrap();
    std::fs::write(files.path().join("kernel"), b"kernel Image").unwrap();
    std::fs::write(files.path().join("init"), b"\x7fELF my own init").unwrap();
    Setup {
        files: files.path().to_path_buf(),
        _dirs: vec![src, home, files],
        store,
    }
}

impl Setup {
    /// `app` with boot layers, tagged `tag`: a kernel layer naming `profile` and an
    /// init layer around `init` (convert adds such layers once kiln pins a kernel).
    fn with_boot_layers(&self, tag: &str, profile: &str, init: &[u8]) {
        let loaded = load(&self.store, &resolve_name(&self.store, "app").unwrap()).unwrap();
        let image = &loaded.entries[0].1;
        let mut config = image.config.clone();
        config.kernel = Some(KernelRef {
            profile: profile.into(),
            version: "6.18.54".into(),
        });
        config.init = Some(InitRef {
            version: "0.0.1".into(),
        });
        let config = serde_json::to_vec(&config).unwrap();
        let mut manifest = image.manifest.clone();
        manifest.config = Descriptor::new(KILN_CONFIG, self.store.put_bytes(&config).unwrap(), config.len() as u64);
        let kernel = b"an image's kernel";
        let init_layer = kiln_image::init_layer(init, &self.store.tmp_dir()).unwrap();
        manifest.layers.splice(
            0..0,
            [
                Descriptor::new(KILN_KERNEL, self.store.put_bytes(kernel).unwrap(), kernel.len() as u64),
                Descriptor::new(
                    KILN_INIT,
                    self.store.put_bytes(&init_layer).unwrap(),
                    init_layer.len() as u64,
                ),
            ],
        );
        let manifest = self.store.put_bytes(&serde_json::to_vec(&manifest).unwrap()).unwrap();
        self.store.set_ref(tag, &manifest).unwrap();
    }

    fn custom(&self, o: &mut RunOptions) {
        o.kernel = Some(self.files.join("kernel"));
        o.allow_custom_kernel = true;
        o.init = Some(self.files.join("init"));
        o.allow_custom_init = true;
    }
}

fn err(r: kiln_run::Result<kiln_run::prepare::Prepared>) -> String {
    r.map(|_| ()).unwrap_err().to_string()
}

#[test]
fn a_provisional_image_needs_a_custom_kernel_and_init() {
    let s = setup();
    let mut o = RunOptions::new("app");
    let e = err(prepare(&s.store, &o, ARCH));
    assert!(
        e.contains("has no kernel layer") && e.contains("--allow-custom-kernel"),
        "{e}"
    );
    o.kernel = Some(s.files.join("kernel"));
    o.allow_custom_kernel = true;
    let e = err(prepare(&s.store, &o, ARCH));
    assert!(
        e.contains("no pinned kiln-init for arm64") && e.contains("--init PATH --allow-custom-init"),
        "{e}"
    );
    s.custom(&mut o);
    let p = prepare(&s.store, &o, ARCH).unwrap();
    let kernel = std::fs::canonicalize(s.files.join("kernel")).unwrap();
    assert_eq!((&p.kernel, p.custom_kernel.as_ref()), (&kernel, Some(&kernel)));
    assert_eq!(p.custom_init, Some(s.files.join("init")));
    assert!(!p.init_layer.is_empty());
    assert_eq!(p.layers.len(), 1);
    assert!(p.layer_paths[0].exists());
    assert_eq!(p.warnings.len(), 2, "{:?}", p.warnings);
    // Another architecture is refused by name.
    let e = err(prepare(&s.store, &o, "amd64"));
    assert!(e.contains("has no amd64 image (it has arm64)"), "{e}");
    // A missing --init binary is named.
    o.init = Some(s.files.join("missing"));
    assert!(err(prepare(&s.store, &o, ARCH)).contains("--init"));
}

/// T4 without pins: an image's kernel that names a pinned profile cannot be
/// verified and is refused; `--kernel` replaces it.
#[test]
fn an_image_kernel_is_refused_until_kiln_pins_kernels() {
    let s = setup();
    s.with_boot_layers("booted", "base", b"\x7fELF the image's init");
    let mut o = RunOptions::new("booted");
    o.init = Some(s.files.join("init"));
    o.allow_custom_init = true;
    let e = err(prepare(&s.store, &o, ARCH));
    assert!(
        e.contains("kernel base 6.18.54 for arm64") && e.contains("cannot verify") && e.contains("--kernel PATH"),
        "{e}"
    );
    s.custom(&mut o);
    let p = prepare(&s.store, &o, ARCH).unwrap();
    assert_eq!(p.kernel, std::fs::canonicalize(s.files.join("kernel")).unwrap());
    // The kernel and init layers are not app layers.
    assert_eq!(p.layers.len(), 1);
}

/// Spec §8.1: an image whose kernel is `custom` boots only with `--allow-custom-kernel`.
#[test]
fn a_custom_kernel_image_needs_the_flag() {
    let s = setup();
    s.with_boot_layers("custom", "custom", b"\x7fELF the image's init");
    let mut o = RunOptions::new("custom");
    o.init = Some(s.files.join("init"));
    o.allow_custom_init = true;
    let e = err(prepare(&s.store, &o, ARCH));
    assert!(
        e.contains("boots a custom kernel") && e.contains("--allow-custom-kernel"),
        "{e}"
    );
    o.allow_custom_kernel = true;
    let p = prepare(&s.store, &o, ARCH).unwrap();
    assert_eq!(std::fs::read(&p.kernel).unwrap(), b"an image's kernel");
    assert_eq!(p.custom_kernel.as_ref(), Some(&p.kernel));
    assert!(p.warnings[0].contains("custom kernel"), "{:?}", p.warnings);
}

/// Spec §8.1, §11.5: the image's init layer never boots; a different one is
/// replaced with a warning, the same one silently.
#[test]
fn the_init_layer_is_always_replaced_with_a_warning_when_it_differs() {
    let s = setup();
    s.with_boot_layers("theirs", "custom", b"\x7fELF the image's init");
    let mut o = RunOptions::new("theirs");
    s.custom(&mut o);
    let p = prepare(&s.store, &o, ARCH).unwrap();
    assert_eq!(
        p.init_layer,
        kiln_image::init_layer(b"\x7fELF my own init", &s.store.tmp_dir()).unwrap()
    );
    let replaced: Vec<&String> = p.warnings.iter().filter(|w| w.contains("replaced")).collect();
    assert!(
        replaced.len() == 1 && replaced[0].contains("theirs's init layer sha256:"),
        "{:?}",
        p.warnings
    );
    // The same kiln-init as the image's: nothing to warn about.
    s.with_boot_layers("same", "custom", b"\x7fELF my own init");
    o.image = "same".into();
    let p = prepare(&s.store, &o, ARCH).unwrap();
    assert!(!p.warnings.iter().any(|w| w.contains("replaced")), "{:?}", p.warnings);
}
