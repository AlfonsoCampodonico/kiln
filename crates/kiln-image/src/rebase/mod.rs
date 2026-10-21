//! `kiln rebase`: swap an image's base layers for a patched base (spec §16).

pub mod gates;

use std::collections::BTreeSet;
use std::fs::File;
use std::io::Read;

use kiln_erofs::ondisk::{S_IFDIR, S_IFLNK, S_IFMT, S_IFREG};
use kiln_erofs::{Image, Merged, XattrKey};
use kiln_oci::media::{OCI_INDEX, OCI_MANIFEST};
use kiln_oci::{Descriptor, ImageConfig, ImageManifest, Platform, ResolvedImage};
use kiln_store::{Digest, Hasher, Store};

use crate::convert::{
    ConvertOptions, LayerReport, LayerSource, SquashKind, app_layers, commit_manifest, convert_layers, platform_entry,
    squash_to,
};
use crate::error::{ImageError, Result, json};
use crate::load::{KilnManifest, Loaded, load, resolve_name};
use crate::pipeline::write_index;
use crate::types::{ANN_INHERITS, ANN_SOURCE_DIGESTS, BaseRecord, KILN_LAYER, KilnConfig};
use gates::{BaseView, PathSig, Refusal, Shadowed};

/// What to rebase: names or digests in the store.
#[derive(Debug, Clone)]
pub struct RebaseRequest<'a> {
    pub old: &'a str,
    pub new: &'a str,
    pub image: &'a str,
    /// Name to record in `refs.json`.
    pub tag: Option<&'a str>,
}

/// One app layer of the rebased image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppLayer {
    pub erofs: Digest,
    /// The image's app layer at the same position, if it had one.
    pub was: Option<Digest>,
}

impl AppLayer {
    pub fn changed(&self) -> bool {
        self.was.as_ref() != Some(&self.erofs)
    }
}

/// One rebased platform.
#[derive(Debug, Clone)]
pub struct RebasedImage {
    pub platform: Platform,
    pub manifest_digest: Digest,
    pub base_layers: usize,
    pub app: Vec<AppLayer>,
    /// App source layers that inherit parent attributes, re-resolved in the new base.
    pub refinalised: usize,
    /// How many app layers were merged into the bottom app layer (0: none).
    pub squashed: usize,
}

#[derive(Debug, Clone)]
pub struct Rebased {
    pub digest: Digest,
    pub media_type: &'static str,
    pub images: Vec<RebasedImage>,
}

/// Source tars must already be in the store: rebase never fetches.
struct StoredOnly<'a>(&'a Store);

impl LayerSource for StoredOnly<'_> {
    fn fetch(&self, layer: &Descriptor) -> Result<()> {
        if self.0.has_blob(&layer.digest) {
            Ok(())
        } else {
            Err(ImageError::SourceMissing(format!(
                "source layer {} is no longer in the store",
                layer.digest
            )))
        }
    }
}

/// The image's source OCI manifest and config, which re-finalisation needs (§16.4).
fn source_image(store: &Store, m: &KilnManifest, platform: &Platform) -> Result<ResolvedImage> {
    let digest = &m.config.source.manifest_digest;
    let missing = || ImageError::SourceMissing(format!("the source manifest {digest} is no longer in the store"));
    if !store.has_blob(digest) {
        return Err(missing());
    }
    let manifest: ImageManifest = json("source manifest", &store.read_metadata(digest)?)?;
    if !store.has_blob(&manifest.config.digest) {
        return Err(missing());
    }
    let config: ImageConfig = json("source config", &store.read_metadata(&manifest.config.digest)?)?;
    if config.rootfs.diff_ids.len() != manifest.layers.len() {
        return Err(ImageError::SourceMissing(format!(
            "the source manifest {digest} is inconsistent"
        )));
    }
    Ok(ResolvedImage {
        platform: platform.clone(),
        manifest_digest: digest.clone(),
        config_digest: manifest.config.digest.clone(),
        manifest,
        config,
        ref_name: None,
    })
}

/// The digests of an image's app layers (kernel and init excluded).
fn digests(m: &KilnManifest) -> Vec<Digest> {
    app_layers(&m.manifest).into_iter().map(|l| l.digest).collect()
}

fn merged(store: &Store, m: &KilnManifest) -> Result<Merged<File>> {
    let images = digests(m)
        .iter()
        .map(|d| Ok(Image::open(store.open_blob(d)?)?))
        .collect::<Result<Vec<_>>>()?;
    Ok(Merged::new(images))
}

/// A base's merged view as the shadowed-path gate reads it.
struct ErofsView(Merged<File>);

