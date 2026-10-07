//! Pinned boot artifacts (spec §8.1, T4): the kernels and `kiln-init` binaries this
//! build of kiln trusts, by digest, and fetching them by that digest.
//!
//! The table is compiled in. Kernels come from vmkit's releases, published as OCI
//! artifacts at `ghcr.io/<org>/vmkit-kernels/<profile>:<version>-<arch>`; `kiln-init`
//! comes from kiln's own releases. A pin names the blob itself (the kernel image, or
//! the static `kiln-init` binary) and the repository holding it, so a fetch is one
//! verified blob GET: no tag or manifest is resolved, and nothing the registry says
//! can change what boots.
//!
//! Fetching looks in the store first and builds no registry client when the blob is
//! there, so cached runs work offline (a client resolves its registry's name when it
//! is built). `kiln gc` keeps every pinned blob and the init layers built from the
//! pinned `kiln-init` binaries ([`gc_roots`], spec §5.2), so they stay offline too.

use std::fmt;

use kiln_erofs::FORMAT_VERSION;
use kiln_registry::{Client, DockerConfig, Reference};
use kiln_store::{CacheKind, Digest, Store};

use crate::error::{ImageError, Result};
use crate::init::init_layer;

/// What a pin is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactKind {
    /// A kernel image (`Image` on arm64, PVH `vmlinux` on amd64), booted as is.
    Kernel,
    /// A static `kiln-init` binary, wrapped in the init layer at use.
    Init,
}

impl fmt::Display for ArtifactKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ArtifactKind::Kernel => "kernel",
            ArtifactKind::Init => "kiln-init",
        })
    }
}

/// One trusted artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    pub kind: ArtifactKind,
    /// The kernel profile (`base`), or `kiln-init`.
    pub name: &'static str,
    pub version: &'static str,
    /// `arm64` or `amd64`, as in kiln image configs.
    pub arch: &'static str,
    /// The registry repository holding the blob, e.g. `ghcr.io/org/vmkit-kernels/base`.
    pub repository: &'static str,
    /// `sha256:<hex>` of the blob.
    pub digest: &'static str,
    pub size: u64,
}

impl Pin {
    /// The pinned digest (the table's are checked by a test; others may be wrong).
    pub fn digest(&self) -> Result<Digest> {
        Ok(Digest::parse(self.digest)?)
    }

    /// What the pin is, for messages: `kernel base 6.18.54 for arm64`.
    pub fn describe(&self) -> String {
        format!("{} {} {} for {}", self.kind, self.name, self.version, self.arch)
    }
}

/// The kernel profile new images get.
pub const KERNEL_PROFILE: &str = "base";
/// The kernel version new images get: vmkit's pinned LTS.
pub const KERNEL_VERSION: &str = "6.18.54";
/// Where vmkit publishes the `base` kernels (spec §8.2).
pub const KERNEL_REPOSITORY: &str = "ghcr.io/alfonsocampodonico/vmkit-kernels/base";
/// The `kiln-init` this build boots: the one released with it.
pub const INIT_VERSION: &str = env!("CARGO_PKG_VERSION");
/// The profile of kernels the user supplies (`--kernel`); never pinned.
pub const CUSTOM_PROFILE: &str = "custom";

/// The compiled-in table (ruling 7).
///
/// Empty until the artifacts are released; until then images are converted without
/// boot layers and `kiln run` needs `--kernel` and `--init`.
/// - **Kernels:** vmkit's `kernels-6.18.54` release. Each arch gets one entry, from
///   that release's OCI artifact `KERNEL_REPOSITORY:6.18.54-<arch>`: the digest and
///   size of its kernel blob (the artifact's layer, not its manifest). For example:
///   `Pin { kind: ArtifactKind::Kernel, name: KERNEL_PROFILE, version: KERNEL_VERSION,
///   arch: "arm64", repository: KERNEL_REPOSITORY, digest: "sha256:<hex>", size: <bytes> },`
/// - **kiln-init:** kiln's own releases (milestone M3c), `name: "kiln-init"`,
///   `version: INIT_VERSION`.
pub const PINS: &[Pin] = &[
    // The kernel entries (vmkit `kernels-6.18.54`, arm64 and amd64) go here.
];

/// The pinned kernel `profile`/`version` for `arch`.
pub fn kernel_pin(profile: &str, version: &str, arch: &str) -> Option<&'static Pin> {
    find(PINS, ArtifactKind::Kernel, profile, version, arch)
}

/// The pinned `kiln-init` of this build for `arch`.
pub fn init_pin(arch: &str) -> Option<&'static Pin> {
    find(PINS, ArtifactKind::Init, "kiln-init", INIT_VERSION, arch)
}

/// The pin in `table` for `kind`, `name`, `version` and `arch`.
pub fn find<'a>(table: &'a [Pin], kind: ArtifactKind, name: &str, version: &str, arch: &str) -> Option<&'a Pin> {
    table
        .iter()
        .find(|p| p.kind == kind && p.name == name && p.version == version && p.arch == arch)
}

