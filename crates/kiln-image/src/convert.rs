//! OCI image → kiln image (spec §6.2–§6.4).

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use kiln_erofs::{DirAttrs, FORMAT_VERSION, Image, LayerWriter, Limits};
use kiln_oci::media::{self, Compression};
use kiln_oci::{Descriptor, ImageManifest, Platform, ResolvedImage, canonical_json};
use kiln_store::{CacheKind, Digest, HashingReader, Store, TmpBlob};

use crate::ctx::{ctx_hash, parents_entry, parse_parents};
use crate::decompress::{Budget, Tripped, layer_limit, open_bounded};
use crate::error::{ImageError, Result};
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
}

impl Default for ConvertOptions {
    fn default() -> Self {
        Self {
            max_layers: 10,
            limits: Limits::default(),
            max_image_bytes: 64 << 30,
            max_expansion_ratio: 200,
            jobs: std::thread::available_parallelism().map_or(4, |n| n.get()),
        }
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
}

/// One converted platform.
#[derive(Debug, Clone)]
pub struct Converted {
    pub platform: Platform,
    pub manifest_digest: Digest,
    pub manifest_size: u64,
    pub layers: Vec<LayerReport>,
    /// How many source layers were merged into the bottom layer (0: no squash).
    pub squashed: usize,
}

/// A layer whose tar is consumed but whose metadata waits for its lowers.
struct Pending {
    tmp: TmpBlob,
    writer: LayerWriter<File>,
    implicit: Vec<Vec<u8>>,
    warnings: Vec<String>,
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
) -> Result<Pending> {
    let desc = &img.manifest.layers[i];
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
    Ok(Pending {
        tmp,
        writer,
        implicit,
        warnings: Vec::new(),
    })
}

/// Runs `stream_layer` for `todo` on up to `opts.jobs` threads; stops early on error.
fn stream_parallel(
    store: &Store,
    img: &ResolvedImage,
    todo: &[usize],
    opts: &ConvertOptions,
    budget: &Arc<Budget>,
) -> Result<BTreeMap<usize, Pending>> {
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
                    let r = stream_layer(store, img, i, opts, budget);
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

/// Finalises `p` against `lowers`, commits it, then writes its cache entries.
fn finish_layer(
    store: &Store,
    key: &str,
    p: Pending,
    lowers: &mut [Image<File>],
) -> Result<(Digest, bool, Vec<String>)> {
    let inherited: BTreeMap<Vec<u8>, DirAttrs> = if p.implicit.is_empty() {
        BTreeMap::new()
    } else {
        kiln_erofs::resolve_inherited(lowers, &p.implicit)?
    };
    let (_out, summary) = p.writer.finish(&inherited)?;
    let d = store.commit(p.tmp)?;
    if p.implicit.is_empty() {
        store.cache_put(CacheKind::Layers, key, &format!("erofs {d}"))?;
    } else {
        let ctx = ctx_hash(&p.implicit, &inherited);
        store.cache_put(CacheKind::LayersCtx, &format!("{key}@{ctx}"), &d.to_string())?;
        store.cache_put(CacheKind::Layers, key, &parents_entry(&p.implicit))?;
    }
    let mut warnings = p.warnings;
    warnings.extend(summary.warnings);
    Ok((d, !p.implicit.is_empty(), warnings))
}

fn squash_key(layers: &[LayerReport]) -> String {
    let joined: Vec<String> = layers.iter().map(|l| l.erofs.to_string()).collect();
    format!("{}@{FORMAT_VERSION}", Digest::of(joined.join("\n").as_bytes()).hex())
}

/// Merges the bottom `k` layers into one (spec §6.4).
fn squash_bottom(store: &Store, layers: Vec<LayerReport>, k: usize) -> Result<Vec<LayerReport>> {
    let (bottom, rest) = layers.split_at(k);
    let key = squash_key(bottom);
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
            kiln_erofs::squash(&mut images, tmp.reopen()?, &store.tmp_dir())?;
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
    }];
    out.extend_from_slice(rest);
    Ok(out)
}

