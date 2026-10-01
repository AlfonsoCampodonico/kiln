#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::Cursor;

use kiln_erofs::ondisk::{S_IFBLK, S_IFCHR, S_IFDIR, S_IFIFO, S_IFLNK, S_IFMT, S_IFREG};
use kiln_erofs::{Image, LayerSummary, LayerWriter, Limits};

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
