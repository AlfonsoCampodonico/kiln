//! Layer semantics: applying one tar's entries to a tree (spec §7.4).

use std::collections::{BTreeMap, HashSet};

use crate::error::{Error, Result, lossy};
use crate::path::{components, split_parent};
use crate::tree::{Data, DirAttrs, Kind, Meta, Node, NodeId, Timestamp, Tree, XattrKey, Xattrs};

#[cfg(test)]
use crate::tree::FileData;

#[derive(Debug, Clone)]
pub(crate) enum EntryKind {
    Dir,
    File {
        size: u64,
    },
    Symlink {
        target: Vec<u8>,
    },
    /// `target` is already normalized.
    Hardlink {
        target: Vec<u8>,
    },
    CharDev {
        major: u32,
        minor: u32,
    },
    BlockDev {
        major: u32,
        minor: u32,
    },
    Fifo,
}

/// One filesystem entry decoded from a tar header. `path` is normalized.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub path: Vec<u8>,
    pub kind: EntryKind,
    pub meta: Meta,
    pub xattrs: Xattrs,
}

const WHITEOUT_PREFIX: &[u8] = b".wh.";
const OPAQUE_NAME: &[u8] = b".wh..wh..opq";

pub(crate) struct LayerBuilder {
    tree: Tree,
    base: Option<Timestamp>,
    /// Directories created because a descendant needed them (even if described later).
    created_implicitly: HashSet<NodeId>,
}

impl LayerBuilder {
    pub fn new() -> Self {
        Self {
            tree: Tree::new(Node::dir(Meta::default_dir(Timestamp::default()), true)),
            base: None,
            created_implicitly: HashSet::new(),
        }
    }

    #[cfg(test)]
    pub fn tree(&self) -> &Tree {
        &self.tree
    }

    /// Sorted paths of directories created implicitly (root excluded), including ones a
    /// later header described: they still inherit xattrs from the layers below.
    pub fn implicit_dirs(&self) -> Vec<Vec<u8>> {
        self.tree.dir_paths(|id, _| self.created_implicitly.contains(&id))
    }

    /// Minimum mtime of explicit inode-creating entries; 0 for an empty layer.
    pub fn base_time(&self) -> Timestamp {
        self.base.unwrap_or_default()
    }

    fn note_time(&mut self, t: Timestamp) {
        self.base = Some(self.base.map_or(t, |b| b.min(t)));
    }

    /// Applies one entry. `data` must be `Some` exactly for `EntryKind::File`.
    pub fn apply(&mut self, e: Entry, data: Option<Data>) -> Result<()> {
        if e.path.is_empty() {
            return match e.kind {
                EntryKind::Dir => {
                    self.note_time(e.meta.mtime);
                    let root = self.tree.root;
                    self.merge_dir(root, e.meta, e.xattrs);
                    Ok(())
                }
                _ => Err(Error::InvalidPath {
                    path: "/".into(),
                    reason: "layer root must be a directory",
                }),
            };
        }
        let (parent_path, name) = split_parent(&e.path);
        if name == OPAQUE_NAME {
            let dir = self.ensure_dir(parent_path, &e.path)?;
            self.tree.nodes[dir].xattrs.insert(XattrKey::opaque(), b"y".to_vec());
            return Ok(());
        }
        if let Some(hidden) = name.strip_prefix(WHITEOUT_PREFIX) {
            if hidden.is_empty() || hidden == b"." || hidden == b".." {
                return Err(Error::InvalidPath {
                    path: lossy(&e.path),
                    reason: "invalid whiteout name",
                });
            }
            let parent = self.ensure_dir(parent_path, &e.path)?;
            if self.tree.child(parent, hidden).is_some() {
                // OCI: a whiteout cannot hide an entry from its own layer; containerd rejects it.
                return Err(Error::InvalidPath {
                    path: lossy(&e.path),
                    reason: "whiteout for an entry already in this layer",
                });
            }
            self.note_time(e.meta.mtime);
            let id = self.tree.add(Node::whiteout(e.meta.mtime));
            self.tree.children_mut(parent).insert(hidden.to_vec(), id);
            return Ok(());
        }
        let parent = self.ensure_dir(parent_path, &e.path)?;
        let existing = self.tree.child(parent, name);
        let kind = match e.kind {
            EntryKind::Hardlink { target } => {
                let id = self.resolve_link(&e.path, &target)?;
                // Like containerd, apply the link header's metadata to the shared inode.
                self.note_time(e.meta.mtime);
                let node = &mut self.tree.nodes[id];
                if !matches!(node.kind, Kind::Symlink { .. }) {
                    node.meta.mode = e.meta.mode;
                }
                node.meta.uid = e.meta.uid;
                node.meta.gid = e.meta.gid;
                node.meta.mtime = e.meta.mtime;
                node.xattrs.extend(e.xattrs);
                self.tree.children_mut(parent).insert(name.to_vec(), id);
                return Ok(());
            }
            EntryKind::Dir => {
                self.note_time(e.meta.mtime);
                if let Some(id) = existing.filter(|&id| self.tree.nodes[id].is_dir()) {
                    self.merge_dir(id, e.meta, e.xattrs);
                    return Ok(());
                }
                Kind::Dir {
                    children: BTreeMap::new(),
                    implicit: false,
                }
            }
            EntryKind::File { size } => Kind::File {
                size,
                data: data.expect("file entry without data"),
            },
            EntryKind::Symlink { target } => Kind::Symlink { target },
            EntryKind::CharDev { major, minor } => Kind::CharDev { major, minor },
            EntryKind::BlockDev { major, minor } => Kind::BlockDev { major, minor },
            EntryKind::Fifo => Kind::Fifo,
        };
        self.note_time(e.meta.mtime);
        let mut meta = e.meta;
        if matches!(kind, Kind::Symlink { .. }) {
            // Linux ignores symlink permission bits; they always read back as 0777.
            meta.mode = 0o777;
        }
        let id = self.tree.add(Node {
            kind,
            meta,
            xattrs: e.xattrs,
        });
        self.tree.children_mut(parent).insert(name.to_vec(), id);
        Ok(())
    }

