//! OCI image → kiln image (spec §6.2–§6.4).

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use kiln_erofs::{DirAttrs, FORMAT_VERSION, Image, LayerWriter, Limits};
use kiln_oci::media::{self, Compression};
use kiln_oci::{Descriptor, ImageManifest, Platform, ResolvedImage, canonical_json};
use kiln_store::{CacheKind, Digest, HashingReader, Store, TmpBlob};

use crate::artifacts::{self, Pin, RegistryFetcher};
use crate::ctx::{ctx_hash, parents_entry, parse_parents};
use crate::decompress::{Budget, Tripped, layer_limit, open_bounded};
use crate::error::{ImageError, Result};
use crate::load::{KilnManifest, Loaded};
use crate::types::*;

/// Conversion settings (spec §6.4, §7.6).
#[derive(Debug, Clone)]
pub struct ConvertOptions {
    /// App layers above this count are squashed into the bottom layer.
    pub max_layers: usize,
    pub limits: Limits,
    /// Decompressed bytes across all layers of one image.
    pub max_image_bytes: u64,
    /// Decompressed / compressed bytes for one layer.
    pub max_expansion_ratio: u64,
    /// Layers converted in parallel.
    pub jobs: usize,
    /// The image's base, so that its boundary is recorded (spec §16.2).
    pub base: Option<BaseImage>,
    /// Which kernel and init layers to add (spec §6.5).
    pub boot: BootLayers,
}

/// The kernel and `kiln-init` an image boots, as pins (spec §8.1). Each is added
/// when it is pinned: an image may carry the kernel layer before a `kiln-init`
/// release exists (`kiln run` then needs only `--init`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Boot {
    pub kernel: Option<Pin>,
    pub init: Option<Pin>,
}

/// Which boot layers `convert` adds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum BootLayers {
    /// None: a provisional image. The default, so that library callers and tests
    /// never fetch boot artifacts unless they ask; `kiln convert` asks for `Pinned`.
    #[default]
    Provisional,
    /// The compiled-in pins (`kiln_image::artifacts::PINS`) for the image's
    /// architecture: the `base` kernel and this build's `kiln-init`, each when pinned.
    Pinned,
    /// These artifacts (tests, and later explicit choices).
    Explicit(Boot),
}

impl BootLayers {
    /// The pins to add for `arch`; `None` when there are none (a provisional image).
    pub fn for_arch(&self, arch: &str) -> Option<Boot> {
        let boot = match self {
            BootLayers::Provisional => return None,
            BootLayers::Pinned => Boot {
                kernel: artifacts::kernel_pin(artifacts::KERNEL_PROFILE, artifacts::KERNEL_VERSION, arch).cloned(),
                init: artifacts::init_pin(arch).cloned(),
            },
            BootLayers::Explicit(b) => b.clone(),
        };
        (boot.kernel.is_some() || boot.init.is_some()).then_some(boot)
    }
}

/// A kiln image in the store that another image is built on.
#[derive(Debug, Clone)]
pub struct BaseImage {
    /// The name it was given as, recorded for information.
    pub name: String,
    pub image: Loaded,
}

impl Default for ConvertOptions {
    fn default() -> Self {
        Self {
            max_layers: 10,
            limits: Limits::default(),
            max_image_bytes: 64 << 30,
            max_expansion_ratio: 200,
            jobs: std::thread::available_parallelism().map_or(4, |n| n.get()),
            base: None,
            boot: BootLayers::default(),
        }
    }
}

/// Makes a layer blob available in the store just before the pipeline streams it.
/// It is called only for layers that are converted, never for cache hits, from
/// up to `jobs` threads at once.
pub trait LayerSource: Sync {
    fn fetch(&self, layer: &Descriptor) -> Result<()>;
}

/// Layers that resolve already verified into the store (local inputs).
#[derive(Debug, Clone, Copy, Default)]
pub struct StoredLayers;

