//! The in-memory filesystem tree built from one layer (or a merged stack).

use std::collections::BTreeMap;

use crate::ondisk::{
    XATTR_INDEX_POSIX_ACL_ACCESS, XATTR_INDEX_POSIX_ACL_DEFAULT, XATTR_INDEX_SECURITY, XATTR_INDEX_TRUSTED,
    XATTR_INDEX_USER,
};
use crate::path::components;

/// Seconds and nanoseconds since the Unix epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Timestamp {
    pub sec: i64,
    pub nsec: u32,
}

/// An erofs xattr key: name index plus the name without its prefix.
/// Ordering by `(index, name)` is the on-disk inline order.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct XattrKey {
    pub index: u8,
    pub name: Vec<u8>,
}

pub type Xattrs = BTreeMap<XattrKey, Vec<u8>>;

const NAMED_PREFIXES: [(&[u8], u8); 3] = [
    (b"user.", XATTR_INDEX_USER),
    (b"trusted.", XATTR_INDEX_TRUSTED),
    (b"security.", XATTR_INDEX_SECURITY),
];
const ACL_ACCESS: &[u8] = b"system.posix_acl_access";
const ACL_DEFAULT: &[u8] = b"system.posix_acl_default";

impl XattrKey {
    /// Maps a full Linux xattr name to an erofs key; `None` if erofs cannot store it.
    pub fn from_full_name(full: &[u8]) -> Option<Self> {
        if full == ACL_ACCESS {
            return Some(Self {
                index: XATTR_INDEX_POSIX_ACL_ACCESS,
                name: Vec::new(),
            });
        }
        if full == ACL_DEFAULT {
            return Some(Self {
                index: XATTR_INDEX_POSIX_ACL_DEFAULT,
                name: Vec::new(),
            });
        }
        NAMED_PREFIXES.iter().find_map(|(prefix, index)| {
            full.strip_prefix(*prefix)
                .filter(|rest| !rest.is_empty())
                .map(|rest| Self {
                    index: *index,
                    name: rest.to_vec(),
                })
        })
    }

    pub fn full_name(&self) -> Vec<u8> {
        match self.index {
            XATTR_INDEX_POSIX_ACL_ACCESS => ACL_ACCESS.to_vec(),
            XATTR_INDEX_POSIX_ACL_DEFAULT => ACL_DEFAULT.to_vec(),
            i => {
                let prefix = NAMED_PREFIXES
                    .iter()
                    .find(|(_, x)| *x == i)
                    .map_or(&b""[..], |(p, _)| *p);
                [prefix, self.name.as_slice()].concat()
            }
        }
    }

    /// `trusted.overlay.opaque`.
    pub fn opaque() -> Self {
        Self {
            index: XATTR_INDEX_TRUSTED,
            name: b"overlay.opaque".to_vec(),
        }
    }

    /// Any `trusted.overlay.*` key.
    pub fn is_overlay(&self) -> bool {
        self.index == XATTR_INDEX_TRUSTED && self.name.starts_with(b"overlay.")
    }
}

/// Inode attributes. `mode` holds permission bits only (`0o7777`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meta {
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime: Timestamp,
}

impl Meta {
    /// Attributes for a directory nothing else describes (spec §6.3).
    pub fn default_dir(mtime: Timestamp) -> Self {
        Self {
            mode: 0o755,
            uid: 0,
            gid: 0,
            mtime,
        }
    }
}

/// Attributes inherited by an implicit directory from the layers below.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirAttrs {
    pub meta: Meta,
    pub xattrs: Xattrs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TailRef {
    pub offset: u64,
    pub len: u32,
}

/// Where a regular file's bytes were written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileData {
    /// First data block (0 when `blocks == 0`).
    pub start_blk: u32,
    /// Data-area blocks, including a zero-padded last block when the tail is not inline.
    pub blocks: u32,
    /// Tail bytes held in the spill file, to be stored inline.
    pub tail: Option<TailRef>,
}

