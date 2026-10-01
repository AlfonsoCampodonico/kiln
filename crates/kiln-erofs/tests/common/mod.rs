#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::Cursor;

use kiln_erofs::ondisk::{S_IFBLK, S_IFCHR, S_IFDIR, S_IFIFO, S_IFLNK, S_IFMT, S_IFREG};
use kiln_erofs::testtar::{Opts, TarBuilder};
use kiln_erofs::{Image, LayerSummary, LayerWriter, Limits};

/// A valid `vfs_cap_data` v2 value granting cap_net_bind_service (the kernel
/// rejects malformed `security.capability` values with EINVAL).
pub const CAP_NET_BIND_SERVICE: [u8; 20] = [0, 0, 0, 2, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

/// Valid minimal POSIX ACL v2: USER_OBJ rw-, GROUP_OBJ r--, OTHER r--.
pub const ACL_MINIMAL: [u8; 28] = [
    2, 0, 0, 0, 1, 0, 6, 0, 255, 255, 255, 255, 4, 0, 4, 0, 255, 255, 255, 255, 32, 0, 4, 0, 255, 255, 255, 255,
];

/// Fixed layers covering every entry kind and layout decision.
pub fn fixtures() -> Vec<(&'static str, Vec<u8>)> {
    let pattern = |n: usize| -> Vec<u8> { (0..n).map(|i| (i * 7 % 256) as u8).collect() };
    let mixed = TarBuilder::new()
        .dir("etc", &Opts::default().mode(0o755))
        .file(
            "etc/os-release",
            b"ID=kiln\n",
            &Opts::default().xattr("user.common", b"1"),
        )
        .file(
            "usr/bin/tool",
            &pattern(10_000),
            &Opts::default()
                .mode(0o755)
                .xattr("security.capability", &CAP_NET_BIND_SERVICE)
                .xattr("user.common", b"1"),
        )
        .symlink("usr/bin/alias", "tool", &Opts::default())
        .hardlink("usr/bin/tool2", "usr/bin/tool")
        .chardev("dev/console", 5, 1, &Opts::default().mode(0o600))
        .fifo("run/fifo", &Opts::default())
        .whiteout("etc/old")
        .dir("var/cache", &Opts::default().mode(0o755))
        .opaque("var/cache")
        .file(
            "home/u/f",
            &pattern(4096),
            &Opts::default().uid(70_000).pax("mtime", b"1700000000.123456789"),
        )
        .finish();
    let mut many = TarBuilder::new();
    for i in 0..700 {
        many.file(&format!("d/f{i:04}"), &pattern(i % 50), &Opts::default());
    }
    for sub in ["d/x", "d/y", "d/z"] {
        many.dir(sub, &Opts::default().mode(0o750));
    }
    let relocate = TarBuilder::new()
        .file("a", &pattern(4096 + 4030), &Opts::default())
        .entry(
            b"b",
            b'1',
            b"",
            b"a",
            (0, 0),
            &Opts::default().xattr("user.k", &[1u8; 64]),
        )
        .finish();
    let xattrs = TarBuilder::new()
        .file(
            "forced",
            &vec![7u8; 4030],
            &Opts::default().xattr("user.big", &vec![7u8; 4030]),
        )
        .file("plain", &pattern(4090), &Opts::default().xattr("user.k", &[9u8; 100]))
        .file(
            "acl",
            &[],
            &Opts::default().xattr("system.posix_acl_access", &ACL_MINIMAL),
        )
        .entry(b"dev/sda", b'4', b"", b"", (8, 0), &Opts::default().mode(0o660))
        .finish();
    vec![
        ("empty", TarBuilder::new().finish()),
        ("mixed", mixed),
        ("many", many.finish()),
        ("relocate", relocate),
        ("xattrs", xattrs),
    ]
}

/// Everything a test may assert about one path in an image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    pub kind: char,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime: (i64, u32),
    pub nlink: u32,
    pub xattrs: BTreeMap<Vec<u8>, Vec<u8>>,
    pub data: Vec<u8>,
    pub rdev: (u32, u32),
    pub nid: u64,
    pub compact: bool,
    pub layout: u16,
}

pub fn p(s: &str) -> Vec<u8> {
    s.as_bytes().to_vec()
}

