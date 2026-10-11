//! From an image name to what boots (spec §8.1, T4): the image's platform manifest,
//! its app layers, the kernel and the init layer.
//! - The kernel is the image's kernel layer, only when its digest is the one the
//!   table pins for the `(profile, version, arch)` its config names; or `--kernel
//!   PATH`, or an image's `custom`-profile kernel layer, each only with
//!   `--allow-custom-kernel`.
//! - The init layer is always the pinned `kiln-init`'s, fetched by digest and built
//!   once (`cache/init-layers`), or built from `--init PATH` (with
//!   `--allow-custom-init`). An image's own init layer never boots: it is replaced,
//!   with a warning when it differs.
//!
//! The table ([`kiln_image::artifacts::PINS`]) pins vmkit's `base` kernels; no
//! kiln-init is released yet (M3c), so until then `--init` is needed.

use std::path::{Path, PathBuf};

pub use kiln_image::artifacts::CUSTOM_PROFILE;
use kiln_image::artifacts::{self, ArtifactKind, Fetcher, Pin};
use kiln_image::types::{KILN_INIT, KILN_KERNEL, KILN_LAYER, Process};
use kiln_image::{load, resolve_name};
use kiln_proto::sanitize::clean_line;
use kiln_registry::Reference;
use kiln_store::{Digest, Store};

use crate::error::{Error, Result};
use crate::options::RunOptions;

/// The largest `--init` binary read (a static kiln-init is a few MiB).
const MAX_INIT_BINARY: u64 = 64 << 20;

/// The trusted artifacts: the compiled-in table, or another one in tests.
pub trait Pins {
    fn kernel(&self, profile: &str, version: &str, arch: &str) -> Option<Pin>;
    fn init(&self, arch: &str) -> Option<Pin>;
}

/// [`kiln_image::artifacts::PINS`].
#[derive(Debug, Clone, Copy, Default)]
pub struct CompiledPins;

impl Pins for CompiledPins {
    fn kernel(&self, profile: &str, version: &str, arch: &str) -> Option<Pin> {
        artifacts::kernel_pin(profile, version, arch).cloned()
    }

    fn init(&self, arch: &str) -> Option<Pin> {
        artifacts::init_pin(arch).cloned()
    }
}

/// Another table of pins (tests).
#[derive(Debug, Clone, Copy)]
pub struct Table<'a>(pub &'a [Pin]);

impl Pins for Table<'_> {
    fn kernel(&self, profile: &str, version: &str, arch: &str) -> Option<Pin> {
        artifacts::find(self.0, ArtifactKind::Kernel, profile, version, arch).cloned()
    }

    fn init(&self, arch: &str) -> Option<Pin> {
        artifacts::find(self.0, ArtifactKind::Init, "kiln-init", artifacts::INIT_VERSION, arch).cloned()
    }
}

/// The init layer that boots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InitLayer {
    /// The pinned kiln-init's layer: a blob in the store.
    Stored(PathBuf),
    /// Built from `--init` for this run.
    Built(Vec<u8>),
}

/// Everything resolved from the store before boot.
#[derive(Debug, Clone)]
pub struct Prepared {
    /// The platform manifest.
    pub manifest: Digest,
    pub arch: String,
    pub process: Process,
    /// App layers, lowest first, and their blobs' paths.
    pub layers: Vec<Digest>,
    pub layer_paths: Vec<PathBuf>,
    pub kernel: PathBuf,
    /// Set when the kernel is not a pinned one (recorded in `run.json`).
    pub custom_kernel: Option<PathBuf>,
    /// The init layer (an erofs image).
    pub init: InitLayer,
    /// Set when kiln-init is not the pinned one (recorded in `run.json`).
    pub custom_init: Option<PathBuf>,
    /// One-line warnings for the user.
    pub warnings: Vec<String>,
}

/// This machine's architecture as kiln images name it.
pub fn host_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    }
}