impl LayerSource for StoredLayers {
    fn fetch(&self, _layer: &Descriptor) -> Result<()> {
        Ok(())
    }
}

/// One erofs layer of a converted image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerReport {
    /// The OCI layers it was built from (several when squashed).
    pub sources: Vec<Digest>,
    pub erofs: Digest,
    pub size: u64,
    /// Taken from a cache instead of converted.
    pub cached: bool,
    pub inherits: bool,
    pub warnings: Vec<String>,
    /// The implicit directories of a layer built from one source (spec §6.3).
    pub implicit: Vec<Vec<u8>>,
}

/// One converted platform.
#[derive(Debug, Clone)]
pub struct Converted {
    pub platform: Platform,
    pub manifest_digest: Digest,
    pub manifest_size: u64,
    pub layers: Vec<LayerReport>,
    /// How many layers were merged into the bottom layer, or the bottom app layer
    /// when a base is recorded (0: no squash).
    pub squashed: usize,
    pub base: Option<BaseRecord>,
    /// The kernel and init layers the image carries; `None` for a provisional image.
    pub boot: Option<Boot>,
}

/// A layer whose tar is consumed but whose metadata waits for its lowers.
struct Pending {
    tmp: TmpBlob,
    writer: LayerWriter<File>,
    implicit: Vec<Vec<u8>>,
}

/// Result of streaming one layer in phase A. A layer with no implicit directories
/// needs none of its lowers, so it is finished and committed on the spot (its
/// temp file, writer handle and spill file are released); only layers that must
/// inherit directory metadata stay open until phase B.
enum Streamed {
    Committed { erofs: Digest, warnings: Vec<String> },
    Pending(Box<Pending>),
}

enum Plan {
    Done(Digest),
    Parents(Vec<Vec<u8>>),
    Convert,
}

/// Keyed by the blob and the verified decompressed content (`diff_id`), so a hit
/// implies the diff_id check already passed and compression cannot alias entries.
fn layer_key(src: &Digest, diff_id: &Digest) -> String {
    format!("{src}@{}@{FORMAT_VERSION}", diff_id.hex())
}

fn plan_layer(store: &Store, desc: &Descriptor, diff_id: &Digest) -> Result<Plan> {
    let Some(entry) = store.cache_get(CacheKind::Layers, &layer_key(&desc.digest, diff_id))? else {
        return Ok(Plan::Convert);
    };
    if let Some(d) = entry.strip_prefix("erofs ").and_then(|d| Digest::parse(d.trim()).ok()) {
        return Ok(if store.has_blob(&d) {
            Plan::Done(d)
        } else {
            Plan::Convert
        });
    }
    Ok(parse_parents(&entry).map_or(Plan::Convert, Plan::Parents))
}

fn check_platform(p: &Platform) -> Result<()> {
    if p.os == "linux" && (p.architecture == "amd64" || p.architecture == "arm64") {
        Ok(())
    } else {
        Err(ImageError::UnsupportedPlatform(p.to_string()))
    }
}

