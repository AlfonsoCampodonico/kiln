//! Finding a base inside an image's converted layers (spec §16.2): the `k` such that
//! the image's first `k` source layers, layered as the base was, are exactly the
//! base's app layers. Only erofs digests decide; annotations and source manifests
//! only suggest which `k` to try, and a squash that does not match is never kept.

use std::collections::BTreeSet;

use kiln_erofs::Image;
use kiln_oci::{Descriptor, ImageManifest};
use kiln_store::{CacheKind, Digest, HashingReader, Store};

use crate::convert::{LayerReport, SquashKind, app_layers, squash_key, squash_report};
use crate::error::{ImageError, Result, json};
use crate::load::{KilnManifest, load_manifest};
use crate::types::ANN_SOURCE_DIGESTS;

/// How deep a chain of bases (a base built on a base, …) may go.
const MAX_CHAIN: usize = 8;

/// Merges `layers` as `kind` if that gives `want`. The squash is committed and
/// cached only when it matches.
pub(crate) fn squash_if(
    store: &Store,
    layers: &[LayerReport],
    kind: SquashKind,
    want: &Digest,
) -> Result<Option<LayerReport>> {
    let key = squash_key(layers, kind);
    if let Some(d) = store.cache_get_blob(CacheKind::Squash, &key)? {
        return (&d == want)
            .then(|| squash_report(store, layers, kind, d, true))
            .transpose();
    }
    let mut images = layers
        .iter()
        .map(|l| Ok(Image::open(store.open_blob(&l.erofs)?)?))
        .collect::<Result<Vec<_>>>()?;
    let tmp = store.tmp_blob()?;
    match kind {
        SquashKind::Bottom => kiln_erofs::squash(&mut images, tmp.reopen()?, &store.tmp_dir())?,
        SquashKind::Upper => kiln_erofs::squash_upper(&mut images, tmp.reopen()?, &store.tmp_dir())?,
    };
    let (digest, _) = HashingReader::new(tmp.reopen()?).finish_to_eof()?;
    if &digest != want {
        return Ok(None);
    }
    let d = store.commit(tmp)?;
    store.cache_put(CacheKind::Squash, &key, &d.to_string())?;
    Ok(Some(squash_report(store, layers, kind, d, false)?))
}

/// How many source layers a squashed layer says it merged (a hint only).
fn hinted_width(d: &Descriptor) -> Option<usize> {
    d.annotation(ANN_SOURCE_DIGESTS).map(|s| s.split(',').count())
}

/// The number of layers in a kiln image's source manifest, when it is in the store.
fn source_width(store: &Store, m: &KilnManifest) -> Option<usize> {
    let d = &m.config.source.manifest_digest;
    let bytes = store.has_blob(d).then(|| store.read_metadata(d).ok()).flatten()?;
    json::<ImageManifest>("source manifest", &bytes)
        .ok()
        .map(|s| s.layers.len())
}

/// Matches `want` (a run of layers whose bottom one may merge several of
/// `reports`) against `reports[start..]`: returns where the run ends and its
/// layers. `widths` are the merge widths worth trying for the bottom layer.
pub(crate) fn match_run(
    store: &Store,
    reports: &[LayerReport],
    start: usize,
    want: &[Descriptor],
    kind: SquashKind,
    widths: impl IntoIterator<Item = usize>,
) -> Result<Option<(usize, Vec<LayerReport>)>> {
    let (n, q) = (reports.len(), want.len());
    let same =
        |a: &[LayerReport], b: &[Descriptor]| a.len() == b.len() && a.iter().zip(b).all(|(r, d)| r.erofs == d.digest);
    if q == 0 {
        return Ok(Some((start, Vec::new())));
    }
    if start + q <= n && same(&reports[start..start + q], want) {
        return Ok(Some((start + q, reports[start..start + q].to_vec())));
    }
    // With other layers above the merged one, their exact match pins the width;
    // alone, only the suggested widths are tried, so a one-layer base never
    // squashes every prefix.
    let mut candidates: BTreeSet<usize> = widths.into_iter().filter(|&g| g >= 2).collect();
    if q > 1 {
        candidates.extend((2..=n.saturating_sub(start + q - 1)).filter(|&g| {
            let end = start + g + q - 1;
            end <= n && same(&reports[start + g..end], &want[1..])
        }));
    }
    for g in candidates {
        let end = start + g + q - 1;
        if end > n || !same(&reports[start + g..end], &want[1..]) {
            continue;
        }
        if let Some(merged) = squash_if(store, &reports[start..start + g], kind, &want[0].digest)? {
            let mut layers = vec![merged];
            layers.extend_from_slice(&reports[start + g..end]);
            return Ok(Some((end, layers)));
        }
    }
    Ok(None)
}

/// Where `base` ends inside `reports`, and the base's layers rebuilt from them:
/// `base`'s own base first when it records one (a chain), then its app half as an
/// upper squash; otherwise its layers as a bottom squash (spec §16.2).
pub(crate) fn match_base(
    store: &Store,
    reports: &[LayerReport],
    base: &KilnManifest,
) -> Result<Option<(usize, Vec<LayerReport>)>> {
    match_base_at(store, reports, base, 0)
}

fn match_base_at(
    store: &Store,
    reports: &[LayerReport],
    base: &KilnManifest,
    depth: usize,
) -> Result<Option<(usize, Vec<LayerReport>)>> {
    let want = app_layers(&base.manifest);
    if want.is_empty() {
        return Ok(None);
    }
    let source = source_width(store, base);
    let Some(r) = &base.config.base else {
        let widths = want
            .first()
            .and_then(hinted_width)
            .into_iter()
            .chain(source.map(|s| s + 1 - want.len()));
        return match_run(store, reports, 0, &want, SquashKind::Bottom, widths);
    };
    if depth >= MAX_CHAIN || r.layers > want.len() {
        return Ok(None);
    }
    let inner = match load_manifest(store, &r.manifest_digest) {
        Ok(m) => m,
        Err(ImageError::Store(_)) => return Ok(None),
        Err(e) => return Err(e),
    };
    let Some((k0, mut layers)) = match_base_at(store, reports, &inner, depth + 1)? else {
        return Ok(None);
    };
    if layers
        .iter()
        .map(|l| &l.erofs)
        .ne(want[..r.layers].iter().map(|d| &d.digest))
    {
        return Ok(None);
    }
    let app = &want[r.layers..];
    let widths = app
        .first()
        .and_then(hinted_width)
        .into_iter()
        .chain(source.map(|s| (s + 1).saturating_sub(r.source_layers + app.len())));
    let Some((k, top)) = match_run(store, reports, k0, app, SquashKind::Upper, widths)? else {
        return Ok(None);
    };
    layers.extend(top);
    Ok(Some((k, layers)))
}
