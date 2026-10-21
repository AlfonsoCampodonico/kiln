//! Merged (overlayfs) views over kiln layers: parent inheritance (§6.3) and squash (§6.4).

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{Read, Seek, Write};
use std::path::Path;

use crate::error::{Error, Result};
use crate::limits::Limits;
use crate::ondisk::{S_IFBLK, S_IFCHR, S_IFDIR, S_IFIFO, S_IFLNK, S_IFMT, S_IFREG};
use crate::path::components;
use crate::reader::{Image, InodeInfo};
use crate::tree::{Data, DirAttrs, Kind, Meta, Node, NodeId, Timestamp, Tree, XattrKey, Xattrs};
use crate::writer::{DataStore, ExternalData, LayerSummary, emit};

fn meta_of(info: &InodeInfo) -> Meta {
    Meta {
        mode: info.mode & 0o7777,
        uid: info.uid,
        gid: info.gid,
        mtime: info.mtime,
    }
}

/// Attributes of `paths` in the merged view of `lowers` (bottom first). Paths that
/// are absent or not directories there are omitted; overlay xattrs are stripped.
pub fn resolve_inherited<R: Read + Seek>(
    lowers: &mut [Image<R>],
    paths: &[Vec<u8>],
) -> Result<BTreeMap<Vec<u8>, DirAttrs>> {
    let mut out = BTreeMap::new();
    let mut dirs = DirCache::new();
    for path in paths {
        let Some((layer, nid)) = merged_lookup(lowers, &mut dirs, path)? else {
            continue;
        };
        let img = &mut lowers[layer];
        let info = img.inode(nid)?;
        if !info.is_dir() {
            continue;
        }
        let mut xattrs = img.xattrs(nid)?;
        xattrs.retain(|k, _| !k.is_overlay());
        out.insert(
            path.clone(),
            DirAttrs {
                meta: meta_of(&info),
                xattrs,
            },
        );
    }
    Ok(out)
}

/// overlayfs treats a directory as opaque only when `trusted.overlay.opaque` is exactly `y`.
fn is_opaque(xattrs: &Xattrs) -> bool {
    xattrs.get(&XattrKey::opaque()).is_some_and(|v| v.as_slice() == b"y")
}

/// Parsed directories by `(layer, nid)`, so resolving many paths reads each directory once.
type DirCache = HashMap<(usize, u64), HashMap<Vec<u8>, u64>>;

fn child_nid<R: Read + Seek>(
    img: &mut Image<R>,
    dirs: &mut DirCache,
    layer: usize,
    dir: u64,
    name: &[u8],
) -> Result<Option<u64>> {
    let entries = match dirs.entry((layer, dir)) {
        Entry::Occupied(e) => e.into_mut(),
        Entry::Vacant(e) => e.insert(img.read_dir(dir)?.into_iter().map(|e| (e.name, e.nid)).collect()),
    };
    Ok(entries.get(name).copied())
}

/// The topmost `(layer, nid)` providing `path` in the overlay of `layers`.
fn merged_lookup<R: Read + Seek>(
    layers: &mut [Image<R>],
    dirs: &mut DirCache,
    path: &[u8],
) -> Result<Option<(usize, u64)>> {
    let comps: Vec<&[u8]> = components(path).collect();
    for layer in (0..layers.len()).rev() {
        let img = &mut layers[layer];
        let mut cur = img.root_nid();
        if comps.is_empty() {
            return Ok(Some((layer, cur)));
        }
        // overlayfs ignores an opaque marker on a layer's root directory.
        let mut opaque_above = false;
        for (i, c) in comps.iter().enumerate() {
            let Some(nid) = child_nid(img, dirs, layer, cur, c)? else {
                break;
            };
            let info = img.inode(nid)?;
            if info.is_whiteout() {
                return Ok(None);
            }
            if i + 1 == comps.len() {
                return Ok(Some((layer, nid)));
            }
            if !info.is_dir() {
                return Ok(None);
            }
            if is_opaque(&img.xattrs(nid)?) {
                opaque_above = true;
            }
            cur = nid;
        }
        if opaque_above {
            return Ok(None);
        }
    }
    Ok(None)
}

/// The merged (overlayfs) view of a layer stack, bottom first, for comparing what
/// two bases show at the same paths (spec §16.3). Layers may be hostile (a pulled
/// base), so walks are bounded: a directory linked twice in a layer is an error,
/// and depth and entry counts are capped by [`Limits`] (spec §7.6).
pub struct Merged<R> {
    layers: Vec<Image<R>>,
    dirs: DirCache,
    limits: Limits,
}