/// Streams layer `i` through the decompressor, limits and tar reader, then checks
/// the decompressed digest against `rootfs.diff_ids[i]` (spec §6.1 step 3).
fn stream_layer(
    store: &Store,
    img: &ResolvedImage,
    i: usize,
    opts: &ConvertOptions,
    budget: &Arc<Budget>,
    source: &dyn LayerSource,
) -> Result<Streamed> {
    let desc = &img.manifest.layers[i];
    source.fetch(desc)?;
    let compression = media::layer_compression(&desc.media_type)?;
    let max = layer_limit(
        compression,
        desc.size,
        opts.max_expansion_ratio,
        opts.limits.max_layer_bytes,
    );
    let tripped = Arc::new(Tripped::default());
    let limit_error = |t: &Tripped| {
        if t.image() {
            Some(ImageError::LimitExceeded {
                what: "uncompressed bytes per image",
                max: opts.max_image_bytes,
            })
        } else if t.layer() {
            let what = if max < opts.limits.max_layer_bytes && compression != Compression::None {
                "expansion ratio"
            } else {
                "uncompressed bytes per layer"
            };
            Some(ImageError::LimitExceeded { what, max })
        } else {
            None
        }
    };
    let blob = store.open_blob(&desc.digest)?;
    let bounded = open_bounded(
        blob,
        compression,
        max,
        budget.clone(),
        opts.max_image_bytes,
        tripped.clone(),
    )?;
    let mut hashed = HashingReader::new(bounded);
    let tmp = store.tmp_blob()?;
    let mut writer = LayerWriter::new(tmp.reopen()?, &store.tmp_dir(), opts.limits.clone())?;
    if let Err(e) = writer.append_tar(&mut hashed) {
        return Err(limit_error(&tripped).unwrap_or(e.into()));
    }
    // Read to EOF: only zero padding may follow the end-of-archive marker.
    let mut buf = vec![0u8; 64 << 10];
    loop {
        let n = match hashed.read(&mut buf) {
            Ok(n) => n,
            Err(e) => return Err(limit_error(&tripped).unwrap_or(e.into())),
        };
        if n == 0 {
            break;
        }
        if buf[..n].iter().any(|&b| b != 0) {
            return Err(ImageError::TrailingData { layer: i });
        }
    }
    let (actual, _) = hashed.finish_to_eof()?;
    let expected = &img.config.rootfs.diff_ids[i];
    if &actual != expected {
        return Err(ImageError::DiffIdMismatch {
            layer: i,
            expected: expected.clone(),
            actual,
        });
    }
    let implicit = writer.implicit_dirs();
    if implicit.is_empty() {
        let (_out, summary) = writer.finish(&BTreeMap::new())?;
        let erofs = store.commit(tmp)?;
        return Ok(Streamed::Committed {
            erofs,
            warnings: summary.warnings,
        });
    }
    Ok(Streamed::Pending(Box::new(Pending { tmp, writer, implicit })))
}

/// Runs `stream_layer` for `todo` on up to `opts.jobs` threads; stops early on error.
fn stream_parallel(
    store: &Store,
    img: &ResolvedImage,
    todo: &[usize],
    opts: &ConvertOptions,
    budget: &Arc<Budget>,
    source: &dyn LayerSource,
) -> Result<BTreeMap<usize, Streamed>> {
    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let results = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..opts.jobs.clamp(1, todo.len().max(1)) {
            s.spawn(|| {
                while !failed.load(Ordering::SeqCst) {
                    let Some(&i) = todo.get(next.fetch_add(1, Ordering::SeqCst)) else {
                        break;
                    };
                    let r = stream_layer(store, img, i, opts, budget, source);
                    if r.is_err() {
                        failed.store(true, Ordering::SeqCst);
                    }
                    results.lock().expect("no poisoning").push((i, r));
                }
            });
        }
    });
    let mut results = results.into_inner().expect("no poisoning");
    results.sort_by_key(|(i, _)| *i);
    results.into_iter().map(|(i, r)| r.map(|p| (i, p))).collect()
}

/// The finished layers below the current one. Their images are opened only while
/// a layer resolves inherited directories, so phase B holds no descriptors otherwise.
struct Lowers<'a> {
    store: &'a Store,
    digests: Vec<Digest>,
}

impl Lowers<'_> {
    fn resolve(&self, paths: &[Vec<u8>]) -> Result<BTreeMap<Vec<u8>, DirAttrs>> {
        let mut images = self
            .digests
            .iter()
            .map(|d| Ok(Image::open(self.store.open_blob(d)?)?))
            .collect::<Result<Vec<_>>>()?;
        Ok(kiln_erofs::resolve_inherited(&mut images, paths)?)
    }
}