/// Reads every path in `img` (root is `b""`).
pub fn walk(img: &[u8]) -> BTreeMap<Vec<u8>, Seen> {
    let mut image = Image::open(Cursor::new(img)).unwrap();
    let mut out = BTreeMap::new();
    let mut queue = vec![(Vec::new(), image.root_nid())];
    while let Some((path, nid)) = queue.pop() {
        let info = image.inode(nid).unwrap();
        let kind = match info.mode & S_IFMT {
            S_IFDIR => 'd',
            S_IFREG => 'f',
            S_IFLNK => 'l',
            S_IFCHR => 'c',
            S_IFBLK => 'b',
            S_IFIFO => 'p',
            _ => '?',
        };
        let data = if kind == 'f' || kind == 'l' {
            image.read_data(nid).unwrap()
        } else {
            Vec::new()
        };
        let xattrs = image
            .xattrs(nid)
            .unwrap()
            .into_iter()
            .map(|(k, v)| (k.full_name(), v))
            .collect();
        if kind == 'd' {
            for e in image.read_dir(nid).unwrap() {
                let mut child = path.clone();
                if !child.is_empty() {
                    child.push(b'/');
                }
                child.extend_from_slice(&e.name);
                queue.push((child, e.nid));
            }
        }
        out.insert(
            path,
            Seen {
                kind,
                mode: info.mode & 0o7777,
                uid: info.uid,
                gid: info.gid,
                mtime: (info.mtime.sec, info.mtime.nsec),
                nlink: info.nlink,
                xattrs,
                data,
                rdev: info.rdev,
                nid,
                compact: info.isize == 32,
                layout: info.layout,
            },
        );
    }
    out
}

/// Converts one tar with no lower layers.
pub fn convert(tar: &[u8]) -> (Vec<u8>, LayerSummary) {
    let dir = tempfile::tempdir().unwrap();
    let mut w = LayerWriter::new(Cursor::new(Vec::new()), dir.path(), Limits::default()).unwrap();
    w.append_tar(tar).unwrap();
    let (out, summary) = w.finish(&BTreeMap::new()).unwrap();
    (out.into_inner(), summary)
}

use kiln_erofs::{resolve_inherited, squash};

/// Converts layers bottom-up, resolving implicit parents against the layers below.
pub fn try_convert_stack(tars: &[Vec<u8>]) -> kiln_erofs::Result<Vec<Vec<u8>>> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    for tar in tars {
        let dir = tempfile::tempdir()?;
        let mut w = LayerWriter::new(Cursor::new(Vec::new()), dir.path(), Limits::default())?;
        w.append_tar(tar.as_slice())?;
        let implicit = w.implicit_dirs();
        let inherited = {
            let mut lowers = out
                .iter()
                .map(|b| Image::open(Cursor::new(b.as_slice())))
                .collect::<kiln_erofs::Result<Vec<_>>>()?;
            resolve_inherited(&mut lowers, &implicit)?
        };
        let (cur, _) = w.finish(&inherited)?;
        out.push(cur.into_inner());
    }
    Ok(out)
}

pub fn convert_stack(tars: &[Vec<u8>]) -> Vec<Vec<u8>> {
    try_convert_stack(tars).unwrap()
}

pub fn squash_all(layers: &[Vec<u8>]) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let mut imgs: Vec<_> = layers
        .iter()
        .map(|b| Image::open(Cursor::new(b.as_slice())).unwrap())
        .collect();
    let (out, _) = squash(&mut imgs, Cursor::new(Vec::new()), dir.path()).unwrap();
    out.into_inner()
}

use std::collections::BTreeSet;

/// The comparable projection of a `Seen`. Mtime is `None` for directories when ignored.
pub type View = (
    char,
    u32,
    u32,
    u32,
    Option<(i64, u32)>,
    BTreeMap<Vec<u8>, Vec<u8>>,
    Vec<u8>,
    (u32, u32),
);

pub fn view(m: &BTreeMap<Vec<u8>, Seen>, ignore_dir_mtime: bool) -> BTreeMap<Vec<u8>, View> {
    m.iter()
        .map(|(path, s)| {
            let mtime = if ignore_dir_mtime && s.kind == 'd' {
                None
            } else {
                Some(s.mtime)
            };
            let xattrs = s
                .xattrs
                .iter()
                .filter(|(k, _)| !k.starts_with(b"trusted.overlay."))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            (
                path.clone(),
                (s.kind, s.mode, s.uid, s.gid, mtime, xattrs, s.data.clone(), s.rdev),
            )
        })
        .collect()
}

/// Hardlink groups: sets of non-directory paths that share an inode (size ≥ 2).
pub fn groups(m: &BTreeMap<Vec<u8>, Seen>) -> BTreeSet<Vec<Vec<u8>>> {
    let mut by_nid: BTreeMap<u64, Vec<Vec<u8>>> = BTreeMap::new();
    for (path, s) in m {
        if s.kind != 'd' {
            by_nid.entry(s.nid).or_default().push(path.clone());
        }
    }
    by_nid.into_values().filter(|g| g.len() > 1).collect()
}
