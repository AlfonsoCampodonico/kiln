//! Choosing what boots (spec §8.1, T4, §11.5 "kernel and init swap"), without a VM.
//! The tests never depend on the compiled table: each passes its own, none or pins
//! for blobs it puts in the store or serves from the in-process registry.

use std::cell::Cell;
use std::path::PathBuf;

use kiln_erofs::testtar::{Opts, TarBuilder};
use kiln_image::artifacts::{ArtifactKind, Fetcher, INIT_VERSION, Pin, RegistryFetcher};
use kiln_image::types::{
    InitRef, KILN_ARTIFACT, KILN_CONFIG, KILN_INIT, KILN_KERNEL, KILN_LAYER, KernelRef, KilnConfig,
};
use kiln_image::{ConvertOptions, LocalRequest, convert_local, load, resolve_name};
use kiln_oci::testlayout::{LayoutBuilder, TestLayer};
use kiln_oci::{Descriptor, ImageIndex, Platform};
use kiln_registry::DockerConfig;
use kiln_registry::testregistry::{Config, TestRegistry};
use kiln_run::RunOptions;
use kiln_run::prepare::{InitLayer, Pins, Prepared, Table};
use kiln_store::{Digest, Store};

const ARCH: &str = "arm64";
const INDEX: &str = "application/vnd.oci.image.index.v1+json";
const MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";

struct Setup {
    _dirs: Vec<tempfile::TempDir>,
    store: Store,
    files: PathBuf,
}

/// A store with `app`, a provisional arm64 image (no boot layers: the library's
/// default), and a kernel and an init binary in `files`.
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

    /// `app` with its config and layers edited, tagged `tag`; returns the manifest's
    /// descriptor.
    fn edited(&self, tag: &str, edit: impl FnOnce(&mut KilnConfig, &mut Vec<Descriptor>)) -> Descriptor {
        let loaded = load(&self.store, &resolve_name(&self.store, "app").unwrap()).unwrap();
        let image = &loaded.entries[0].1;
        let (mut config, mut manifest) = (image.config.clone(), image.manifest.clone());
        edit(&mut config, &mut manifest.layers);
        let config = serde_json::to_vec(&config).unwrap();
        manifest.config = Descriptor::new(KILN_CONFIG, self.store.put_bytes(&config).unwrap(), config.len() as u64);
        let bytes = serde_json::to_vec(&manifest).unwrap();
        let digest = self.store.put_bytes(&bytes).unwrap();
        self.store.set_ref(tag, &digest).unwrap();
        let mut d = Descriptor::new(MANIFEST, digest, bytes.len() as u64);
        d.artifact_type = Some(KILN_ARTIFACT.into());
        d
    }

    /// A kiln index tagged `tag` listing `entries`.
    fn index(&self, tag: &str, entries: &[(&str, &Descriptor)]) {
        let index = ImageIndex {
            schema_version: 2,
            media_type: Some(INDEX.into()),
            artifact_type: Some(KILN_ARTIFACT.into()),
            manifests: entries
                .iter()
                .map(|(platform, d)| {
                    let mut d = (*d).clone();
                    d.platform = Some(Platform::parse(platform).unwrap());
                    d
                })
                .collect(),
            annotations: None,
        };
        let digest = self.store.put_bytes(&serde_json::to_vec(&index).unwrap()).unwrap();
        self.store.set_ref(tag, &digest).unwrap();
    }

    fn blob(&self, media_type: &str, bytes: &[u8]) -> Descriptor {
        Descriptor::new(media_type, self.store.put_bytes(bytes).unwrap(), bytes.len() as u64)
    }

    fn custom(&self, o: &mut RunOptions) {
        o.kernel = Some(self.files.join("kernel"));
        o.allow_custom_kernel = true;
        o.init = Some(self.files.join("init"));
        o.allow_custom_init = true;
    }
}

fn err(r: kiln_run::Result<Prepared>) -> String {
    r.map(|_| ()).unwrap_err().to_string()
}

/// No pins, and a fetcher that must never be asked.
fn prepare(store: &Store, o: &RunOptions, arch: &str) -> kiln_run::Result<Prepared> {
    kiln_run::prepare::prepare(store, o, arch, &Table(&[]), &Offline::default())
}