    fn merge_dir(&mut self, id: NodeId, meta: Meta, xattrs: Xattrs) {
        let node = &mut self.tree.nodes[id];
        node.meta = meta;
        node.xattrs.extend(xattrs);
        if let Kind::Dir { implicit, .. } = &mut node.kind {
            *implicit = false;
        }
    }

    /// Walks `dir_path`, creating implicit directories as needed.
    fn ensure_dir(&mut self, dir_path: &[u8], full: &[u8]) -> Result<NodeId> {
        let mut cur = self.tree.root;
        for c in components(dir_path) {
            cur = match self.tree.child(cur, c) {
                Some(id) if self.tree.nodes[id].is_dir() => id,
                Some(_) => return Err(Error::ParentNotDirectory { path: lossy(full) }),
                None => {
                    let id = self.tree.add(Node::dir(Meta::default_dir(Timestamp::default()), true));
                    self.tree.children_mut(cur).insert(c.to_vec(), id);
                    self.created_implicitly.insert(id);
                    id
                }
            };
        }
        Ok(cur)
    }

    fn resolve_link(&self, path: &[u8], target: &[u8]) -> Result<NodeId> {
        let err = |reason| Error::InvalidHardlink {
            path: lossy(path),
            target: lossy(target),
            reason,
        };
        if target == path {
            return Err(err("link to itself"));
        }
        if target.len() > path.len() && target.starts_with(path) && target[path.len()] == b'/' {
            // Replacing `path` removes its subtree, and the target with it.
            return Err(err("target is inside the entry being replaced"));
        }
        let id = self.tree.lookup(target).ok_or_else(|| err("target does not exist"))?;
        let node = &self.tree.nodes[id];
        if node.is_whiteout() {
            return Err(err("target does not exist"));
        }
        if node.is_dir() {
            return Err(err("target is a directory"));
        }
        Ok(id)
    }