/// Records a layer's conversion warnings under its cache key, so a later cache hit
/// reports them too. Written before the layer entry it annotates.
fn save_warnings(store: &Store, key: &str, warnings: &[String]) -> Result<()> {
    if warnings.is_empty() {
        return Ok(());
    }
    let json = serde_json::to_string(warnings).expect("strings serialize");
    Ok(store.cache_put(CacheKind::Warnings, key, &json)?)
}

fn cached_warnings(store: &Store, key: &str) -> Result<Vec<String>> {
    let entry = store.cache_get(CacheKind::Warnings, key)?;
    // A corrupt entry only loses warnings; it never affects the conversion.
    Ok(entry.and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default())
}

/// Completes a streamed layer against its lowers and writes its cache entries
/// (always in phase B, bottom-up, so a failing sibling leaves no entry).
/// `inherited` is passed when the caller already resolved the layer's parents.
/// Returns the erofs digest, the warnings and the layer's implicit directories.
fn finish_layer(
    store: &Store,
    key: &str,
    streamed: Streamed,
    lowers: &Lowers,
    inherited: Option<BTreeMap<Vec<u8>, DirAttrs>>,
) -> Result<(Digest, Vec<String>, Vec<Vec<u8>>)> {
    let p = match streamed {
        Streamed::Committed { erofs, warnings } => {
            save_warnings(store, key, &warnings)?;
            store.cache_put(CacheKind::Layers, key, &format!("erofs {erofs}"))?;
            return Ok((erofs, warnings, Vec::new()));
        }
        Streamed::Pending(p) => *p,
    };
    let inherited = match inherited {
        Some(m) => m,
        None => lowers.resolve(&p.implicit)?,
    };
    let (_out, summary) = p.writer.finish(&inherited)?;
    let d = store.commit(p.tmp)?;
    save_warnings(store, key, &summary.warnings)?;
    let ctx = ctx_hash(&p.implicit, &inherited);
    store.cache_put(CacheKind::LayersCtx, &format!("{key}@{ctx}"), &d.to_string())?;
    store.cache_put(CacheKind::Layers, key, &parents_entry(&p.implicit))?;
    Ok((d, summary.warnings, p.implicit))
}

/// A bottom squash applies and drops overlay markers; an upper one keeps them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SquashKind {
    Bottom,
    Upper,
}

fn squash_key(layers: &[LayerReport], kind: SquashKind) -> String {
    let mut joined: Vec<String> = layers.iter().map(|l| l.erofs.to_string()).collect();
    if kind == SquashKind::Upper {
        joined.insert(0, "upper".into());
    }
    format!("{}@{FORMAT_VERSION}", Digest::of(joined.join("\n").as_bytes()).hex())
}

/// Merges the bottom `k` of `layers` into one (spec §6.4).
///
/// `kiln_erofs::squash` reads file data from every merged layer while it writes,
/// so all `k` images are open at once. The CLI raises the soft descriptor limit;
/// an image needing more than the hard limit allows would fail here.
fn squash_bottom(store: &Store, layers: Vec<LayerReport>, k: usize, kind: SquashKind) -> Result<Vec<LayerReport>> {
    let (bottom, rest) = layers.split_at(k);
    let key = squash_key(bottom, kind);
    let sources: Vec<Digest> = bottom.iter().flat_map(|l| l.sources.clone()).collect();
    let warnings: Vec<String> = bottom.iter().flat_map(|l| l.warnings.iter().cloned()).collect();
    let (erofs, cached) = match store.cache_get_blob(CacheKind::Squash, &key)? {
        Some(d) => (d, true),
        None => {
            let mut images = bottom
                .iter()
                .map(|l| Ok(Image::open(store.open_blob(&l.erofs)?)?))
                .collect::<Result<Vec<_>>>()?;
            let tmp = store.tmp_blob()?;
            match kind {
                SquashKind::Bottom => kiln_erofs::squash(&mut images, tmp.reopen()?, &store.tmp_dir())?,
                SquashKind::Upper => kiln_erofs::squash_upper(&mut images, tmp.reopen()?, &store.tmp_dir())?,
            };
            let d = store.commit(tmp)?;
            store.cache_put(CacheKind::Squash, &key, &d.to_string())?;
            (d, false)
        }
    };
    let size = store.blob_size(&erofs)?;
    let mut out = vec![LayerReport {
        sources,
        erofs,
        size,
        cached,
        inherits: false,
        warnings,
        implicit: Vec::new(),
    }];
    out.extend_from_slice(rest);
    Ok(out)
}