#[derive(Debug, Clone)]
pub(crate) enum Data {
    Written(FileData),
    /// Still inside source image `layer` at inode `nid` (squash).
    External {
        layer: usize,
        nid: u64,
    },
}

pub(crate) type NodeId = usize;

#[derive(Debug, Clone)]
pub(crate) enum Kind {
    Dir {
        children: BTreeMap<Vec<u8>, NodeId>,
        implicit: bool,
    },
    File {
        size: u64,
        data: Data,
    },
    Symlink {
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

#[derive(Debug, Clone)]
pub(crate) struct Node {
    pub kind: Kind,
    pub meta: Meta,
    pub xattrs: Xattrs,
}

impl Node {
    pub fn dir(meta: Meta, implicit: bool) -> Self {
        Node {
            kind: Kind::Dir {
                children: BTreeMap::new(),
                implicit,
            },
            meta,
            xattrs: Xattrs::new(),
        }
    }

    pub fn whiteout(mtime: Timestamp) -> Self {
        Node {
            kind: Kind::CharDev { major: 0, minor: 0 },
            meta: Meta {
                mode: 0,
                uid: 0,
                gid: 0,
                mtime,
            },
            xattrs: Xattrs::new(),
        }
    }

    pub fn is_dir(&self) -> bool {
        matches!(self.kind, Kind::Dir { .. })
    }

    pub fn is_whiteout(&self) -> bool {
        matches!(self.kind, Kind::CharDev { major: 0, minor: 0 })
    }
}

/// An arena of nodes. Directories own name → node maps; hardlinks share a node.
#[derive(Debug, Clone)]
pub(crate) struct Tree {
    pub nodes: Vec<Node>,
    pub root: NodeId,
}

impl Tree {
    pub fn new(root: Node) -> Self {
        Tree {
            nodes: vec![root],
            root: 0,
        }
    }

    pub fn add(&mut self, node: Node) -> NodeId {
        self.nodes.push(node);
        self.nodes.len() - 1
    }

    pub fn children(&self, dir: NodeId) -> Option<&BTreeMap<Vec<u8>, NodeId>> {
        match &self.nodes[dir].kind {
            Kind::Dir { children, .. } => Some(children),
            _ => None,
        }
    }

    pub fn children_mut(&mut self, dir: NodeId) -> &mut BTreeMap<Vec<u8>, NodeId> {
        match &mut self.nodes[dir].kind {
            Kind::Dir { children, .. } => children,
            _ => unreachable!("children_mut on a non-directory"),
        }
    }

    pub fn child(&self, dir: NodeId, name: &[u8]) -> Option<NodeId> {
        self.children(dir)?.get(name).copied()
    }

    /// Looks up a normalized path; every component must resolve through directories.
    pub fn lookup(&self, path: &[u8]) -> Option<NodeId> {
        let mut cur = self.root;
        for c in components(path) {
            cur = self.child(cur, c)?;
        }
        Some(cur)
    }

    /// Sorted paths of reachable directories (excluding the root) for which `keep` holds.
    pub fn dir_paths(&self, keep: impl Fn(NodeId, &Node) -> bool) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut stack: Vec<(NodeId, Vec<u8>)> = vec![(self.root, Vec::new())];
        while let Some((id, path)) = stack.pop() {
            if let Kind::Dir { children, .. } = &self.nodes[id].kind {
                if !path.is_empty() && keep(id, &self.nodes[id]) {
                    out.push(path.clone());
                }
                for (name, &child) in children {
                    let mut p = path.clone();
                    if !p.is_empty() {
                        p.push(b'/');
                    }
                    p.extend_from_slice(name);
                    stack.push((child, p));
                }
            }
        }
        out.sort();
        out
    }

    /// Minimum mtime over reachable nodes (the root counts).
    pub fn min_mtime(&self) -> Timestamp {
        let mut min = self.nodes[self.root].meta.mtime;
        let mut stack = vec![self.root];
        while let Some(id) = stack.pop() {
            min = min.min(self.nodes[id].meta.mtime);
            if let Some(children) = self.children(id) {
                stack.extend(children.values().copied());
            }
        }
        min
    }
}
