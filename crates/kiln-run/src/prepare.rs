//! From an image name to what boots (spec §8.1, T4): the image's platform manifest,
//! its app layers, the kernel and the init layer.
//!
//! The kernels and kiln-init that §8.1's table pins have not been released yet, so
//! this kiln pins none, and every kernel and init that boots is the user's choice:
//! - the kernel is `--kernel PATH`, or an image's `custom`-profile kernel layer, each
//!   only with `--allow-custom-kernel`. An image's kernel layer that names another
//!   profile cannot be verified against a pin, so it is refused;
//! - the init layer is always built from `--init PATH` (with `--allow-custom-init`).
//!   An image's own init layer never boots: it is replaced, with a warning when it
//!   differs.

use std::path::{Path, PathBuf};

use kiln_image::types::{KILN_INIT, KILN_KERNEL, KILN_LAYER, Process};
use kiln_image::{load, resolve_name};
use kiln_proto::sanitize::clean_line;
use kiln_store::{Digest, Store};

use crate::error::{Error, Result};
use crate::options::RunOptions;

/// The kernel profile of images that boot a kernel kiln does not pin (spec §8.1).
pub const CUSTOM_PROFILE: &str = "custom";
/// The largest `--init` binary read (a static kiln-init is a few MiB).
const MAX_INIT_BINARY: u64 = 64 << 20;

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
    /// The init layer (an erofs image) built for this run.
    pub init_layer: Vec<u8>,
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

/// Resolves `opts.image` for `arch` and decides the kernel and init (spec §8.1).
pub fn prepare(store: &Store, opts: &RunOptions, arch: &str) -> Result<Prepared> {
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
    let kernel = if let Some(path) = &opts.kernel {
        let path =
            std::fs::canonicalize(path).map_err(|e| Error::refused(format!("--kernel {}: {e}", path.display())))?;
        warnings.push(format!(
            "booting the custom kernel {} (--allow-custom-kernel)",
            path.display()
        ));
        path
    } else if let Some(layer) = &kernel_layer {
        let Some(k) = &image.config.kernel else {
            return Err(Error::refused(format!(
                "{shown} has a kernel layer but no kernel in its config"
            )));
        };
        if k.profile != CUSTOM_PROFILE {
            return Err(Error::refused(format!(
                "{shown}'s kernel layer is the kernel {} {} for {arch}, which this kiln cannot verify: it pins \
                 no kernels yet; pass --kernel PATH --allow-custom-kernel",
                clean_line(&k.profile),
                clean_line(&k.version)
            )));
        }
        if !opts.allow_custom_kernel {
            return Err(Error::refused(format!(
                "{shown} boots a custom kernel; pass --allow-custom-kernel to accept it"
            )));
        }
        if !store.has_blob(layer) {
            return Err(Error::refused(format!(
                "{shown}: kernel layer {layer} is missing from the store"
            )));
        }
        warnings.push(format!("booting {shown}'s custom kernel (--allow-custom-kernel)"));
        store.blob_path(layer)
    } else {
        return Err(Error::refused(format!(
            "{shown} has no kernel layer, and this kiln pins no kernel for {arch} yet; \
             pass --kernel PATH --allow-custom-kernel"
        )));
    };

    // The init layer: always replaced (spec §8.1).
    let Some(init_path) = &opts.init else {
        return Err(Error::refused(format!(
            "this kiln has no pinned kiln-init for {arch} (none has been released yet); \
             pass --init PATH --allow-custom-init"
        )));
    };
    let bin = read_capped(init_path)?;
    let built = kiln_image::init_layer(&bin, &store.tmp_dir())?;
    warnings.push(format!(
        "booting the custom kiln-init {} (--allow-custom-init)",
        init_path.display()
    ));
    let built_digest = Digest::of(&built);
    if let Some(theirs) = &init_layer
        && *theirs != built_digest
    {
        warnings.push(format!(
            "{shown}'s init layer {theirs} is not the one booting; it is replaced by {built_digest}"
        ));
    }
    Ok(Prepared {
        manifest: image.digest.clone(),
        arch: arch.to_string(),
        process: image.config.process.clone(),
        layers,
        layer_paths,
        custom_kernel: Some(kernel.clone()),
        kernel,
        init_layer: built,
        custom_init: Some(init_path.clone()),
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