/// Squashes `layers` down to `max` when there are more (spec §6.4); returns how
/// many were merged (0: none).
pub(crate) fn squash_to(
    store: &Store,
    layers: Vec<LayerReport>,
    max: usize,
    kind: SquashKind,
) -> Result<(Vec<LayerReport>, usize)> {
    if layers.len() <= max {
        return Ok((layers, 0));
    }
    let k = layers.len() - max + 1;
    Ok((squash_bottom(store, layers, k, kind)?, k))
}

/// Converts `img`'s layers `range`, bottom-up, above the finished erofs layers
/// `below` (the merged view an implicit directory inherits from).
pub(crate) fn convert_layers(
    store: &Store,
    img: &ResolvedImage,
    range: Range<usize>,
    below: Vec<Digest>,
    opts: &ConvertOptions,
    source: &dyn LayerSource,
) -> Result<Vec<LayerReport>> {
    let layers = &img.manifest.layers;
    let plans = range
        .clone()
        .map(|i| plan_layer(store, &layers[i], &img.config.rootfs.diff_ids[i]))
        .collect::<Result<Vec<_>>>()?;
    let todo: Vec<usize> = range
        .clone()
        .zip(&plans)
        .filter(|(_, p)| matches!(p, Plan::Convert))
        .map(|(i, _)| i)
        .collect();
    let budget = Arc::new(Budget::default());
    let mut pending = stream_parallel(store, img, &todo, opts, &budget, source)?;

    // Bottom-up: each layer's lowers are final before it is.
    let mut reports: Vec<LayerReport> = Vec::new();
    let mut lowers = Lowers { store, digests: below };
    for (i, plan) in range.zip(plans) {
        let src = &layers[i].digest;
        let lkey = layer_key(src, &img.config.rootfs.diff_ids[i]);
        let (erofs, cached, warnings, implicit) = match plan {
            Plan::Done(d) => (d, true, cached_warnings(store, &lkey)?, Vec::new()),
            Plan::Parents(paths) => {
                let inherited = lowers.resolve(&paths)?;
                let key = format!("{lkey}@{}", ctx_hash(&paths, &inherited));
                match store.cache_get_blob(CacheKind::LayersCtx, &key)? {
                    Some(d) => (d, true, cached_warnings(store, &lkey)?, paths),
                    None => {
                        let p = stream_layer(store, img, i, opts, &budget, source)?;
                        // Reuse the resolution only if the cached path list still matches
                        // the layer (a stale entry must not change the output).
                        let reuse = matches!(&p, Streamed::Pending(q) if q.implicit == paths);
                        let (d, w, implicit) = finish_layer(store, &lkey, p, &lowers, reuse.then_some(inherited))?;
                        (d, false, w, implicit)
                    }
                }
            }
            Plan::Convert => {
                let p = pending.remove(&i).expect("streamed in phase A");
                let (d, w, implicit) = finish_layer(store, &lkey, p, &lowers, None)?;
                (d, false, w, implicit)
            }
        };
        lowers.digests.push(erofs.clone());
        let size = store.blob_size(&erofs)?;
        reports.push(LayerReport {
            sources: vec![src.clone()],
            erofs,
            size,
            cached,
            inherits: !implicit.is_empty(),
            warnings,
            implicit,
        });
    }
    Ok(reports)
}

