//! A deliberately naive model of kiln's layer semantics (spec §7.4) and overlayfs
//! merging. Written independently of `apply.rs`/`merge.rs`; used as a test oracle.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};

use kiln_erofs::testtar::{Opts, TarBuilder};
use proptest::prelude::*;

use crate::common::Seen;

#[derive(Debug, Clone)]
pub enum Op {
    Dir {
        path: String,
        mode: u32,
        mtime: (i64, u32),
        uid: u32,
        xattr: Option<String>,
    },
    File {
        path: String,
        data: Vec<u8>,
        mode: u32,
        mtime: (i64, u32),
        uid: u32,
        xattr: Option<String>,
    },
    Symlink {
        path: String,
        target: String,
        mtime: (i64, u32),
    },
    Hardlink {
        path: String,
        target: String,
        mode: u32,
        mtime: (i64, u32),
        uid: u32,
    },
    Whiteout {
        path: String,
        mtime: (i64, u32),
    },
    Opaque {
        dir: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MKind {
    Dir,
    File(Vec<u8>),
    Symlink(Vec<u8>),
    Whiteout,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MNode {
    pub kind: MKind,
    pub mode: u32,
    pub uid: u32,
    pub mtime: (i64, u32),
    pub xattrs: BTreeMap<Vec<u8>, Vec<u8>>,
    pub opaque: bool,
    pub implicit: bool,
    /// Created implicitly; keeps inherited xattrs even once a header describes it.
    pub inherits: bool,
    /// Inode identity; hardlinks copy it.
    pub ident: u64,
}

/// path → node; the root is `""`.
pub type State = BTreeMap<String, MNode>;

fn dir_node(implicit: bool) -> MNode {
    MNode {
        kind: MKind::Dir,
        mode: 0o755,
        uid: 0,
        mtime: (0, 0),
        xattrs: BTreeMap::new(),
        opaque: false,
        implicit,
        inherits: implicit,
        ident: 0,
    }
}

/// The merged state before any layer: an empty root.
pub fn initial() -> State {
    State::from([(String::new(), dir_node(false))])
}

fn opts(mode: u32, mtime: (i64, u32), uid: u32, xattr: &Option<String>) -> Opts {
    let mut o = Opts::default().mode(mode).uid(u64::from(uid)).mtime(mtime.0 as u64);
    if mtime.1 != 0 {
        o = o.pax("mtime", format!("{}.{:09}", mtime.0, mtime.1).as_bytes());
    }
    if let Some(v) = xattr {
        o = o.xattr("user.x", v.as_bytes());
    }
    o
}

pub fn to_tar(ops: &[Op]) -> Vec<u8> {
    let mut b = TarBuilder::new();
    for op in ops {
        match op {
            Op::Dir {
                path,
                mode,
                mtime,
                uid,
                xattr,
            } => {
                b.dir(path, &opts(*mode, *mtime, *uid, xattr));
            }
            Op::File {
                path,
                data,
                mode,
                mtime,
                uid,
                xattr,
            } => {
                b.file(path, data, &opts(*mode, *mtime, *uid, xattr));
            }
            Op::Symlink { path, target, mtime } => {
                // A non-0777 header mode: kiln must normalize it like Linux does.
                b.symlink(path, target, &opts(0o640, *mtime, 0, &None));
            }
            Op::Hardlink {
                path,
                target,
                mode,
                mtime,
                uid,
            } => {
                b.entry(
                    path.as_bytes(),
                    b'1',
                    b"",
                    target.as_bytes(),
                    (0, 0),
                    &opts(*mode, *mtime, *uid, &None),
                );
            }
            Op::Whiteout { path, mtime } => {
                let marker = match path.rsplit_once('/') {
                    Some((dir, name)) => format!("{dir}/.wh.{name}"),
                    None => format!(".wh.{path}"),
                };
                b.entry(
                    marker.as_bytes(),
                    b'0',
                    b"",
                    b"",
                    (0, 0),
                    &opts(0o644, *mtime, 0, &None),
                );
            }
            Op::Opaque { dir } => {
                b.opaque(dir);
            }
        }
    }
    b.finish()
}

fn parent_prefixes(path: &str) -> Vec<String> {
    let parts: Vec<&str> = path.split('/').collect();
    (1..parts.len()).map(|i| parts[..i].join("/")).collect()
}

fn ensure_parents(s: &mut State, path: &str) -> Result<(), String> {
    for prefix in parent_prefixes(path) {
        match s.get(&prefix) {
            Some(n) if n.kind == MKind::Dir => {}
            Some(_) => return Err(format!("parent {prefix} of {path} is not a directory")),
            None => {
                s.insert(prefix, dir_node(true));
            }
        }
    }
    Ok(())
}

fn ensure_dir(s: &mut State, dir: &str) -> Result<(), String> {
    if dir.is_empty() {
        return Ok(());
    }
    ensure_parents(s, dir)?;
    match s.get(dir) {
        Some(n) if n.kind == MKind::Dir => Ok(()),
        Some(_) => Err(format!("{dir} is not a directory")),
        None => {
            s.insert(dir.to_string(), dir_node(true));
            Ok(())
        }
    }
}

fn remove_tree(s: &mut State, path: &str) {
    let prefix = format!("{path}/");
    s.retain(|k, _| k != path && !k.starts_with(&prefix));
}

fn xattrs_of(xattr: &Option<String>) -> BTreeMap<Vec<u8>, Vec<u8>> {
    xattr
        .iter()
        .map(|v| (b"user.x".to_vec(), v.clone().into_bytes()))
        .collect()
}

/// Applies one layer's ops (spec §7.4) and resolves implicit directories against
/// `lower`, the merged state of the layers below.
pub fn apply_layer(ops: &[Op], layer: u64, lower: &State) -> Result<State, String> {
    apply_layer_reporting(ops, layer, lower).map(|(state, _)| state)
}

/// Like `apply_layer`, also returning the implicit directory paths (root excluded).
pub fn apply_layer_reporting(ops: &[Op], layer: u64, lower: &State) -> Result<(State, Vec<String>), String> {
    let mut s = State::from([(String::new(), dir_node(true))]);
    let mut serial = 0u64;
    let mut next = || {
        serial += 1;
        (layer << 32) | serial
    };
    let mut base: Option<(i64, u32)> = None;
    let mut note = |t: (i64, u32)| base = Some(base.map_or(t, |b| b.min(t)));
    for op in ops {
        match op {
            Op::Dir {
                path,
                mode,
                mtime,
                uid,
                xattr,
            } => {
                ensure_parents(&mut s, path)?;
                note(*mtime);
                match s.get_mut(path) {
                    Some(n) if n.kind == MKind::Dir => {
                        n.mode = *mode;
                        n.uid = *uid;
                        n.mtime = *mtime;
                        n.xattrs.extend(xattrs_of(xattr));
                        n.implicit = false;
                    }
                    _ => {
                        remove_tree(&mut s, path);
                        let node = MNode {
                            kind: MKind::Dir,
                            mode: *mode,
                            uid: *uid,
                            mtime: *mtime,
                            xattrs: xattrs_of(xattr),
                            opaque: false,
                            implicit: false,
                            inherits: false,
                            ident: next(),
                        };
                        s.insert(path.clone(), node);
                    }
                }
            }
            Op::File {
                path,
                data,
                mode,
                mtime,
                uid,
                xattr,
            } => {
                ensure_parents(&mut s, path)?;
                note(*mtime);
                remove_tree(&mut s, path);
                let node = MNode {
                    kind: MKind::File(data.clone()),
                    mode: *mode,
                    uid: *uid,
                    mtime: *mtime,
                    xattrs: xattrs_of(xattr),
                    opaque: false,
                    implicit: false,
                    inherits: false,
                    ident: next(),
                };
                s.insert(path.clone(), node);
            }
            Op::Symlink { path, target, mtime } => {
                ensure_parents(&mut s, path)?;
                note(*mtime);
                remove_tree(&mut s, path);
                let node = MNode {
                    kind: MKind::Symlink(target.clone().into_bytes()),
                    mode: 0o777,
                    uid: 0,
                    mtime: *mtime,
                    xattrs: BTreeMap::new(),
                    opaque: false,
                    implicit: false,
                    inherits: false,
                    ident: next(),
                };
                s.insert(path.clone(), node);
            }
            Op::Hardlink {
                path,
                target,
                mode,
                mtime,
                uid,
            } => {
                ensure_parents(&mut s, path)?;
                if path == target {
                    return Err(format!("{path} links to itself"));
                }
                if target.starts_with(&format!("{path}/")) {
                    return Err(format!("hardlink {path} replaces its own target {target}"));
                }
                let t = s
                    .get(target)
                    .cloned()
                    .ok_or_else(|| format!("hardlink target {target} missing"))?;
                if matches!(t.kind, MKind::Dir | MKind::Whiteout) {
                    return Err(format!("hardlink target {target} is not a regular entry"));
                }
                // The link header's metadata applies to the shared inode, i.e. every
                // path with the same identity (containerd semantics).
                note(*mtime);
                for n in s.values_mut().filter(|n| n.ident == t.ident) {
                    if !matches!(n.kind, MKind::Symlink(_)) {
                        n.mode = *mode;
                    }
                    n.uid = *uid;
                    n.mtime = *mtime;
                }
                let linked = s
                    .values()
                    .find(|n| n.ident == t.ident)
                    .cloned()
                    .expect("target present");
                remove_tree(&mut s, path);
                s.insert(path.clone(), linked);
            }
            Op::Whiteout { path, mtime } => {
                ensure_parents(&mut s, path)?;
                if s.contains_key(path) {
                    return Err(format!("whiteout {path} names an entry already in this layer"));
                }
                note(*mtime);
                remove_tree(&mut s, path);
                let node = MNode {
                    kind: MKind::Whiteout,
                    mode: 0,
                    uid: 0,
                    mtime: *mtime,
                    xattrs: BTreeMap::new(),
                    opaque: false,
                    implicit: false,
                    inherits: false,
                    ident: next(),
                };
                s.insert(path.clone(), node);
            }
            Op::Opaque { dir } => {
                ensure_dir(&mut s, dir)?;
                s.get_mut(dir.as_str()).expect("ensured").opaque = true;
            }
        }
    }
    let base = base.unwrap_or((0, 0));
    let implicit: Vec<String> = s
        .iter()
        .filter(|(p, n)| n.inherits && !p.is_empty())
        .map(|(p, _)| p.clone())
        .collect();
    for (path, n) in s.iter_mut() {
        if n.inherits && !n.implicit && !path.is_empty() {
            if let Some(l) = lower.get(path).filter(|l| l.kind == MKind::Dir) {
                let own = std::mem::take(&mut n.xattrs);
                n.xattrs = l.xattrs.clone();
                n.xattrs.extend(own);
            }
            n.inherits = false;
            continue;
        }
        if !n.implicit {
            continue;
        }
        match lower.get(path) {
            Some(l) if l.kind == MKind::Dir && !path.is_empty() => {
                n.mode = l.mode;
                n.uid = l.uid;
                n.mtime = l.mtime;
                n.xattrs = l.xattrs.clone();
            }
            _ => {
                n.mode = 0o755;
                n.uid = 0;
                n.mtime = base;
                n.xattrs.clear();
            }
        }
        n.implicit = false;
        n.inherits = false;
    }
    Ok((s, implicit))
}

/// overlayfs: `layer` on top of the merged state `lower`.
pub fn merge(lower: &State, layer: &State) -> State {
    let mut out = lower.clone();
    let root = &layer[""];
    if root.opaque {
        out.retain(|k, _| k.is_empty());
    }
    let r = out.get_mut("").expect("root");
    r.mode = root.mode;
    r.uid = root.uid;
    r.mtime = root.mtime;
    r.xattrs = root.xattrs.clone();
    for (path, n) in layer.iter().filter(|(p, _)| !p.is_empty()) {
        match n.kind {
            MKind::Whiteout => remove_tree(&mut out, path),
            MKind::Dir => {
                match out.get_mut(path) {
                    Some(o) if o.kind == MKind::Dir => {
                        o.mode = n.mode;
                        o.uid = n.uid;
                        o.mtime = n.mtime;
                        o.xattrs = n.xattrs.clone();
                    }
                    _ => {
                        remove_tree(&mut out, path);
                        out.insert(
                            path.clone(),
                            MNode {
                                opaque: false,
                                ..n.clone()
                            },
                        );
                    }
                }
                if n.opaque {
                    let prefix = format!("{path}/");
                    out.retain(|k, _| !k.starts_with(&prefix));
                }
            }
            _ => {
                remove_tree(&mut out, path);
                out.insert(path.clone(), n.clone());
            }
        }
    }
    for n in out.values_mut() {
        n.opaque = false;
    }
    out
}

/// Compares the model's final state with a walked kiln image.
pub fn compare(model: &State, seen: &BTreeMap<Vec<u8>, Seen>) -> Result<(), String> {
    let model_paths: BTreeSet<Vec<u8>> = model.keys().map(|k| k.as_bytes().to_vec()).collect();
    let seen_paths: BTreeSet<Vec<u8>> = seen.keys().cloned().collect();
    if model_paths != seen_paths {
        return Err(format!(
            "paths differ: model-only {:?}, kiln-only {:?}",
            model_paths
                .difference(&seen_paths)
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .collect::<Vec<_>>(),
            seen_paths
                .difference(&model_paths)
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .collect::<Vec<_>>()
        ));
    }
    let mut model_groups: BTreeMap<u64, Vec<Vec<u8>>> = BTreeMap::new();
    for (path, m) in model {
        let s = &seen[path.as_bytes()];
        let (kind, data) = match &m.kind {
            MKind::Dir => ('d', Vec::new()),
            MKind::File(d) => ('f', d.clone()),
            MKind::Symlink(t) => ('l', t.clone()),
            MKind::Whiteout => return Err(format!("whiteout {path} survived the merge")),
        };
        let expect = (kind, m.mode, m.uid, m.mtime, &m.xattrs, &data);
        let got = (s.kind, s.mode, s.uid, s.mtime, &s.xattrs, &s.data);
        if expect != got {
            return Err(format!("{path:?}: model {expect:?} != kiln {got:?}"));
        }
        if kind != 'd' {
            model_groups.entry(m.ident).or_default().push(path.as_bytes().to_vec());
        }
    }
    let model_groups: BTreeSet<Vec<Vec<u8>>> = model_groups.into_values().filter(|g| g.len() > 1).collect();
    let kiln_groups = crate::common::groups(seen);
    if model_groups != kiln_groups {
        return Err(format!(
            "hardlink groups differ: model {model_groups:?} kiln {kiln_groups:?}"
        ));
    }
    for g in &kiln_groups {
        for p in g {
            if seen[p].nlink as usize != g.len() {
                return Err(format!(
                    "nlink of {:?} is {} but its group has {}",
                    String::from_utf8_lossy(p),
                    seen[p].nlink,
                    g.len()
                ));
            }
        }
    }
    Ok(())
}

fn dir_path() -> impl Strategy<Value = String> {
    prop::collection::vec(prop::sample::select(vec!["a", "b"]), 1..=3).prop_map(|v| v.join("/"))
}

/// A leaf path: up to two directory components, then a name that only sometimes
/// collides with a directory name (forcing replace and parent-not-dir cases).
fn leaf_path() -> impl Strategy<Value = String> {
    (
        prop::collection::vec(prop::sample::select(vec!["a", "b"]), 0..=2),
        prop::sample::select(vec!["f", "g", "f", "g", "a"]),
    )
        .prop_map(|(mut v, leaf)| {
            v.push(leaf);
            v.join("/")
        })
}

fn mtime() -> impl Strategy<Value = (i64, u32)> {
    (0i64..3, prop::sample::select(vec![0u32, 500])).prop_map(|(s, n)| (1_700_000_000 + s, n))
}

/// Ops as generated; hardlinks name an earlier file by index so most are valid.
#[derive(Debug, Clone)]
enum Raw {
    Op(Op),
    Link {
        path: String,
        pick: Option<usize>,
        fallback: String,
        mode: u32,
        mtime: (i64, u32),
        uid: u32,
    },
}

fn raw_op() -> impl Strategy<Value = Raw> {
    let mode = || prop::sample::select(vec![0o755u32, 0o644, 0o1777, 0o4755]);
    let uid = || prop::sample::select(vec![0u32, 1000, 70_000]);
    let xattr = || prop::option::of(prop::sample::select(vec!["v1".to_string(), "v2".to_string()]));
    let data = (
        prop::sample::select(vec![0usize, 1, 100, 4031, 4032, 4096, 5000]),
        any::<u8>(),
    )
        .prop_map(|(n, seed)| (0..n).map(|i| seed.wrapping_add(i as u8)).collect::<Vec<u8>>());
    prop_oneof![
        3 => (dir_path(), mode(), mtime(), uid(), xattr()).prop_map(|(path, mode, mtime, uid, xattr)| Raw::Op(Op::Dir { path, mode, mtime, uid, xattr })),
        5 => (leaf_path(), data, mode(), mtime(), uid(), xattr()).prop_map(|(path, data, mode, mtime, uid, xattr)| Raw::Op(Op::File { path, data, mode, mtime, uid, xattr })),
        // Targets live outside the generated namespace: containerd resolves parents
        // through lower-layer symlinks, which real layers never rely on.
        1 => (leaf_path(), prop::sample::select(vec!["t", "t/u", "/t/v", "../t"]), mtime())
            .prop_map(|(path, target, mtime)| Raw::Op(Op::Symlink { path, target: target.to_string(), mtime })),
        2 => (leaf_path(), prop::option::weighted(0.85, any::<usize>()), leaf_path(), mode(), mtime(), uid())
            .prop_map(|(path, pick, fallback, mode, mtime, uid)| Raw::Link { path, pick, fallback, mode, mtime, uid }),
        2 => (prop_oneof![dir_path(), leaf_path()], mtime()).prop_map(|(path, mtime)| Raw::Op(Op::Whiteout { path, mtime })),
        1 => prop_oneof![Just(String::new()), dir_path()].prop_map(|dir| Raw::Op(Op::Opaque { dir })),
    ]
}

fn concretize(raw: Vec<Raw>) -> Vec<Op> {
    let mut files: Vec<String> = Vec::new();
    raw.into_iter()
        .map(|r| match r {
            Raw::Op(op) => {
                if let Op::File { path, .. } = &op {
                    files.push(path.clone());
                }
                op
            }
            Raw::Link {
                path,
                pick,
                fallback,
                mode,
                mtime,
                uid,
            } => {
                let target = match pick {
                    Some(i) if !files.is_empty() => files[i % files.len()].clone(),
                    _ => fallback,
                };
                Op::Hardlink {
                    path,
                    target,
                    mode,
                    mtime,
                    uid,
                }
            }
        })
        .collect()
}

/// Drops each op that would make the layer invalid on its own, so most generated
/// layers are valid and reach the comparison.
fn repair(ops: Vec<Op>) -> Vec<Op> {
    let mut kept: Vec<Op> = Vec::new();
    for op in ops {
        kept.push(op);
        if apply_layer(&kept, 0, &initial()).is_err() {
            kept.pop();
        }
    }
    kept
}

/// One to four layers of up to twelve ops over a tiny namespace (to force collisions).
/// Four in five layers are repaired; the rest stay raw so kiln and the model must
/// also agree on which inputs are errors.
pub fn layers_strategy() -> impl Strategy<Value = Vec<Vec<Op>>> {
    let layer = (prop::collection::vec(raw_op(), 0..12), 0u8..5).prop_map(|(raw, roll)| {
        if roll == 0 {
            concretize(raw)
        } else {
            repair(concretize(raw))
        }
    });
    prop::collection::vec(layer, 1..5)
}

fn op_path(op: &Op) -> &str {
    match op {
        Op::Dir { path, .. }
        | Op::File { path, .. }
        | Op::Symlink { path, .. }
        | Op::Hardlink { path, .. }
        | Op::Whiteout { path, .. } => path,
        Op::Opaque { dir } => dir,
    }
}

/// containerd and moby set directory mtimes in a final pass over the layer's
/// directory headers. If a later entry replaced that directory or one of its
/// parents with a non-directory, the pass fails (or re-times the replacement),
/// so such layers are not comparable. kiln keeps each entry's own attributes.
pub fn hits_deferred_dir_times(layers: &[Vec<Op>]) -> bool {
    layers.iter().any(|ops| {
        ops.iter().enumerate().any(|(i, op)| {
            let Op::Dir { path, .. } = op else { return false };
            let prefix_of = |q: &str| q == path.as_str() || path.starts_with(&format!("{q}/"));
            let mut replaced = false;
            for later in &ops[i + 1..] {
                match later {
                    Op::Dir { path: q, .. } if q == path => replaced = false,
                    Op::Dir { .. } | Op::Opaque { .. } => {}
                    other if prefix_of(op_path(other)) => replaced = true,
                    _ => {}
                }
            }
            replaced
        })
    })
}

/// containerd resolves an implicit parent by searching each lower layer on its own,
/// skipping non-directories and ignoring whiteouts and opaque directories; kiln uses
/// the overlay view. They agree unless a lower layer has a non-directory at the path
/// or one of its parents, or an opaque parent; builders never omit such parents.
pub fn ambiguous_inheritance(layers: &[Vec<Op>]) -> bool {
    let mut merged = initial();
    let mut below: Vec<State> = Vec::new();
    for (i, ops) in layers.iter().enumerate() {
        let Ok((state, implicit)) = apply_layer_reporting(ops, i as u64, &merged) else {
            return false;
        };
        for p in &implicit {
            let mut prefixes = vec![String::new()];
            prefixes.extend(parent_prefixes(p));
            for lower in &below {
                if prefixes
                    .iter()
                    .any(|q| lower.get(q).is_some_and(|n| n.kind != MKind::Dir || n.opaque))
                    || lower.get(p).is_some_and(|n| n.kind != MKind::Dir)
                {
                    return true;
                }
            }
        }
        merged = merge(&merged, &state);
        below.push(state);
    }
    false
}

/// Runs the model over all layers; `None` if any layer is invalid.
pub fn model_final(layers: &[Vec<Op>]) -> Option<State> {
    let mut state = initial();
    for (i, ops) in layers.iter().enumerate() {
        let layer = apply_layer(ops, i as u64, &state).ok()?;
        state = merge(&state, &layer);
    }
    Some(state)
}