    /// Resolves implicit directories and returns `(tree, base time, implicit paths)`.
    ///
    /// A directory still undescribed takes the inherited attributes (or defaults). One
    /// that a later header described keeps its own meta but, as in containerd, keeps the
    /// inherited xattrs underneath its own.
    pub fn finalize(mut self, inherited: &BTreeMap<Vec<u8>, DirAttrs>) -> (Tree, Timestamp, Vec<Vec<u8>>) {
        let base = self.base_time();
        let implicit = self.implicit_dirs();
        for path in &implicit {
            let id = self.tree.lookup(path).expect("implicit dir is reachable");
            let node = &mut self.tree.nodes[id];
            if matches!(node.kind, Kind::Dir { implicit: false, .. }) {
                if let Some(attrs) = inherited.get(path) {
                    let own = std::mem::take(&mut node.xattrs);
                    node.xattrs = attrs.xattrs.clone();
                    node.xattrs.extend(own);
                }
                continue;
            }
            let opaque = node.xattrs.get(&XattrKey::opaque()).cloned();
            match inherited.get(path) {
                Some(attrs) => {
                    node.meta = attrs.meta.clone();
                    node.xattrs = attrs.xattrs.clone();
                }
                None => node.meta = Meta::default_dir(base),
            }
            if let Some(v) = opaque {
                node.xattrs.insert(XattrKey::opaque(), v);
            }
            if let Kind::Dir { implicit, .. } = &mut node.kind {
                *implicit = false;
            }
        }
        let root = &mut self.tree.nodes[self.tree.root];
        if let Kind::Dir { implicit, .. } = &mut root.kind
            && *implicit
        {
            root.meta = Meta::default_dir(base);
            *implicit = false;
        }
        (self.tree, base, implicit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;

    fn meta(mode: u32, sec: i64) -> Meta {
        Meta {
            mode,
            uid: 0,
            gid: 0,
            mtime: Timestamp { sec, nsec: 0 },
        }
    }
    fn entry(path: &str, kind: EntryKind, sec: i64) -> Entry {
        Entry {
            path: path.as_bytes().to_vec(),
            kind,
            meta: meta(0o644, sec),
            xattrs: Xattrs::new(),
        }
    }
    fn data() -> Option<Data> {
        Some(Data::Written(FileData {
            start_blk: 0,
            blocks: 0,
            tail: None,
        }))
    }
    fn file(b: &mut LayerBuilder, p: &str, sec: i64) {
        b.apply(entry(p, EntryKind::File { size: 0 }, sec), data()).unwrap();
    }
    fn dir(b: &mut LayerBuilder, p: &str, sec: i64) {
        b.apply(entry(p, EntryKind::Dir, sec), None).unwrap();
    }
    fn marker(b: &mut LayerBuilder, p: &str, sec: i64) -> crate::Result<()> {
        b.apply(entry(p, EntryKind::File { size: 0 }, sec), data())
    }
    fn user(name: &str) -> XattrKey {
        XattrKey {
            index: crate::ondisk::XATTR_INDEX_USER,
            name: name.as_bytes().to_vec(),
        }
    }

    #[test]
    fn implicit_parents_are_reported_even_once_described() {
        let mut b = LayerBuilder::new();
        file(&mut b, "a/b/c", 10);
        assert_eq!(b.implicit_dirs(), vec![b"a".to_vec(), b"a/b".to_vec()]);
        dir(&mut b, "a", 10);
        assert_eq!(b.implicit_dirs(), vec![b"a".to_vec(), b"a/b".to_vec()]);
        file(&mut b, "a", 10);
        assert!(b.implicit_dirs().is_empty(), "replaced directories are gone");
    }

    #[test]
    fn described_implicit_dir_keeps_inherited_xattrs_under_its_own() {
        let mut b = LayerBuilder::new();
        file(&mut b, "a/f", 10);
        let mut d = entry("a", EntryKind::Dir, 11);
        d.meta.mode = 0o700;
        d.xattrs.insert(user("y"), b"own".to_vec());
        b.apply(d, None).unwrap();
        let mut xattrs = Xattrs::new();
        xattrs.insert(user("k"), b"lower".to_vec());
        xattrs.insert(user("y"), b"lower".to_vec());
        let inherited = BTreeMap::from([(
            b"a".to_vec(),
            DirAttrs {
                meta: meta(0o750, 1),
                xattrs,
            },
        )]);
        let (tree, _, _) = b.finalize(&inherited);
        let a = &tree.nodes[tree.lookup(b"a").unwrap()];
        assert_eq!(a.meta.mode, 0o700, "the header's meta wins");
        assert_eq!(a.xattrs[&user("k")], b"lower");
        assert_eq!(a.xattrs[&user("y")], b"own");
    }

    #[test]
    fn dir_over_dir_merges_attrs_children_and_opaque() {
        let mut b = LayerBuilder::new();
        let mut d = entry("d", EntryKind::Dir, 10);
        d.xattrs.insert(user("x"), b"1".to_vec());
        b.apply(d, None).unwrap();
        file(&mut b, "d/f", 10);
        marker(&mut b, "d/.wh..wh..opq", 10).unwrap();
        let mut d2 = entry("d", EntryKind::Dir, 11);
        d2.meta.mode = 0o700;
        d2.xattrs.insert(user("y"), b"2".to_vec());
        b.apply(d2, None).unwrap();
        let t = b.tree();
        let id = t.lookup(b"d").unwrap();
        assert_eq!(t.nodes[id].meta.mode, 0o700);
        let keys: Vec<_> = t.nodes[id].xattrs.keys().cloned().collect();
        assert_eq!(keys, vec![user("x"), user("y"), XattrKey::opaque()]);
        assert!(t.lookup(b"d/f").is_some());
    }

    #[test]
    fn non_dir_replaces_subtree_and_dir_replaces_non_dir() {
        let mut b = LayerBuilder::new();
        dir(&mut b, "d", 10);
        file(&mut b, "d/f", 10);
        file(&mut b, "d", 10);
        assert!(!b.tree().nodes[b.tree().lookup(b"d").unwrap()].is_dir());
        assert!(b.tree().lookup(b"d/f").is_none());
        dir(&mut b, "d", 10);
        let id = b.tree().lookup(b"d").unwrap();
        assert!(b.tree().children(id).unwrap().is_empty());
    }

    #[test]
    fn hardlink_binds_to_the_node_present_when_read() {
        let mut b = LayerBuilder::new();
        file(&mut b, "a", 1);
        let link = Entry {
            path: b"b".to_vec(),
            kind: EntryKind::Hardlink { target: b"a".to_vec() },
            meta: meta(0o644, 1),
            xattrs: Xattrs::new(),
        };
        b.apply(link, None).unwrap();
        file(&mut b, "a", 2);
        let t = b.tree();
        assert_eq!(t.nodes[t.lookup(b"b").unwrap()].meta.mtime.sec, 1);
        assert_eq!(t.nodes[t.lookup(b"a").unwrap()].meta.mtime.sec, 2);
        assert_ne!(t.lookup(b"a"), t.lookup(b"b"));
    }

    #[test]
    fn hardlink_header_metadata_applies_to_the_shared_inode() {
        let mut b = LayerBuilder::new();
        file(&mut b, "a", 1);
        let mut link = Entry {
            path: b"b".to_vec(),
            kind: EntryKind::Hardlink { target: b"a".to_vec() },
            meta: Meta {
                mode: 0o600,
                uid: 7,
                gid: 8,
                mtime: Timestamp { sec: 9, nsec: 0 },
            },
            xattrs: Xattrs::new(),
        };
        link.xattrs.insert(user("k"), b"v".to_vec());
        b.apply(link, None).unwrap();
        let t = b.tree();
        let a = &t.nodes[t.lookup(b"a").unwrap()];
        assert_eq!(
            a.meta,
            Meta {
                mode: 0o600,
                uid: 7,
                gid: 8,
                mtime: Timestamp { sec: 9, nsec: 0 }
            }
        );
        assert!(a.xattrs.contains_key(&user("k")));
        assert_eq!(t.lookup(b"a"), t.lookup(b"b"));
    }

    #[test]
    fn hardlink_errors() {
        let mut b = LayerBuilder::new();
        dir(&mut b, "d", 1);
        file(&mut b, "f", 1);
        marker(&mut b, ".wh.gone", 1).unwrap();
        file(&mut b, "d/inner", 1);
        for (path, target) in [
            ("x", "missing"),
            ("x", "d"),
            ("f", "f"),
            ("x", "gone"),
            ("d", "d/inner"),
        ] {
            let e = Entry {
                path: path.into(),
                kind: EntryKind::Hardlink { target: target.into() },
                meta: meta(0, 0),
                xattrs: Xattrs::new(),
            };
            assert!(
                matches!(b.apply(e, None), Err(Error::InvalidHardlink { .. })),
                "{path} -> {target}"
            );
        }
    }

    #[test]
    fn whiteouts_translate_and_collide_by_translated_name() {
        let mut b = LayerBuilder::new();
        marker(&mut b, "etc/.wh.foo", 1).unwrap();
        assert!(b.tree().nodes[b.tree().lookup(b"etc/foo").unwrap()].is_whiteout());
        file(&mut b, "etc/foo", 1);
        assert!(
            !b.tree().nodes[b.tree().lookup(b"etc/foo").unwrap()].is_whiteout(),
            "a later entry replaces the whiteout"
        );
        assert!(
            matches!(marker(&mut b, "etc/.wh.foo", 1), Err(Error::InvalidPath { .. })),
            "a whiteout cannot hide its own layer"
        );
        assert!(
            matches!(marker(&mut b, ".wh.etc", 1), Err(Error::InvalidPath { .. })),
            "nor an implicit directory"
        );
        for bad in [".wh.", ".wh..", ".wh..."] {
            assert!(
                matches!(marker(&mut b, bad, 1), Err(Error::InvalidPath { .. })),
                "{bad}"
            );
        }
    }

    #[test]
    fn parent_must_be_a_directory() {
        let mut b = LayerBuilder::new();
        file(&mut b, "f", 1);
        assert!(matches!(
            marker(&mut b, "f/x", 1),
            Err(Error::ParentNotDirectory { .. })
        ));
        marker(&mut b, ".wh.w", 1).unwrap();
        assert!(matches!(
            marker(&mut b, "w/x", 1),
            Err(Error::ParentNotDirectory { .. })
        ));
    }

    #[test]
    fn symlink_mode_is_always_0777() {
        let mut b = LayerBuilder::new();
        b.apply(entry("l", EntryKind::Symlink { target: b"x".to_vec() }, 1), None)
            .unwrap();
        assert_eq!(b.tree().nodes[b.tree().lookup(b"l").unwrap()].meta.mode, 0o777);
    }

    #[test]
    fn root_entry() {
        let mut b = LayerBuilder::new();
        let mut root = entry("", EntryKind::Dir, 5);
        root.meta.mode = 0o700;
        b.apply(root, None).unwrap();
        assert_eq!(b.tree().nodes[b.tree().root].meta.mode, 0o700);
        assert!(matches!(marker(&mut b, "", 1), Err(Error::InvalidPath { .. })));
    }

    #[test]
    fn base_time_ignores_opaque_markers_only() {
        let mut b = LayerBuilder::new();
        assert_eq!(b.base_time(), Timestamp::default());
        file(&mut b, "f", 50);
        dir(&mut b, "d", 40);
        marker(&mut b, "d/.wh..wh..opq", 1).unwrap();
        assert_eq!(b.base_time(), Timestamp { sec: 40, nsec: 0 });
        marker(&mut b, ".wh.x", 30).unwrap();
        assert_eq!(b.base_time(), Timestamp { sec: 30, nsec: 0 });
        let link = Entry {
            path: b"l".to_vec(),
            kind: EntryKind::Hardlink { target: b"f".to_vec() },
            meta: meta(0o644, 20),
            xattrs: Xattrs::new(),
        };
        b.apply(link, None).unwrap();
        assert_eq!(
            b.base_time(),
            Timestamp { sec: 20, nsec: 0 },
            "a hardlink header sets its inode's mtime"
        );
    }

    #[test]
    fn finalize_inherits_or_defaults_and_keeps_opaque() {
        let mut b = LayerBuilder::new();
        file(&mut b, "a/b/f", 100);
        marker(&mut b, "a/.wh..wh..opq", 100).unwrap();
        let mut inherited = BTreeMap::new();
        let mut xattrs = Xattrs::new();
        xattrs.insert(user("k"), b"v".to_vec());
        inherited.insert(
            b"a".to_vec(),
            DirAttrs {
                meta: Meta {
                    mode: 0o750,
                    uid: 5,
                    gid: 6,
                    mtime: Timestamp { sec: 50, nsec: 0 },
                },
                xattrs,
            },
        );
        let (tree, base, implicit) = b.finalize(&inherited);
        assert_eq!(base, Timestamp { sec: 100, nsec: 0 });
        assert_eq!(implicit, vec![b"a".to_vec(), b"a/b".to_vec()]);
        let a = &tree.nodes[tree.lookup(b"a").unwrap()];
        assert_eq!(a.meta.mode, 0o750);
        assert_eq!(a.meta.uid, 5);
        assert!(a.xattrs.contains_key(&user("k")));
        assert!(a.xattrs.contains_key(&XattrKey::opaque()));
        let ab = &tree.nodes[tree.lookup(b"a/b").unwrap()];
        assert_eq!(ab.meta, Meta::default_dir(base));
        assert_eq!(tree.nodes[tree.root].meta, Meta::default_dir(base));
        assert!(
            tree.dir_paths(|_, n| matches!(n.kind, Kind::Dir { implicit: true, .. }))
                .is_empty()
        );
    }

    #[test]
    fn hardlink_to_symlink_keeps_mode_0777() {
        let mut b = LayerBuilder::new();
        b.apply(entry("l", EntryKind::Symlink { target: b"x".to_vec() }, 1), None)
            .unwrap();
        let link = Entry {
            path: b"h".to_vec(),
            kind: EntryKind::Hardlink { target: b"l".to_vec() },
            meta: Meta {
                mode: 0o600,
                uid: 7,
                gid: 0,
                mtime: Timestamp { sec: 9, nsec: 0 },
            },
            xattrs: Xattrs::new(),
        };
        b.apply(link, None).unwrap();
        let t = b.tree();
        let symlink = &t.nodes[t.lookup(b"l").unwrap()];
        assert_eq!(symlink.meta.mode, 0o777, "symlink mode stays 0777");
        assert_eq!(symlink.meta.uid, 7, "hardlink header uid applies");
        assert_eq!(symlink.meta.mtime.sec, 9, "hardlink header mtime applies");
    }

    #[test]
    fn base_time_includes_explicit_root_symlink_and_devices() {
        // Explicit root directory entry
        let mut b = LayerBuilder::new();
        let root = entry("", EntryKind::Dir, 5);
        b.apply(root, None).unwrap();
        assert_eq!(
            b.base_time(),
            Timestamp { sec: 5, nsec: 0 },
            "explicit root counts as base_time"
        );

        // Symlink entry
        let mut b = LayerBuilder::new();
        b.apply(entry("l", EntryKind::Symlink { target: b"x".to_vec() }, 7), None)
            .unwrap();
        assert_eq!(
            b.base_time(),
            Timestamp { sec: 7, nsec: 0 },
            "symlink counts as base_time"
        );

        // Character device
        let mut b = LayerBuilder::new();
        b.apply(
            Entry {
                path: b"dev".to_vec(),
                kind: EntryKind::CharDev { major: 1, minor: 3 },
                meta: meta(0o666, 7),
                xattrs: Xattrs::new(),
            },
            None,
        )
        .unwrap();
        let t = b.tree();
        let dev = &t.nodes[t.lookup(b"dev").unwrap()];
        assert!(matches!(dev.kind, Kind::CharDev { major: 1, minor: 3 }));
        assert_eq!(
            b.base_time(),
            Timestamp { sec: 7, nsec: 0 },
            "char device counts as base_time"
        );

        // Block device
        let mut b = LayerBuilder::new();
        b.apply(
            Entry {
                path: b"dev".to_vec(),
                kind: EntryKind::BlockDev { major: 8, minor: 0 },
                meta: meta(0o666, 7),
                xattrs: Xattrs::new(),
            },
            None,
        )
        .unwrap();
        let t = b.tree();
        let dev = &t.nodes[t.lookup(b"dev").unwrap()];
        assert!(matches!(dev.kind, Kind::BlockDev { major: 8, minor: 0 }));
        assert_eq!(
            b.base_time(),
            Timestamp { sec: 7, nsec: 0 },
            "block device counts as base_time"
        );

        // FIFO
        let mut b = LayerBuilder::new();
        b.apply(
            Entry {
                path: b"fifo".to_vec(),
                kind: EntryKind::Fifo,
                meta: meta(0o666, 7),
                xattrs: Xattrs::new(),
            },
            None,
        )
        .unwrap();
        let t = b.tree();
        let fifo = &t.nodes[t.lookup(b"fifo").unwrap()];
        assert!(matches!(fifo.kind, Kind::Fifo));
        assert_eq!(b.base_time(), Timestamp { sec: 7, nsec: 0 }, "FIFO counts as base_time");
    }

    #[test]
    fn dir_over_dir_header_xattrs_override() {
        let mut b = LayerBuilder::new();
        let mut d = entry("d", EntryKind::Dir, 10);
        d.xattrs.insert(user("k"), b"old".to_vec());
        d.xattrs.insert(user("keep"), b"unrelated".to_vec());
        b.apply(d, None).unwrap();
        let mut d2 = entry("d", EntryKind::Dir, 11);
        d2.xattrs.insert(user("k"), b"new".to_vec());
        b.apply(d2, None).unwrap();
        let t = b.tree();
        let d_node = &t.nodes[t.lookup(b"d").unwrap()];
        assert_eq!(
            d_node.xattrs[&user("k")],
            b"new",
            "header xattr overrides same-named one"
        );
        assert_eq!(d_node.xattrs[&user("keep")], b"unrelated", "unrelated xattr survives");
    }
}