/// The base's layers inside `reports` (spec §16.2): the `k` such that `reports[..k]`,
/// squashed as the base was, are exactly `base`'s layers. Returns `k` and those
/// layers. Only erofs digests are compared; the base's source annotation is only a
/// hint for how many layers its bottom squash merged.
fn match_base(
    store: &Store,
    reports: &[LayerReport],
    base: &[Descriptor],
) -> Result<Option<(usize, Vec<LayerReport>)>> {
    let (n, m) = (reports.len(), base.len());
    let same = |a: &[LayerReport], b: &[Descriptor]| a.iter().zip(b).all(|(r, d)| r.erofs == d.digest);
    if m == 0 || n < m {
        return Ok(None);
    }
    if same(&reports[..m], base) {
        return Ok(Some((m, reports[..m].to_vec())));
    }
    let hint = base[0].annotation(ANN_SOURCE_DIGESTS).map(|s| s.split(',').count());
    let mut tried = std::collections::HashSet::new();
    for j in hint.into_iter().chain(2..=n + 1 - m) {
        let k = j + m - 1;
        if j < 2 || k > n || !tried.insert(j) || !same(&reports[j..k], &base[1..]) {
            continue;
        }
        let squashed = squash_bottom(store, reports[..k].to_vec(), j, SquashKind::Bottom)?;
        if squashed[0].erofs == base[0].digest {
            return Ok(Some((k, squashed)));
        }
    }
    Ok(None)
}

/// The base's manifest for `platform`: same os and architecture, and the same
/// variant when both name one (single-manifest kiln images record none).
pub(crate) fn platform_entry<'a>(loaded: &'a Loaded, platform: &Platform) -> Option<&'a KilnManifest> {
    loaded
        .entries
        .iter()
        .find(|(p, _)| same_platform(p, platform))
        .map(|(_, m)| m)
}

pub(crate) fn same_platform(a: &Platform, b: &Platform) -> bool {
    a.os == b.os
        && a.architecture == b.architecture
        && match (&a.variant, &b.variant) {
            (Some(x), Some(y)) => x == y,
            _ => true,
        }
}