impl BaseView for ErofsView {
    fn sig(&mut self, path: &[u8]) -> Result<Option<PathSig>> {
        let Some((layer, nid)) = self.0.lookup(path)? else {
            return Ok(None);
        };
        let img = self.0.image(layer);
        let info = img.inode(nid)?;
        let mut xattrs = img.xattrs(nid)?;
        xattrs.retain(|k, _| !k.is_overlay());
        let kind = info.mode & S_IFMT;
        let (size, content, target) = match kind {
            S_IFREG => {
                let mut h = Hasher::new();
                std::io::copy(&mut img.data_reader(nid)?, &mut h)?;
                (info.size, Some(h.finish()), None)
            }
            S_IFLNK => (0, None, Some(img.readlink(nid)?)),
            _ => (0, None, None),
        };
        Ok(Some(PathSig {
            kind,
            mode: info.mode & 0o7777,
            uid: info.uid,
            gid: info.gid,
            xattrs,
            size,
            content,
            target,
            rdev: info.rdev,
        }))
    }

    fn subtree(&mut self, path: &[u8]) -> Result<Vec<Vec<u8>>> {
        Ok(self.0.subtree(path)?)
    }
}

/// How large an os-release file may be.
const OS_RELEASE_MAX: u64 = 64 << 10;

/// `/etc/os-release`, else `/usr/lib/os-release`, following symlinks in the final
/// component (Debian links the first to the second).
fn os_release(view: &mut Merged<File>) -> Result<Option<String>> {
    for start in [&b"etc/os-release"[..], b"usr/lib/os-release"] {
        let mut path = start.to_vec();
        for _ in 0..8 {
            let Some((layer, nid)) = view.lookup(&path)? else { break };
            let img = view.image(layer);
            let info = img.inode(nid)?;
            match info.mode & S_IFMT {
                S_IFLNK => {
                    let target = img.readlink(nid)?;
                    path = match target.strip_prefix(b"/") {
                        Some(abs) => abs.to_vec(),
                        None => {
                            let dir = path.iter().rposition(|&b| b == b'/').map_or(&b""[..], |i| &path[..i]);
                            [dir, b"/", &target].concat()
                        }
                    };
                    path = normalise(&path);
                }
                S_IFREG => {
                    let mut text = Vec::new();
                    img.data_reader(nid)?.take(OS_RELEASE_MAX).read_to_end(&mut text)?;
                    return Ok(Some(String::from_utf8_lossy(&text).into_owned()));
                }
                _ => break,
            }
        }
    }
    Ok(None)
}