/// Gets a pinned blob into the store from its registry.
pub trait Fetcher {
    fn fetch(&self, pin: &Pin, store: &Store) -> Result<()>;
}

/// Fetches from the pin's registry with the Docker config's credentials. The
/// client is built per fetch, so a run that finds everything in the store never
/// resolves a registry name.
#[derive(Debug, Clone)]
pub struct RegistryFetcher {
    pub config: DockerConfig,
}

impl RegistryFetcher {
    pub fn from_env() -> Self {
        Self {
            config: DockerConfig::from_env(),
        }
    }
}

impl Fetcher for RegistryFetcher {
    fn fetch(&self, pin: &Pin, store: &Store) -> Result<()> {
        let r = Reference::parse(&format!("{}@{}", pin.repository, pin.digest))?;
        let client = Client::new(r.registry(), self.config.clone())?;
        client.fetch_blob(r.repository(), &pin.digest()?, pin.size, store)?;
        Ok(())
    }
}

/// Makes `pin`'s blob available in the store, verified against its digest and size
/// (T1): from the store when it is there, else from `fetcher`.
pub fn ensure(store: &Store, pin: &Pin, fetcher: &dyn Fetcher) -> Result<Digest> {
    let digest = pin.digest()?;
    if !store.has_blob(&digest) {
        fetcher.fetch(pin, store).map_err(|e| ImageError::Artifact {
            what: pin.describe(),
            source: Box::new(e),
        })?;
    }
    let size = store.blob_size(&digest)?;
    if size != pin.size {
        return Err(ImageError::Store(kiln_store::StoreError::SizeMismatch {
            digest,
            expected: pin.size,
            actual: size,
        }));
    }
    Ok(digest)
}

/// The `cache/init-layers` key of the init layer around the `kiln-init` binary `init`.
fn init_layer_key(init: &Digest) -> String {
    format!("{init}@{FORMAT_VERSION}")
}

/// The init layer around the `kiln-init` binary `init` (a blob in the store),
/// built once and cached by the binary's digest and the erofs format (spec §6.5).
pub fn init_layer_blob(store: &Store, init: &Digest) -> Result<Digest> {
    let key = init_layer_key(init);
    if let Some(d) = store.cache_get_blob(CacheKind::InitLayers, &key)? {
        return Ok(d);
    }
    let bin = std::fs::read(store.blob_path(init))?;
    let layer = init_layer(&bin, &store.tmp_dir())?;
    let d = store.put_bytes(&layer)?;
    store.cache_put(CacheKind::InitLayers, &key, &d.to_string())?;
    Ok(d)
}

/// What `kiln gc` keeps although no ref reaches it (ruling 3): every blob the
/// compiled table pins, and the init layers built from its `kiln-init` binaries.
pub fn gc_roots(store: &Store) -> kiln_store::Result<Vec<Digest>> {
    gc_roots_in(PINS, store)
}

/// [`gc_roots`] for another table (tests).
pub fn gc_roots_in(table: &[Pin], store: &Store) -> kiln_store::Result<Vec<Digest>> {
    let mut roots = Vec::new();
    for p in table {
        let d = Digest::parse(p.digest)?;
        if p.kind == ArtifactKind::Init
            && let Some(layer) = store.cache_get_blob(CacheKind::InitLayers, &init_layer_key(&d))?
        {
            roots.push(layer);
        }
        roots.push(d);
    }
    Ok(roots)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_digests_are_valid_and_unique() {
        for (i, p) in PINS.iter().enumerate() {
            p.digest().unwrap();
            assert!(p.arch == "arm64" || p.arch == "amd64", "{p:?}");
            assert!(p.size > 0, "{p:?}");
            assert!(
                PINS[..i]
                    .iter()
                    .all(|q| (q.kind, q.name, q.version, q.arch) != (p.kind, p.name, p.version, p.arch)),
                "{p:?} is pinned twice"
            );
        }
    }

    #[test]
    fn lookups_match_every_field() {
        let table = [Pin {
            kind: ArtifactKind::Kernel,
            name: "base",
            version: "6.18.54",
            arch: "arm64",
            repository: "ghcr.io/o/vmkit-kernels/base",
            digest: "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            size: 1,
        }];
        assert!(find(&table, ArtifactKind::Kernel, "base", "6.18.54", "arm64").is_some());
        assert!(find(&table, ArtifactKind::Kernel, "base", "6.18.54", "amd64").is_none());
        assert!(find(&table, ArtifactKind::Kernel, "base", "6.18.55", "arm64").is_none());
        assert!(find(&table, ArtifactKind::Init, "base", "6.18.54", "arm64").is_none());
        assert_eq!(table[0].describe(), "kernel base 6.18.54 for arm64");
        let bad = Pin {
            digest: "sha256:nothex",
            ..table[0].clone()
        };
        assert!(bad.digest().is_err());
    }
}