impl<R: Read + Seek> Merged<R> {
    pub fn new(layers: Vec<Image<R>>) -> Self {
        Self::with_limits(layers, Limits::default())
    }

    /// `limits.max_path_depth` bounds walk depth and `limits.max_entries` the paths
    /// one [`Merged::subtree`] returns.
    pub fn with_limits(layers: Vec<Image<R>>, limits: Limits) -> Self {
        Self {
            layers,
            dirs: DirCache::new(),
            limits,
        }
    }

    /// The topmost `(layer, nid)` showing `path`, or `None` if it is absent or hidden.
    pub fn lookup(&mut self, path: &[u8]) -> Result<Option<(usize, u64)>> {
        merged_lookup(&mut self.layers, &mut self.dirs, path)
    }

    pub fn image(&mut self, layer: usize) -> &mut Image<R> {
        &mut self.layers[layer]
    }

    /// `(layer, nid)` of every layer's own directory at `path`, ignoring overlay
    /// semantics: the directories a merged `read_dir` unions.
    fn layer_dirs(&mut self, path: &[u8]) -> Result<Vec<(usize, u64)>> {
        let comps: Vec<&[u8]> = components(path).collect();
        let mut out = Vec::new();
        for layer in 0..self.layers.len() {
            let img = &mut self.layers[layer];
            let mut cur = Some(img.root_nid());
            for c in &comps {
                cur = match cur {
                    Some(d) if img.inode(d)?.is_dir() => child_nid(img, &mut self.dirs, layer, d, c)?,
                    _ => None,
                };
            }
            if let Some(d) = cur
                && img.inode(d)?.is_dir()
            {
                out.push((layer, d));
            }
        }
        Ok(out)
    }

    fn names(&mut self, path: &[u8], dirs: &[(usize, u64)]) -> Result<Vec<Vec<u8>>> {
        let mut names = std::collections::BTreeSet::new();
        for &(layer, d) in dirs {
            names.extend(self.layers[layer].read_dir(d)?.into_iter().map(|e| e.name));
        }
        let mut out = Vec::new();
        for name in names {
            if self.lookup(&join(path, &name))?.is_some() {
                out.push(name);
            }
        }
        Ok(out)
    }

    /// Sorted names visible in the merged directory at `path` (empty unless it is one).
    pub fn read_dir(&mut self, path: &[u8]) -> Result<Vec<Vec<u8>>> {
        match self.lookup(path)? {
            Some((layer, nid)) if self.layers[layer].inode(nid)?.is_dir() => {}
            _ => return Ok(Vec::new()),
        }
        let dirs = self.layer_dirs(path)?;
        self.names(path, &dirs)
    }

    /// Every path visible below `path` in the merged view, sorted, `path` excluded.
    pub fn subtree(&mut self, path: &[u8]) -> Result<Vec<Vec<u8>>> {
        let too_many = |limit, max: usize, p: &[u8]| Error::LimitExceeded {
            limit,
            max: max as u64,
            path: String::from_utf8_lossy(p).into_owned(),
        };
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut stack = vec![(path.to_vec(), components(path).count())];
        while let Some((dir, depth)) = stack.pop() {
            match self.lookup(&dir)? {
                Some((layer, nid)) if self.layers[layer].inode(nid)?.is_dir() => {}
                _ => continue,
            }
            let dirs = self.layer_dirs(&dir)?;
            // In a well-formed layer each directory sits at one path, so meeting one
            // again means a cycle or a shared directory.
            for &(layer, nid) in &dirs {
                if !seen.insert((layer, nid)) {
                    return Err(Error::Corrupt(format!(
                        "directory nid {nid} of layer {layer} is linked twice"
                    )));
                }
            }
            for name in self.names(&dir, &dirs)? {
                let child = join(&dir, &name);
                if depth + 1 > self.limits.max_path_depth {
                    return Err(too_many("path depth", self.limits.max_path_depth, &child));
                }
                if out.len() as u64 >= self.limits.max_entries {
                    return Err(too_many("merged entries", self.limits.max_entries as usize, &child));
                }
                stack.push((child.clone(), depth + 1));
                out.push(child);
            }
        }
        out.sort();
        Ok(out)
    }
}

/// `dir/name`, where `b""` is the root.
pub(crate) fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    if dir.is_empty() {
        name.to_vec()
    } else {
        [dir, b"/", name].concat()
    }
}