/// Resolves `.` and `..` components (never above the root).
fn normalise(path: &[u8]) -> Vec<u8> {
    let mut out: Vec<&[u8]> = Vec::new();
    for c in path.split(|&b| b == b'/') {
        match c {
            b"" | b"." => {}
            b".." => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out.join(&b'/')
}

/// Every path the app's source layers contain, except their implicit directories
/// (spec §16.3).
fn shadowed(store: &Store, app: &[LayerReport]) -> Result<Vec<Shadowed>> {
    let mut out = BTreeSet::new();
    for l in app {
        let implicit: BTreeSet<&[u8]> = l.implicit.iter().map(Vec::as_slice).collect();
        let mut img = Image::open(store.open_blob(&l.erofs)?)?;
        for (path, nid) in img.walk()? {
            if implicit.contains(path.as_slice()) {
                continue;
            }
            let info = img.inode(nid)?;
            let hides = if info.mode & S_IFMT == S_IFDIR {
                img.xattrs(nid)?
                    .get(&XattrKey::opaque())
                    .is_some_and(|v| v.as_slice() == b"y")
            } else {
                true
            };
            out.insert(Shadowed { path, hides });
        }
    }
    Ok(out.into_iter().collect())
}

/// A base layer carried into the rebased manifest as it is.
fn base_layer(d: &Descriptor) -> LayerReport {
    let sources = d
        .annotation(ANN_SOURCE_DIGESTS)
        .map(|s| s.split(',').filter_map(|x| Digest::parse(x).ok()).collect())
        .unwrap_or_default();
    LayerReport {
        sources,
        erofs: d.digest.clone(),
        size: d.size,
        cached: true,
        inherits: d.annotation(ANN_INHERITS).is_some(),
        warnings: Vec::new(),
        implicit: Vec::new(),
    }
}

/// Rebases one platform; `Ok(Err(..))` when gates refuse.
fn rebase_one(
    store: &Store,
    req: &RebaseRequest,
    platform: &Platform,
    im: &KilnManifest,
    old: &Loaded,
    new: &Loaded,
    opts: &ConvertOptions,
) -> Result<std::result::Result<RebasedImage, Vec<Refusal>>> {
    let platforms = |l: &Loaded| l.entries.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>();
    if let Err(r) = gates::platform(platform, &platforms(old), &platforms(new)) {
        return Ok(Err(vec![r]));
    }
    let om = platform_entry(old, platform).expect("platform gate passed");
    let nm = platform_entry(new, platform).expect("platform gate passed");
    if let Err(r) = gates::bottom(im.config.base.as_ref(), &digests(im), &digests(om)) {
        return Ok(Err(vec![r]));
    }
    let record = im.config.base.as_ref().expect("bottom gate passed");
    let (mut old_view, mut new_view) = (merged(store, om)?, merged(store, nm)?);
    let mut refusals: Vec<Refusal> = [
        gates::distro(
            os_release(&mut old_view)?.as_deref(),
            os_release(&mut new_view)?.as_deref(),
        ),
        gates::runtime(&om.config.labels, &nm.config.labels),
        gates::env(&om.config.process.env, &nm.config.process.env),
        gates::process(&om.config.process, &nm.config.process),
    ]
    .into_iter()
    .filter_map(std::result::Result::err)
    .collect();

    // Re-derive the app from its sources above the new base (§16.4).
    let derived = source_image(store, im, platform).and_then(|src| {
        let n = src.manifest.layers.len();
        if record.source_layers >= n {
            return Err(ImageError::SourceMissing(format!(
                "the source manifest has {n} layers, but the base record claims {}",
                record.source_layers
            )));
        }
        convert_layers(
            store,
            &src,
            record.source_layers..n,
            digests(nm),
            opts,
            &StoredOnly(store),
        )
    });
    let app = match derived {
        Ok(app) => app,
        // A refusal says more than a missing source; report it first.
        Err(_) if !refusals.is_empty() => return Ok(Err(refusals)),
        Err(e) => return Err(e),
    };
    let (mut ov, mut nv) = (ErofsView(old_view), ErofsView(new_view));
    if let Err(r) = gates::shadow(&shadowed(store, &app)?, &mut ov, &mut nv)? {
        refusals.push(r);
    }
    if !refusals.is_empty() {
        return Ok(Err(refusals));
    }

    let new_base = app_layers(&nm.manifest);
    let m = new_base.len();
    if opts.max_layers <= m {
        return Err(ImageError::BadOption(format!(
            "the new base has {m} layers, so --max-layers must be more than {m}"
        )));
    }
    let refinalised = app.iter().filter(|l| l.inherits).count();
    let (app, squashed) = squash_to(store, app, opts.max_layers - m, SquashKind::Upper)?;
    let was = &app_layers(&im.manifest)[record.layers..];
    let app_layers = app
        .iter()
        .enumerate()
        .map(|(i, l)| AppLayer {
            erofs: l.erofs.clone(),
            was: was.get(i).map(|d| d.digest.clone()),
        })
        .collect();
    let mut layers: Vec<LayerReport> = new_base.iter().map(base_layer).collect();
    layers.extend(app);

    let mut config: KilnConfig = im.config.clone();
    config.process.env = gates::merge_env(&im.config.process.env, &om.config.process.env, &nm.config.process.env);
    config.labels = gates::merge_labels(&im.config.labels, &om.config.labels, &nm.config.labels);
    config.base = Some(BaseRecord {
        name: req.new.to_string(),
        manifest_digest: nm.digest.clone(),
        layers: m,
        source_layers: record.source_layers,
    });
    // The image keeps its own kernel and init layers: the base supplies only app layers.
    let boot: Vec<Descriptor> = im
        .manifest
        .layers
        .iter()
        .filter(|l| l.media_type != KILN_LAYER)
        .cloned()
        .collect();
    let (manifest_digest, _) = commit_manifest(store, &config, boot, &layers)?;
    Ok(Ok(RebasedImage {
        platform: platform.clone(),
        manifest_digest,
        base_layers: m,
        app: app_layers,
        refinalised,
        squashed,
    }))
}

/// Rebases every platform of `req.image` from `req.old` onto `req.new` (spec §16),
/// then tags the result. Nothing is tagged unless every platform passes every gate.
pub fn rebase(store: &Store, req: &RebaseRequest, opts: &ConvertOptions) -> Result<Rebased> {
    if opts.max_layers == 0 || opts.jobs == 0 {
        return Err(ImageError::BadOption("max-layers and jobs must be at least 1".into()));
    }
    let _lock = store.lock_shared()?;
    let image = load(store, &resolve_name(store, req.image)?)?;
    let old = load(store, &resolve_name(store, req.old)?)?;
    let new = load(store, &resolve_name(store, req.new)?)?;
    let mut images = Vec::new();
    let mut refusals = Vec::new();
    for (platform, im) in &image.entries {
        match rebase_one(store, req, platform, im, &old, &new, opts)? {
            Ok(r) => images.push(r),
            Err(rs) if image.is_index => refusals.extend(rs.into_iter().map(|r| Refusal {
                reason: format!("{platform}: {}", r.reason),
                ..r
            })),
            Err(rs) => refusals.extend(rs),
        }
    }
    if !refusals.is_empty() {
        return Err(ImageError::RebaseRefused(refusals));
    }
    let (digest, media_type) = if image.is_index {
        let entries = images
            .iter()
            .map(|r| {
                let size = store.blob_size(&r.manifest_digest)?;
                Ok((r.platform.clone(), r.manifest_digest.clone(), size))
            })
            .collect::<Result<Vec<_>>>()?;
        (write_index(store, entries)?, OCI_INDEX)
    } else {
        (images[0].manifest_digest.clone(), OCI_MANIFEST)
    };
    if let Some(tag) = req.tag {
        store.set_ref(tag, &digest)?;
    }
    Ok(Rebased {
        digest,
        media_type,
        images,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalise_resolves_dots() {
        assert_eq!(normalise(b"etc/../usr/lib/os-release"), b"usr/lib/os-release");
        assert_eq!(normalise(b"../../x/./y"), b"x/y");
    }
}