/// Converts the app layers of one resolved image and commits its kiln manifest.
pub fn convert_image(
    store: &Store,
    img: &ResolvedImage,
    reference: Option<&str>,
    opts: &ConvertOptions,
) -> Result<Converted> {
    if opts.max_layers == 0 || opts.jobs == 0 {
        return Err(ImageError::BadOption("max-layers and jobs must be at least 1".into()));
    }
    let platform = img.config.platform();
    check_platform(&platform)?;
    let layers = &img.manifest.layers;
    let plans = layers
        .iter()
        .enumerate()
        .map(|(i, d)| plan_layer(store, d, &img.config.rootfs.diff_ids[i]))
        .collect::<Result<Vec<_>>>()?;
    let todo: Vec<usize> = plans
        .iter()
        .enumerate()
        .filter(|(_, p)| matches!(p, Plan::Convert))
        .map(|(i, _)| i)
        .collect();
    let budget = Arc::new(Budget::default());
    let mut pending = stream_parallel(store, img, &todo, opts, &budget)?;

    // Bottom-up: each layer's lowers are final before it is.
    let mut reports: Vec<LayerReport> = Vec::new();
    let mut lowers: Vec<Image<File>> = Vec::new();
    for (i, plan) in plans.into_iter().enumerate() {
        let src = &layers[i].digest;
        let lkey = layer_key(src, &img.config.rootfs.diff_ids[i]);
        let (erofs, cached, inherits, warnings) = match plan {
            Plan::Done(d) => (d, true, false, Vec::new()),
            Plan::Parents(paths) => {
                let inherited = kiln_erofs::resolve_inherited(&mut lowers, &paths)?;
                let key = format!("{lkey}@{}", ctx_hash(&paths, &inherited));
                match store.cache_get_blob(CacheKind::LayersCtx, &key)? {
                    Some(d) => (d, true, true, Vec::new()),
                    None => {
                        let p = stream_layer(store, img, i, opts, &budget)?;
                        let (d, inherits, w) = finish_layer(store, &lkey, p, &mut lowers)?;
                        (d, false, inherits, w)
                    }
                }
            }
            Plan::Convert => {
                let p = pending.remove(&i).expect("streamed in phase A");
                let (d, inherits, w) = finish_layer(store, &lkey, p, &mut lowers)?;
                (d, false, inherits, w)
            }
        };
        lowers.push(Image::open(store.open_blob(&erofs)?)?);
        let size = store.blob_size(&erofs)?;
        reports.push(LayerReport {
            sources: vec![src.clone()],
            erofs,
            size,
            cached,
            inherits,
            warnings,
        });
    }
    drop(lowers);

    let mut squashed = 0;
    if reports.len() > opts.max_layers {
        squashed = reports.len() - opts.max_layers + 1;
        reports = squash_bottom(store, reports, squashed)?;
    }

    let config = KilnConfig {
        schema_version: SCHEMA_VERSION,
        architecture: platform.architecture.clone(),
        process: Process::from_oci(img.config.config.as_ref()),
        kernel: None,
        init: None,
        source: SourceRef {
            manifest_digest: img.manifest_digest.clone(),
            reference: reference.map(str::to_string),
        },
        erofs_format_version: FORMAT_VERSION,
    };
    let config_bytes = canonical_json(&config);
    let config_digest = store.put_bytes(&config_bytes)?;
    let manifest = ImageManifest {
        schema_version: 2,
        media_type: Some(media::OCI_MANIFEST.to_string()),
        artifact_type: Some(KILN_ARTIFACT.to_string()),
        config: Descriptor::new(KILN_CONFIG, config_digest, config_bytes.len() as u64),
        layers: reports.iter().map(layer_descriptor).collect(),
        annotations: None,
    };
    let bytes = canonical_json(&manifest);
    let manifest_digest = store.put_bytes(&bytes)?;
    Ok(Converted {
        platform,
        manifest_digest,
        manifest_size: bytes.len() as u64,
        layers: reports,
        squashed,
    })
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