/// Fails the test's fetch if asked: the store must have answered.
#[derive(Default)]
struct Offline(Cell<usize>);

impl Fetcher for Offline {
    fn fetch(&self, _: &Pin, _: &Store) -> kiln_image::Result<()> {
        self.0.set(self.0.get() + 1);
        Err(kiln_image::ImageError::BadOption("offline".into()))
    }
}

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// The kernel `with_boot_layers` puts in images, pinned as `base` 6.18.54.
fn kernel_pin(bytes: &[u8]) -> Pin {
    Pin {
        kind: ArtifactKind::Kernel,
        name: "base",
        version: "6.18.54",
        arch: ARCH,
        repository: "127.0.0.1:1/unused",
        digest: leak(Digest::of(bytes).to_string()),
        size: bytes.len() as u64,
    }
}

/// This build's kiln-init, served by `reg`.
fn init_pin(reg: &TestRegistry, bytes: &[u8]) -> Pin {
    Pin {
        kind: ArtifactKind::Init,
        name: "kiln-init",
        version: INIT_VERSION,
        arch: ARCH,
        repository: leak(format!("{}/kiln/kiln-init", reg.host())),
        digest: leak(Digest::of(bytes).to_string()),
        size: bytes.len() as u64,
    }
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
    assert!(matches!(&p.init, InitLayer::Built(b) if !b.is_empty()));
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

/// T4: an image's kernel that names a profile this kiln does not pin cannot be
/// verified and is refused; `--kernel` replaces it.
#[test]
fn an_image_kernel_kiln_does_not_pin_is_refused() {
    let s = setup();
    s.with_boot_layers("booted", "base", b"\x7fELF the image's init");
    let mut o = RunOptions::new("booted");
    o.init = Some(s.files.join("init"));
    o.allow_custom_init = true;
    let e = err(prepare(&s.store, &o, ARCH));
    assert!(
        e.contains("does not pin the kernel base 6.18.54 for arm64") && e.contains("--kernel PATH"),
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
        p.init,
        InitLayer::Built(kiln_image::init_layer(b"\x7fELF my own init", &s.store.tmp_dir()).unwrap())
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

/// `prepare` enforces the custom kernel and init flags itself, not only `kiln run`.
#[test]
fn prepare_checks_the_custom_flags_itself() {
    let s = setup();
    let mut o = RunOptions::new("app");
    s.custom(&mut o);
    o.allow_custom_kernel = false;
    assert!(err(prepare(&s.store, &o, ARCH)).contains("--allow-custom-kernel"));
    s.custom(&mut o);
    o.allow_custom_init = false;
    assert!(err(prepare(&s.store, &o, ARCH)).contains("--allow-custom-init"));
}

/// The index entry, the image's config and the host must agree; one entry per
/// architecture.
#[test]
fn inconsistent_platforms_are_refused() {
    let s = setup();
    let mut o = RunOptions::new("x");
    s.custom(&mut o);
    let arm = s.edited("arm", |_, _| {});
    let amd = s.edited("amd", |c, _| c.architecture = "amd64".into());
    // The index says arm64, the config amd64.
    s.index("lying", &[("linux/arm64", &amd)]);
    o.image = "lying".into();
    let e = err(prepare(&s.store, &o, ARCH));
    assert!(e.contains("config says amd64"), "{e}");
    // Not linux.
    s.index("freebsd", &[("freebsd/arm64", &arm)]);
    o.image = "freebsd".into();
    let e = err(prepare(&s.store, &o, ARCH));
    assert!(e.contains("is for freebsd, not linux"), "{e}");
    // Two arm64 entries.
    s.index(
        "twice",
        &[("linux/arm64", &arm), ("linux/arm64", &arm), ("linux/amd64", &amd)],
    );
    o.image = "twice".into();
    let e = err(prepare(&s.store, &o, ARCH));
    assert!(e.contains("has 2 arm64 images"), "{e}");
    // A consistent index runs.
    s.index("good", &[("linux/arm64", &arm), ("linux/amd64", &amd)]);
    o.image = "good".into();
    prepare(&s.store, &o, ARCH).unwrap();
    assert!(err(prepare(&s.store, &o, "riscv64")).contains("has no riscv64 image"));
}

/// Layers: an optional kernel first, an optional init next, then app layers.
#[test]
fn misplaced_or_unknown_layers_are_refused() {
    let s = setup();
    let mut o = RunOptions::new("x");
    s.custom(&mut o);
    let kernel = s.blob(KILN_KERNEL, b"kernel");
    let init = s.blob(KILN_INIT, b"init layer");
    type Edit<'a> = Box<dyn Fn(&mut KilnConfig, &mut Vec<Descriptor>) + 'a>;
    let cases: [(&str, Edit<'_>, &str); 4] = [
        (
            "kernel-second",
            Box::new(|_, l| l.push(kernel.clone())),
            "unexpected layer 1 of type application/vnd.kiln.kernel.v1",
        ),
        (
            "init-after-app",
            Box::new(|_, l| l.push(init.clone())),
            "unexpected layer 1 of type application/vnd.kiln.init.v1.erofs",
        ),
        (
            "unknown",
            Box::new(|_, l| l.push(Descriptor::new("application/x-evil\n", l[0].digest.clone(), 1))),
            "unexpected layer 1 of type application/x-evil",
        ),
        (
            "kernel-without-config",
            Box::new(|c, l| {
                c.kernel = None;
                l.insert(0, kernel.clone());
            }),
            "has a kernel layer but no kernel in its config",
        ),
    ];
    for (tag, edit, want) in cases {
        s.edited(tag, |c, l| edit(c, l));
        o.image = tag.into();
        // The image's own kernel is what the last case is about.
        o.kernel = (tag != "kernel-without-config").then(|| s.files.join("kernel"));
        let e = err(prepare(&s.store, &o, ARCH));
        assert!(e.contains(want), "{tag}: {e}");
        assert!(!e.contains('\n'), "{tag}: {e:?}");
    }
    // App layers are only of the layer type.
    s.edited("ok", |_, l| assert!(l.iter().all(|d| d.media_type == KILN_LAYER)));
}

/// T4 with pins: the image's kernel boots when it is the pinned one; the init
/// layer is the pinned kiln-init's, fetched once by digest and then built once
/// (`cache/init-layers`); later runs need no registry. Neither is "custom".
#[test]
fn the_pinned_kernel_boots_and_the_pinned_init_is_fetched_once() {
    let s = setup();
    s.with_boot_layers("booted", "base", b"\x7fELF the image's init");
    let reg = TestRegistry::start(Config::default());
    let init = b"\x7fELF the pinned kiln-init".repeat(10);
    reg.put_blob(&init);
    let table = [kernel_pin(b"an image's kernel"), init_pin(&reg, &init)];
    let o = RunOptions::new("booted");
    let fetcher = RegistryFetcher {
        config: DockerConfig::anonymous(),
    };
    let p = kiln_run::prepare::prepare(&s.store, &o, ARCH, &Table(&table), &fetcher).unwrap();
    assert_eq!(std::fs::read(&p.kernel).unwrap(), b"an image's kernel");
    assert_eq!((p.custom_kernel.as_ref(), p.custom_init.as_ref()), (None, None));
    let InitLayer::Stored(path) = &p.init else {
        panic!("{:?}", p.init)
    };
    assert_eq!(
        std::fs::read(path).unwrap(),
        kiln_image::init_layer(&init, &s.store.tmp_dir()).unwrap()
    );
    assert_eq!(reg.blob_requests("GET"), 1);
    // The image's own init layer differs: replaced, with one warning and nothing else.
    assert_eq!(p.warnings.len(), 1, "{:?}", p.warnings);
    assert!(p.warnings[0].contains("is not the one booting"), "{:?}", p.warnings);
    // Cached: the second run asks no registry.
    let offline = Offline::default();
    let again = kiln_run::prepare::prepare(&s.store, &o, ARCH, &Table(&table), &offline).unwrap();
    assert_eq!(again.init, p.init);
    assert_eq!(offline.0.get(), 0);
}

/// A kernel layer whose digest is not the pinned one is refused (T4), whatever
/// its config claims; `--kernel` with `--allow-custom-kernel` still boots another.
#[test]
fn a_kernel_layer_that_is_not_the_pinned_one_is_refused() {
    let s = setup();
    s.with_boot_layers("booted", "base", b"\x7fELF the image's init");
    let table = [kernel_pin(b"the real kernel")];
    let mut o = RunOptions::new("booted");
    o.init = Some(s.files.join("init"));
    o.allow_custom_init = true;
    let none = Offline::default();
    let e = err(kiln_run::prepare::prepare(&s.store, &o, ARCH, &Table(&table), &none));
    assert!(
        e.contains("is not the pinned kernel base 6.18.54 for arm64") && e.contains("convert the image again"),
        "{e}"
    );
    s.custom(&mut o);
    kiln_run::prepare::prepare(&s.store, &o, ARCH, &Table(&table), &none).unwrap();
}

/// The pinned kernel layer is checked like any pinned artifact (`artifacts::ensure`):
/// a blob missing from the store is fetched by digest from the pin's repository,
/// and one whose size is not the pin's is refused.
#[test]
fn the_pinned_kernel_layer_is_fetched_when_missing_and_its_size_checked() {
    let s = setup();
    s.with_boot_layers("booted", "base", b"\x7fELF the image's init");
    let kernel = b"an image's kernel";
    let reg = TestRegistry::start(Config::default());
    reg.put_blob(kernel);
    let pin = Pin {
        repository: leak(format!("{}/vmkit-kernels/base", reg.host())),
        ..kernel_pin(kernel)
    };
    let mut o = RunOptions::new("booted");
    o.init = Some(s.files.join("init"));
    o.allow_custom_init = true;
    let fetcher = RegistryFetcher {
        config: DockerConfig::anonymous(),
    };
    std::fs::remove_file(s.store.blob_path(&Digest::of(kernel))).unwrap();
    let p = kiln_run::prepare::prepare(&s.store, &o, ARCH, &Table(std::slice::from_ref(&pin)), &fetcher).unwrap();
    assert_eq!(std::fs::read(&p.kernel).unwrap(), kernel);
    assert_eq!(reg.blob_requests("GET"), 1);
    // A pin whose size disagrees with the blob is refused, without a fetch.
    let wrong = Pin { size: 1, ..pin };
    let none = Offline::default();
    let e = err(kiln_run::prepare::prepare(&s.store, &o, ARCH, &Table(&[wrong]), &none));
    assert!(e.contains("size"), "{e}");
    assert_eq!(none.0.get(), 0);
}

/// A provisional image converted before the table pinned a kernel: convert it
/// again (or name a kernel). A pinned kiln-init that cannot be fetched is named.
#[test]
fn a_provisional_image_after_kernels_are_pinned_and_an_unreachable_init() {
    let s = setup();
    let reg = TestRegistry::start(Config::default());
    let table = [kernel_pin(b"a kernel"), init_pin(&reg, b"\x7fELF never served")];
    let none = Offline::default();
    let mut o = RunOptions::new("app");
    let e = err(kiln_run::prepare::prepare(&s.store, &o, ARCH, &Table(&table), &none));
    assert!(
        e.contains("converted before this kiln pinned a kernel for arm64") && e.contains("convert it again"),
        "{e}"
    );
    o.kernel = Some(s.files.join("kernel"));
    o.allow_custom_kernel = true;
    let e = err(kiln_run::prepare::prepare(&s.store, &o, ARCH, &Table(&table), &none));
    assert!(
        e.contains(&format!(
            "fetching the pinned kiln-init kiln-init {INIT_VERSION} for arm64"
        )),
        "{e}"
    );
    assert_eq!(none.0.get(), 1);
}

/// The compiled table and a test table answer the same questions.
#[test]
fn pins_are_looked_up_by_profile_version_and_arch() {
    let reg = TestRegistry::start(Config::default());
    let table = [kernel_pin(b"k"), init_pin(&reg, b"i")];
    let pins: &dyn Pins = &Table(&table);
    assert!(pins.kernel("base", "6.18.54", ARCH).is_some());
    assert!(pins.kernel("base", "6.18.54", "amd64").is_none());
    assert!(pins.kernel("custom", "6.18.54", ARCH).is_none());
    assert!(pins.init(ARCH).is_some() && pins.init("amd64").is_none());
}

/// `KILN_IMPORT_FROM` (spec §10, ruling 4): an image the source store has is
/// imported (only missing blobs) and tagged again on every run, so the source's
/// newest convert wins; names the source lacks are left to this store.
#[test]
fn images_are_imported_and_retagged_from_another_store() {
    let mac = setup();
    let home = tempfile::tempdir().unwrap();
    let vm = Store::open(home.path()).unwrap();
    let import = |name: &str| kiln_run::prepare::import_from(&vm, mac.store.root(), name);
    let i = import("app").unwrap().unwrap();
    let note = i.note.unwrap();
    assert!(
        note.starts_with("imported app from ") && note.ends_with(" blobs)"),
        "{note}"
    );
    assert_eq!(i.image, "app");
    let first = vm.get_ref("app").unwrap().unwrap();
    assert_eq!(Some(first.clone()), mac.store.get_ref("app").unwrap());
    // Nothing new: no note, the same tag.
    assert_eq!(import("app").unwrap().unwrap().note, None);
    // A newer convert on the source wins over the older tag here.
    mac.with_boot_layers("app", "custom", b"\x7fELF init");
    assert!(import("app").unwrap().unwrap().note.is_some());
    let second = vm.get_ref("app").unwrap().unwrap();
    assert_ne!(second, first);
    assert_eq!(Some(second), mac.store.get_ref("app").unwrap());
    // A name the source lacks, and a source that is no store.
    assert_eq!(import("other").unwrap(), None);
    assert!(kiln_run::prepare::import_from(&vm, &home.path().join("nowhere"), "app").is_err());
}

/// The run resolves the tag the import set: a source image tagged under its
/// normalised reference wins over a stale exact tag of the name here. A tag moved
/// to an image whose blobs were all here already is noted.
#[test]
fn an_import_wins_over_a_stale_local_tag_and_a_moved_tag_is_noted() {
    let mac = setup();
    let home = tempfile::tempdir().unwrap();
    let vm = Store::open(home.path()).unwrap();
    let full = "docker.io/library/app:latest";
    mac.store
        .set_ref(full, &mac.store.get_ref("app").unwrap().unwrap())
        .unwrap();
    // An older image under the exact name here.
    kiln_run::prepare::import_from(&vm, mac.store.root(), "app").unwrap();
    mac.with_boot_layers("newer", "custom", b"\x7fELF init");
    let newer = mac.store.get_ref("newer").unwrap().unwrap();
    mac.store.set_ref(full, &newer).unwrap();
    mac.store.remove_ref("app").unwrap();
    let i = kiln_run::prepare::import_from(&vm, mac.store.root(), "app")
        .unwrap()
        .unwrap();
    assert_eq!(i.image, full);
    assert_eq!(resolve_name(&vm, &i.image).unwrap(), newer);
    assert_ne!(
        resolve_name(&vm, "app").unwrap(),
        newer,
        "the stale exact tag is still here"
    );
    // The same image again, under a tag here that named another: noted.
    vm.set_ref(full, &vm.get_ref("app").unwrap().unwrap()).unwrap();
    let i = kiln_run::prepare::import_from(&vm, mac.store.root(), "app")
        .unwrap()
        .unwrap();
    let note = i.note.unwrap();
    assert!(
        note.starts_with(&format!("{full} now names {newer} from ")) && note.contains("were here already"),
        "{note}"
    );
}

/// `kiln run sha256:...` imports the image by digest when this store lacks it,
/// without tagging it.
#[test]
fn a_digest_is_imported_untagged_when_missing() {
    let mac = setup();
    let home = tempfile::tempdir().unwrap();
    let vm = Store::open(home.path()).unwrap();
    let d = mac.store.get_ref("app").unwrap().unwrap().to_string();
    let i = kiln_run::prepare::import_from(&vm, mac.store.root(), &d)
        .unwrap()
        .unwrap();
    assert_eq!(i.image, d);
    assert!(i.note.unwrap().starts_with(&format!("imported {d} from ")));
    assert!(vm.refs().unwrap().is_empty());
    load(&vm, &resolve_name(&vm, &d).unwrap()).unwrap();
    // Present now: nothing to do.
    assert_eq!(kiln_run::prepare::import_from(&vm, mac.store.root(), &d).unwrap(), None);
}