/// Reads an image's tree; file data stays in the image (`Data::External`).
fn tree_from_image<R: Read + Seek>(img: &mut Image<R>, layer: usize) -> Result<Tree> {
    let root_nid = img.root_nid();
    let root_info = img.inode(root_nid)?;
    let mut tree = Tree::new(Node {
        kind: Kind::Dir {
            children: BTreeMap::new(),
            implicit: false,
        },
        meta: meta_of(&root_info),
        xattrs: img.xattrs(root_nid)?,
    });
    let mut by_nid: HashMap<u64, NodeId> = HashMap::new();
    by_nid.insert(root_nid, tree.root);
    let mut queue = VecDeque::from([(root_nid, tree.root)]);
    while let Some((dir_nid, dir_id)) = queue.pop_front() {
        for e in img.read_dir(dir_nid)? {
            let id = match by_nid.get(&e.nid) {
                Some(&id) if tree.nodes[id].is_dir() => {
                    return Err(Error::Corrupt(format!("directory nid {} is linked twice", e.nid)));
                }
                Some(&id) => id,
                None => {
                    let info = img.inode(e.nid)?;
                    let kind = match info.mode & S_IFMT {
                        S_IFDIR => Kind::Dir {
                            children: BTreeMap::new(),
                            implicit: false,
                        },
                        S_IFREG => Kind::File {
                            size: info.size,
                            data: Data::External { layer, nid: e.nid },
                        },
                        S_IFLNK => Kind::Symlink {
                            target: img.readlink(e.nid)?,
                        },
                        S_IFCHR => Kind::CharDev {
                            major: info.rdev.0,
                            minor: info.rdev.1,
                        },
                        S_IFBLK => Kind::BlockDev {
                            major: info.rdev.0,
                            minor: info.rdev.1,
                        },
                        S_IFIFO => Kind::Fifo,
                        other => return Err(Error::Corrupt(format!("unsupported inode type {other:#o}"))),
                    };
                    let node = Node {
                        kind,
                        meta: meta_of(&info),
                        xattrs: img.xattrs(e.nid)?,
                    };
                    let is_dir = node.is_dir();
                    let id = tree.add(node);
                    by_nid.insert(e.nid, id);
                    if is_dir {
                        queue.push_back((e.nid, id));
                    }
                    id
                }
            };
            tree.children_mut(dir_id).insert(e.name, id);
        }
    }
    Ok(tree)
}

fn set_dir_attrs(merged: &mut Tree, id: NodeId, src: &Node) {
    let node = &mut merged.nodes[id];
    node.meta = src.meta.clone();
    node.xattrs = src
        .xattrs
        .iter()
        .filter(|(k, _)| !k.is_overlay())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
}

/// Applies `upper` on top of `merged` with overlayfs semantics.
fn overlay(merged: &mut Tree, upper: &Tree) {
    let merged_root = merged.root;
    set_dir_attrs(merged, merged_root, &upper.nodes[upper.root]);
    let mut mapped: HashMap<NodeId, NodeId> = HashMap::new();
    let mut queue = VecDeque::from([(upper.root, merged_root)]);
    while let Some((upper_dir, merged_dir)) = queue.pop_front() {
        // As in overlayfs, an opaque root hides nothing.
        if upper_dir != upper.root && is_opaque(&upper.nodes[upper_dir].xattrs) {
            merged.children_mut(merged_dir).clear();
        }
        for (name, &uc) in upper.children(upper_dir).expect("queued nodes are directories") {
            let un = &upper.nodes[uc];
            if un.is_whiteout() {
                merged.children_mut(merged_dir).remove(name);
                continue;
            }
            if un.is_dir() {
                let target = match merged.child(merged_dir, name) {
                    Some(m) if merged.nodes[m].is_dir() => m,
                    _ => {
                        let m = merged.add(Node::dir(un.meta.clone(), false));
                        merged.children_mut(merged_dir).insert(name.clone(), m);
                        m
                    }
                };
                set_dir_attrs(merged, target, un);
                queue.push_back((uc, target));
            } else {
                let m = *mapped.entry(uc).or_insert_with(|| merged.add(un.clone()));
                merged.children_mut(merged_dir).insert(name.clone(), m);
            }
        }
    }
}