/// With `KILN_IMPORT_FROM` (the Lima VM's read-only mount of the macOS store, spec
/// §10): when that store has `name` (as given, or as its normalised reference),
/// imports it, copying only the blobs this store lacks, and tags it here: every
/// time, so the newest convert on macOS wins over an older tag in the VM (ruling 4).
/// Returns a note for the user when blobs were copied.
pub fn import_from(store: &Store, from: &Path, name: &str) -> Result<Option<String>> {
    let src = Store::open_read_only(from)?;
    let refs = src.refs()?;
    let normalised = Reference::parse(name).ok().map(|r| r.to_string());
    let Some(src_name) = refs
        .keys()
        .find(|k| *k == name)
        .or_else(|| refs.keys().find(|k| Some(k.as_str()) == normalised.as_deref()))
    else {
        return Ok(None);
    };
    let r = kiln_image::import_image(store, &src, src_name, src_name)?;
    Ok((r.blobs_copied > 0).then(|| {
        format!(
            "imported {} from {} ({} blobs)",
            clean_line(src_name),
            from.display(),
            r.blobs_copied
        )
    }))
}

/// Resolves `opts.image` for `arch` and decides the kernel and init (spec §8.1)
/// against `pins`; a pinned kiln-init missing from the store comes from `fetcher`.
pub fn prepare(
    store: &Store,
    opts: &RunOptions,
    arch: &str,
    pins: &dyn Pins,
    fetcher: &dyn Fetcher,
) -> Result<Prepared> {
    // The custom kernel and init flags are this function's to enforce, whoever calls it.
    opts.check()?;
    let loaded = load(store, &resolve_name(store, &opts.image)?)?;
    let shown = clean_line(&opts.image);
    let found: Vec<_> = loaded.entries.iter().filter(|(p, _)| p.architecture == arch).collect();
    let (platform, image) = match found.as_slice() {
        [one] => *one,
        [] => {
            let have: Vec<String> = loaded
                .entries
                .iter()
                .map(|(p, _)| clean_line(&p.architecture))
                .collect();
            return Err(Error::refused(format!(
                "{shown} has no {arch} image (it has {})",
                have.join(", ")
            )));
        }
        more => {
            return Err(Error::refused(format!(
                "{shown} has {} {arch} images in its index; kiln cannot tell which to run",
                more.len()
            )));
        }
    };
    // The index entry, the image's own config and the host must agree.
    if platform.os != "linux" {
        return Err(Error::refused(format!(
            "{shown}'s {arch} image is for {}, not linux",
            clean_line(&platform.os)
        )));
    }
    if image.config.architecture != arch {
        return Err(Error::refused(format!(
            "{shown}'s index lists a {arch} image whose config says {}; the image is inconsistent",
            clean_line(&image.config.architecture)
        )));
    }
    let mut warnings = Vec::new();
    let (mut kernel_layer, mut init_layer, mut layers) = (None, None, Vec::new());
    for (i, l) in image.manifest.layers.iter().enumerate() {
        match (l.media_type.as_str(), i) {
            (KILN_KERNEL, 0) => kernel_layer = Some(l.digest.clone()),
            (KILN_INIT, 0 | 1) if layers.is_empty() && init_layer.is_none() => init_layer = Some(l.digest.clone()),
            (KILN_LAYER, _) => layers.push(l.digest.clone()),
            (t, _) => {
                return Err(Error::refused(format!(
                    "{shown}: unexpected layer {i} of type {} (kernel, init, then app layers)",
                    clean_line(t)
                )));
            }
        }
    }
    let mut layer_paths = Vec::new();
    for d in &layers {
        if !store.has_blob(d) {
            return Err(Error::refused(format!(
                "{shown}: layer {d} is missing from the store; pull or convert the image again"
            )));
        }
        layer_paths.push(store.blob_path(d));
    }

    // The kernel (T4).
    let (kernel, custom_kernel) = if let Some(path) = &opts.kernel {
        let path =
            std::fs::canonicalize(path).map_err(|e| Error::refused(format!("--kernel {}: {e}", path.display())))?;
        warnings.push(format!(
            "booting the custom kernel {} (--allow-custom-kernel)",
            path.display()
        ));
        (path.clone(), Some(path))
    } else if let Some(layer) = &kernel_layer {
        let Some(k) = &image.config.kernel else {
            return Err(Error::refused(format!(
                "{shown} has a kernel layer but no kernel in its config"
            )));
        };
        let (profile, version) = (clean_line(&k.profile), clean_line(&k.version));
        let custom = k.profile == CUSTOM_PROFILE;
        if custom {
            if !opts.allow_custom_kernel {
                return Err(Error::refused(format!(
                    "{shown} boots a custom kernel; pass --allow-custom-kernel to accept it"
                )));
            }
            warnings.push(format!("booting {shown}'s custom kernel (--allow-custom-kernel)"));
        } else {
            match pins.kernel(&k.profile, &k.version, arch) {
                Some(pin) if pin.digest()? == *layer => {}
                Some(_) => {
                    return Err(Error::refused(format!(
                        "{shown}'s kernel layer {layer} is not the pinned kernel {profile} {version} for {arch}; \
                         kiln refuses it (convert the image again, or pass --kernel PATH --allow-custom-kernel)"
                    )));
                }
                None => {
                    return Err(Error::refused(format!(
                        "this kiln does not pin the kernel {profile} {version} for {arch} that {shown} names; \
                         pass --kernel PATH --allow-custom-kernel to boot another"
                    )));
                }
            }
        }
        if !store.has_blob(layer) {
            return Err(Error::refused(format!(
                "{shown}: kernel layer {layer} is missing from the store"
            )));
        }
        let path = store.blob_path(layer);
        (path.clone(), custom.then_some(path))
    } else if pins
        .kernel(artifacts::KERNEL_PROFILE, artifacts::KERNEL_VERSION, arch)
        .is_some()
    {
        return Err(Error::refused(format!(
            "{shown} has no kernel layer: it was converted before this kiln pinned a kernel for {arch}; \
             convert it again, or pass --kernel PATH --allow-custom-kernel"
        )));
    } else {
        return Err(Error::refused(format!(
            "{shown} has no kernel layer, and this kiln pins no kernel for {arch} yet; \
             pass --kernel PATH --allow-custom-kernel"
        )));
    };

    // The init layer: always replaced (spec §8.1).
    let (init, init_digest, custom_init) = if let Some(path) = &opts.init {
        let bin = read_capped(path)?;
        let built = kiln_image::init_layer(&bin, &store.tmp_dir())?;
        warnings.push(format!(
            "booting the custom kiln-init {} (--allow-custom-init)",
            path.display()
        ));
        let d = Digest::of(&built);
        (InitLayer::Built(built), d, Some(path.clone()))
    } else if let Some(pin) = pins.init(arch) {
        let bin = artifacts::ensure(store, &pin, fetcher)?;
        let d = artifacts::init_layer_blob(store, &bin)?;
        (InitLayer::Stored(store.blob_path(&d)), d, None)
    } else {
        return Err(Error::refused(format!(
            "this kiln has no pinned kiln-init for {arch} (none has been released yet); \
             pass --init PATH --allow-custom-init"
        )));
    };
    if let Some(theirs) = &init_layer
        && *theirs != init_digest
    {
        warnings.push(format!(
            "{shown}'s init layer {theirs} is not the one booting; it is replaced by {init_digest}"
        ));
    }
    Ok(Prepared {
        manifest: image.digest.clone(),
        arch: arch.to_string(),
        process: image.config.process.clone(),
        layers,
        layer_paths,
        kernel,
        custom_kernel,
        init,
        custom_init,
        warnings,
    })
}

fn read_capped(path: &Path) -> Result<Vec<u8>> {
    use std::io::Read;
    let f = std::fs::File::open(path).map_err(|e| Error::refused(format!("--init {}: {e}", path.display())))?;
    let mut bin = Vec::new();
    f.take(MAX_INIT_BINARY + 1).read_to_end(&mut bin)?;
    if bin.len() as u64 > MAX_INIT_BINARY {
        return Err(Error::refused(format!(
            "--init {} is larger than 64 MiB",
            path.display()
        )));
    }
    Ok(bin)
}
