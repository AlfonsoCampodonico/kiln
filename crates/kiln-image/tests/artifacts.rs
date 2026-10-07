//! Fetching pinned boot artifacts by digest (spec §8.1) against the in-process registry.

use std::cell::Cell;

use kiln_image::ImageError;
use kiln_image::artifacts::{ArtifactKind, Fetcher, Pin, RegistryFetcher, ensure, gc_roots_in, init_layer_blob};
use kiln_registry::DockerConfig;
use kiln_registry::testregistry::{Config, TestRegistry};
use kiln_store::{Digest, Store};

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

fn pin(reg: &TestRegistry, bytes: &[u8]) -> Pin {
    Pin {
        kind: ArtifactKind::Init,
        name: "kiln-init",
        version: "0.0.0-test",
        arch: "arm64",
        repository: leak(format!("{}/kiln/kiln-init", reg.host())),
        digest: leak(Digest::of(bytes).to_string()),
        size: bytes.len() as u64,
    }
}

fn fetcher() -> RegistryFetcher {
    RegistryFetcher {
        config: DockerConfig::anonymous(),
    }
}

/// Fails the test if asked to fetch: the store must have answered.
struct Offline(Cell<usize>);

impl Fetcher for Offline {
    fn fetch(&self, _: &Pin, _: &Store) -> kiln_image::Result<()> {
        self.0.set(self.0.get() + 1);
        Err(ImageError::BadOption("offline".into()))
    }
}

#[test]
fn fetches_by_digest_once_then_serves_from_the_store() {
    let reg = TestRegistry::start(Config::default());
    let bin = b"\x7fELF a static kiln-init".repeat(100);
    reg.put_blob(&bin);
    let p = pin(&reg, &bin);
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let d = ensure(&store, &p, &fetcher()).unwrap();
    assert_eq!(std::fs::read(store.blob_path(&d)).unwrap(), bin);
    assert_eq!(reg.blob_requests("GET"), 1);
    assert_eq!(reg.manifest_requests("GET") + reg.manifest_requests("HEAD"), 0);
    // Cached: no fetcher (so no registry client, no DNS) is used.
    let offline = Offline(Cell::new(0));
    assert_eq!(ensure(&store, &p, &offline).unwrap(), d);
    assert_eq!(offline.0.get(), 0);
}

#[test]
fn a_registry_serving_other_content_is_refused() {
    let bin = b"the real kiln-init".to_vec();
    let reg = TestRegistry::start(Config {
        corrupt: Some(Digest::of(&bin)),
        ..Config::default()
    });
    reg.put_blob(&bin);
    let p = pin(&reg, &bin);
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let err = ensure(&store, &p, &fetcher()).unwrap_err();
    assert!(
        err.to_string()
            .contains("fetching the pinned kiln-init kiln-init 0.0.0-test for arm64"),
        "{err}"
    );
    assert!(!store.has_blob(&p.digest().unwrap()));
    // A missing blob is an error too, and nothing is stored.
    let empty = TestRegistry::start(Config::default());
    let p = pin(&empty, &bin);
    assert!(ensure(&store, &p, &fetcher()).is_err());
    assert!(!store.has_blob(&p.digest().unwrap()));
}

#[test]
fn a_wrong_size_in_the_pin_is_refused_even_from_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let bin = b"kiln-init".to_vec();
    store.put_bytes(&bin).unwrap();
    let reg = TestRegistry::start(Config::default());
    let mut p = pin(&reg, &bin);
    p.size += 1;
    assert!(ensure(&store, &p, &Offline(Cell::new(0))).is_err());
}

#[test]
fn the_init_layer_is_built_once_per_binary() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let bin = store.put_bytes(&b"\x7fELF init".repeat(50)).unwrap();
    let a = init_layer_blob(&store, &bin).unwrap();
    let want = kiln_image::init_layer(&std::fs::read(store.blob_path(&bin)).unwrap(), dir.path()).unwrap();
    assert_eq!(a, Digest::of(&want));
    // A second call is a cache hit; a missing layer blob is rebuilt.
    assert_eq!(init_layer_blob(&store, &bin).unwrap(), a);
    std::fs::remove_file(store.blob_path(&a)).unwrap();
    assert_eq!(init_layer_blob(&store, &bin).unwrap(), a);
    assert!(store.has_blob(&a));
}

/// `kiln gc` keeps the pinned blobs and the init layers built from pinned
/// kiln-init binaries (ruling 3), so offline runs keep working after it.
#[test]
fn gc_keeps_pinned_artifacts_and_their_init_layers() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let reg = TestRegistry::start(Config::default());
    let (kernel, init) = (b"a kernel Image".to_vec(), b"\x7fELF kiln-init".to_vec());
    let mut kpin = pin(&reg, &kernel);
    (kpin.kind, kpin.name, kpin.version) = (ArtifactKind::Kernel, "base", "6.18.54");
    let ipin = pin(&reg, &init);
    let offline = Offline(Cell::new(0));
    store.put_bytes(&kernel).unwrap();
    store.put_bytes(&init).unwrap();
    let k = ensure(&store, &kpin, &offline).unwrap();
    let i = ensure(&store, &ipin, &offline).unwrap();
    let layer = init_layer_blob(&store, &i).unwrap();
    // An init layer of a kiln-init that is no longer pinned goes.
    let old = store.put_bytes(b"\x7fELF an older kiln-init").unwrap();
    let old_layer = init_layer_blob(&store, &old).unwrap();
    let table = [kpin, ipin];
    let roots = gc_roots_in(&table, &store).unwrap();
    assert_eq!(roots.len(), 3);
    store.gc_with_roots(|s| gc_roots_in(&table, s)).unwrap();
    for d in [&k, &i, &layer] {
        assert!(store.has_blob(d), "{d}");
    }
    assert!(!store.has_blob(&old) && !store.has_blob(&old_layer));
    // The cached init layer is still a hit: nothing is rebuilt.
    assert_eq!(init_layer_blob(&store, &i).unwrap(), layer);
    assert_eq!(offline.0.get(), 0);
}