/// Applies `upper` on top of `merged`, where both sit above layers that are not
/// part of the merge (spec §6.4, upper squash). Whiteouts and opaque markers that
/// may hide those layers are kept, and a directory that replaces a lower
/// non-directory or whiteout becomes opaque, as overlayfs would show it.
fn overlay_upper(merged: &mut Tree, upper: &Tree) {
    let merged_root = merged.root;
    set_dir_attrs(merged, merged_root, &upper.nodes[upper.root]);
    let mut mapped: HashMap<NodeId, NodeId> = HashMap::new();
    let mut queue = VecDeque::from([(upper.root, merged_root)]);
    while let Some((upper_dir, merged_dir)) = queue.pop_front() {
        for (name, &uc) in upper.children(upper_dir).expect("queued nodes are directories") {
            let un = &upper.nodes[uc];
            if !un.is_dir() {
                let m = *mapped.entry(uc).or_insert_with(|| merged.add(un.clone()));
                merged.children_mut(merged_dir).insert(name.clone(), m);
                continue;
            }
            let opaque = is_opaque(&un.xattrs);
            let (target, keep_opaque) = match merged.child(merged_dir, name) {
                Some(m) if merged.nodes[m].is_dir() => {
                    let was = is_opaque(&merged.nodes[m].xattrs);
                    if opaque {
                        merged.children_mut(m).clear();
                    }
                    (m, was || opaque)
                }
                below => {
                    // A lower non-directory or whiteout ends the lookup in overlayfs,
                    // so nothing beneath the merge may show through this directory.
                    let m = merged.add(Node::dir(un.meta.clone(), false));
                    merged.children_mut(merged_dir).insert(name.clone(), m);
                    (m, opaque || below.is_some())
                }
            };
            set_dir_attrs(merged, target, un);
            if keep_opaque {
                merged.nodes[target].xattrs.insert(XattrKey::opaque(), b"y".to_vec());
            }
            queue.push_back((uc, target));
        }
    }
}

struct Sources<'a, R>(&'a mut [Image<R>]);

impl<R: Read + Seek> ExternalData for Sources<'_, R> {
    fn open(&mut self, layer: usize, nid: u64) -> Result<Box<dyn Read + '_>> {
        Ok(Box::new(self.0[layer].data_reader(nid)?))
    }
}

/// Merges `layers` (bottom first) into one bottom layer with no overlay markers.
///
/// `out` has the same contract as for `LayerWriter::new`: it must be empty, readable
/// as well as writable, and not in append mode.
pub fn squash<R: Read + Seek, W: Read + Write + Seek>(
    layers: &mut [Image<R>],
    out: W,
    spill_dir: &Path,
) -> Result<(W, LayerSummary)> {
    merge_into(layers, out, spill_dir, overlay)
}

/// Merges `layers` (bottom first) into one layer that still sits above other
/// layers: unlike [`squash`], it keeps the whiteouts and opaque markers that hide
/// content below the merged range (spec §6.4).
pub fn squash_upper<R: Read + Seek, W: Read + Write + Seek>(
    layers: &mut [Image<R>],
    out: W,
    spill_dir: &Path,
) -> Result<(W, LayerSummary)> {
    merge_into(layers, out, spill_dir, overlay_upper)
}

fn merge_into<R: Read + Seek, W: Read + Write + Seek>(
    layers: &mut [Image<R>],
    out: W,
    spill_dir: &Path,
    apply: fn(&mut Tree, &Tree),
) -> Result<(W, LayerSummary)> {
    let mut merged = Tree::new(Node::dir(Meta::default_dir(Timestamp::default()), false));
    for (layer, img) in layers.iter_mut().enumerate() {
        let upper = tree_from_image(img, layer)?;
        apply(&mut merged, &upper);
    }
    let base = merged.min_mtime();
    let store = DataStore::new(out, spill_dir)?;
    let (out, stats) = emit(merged, base, store, &mut Sources(layers))?;
    Ok((
        out,
        LayerSummary {
            inodes: stats.inodes,
            image_bytes: stats.bytes,
            tar_bytes: 0,
            implicit_dirs: Vec::new(),
            warnings: Vec::new(),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_only_when_value_is_y() {
        let with = |v: &[u8]| Xattrs::from([(XattrKey::opaque(), v.to_vec())]);
        assert!(is_opaque(&with(b"y")));
        for v in [&b""[..], b"n", b"Y", b"yes", b"y\0"] {
            assert!(!is_opaque(&with(v)), "{v:?}");
        }
        assert!(!is_opaque(&Xattrs::new()));
    }
}
