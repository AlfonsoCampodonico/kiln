//! Merged (overlayfs) views over kiln layers: parent inheritance (§6.3) and squash (§6.4).

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{Read, Seek, Write};
use std::path::Path;

use crate::error::{Error, Result};
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
    for path in paths {
        let Some((layer, nid)) = merged_lookup(lowers, path)? else {
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

/// The topmost `(layer, nid)` providing `path` in the overlay of `layers`.
fn merged_lookup<R: Read + Seek>(layers: &mut [Image<R>], path: &[u8]) -> Result<Option<(usize, u64)>> {
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
            let Some(entry) = img.read_dir(cur)?.into_iter().find(|e| e.name == *c) else {
                break;
            };
            let info = img.inode(entry.nid)?;
            if info.is_whiteout() {
                return Ok(None);
            }
            if i + 1 == comps.len() {
                return Ok(Some((layer, entry.nid)));
            }
            if !info.is_dir() {
                return Ok(None);
            }
            if is_opaque(&img.xattrs(entry.nid)?) {
                opaque_above = true;
            }
            cur = entry.nid;
        }
        if opaque_above {
            return Ok(None);
        }
    }
    Ok(None)
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

struct Sources<'a, R>(&'a mut [Image<R>]);

impl<R: Read + Seek> ExternalData for Sources<'_, R> {
    fn open(&mut self, layer: usize, nid: u64) -> Result<Box<dyn Read + '_>> {
        Ok(Box::new(self.0[layer].data_reader(nid)?))
    }
}

/// Merges `layers` (bottom first) into one bottom layer with no overlay markers.
pub fn squash<R: Read + Seek, W: Read + Write + Seek>(
    layers: &mut [Image<R>],
    out: W,
    spill_dir: &Path,
) -> Result<(W, LayerSummary)> {
    let mut merged = Tree::new(Node::dir(Meta::default_dir(Timestamp::default()), false));
    for (layer, img) in layers.iter_mut().enumerate() {
        let upper = tree_from_image(img, layer)?;
        overlay(&mut merged, &upper);
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