/// Converts the app layers of one resolved image and commits its kiln manifest.
/// `source` provides the blobs of the layers that are not cache hits.
pub fn convert_image(
    store: &Store,
    img: &ResolvedImage,
    reference: Option<&str>,
    opts: &ConvertOptions,
    source: &dyn LayerSource,
) -> Result<Converted> {
    if opts.max_layers == 0 || opts.jobs == 0 {
        return Err(ImageError::BadOption("max-layers and jobs must be at least 1".into()));
    }
    let platform = img.config.platform();
    check_platform(&platform)?;
    let reports = convert_layers(store, img, 0..img.manifest.layers.len(), Vec::new(), opts, source)?;
    let (layers, squashed, base) = match &opts.base {
        None => {
            let (layers, squashed) = squash_to(store, reports, opts.max_layers, SquashKind::Bottom)?;
            (layers, squashed, None)
        }
        Some(b) => {
            let bm = platform_entry(&b.image, &platform)
                .ok_or_else(|| ImageError::NotOnBase(b.name.clone(), format!("it has no {platform} image")))?;
            let base_layers = app_layers(&bm.manifest);
            let m = base_layers.len();
            if opts.max_layers <= m {
                return Err(ImageError::BadOption(format!(
                    "the base has {m} layers, so --max-layers must be more than {m}"
                )));
            }
            let Some((k, mut layers)) = match_base(store, &reports, &base_layers)? else {
                return Err(ImageError::NotOnBase(
                    b.name.clone(),
                    "the image's layers do not start with the base's".into(),
                ));
            };
            if k == reports.len() {
                return Err(ImageError::NotOnBase(
                    b.name.clone(),
                    "the image has no layers above the base".into(),
                ));
            }
            let app = reports[k..].to_vec();
            let (app, squashed) = squash_to(store, app, opts.max_layers - m, SquashKind::Upper)?;
            layers.extend(app);
            let record = BaseRecord {
                name: b.name.clone(),
                manifest_digest: bm.digest.clone(),
                layers: m,
                source_layers: k,
            };
            (layers, squashed, Some(record))
        }
    };
    // The kernel, then the init layer (spec §5.1), each fetched by its pinned digest.
    let boot = opts.boot.for_arch(&platform.architecture);
    let (kernel_pin, init_pin) = boot
        .as_ref()
        .map_or((None, None), |b| (b.kernel.as_ref(), b.init.as_ref()));
    let mut boot_layers = Vec::new();
    // The fetcher (the Docker config's location) only when there is something to
    // fetch: a provisional convert never builds one.
    if boot.is_some() {
        let fetcher = RegistryFetcher::from_env();
        if let Some(pin) = kernel_pin {
            let kernel = artifacts::ensure(store, pin, &fetcher)?;
            boot_layers.push(Descriptor::new(KILN_KERNEL, kernel.clone(), store.blob_size(&kernel)?));
        }
        if let Some(pin) = init_pin {
            let init_bin = artifacts::ensure(store, pin, &fetcher)?;
            let init = artifacts::init_layer_blob(store, &init_bin)?;
            boot_layers.push(Descriptor::new(KILN_INIT, init.clone(), store.blob_size(&init)?));
        }
    }
    let config = KilnConfig {
        schema_version: SCHEMA_VERSION,
        architecture: platform.architecture.clone(),
        process: Process::from_oci(img.config.config.as_ref()),
        kernel: kernel_pin.map(|p| KernelRef {
            profile: p.name.to_string(),
            version: p.version.to_string(),
        }),
        init: init_pin.map(|p| InitRef {
            version: p.version.to_string(),
        }),
        source: SourceRef {
            manifest_digest: img.manifest_digest.clone(),
            reference: reference.map(str::to_string),
        },
        erofs_format_version: FORMAT_VERSION,
        labels: img
            .config
            .config
            .as_ref()
            .and_then(|c| c.labels.clone())
            .unwrap_or_default(),
        base,
    };
    let (manifest_digest, manifest_size) = commit_manifest(store, &config, boot_layers, &layers)?;
    Ok(Converted {
        platform,
        manifest_digest,
        manifest_size,
        layers,
        squashed,
        base: config.base,
        boot,
    })
}

/// An image's app layers: its erofs layers, without the kernel and init (spec §5.1).
pub(crate) fn app_layers(m: &ImageManifest) -> Vec<Descriptor> {
    m.layers
        .iter()
        .filter(|l| l.media_type == KILN_LAYER)
        .cloned()
        .collect()
}

/// Writes the config and the manifest (boot layers, then app layers); returns the
/// manifest's digest and size.
pub(crate) fn commit_manifest(
    store: &Store,
    config: &KilnConfig,
    boot: Vec<Descriptor>,
    layers: &[LayerReport],
) -> Result<(Digest, u64)> {
    let config_bytes = canonical_json(config);
    let config_digest = store.put_bytes(&config_bytes)?;
    let manifest = ImageManifest {
        schema_version: 2,
        media_type: Some(media::OCI_MANIFEST.to_string()),
        artifact_type: Some(KILN_ARTIFACT.to_string()),
        config: Descriptor::new(KILN_CONFIG, config_digest, config_bytes.len() as u64),
        layers: boot.into_iter().chain(layers.iter().map(layer_descriptor)).collect(),
        annotations: None,
    };
    let bytes = canonical_json(&manifest);
    Ok((store.put_bytes(&bytes)?, bytes.len() as u64))
}

fn layer_descriptor(l: &LayerReport) -> Descriptor {
    let mut d = Descriptor::new(KILN_LAYER, l.erofs.clone(), l.size);
    let sources: Vec<String> = l.sources.iter().map(Digest::to_string).collect();
    let mut ann = BTreeMap::from([(ANN_SOURCE_DIGESTS.to_string(), sources.join(","))]);
    if l.inherits {
        ann.insert(ANN_INHERITS.to_string(), "true".to_string());
    }
    d.annotations = Some(ann);
    d
}
