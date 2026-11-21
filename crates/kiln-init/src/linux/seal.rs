//! Sealing a build step (spec §9.8 build mode): the overlay's upper directory,
//! once the overlay is unmounted, becomes an OCI tar diff appended to the output
//! disk. Overlay whiteouts (character devices 0:0) become `.wh.<name>`, opaque
//! directories (`trusted.overlay.opaque`) get `.wh..wh..opq`, inodes with more
//! than one link become hard links to their first path, and the overlay's own
//! `trusted.overlay.*` attributes are dropped (only `user.*` and
//! `security.capability` are kept: [`crate::diff::keep_xattr`]). With a
//! `SOURCE_DATE_EPOCH`, every entry's mtime is clamped to it, copied-up parent
//! directories included ([`crate::diff::clamp_mtime`]). Sockets cannot be in a layer and
//! are skipped. Names are walked in byte order, so the same upper gives the same
//! diff.

use std::collections::HashMap;
use std::ffi::CStr;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::os::fd::OwnedFd;

use rustix::fs::{AtFlags, FileType, Mode, OFlags, SeekFrom, Stat};
use rustix::io::Errno;
use sha2::{Digest, Sha256};

use crate::diff::{Kind, Node, TarWriter, Written, clamp_mtime, join, keep_xattr, opaque, whiteout};
use crate::error::{Context, Failure, Result};

const OPAQUE_XATTR: &CStr = c"trusted.overlay.opaque";

/// The output disk: where the next diff goes.
pub struct Output {
    dev: OwnedFd,
    size: u64,
    next: u64,
}

/// Where a diff landed.
pub struct Sealed {
    pub offset: u64,
    pub written: Written,
    pub sha256: String,
}

impl Output {
    pub fn open(path: &str) -> Result<Self> {
        let dev = rustix::fs::open(path, OFlags::RDWR | OFlags::CLOEXEC, Mode::empty())
            .context(format!("open the output disk {path}"))?;
        let size = rustix::fs::seek(&dev, SeekFrom::End(0)).context("size of the output disk")?;
        Ok(Self { dev, size, next: 0 })
    }

    /// Writes the diff of `upper` at the next aligned offset and flushes the disk.
    /// `lowers` are the directories below it: overlayfs marks every directory it
    /// creates in a merged directory opaque, and the marker is only kept where a
    /// lower has something it hides. `epoch` clamps every entry's mtime.
    pub fn seal(&mut self, upper: &str, lowers: &[String], epoch: Option<u64>) -> Result<Sealed> {
        let offset = self.next;
        let w = DiskWriter {
            dev: &self.dev,
            pos: offset,
            end: self.size,
            hash: Sha256::new(),
        };
        let mut tar = TarWriter::new(BufWriter::with_capacity(1 << 20, w));
        let root = rustix::fs::open(
            upper,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .context(format!("open {upper}"))?;
        let mut links = HashMap::new();
        let lowers = lowers
            .iter()
            .map(|l| {
                rustix::fs::open(
                    l.as_str(),
                    OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .context(format!("open {l}"))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut w = Walk {
            tar: &mut tar,
            links: &mut links,
            lowers: &lowers,
            epoch,
        };
        walk_dir(&mut w, &root, &[])?;
        let (buf, written) = tar.finish(kiln_proto::DIFF_ALIGN).map_err(io_failure)?;
        let w = buf
            .into_inner()
            .map_err(|e| io_failure(io::Error::other(e.to_string())))?;
        let sha256 = hex(&w.hash.finalize());
        rustix::fs::fsync(&self.dev).context("flush the output disk")?;
        self.next = offset + written.len;
        Ok(Sealed {
            offset,
            written,
            sha256,
        })
    }
}

fn io_failure(e: io::Error) -> Failure {
    Failure::os("write the step's diff", &e)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Positional writes to the output disk, hashed, refused past its end.
struct DiskWriter<'a> {
    dev: &'a OwnedFd,
    pos: u64,
    end: u64,
    hash: Sha256,
}

impl Write for DiskWriter<'_> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        if self.pos + b.len() as u64 > self.end {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                format!("the output disk ({} bytes) is full", self.end),
            ));
        }
        let n = rustix::io::pwrite(self.dev, b, self.pos)?;
        self.hash.update(&b[..n]);
        self.pos += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn fail(path: &[u8], what: &str, e: impl Into<io::Error>) -> Failure {
    Failure::os(format!("seal: {what} {}", String::from_utf8_lossy(path)), &e.into())
}

/// The directory's entries other than `.` and `..`, in byte order.
fn names(dir: &OwnedFd, path: &[u8]) -> Result<Vec<Vec<u8>>> {
    let d = rustix::fs::openat(
        dir,
        c".",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| fail(path, "open", e))?;
    let mut names = Vec::new();
    for entry in rustix::fs::Dir::read_from(&d).map_err(|e| fail(path, "read", e))? {
        let entry = entry.map_err(|e| fail(path, "read", e))?;
        let name = entry.file_name().to_bytes();
        if name != b"." && name != b".." {
            names.push(name.to_vec());
        }
    }
    names.sort();
    Ok(names)
}

/// The extended attributes a diff keeps ([`keep_xattr`]) of an open file.
fn xattrs(fd: &OwnedFd, path: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut list = vec![0u8; 64 * 1024];
    let n = match rustix::fs::flistxattr(fd, &mut list[..]) {
        Ok(n) => n,
        Err(Errno::NOTSUP) => return Ok(Vec::new()),
        Err(e) => return Err(fail(path, "list the attributes of", e)),
    };
    let mut out = Vec::new();
    for name in list[..n].split(|&b| b == 0).filter(|n| !n.is_empty()) {
        if !keep_xattr(name) {
            continue;
        }
        let cname = std::ffi::CString::new(name).expect("split on NUL");
        let mut value = vec![0u8; 64 * 1024];
        let len = rustix::fs::fgetxattr(fd, cname.as_c_str(), &mut value[..])
            .map_err(|e| fail(path, "read an attribute of", e))?;
        value.truncate(len);
        out.push((name.to_vec(), value));
    }
    // `flistxattr` order is the filesystem's: sort, so the diff's bytes (and
    // potter's cache keys, which hash them) do not depend on it.
    out.sort();
    Ok(out)
}

fn is_opaque(fd: &OwnedFd) -> bool {
    let mut v = [0u8; 4];
    matches!(rustix::fs::fgetxattr(fd, OPAQUE_XATTR, &mut v[..]), Ok(1) if v[0] == b'y')
}

fn base(path: Vec<u8>, st: &Stat, kind: Kind, epoch: Option<u64>) -> Node {
    Node {
        path,
        kind,
        mode: st.st_mode & 0o7777,
        uid: u64::from(st.st_uid),
        gid: u64::from(st.st_gid),
        mtime: clamp_mtime(st.st_mtime, epoch),
        xattrs: Vec::new(),
    }
}

type Links = HashMap<(u64, u64), Vec<u8>>;

struct Walk<'a, W: Write> {
    tar: &'a mut TarWriter<W>,
    links: &'a mut Links,
    lowers: &'a [OwnedFd],
    epoch: Option<u64>,
}

impl<W: Write> Walk<'_, W> {
    /// Whether a lower layer has something at `path`.
    fn in_lowers(&self, path: &[u8]) -> bool {
        self.lowers
            .iter()
            .any(|l| rustix::fs::statat(l, path, AtFlags::SYMLINK_NOFOLLOW).is_ok())
    }
}

/// A directory being walked: its descriptor, its path and the names still to visit.
struct Frame {
    fd: OwnedFd,
    path: Vec<u8>,
    names: std::vec::IntoIter<Vec<u8>>,
}

impl Frame {
    fn new(fd: OwnedFd, path: Vec<u8>) -> Result<Self> {
        let names = names(&fd, &path)?.into_iter();
        Ok(Frame { fd, path, names })
    }
}

/// Walks `dir` (at `path` in the diff), parents before their entries and names in
/// byte order. The directories still open are an explicit stack, so a deep tree
/// costs heap, not the thread's stack (and one descriptor per level).
fn walk_dir<W: Write>(w: &mut Walk<'_, W>, dir: &OwnedFd, path: &[u8]) -> Result<()> {
    let root = dir
        .try_clone()
        .map_err(|e| fail(path, "duplicate the descriptor of", e))?;
    let mut stack = vec![Frame::new(root, path.to_vec())?];
    while let Some(top) = stack.last_mut() {
        let Some(name) = top.names.next() else {
            stack.pop();
            continue;
        };
        if let Some(child) = visit(w, &top.fd, &top.path, &name)? {
            stack.push(child);
        }
    }
    Ok(())
}

/// Seals the entry `name` of `dir`; for a directory, the frame to walk next.
fn visit<W: Write>(w: &mut Walk<'_, W>, dir: &OwnedFd, path: &[u8], name: &[u8]) -> Result<Option<Frame>> {
    let child = join(path, name);
    let st = rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW).map_err(|e| fail(&child, "stat", e))?;
    let ty = FileType::from_raw_mode(st.st_mode);
    let rdev = (rustix::fs::major(st.st_rdev), rustix::fs::minor(st.st_rdev));
    if ty == FileType::CharacterDevice && rdev == (0, 0) {
        w.tar
            .entry(&Node::marker(whiteout(path, name)), Some(&mut io::empty()))
            .map_err(io_failure)?;
        return Ok(None);
    }
    if ty != FileType::Directory && st.st_nlink > 1 {
        let key = (st.st_dev, st.st_ino);
        if let Some(first) = w.links.get(&key) {
            let node = base(child, &st, Kind::HardLink(first.clone()), w.epoch);
            w.tar.entry(&node, None).map_err(io_failure)?;
            return Ok(None);
        }
        w.links.insert(key, child.clone());
    }
    match ty {
        FileType::Directory => {
            let fd = rustix::fs::openat(
                dir,
                name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|e| fail(&child, "open", e))?;
            let mut node = base(child.clone(), &st, Kind::Dir, w.epoch);
            node.xattrs = xattrs(&fd, &child)?;
            w.tar.entry(&node, None).map_err(io_failure)?;
            if is_opaque(&fd) && w.in_lowers(&child) {
                w.tar
                    .entry(&Node::marker(opaque(&child)), Some(&mut io::empty()))
                    .map_err(io_failure)?;
            }
            return Frame::new(fd, child).map(Some);
        }
        FileType::RegularFile => {
            let fd = rustix::fs::openat(
                dir,
                name,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|e| fail(&child, "open", e))?;
            let mut node = base(child.clone(), &st, Kind::File(st.st_size as u64), w.epoch);
            node.xattrs = xattrs(&fd, &child)?;
            let mut file = File::from(fd);
            w.tar.entry(&node, Some(&mut file)).map_err(io_failure)?;
        }
        FileType::Symlink => {
            let target = rustix::fs::readlinkat(dir, name, Vec::new()).map_err(|e| fail(&child, "read the link", e))?;
            let node = base(child, &st, Kind::Symlink(target.into_bytes()), w.epoch);
            w.tar.entry(&node, None).map_err(io_failure)?;
        }
        FileType::CharacterDevice => {
            w.tar
                .entry(&base(child, &st, Kind::Char(rdev.0, rdev.1), w.epoch), None)
                .map_err(io_failure)?;
        }
        FileType::BlockDevice => {
            w.tar
                .entry(&base(child, &st, Kind::Block(rdev.0, rdev.1), w.epoch), None)
                .map_err(io_failure)?;
        }
        FileType::Fifo => w
            .tar
            .entry(&base(child, &st, Kind::Fifo, w.epoch), None)
            .map_err(io_failure)?,
        // A layer cannot hold a socket.
        FileType::Socket | FileType::Unknown => {}
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::path::Path;

    fn open_dir(path: &Path) -> OwnedFd {
        rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap()
    }

    /// The tar `walk_dir` makes of the directory `root` (no lowers, no epoch).
    fn walk_tar(root: &Path) -> Vec<u8> {
        let fd = open_dir(root);
        let mut tar = TarWriter::new(Vec::new());
        let mut links = HashMap::new();
        {
            let mut w = Walk {
                tar: &mut tar,
                links: &mut links,
                lowers: &[],
                epoch: None,
            };
            walk_dir(&mut w, &fd, &[]).unwrap();
        }
        tar.finish(512).unwrap().0
    }

    /// Each entry's type flag, path and link target (PAX `path` and `linkpath`
    /// apply to the next entry; PAX headers are not entries).
    fn entries(tar: &[u8]) -> Vec<(u8, String, String)> {
        let text =
            |b: &[u8]| String::from_utf8_lossy(&b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())]).into_owned();
        let mut out = Vec::new();
        let (mut at, mut path, mut link) = (0, None, None);
        while at + 512 <= tar.len() && tar[at..at + 512].iter().any(|&b| b != 0) {
            let h = &tar[at..at + 512];
            let size = usize::from_str_radix(text(&h[124..136]).trim(), 8).unwrap();
            if h[156] == b'x' {
                for rec in String::from_utf8_lossy(&tar[at + 512..at + 512 + size]).lines() {
                    let kv = rec.split_once(' ').unwrap().1;
                    match kv.split_once('=') {
                        Some(("path", v)) => path = Some(v.to_string()),
                        Some(("linkpath", v)) => link = Some(v.to_string()),
                        _ => {}
                    }
                }
            } else {
                out.push((
                    h[156],
                    path.take().unwrap_or_else(|| text(&h[..100])),
                    link.take().unwrap_or_else(|| text(&h[157..257])),
                ));
            }
            at += 512 + size.div_ceil(512) * 512;
        }
        out
    }

    fn paths(tar: &[u8]) -> Vec<String> {
        entries(tar).into_iter().map(|(_, p, _)| p).collect()
    }

    #[test]
    fn names_are_walked_in_byte_order_parents_first() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Created in an order that is not the sorted one.
        for f in ["b", "\u{e9}", "a.b", "B", "a/z", "a/y", "c/d/e"] {
            let p = root.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, f).unwrap();
        }
        let want = ["B", "a/", "a/y", "a/z", "a.b", "b", "c/", "c/d/", "c/d/e", "\u{e9}"];
        assert_eq!(paths(&walk_tar(root)), want);
        // The same tree again gives the same bytes.
        assert_eq!(walk_tar(root), walk_tar(root));
    }

    #[test]
    fn hard_links_share_the_first_path_in_walk_order() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("d")).unwrap();
        std::fs::write(root.join("x1"), "data").unwrap();
        std::fs::hard_link(root.join("x1"), root.join("d/x3")).unwrap();
        std::fs::hard_link(root.join("x1"), root.join("x2")).unwrap();
        std::fs::write(root.join("alone"), "z").unwrap();
        let tar = walk_tar(root);
        assert_eq!(
            entries(&tar),
            [
                (b'0', "alone".into(), String::new()),
                (b'5', "d/".into(), String::new()),
                // `d/x3` sorts first: the file is stored there, the others link to it.
                (b'0', "d/x3".into(), String::new()),
                (b'1', "x1".into(), "d/x3".into()),
                (b'1', "x2".into(), "d/x3".into()),
            ]
        );
        // The data is in the archive once.
        assert_eq!(tar.windows(4).filter(|w| *w == b"data").count(), 1);
    }

    #[test]
    fn sockets_are_skipped_and_fifos_and_symlinks_kept() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let _listener = std::os::unix::net::UnixListener::bind(root.join("sock")).unwrap();
        let fd = open_dir(root);
        rustix::fs::mknodat(&fd, "fifo", FileType::Fifo, Mode::from_raw_mode(0o644), 0).unwrap();
        symlink("target/../x", root.join("ln")).unwrap();
        std::fs::write(root.join("f"), "").unwrap();
        assert_eq!(
            entries(&walk_tar(root)),
            [
                (b'0', "f".into(), String::new()),
                (b'6', "fifo".into(), String::new()),
                (b'2', "ln".into(), "target/../x".into()),
            ]
        );
    }

    /// The walk keeps no recursion on the thread's stack: a tree far deeper than
    /// a small stack could recurse through is sealed, with every level in order.
    #[test]
    fn a_deep_tree_does_not_use_the_stack() {
        const DEPTH: usize = 1500;
        if let Some(max) = rustix::process::getrlimit(rustix::process::Resource::Nofile).maximum {
            // One descriptor per level, as in `build::run`.
            let _ = rustix::process::setrlimit(
                rustix::process::Resource::Nofile,
                rustix::process::Rlimit {
                    current: Some(max),
                    maximum: Some(max),
                },
            );
        }
        let dir = tempfile::tempdir().unwrap();
        let mut cur = open_dir(dir.path());
        for _ in 0..DEPTH {
            rustix::fs::mkdirat(&cur, "d", Mode::from_raw_mode(0o755)).unwrap();
            cur = rustix::fs::openat(
                &cur,
                "d",
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .unwrap();
        }
        rustix::fs::openat(
            &cur,
            "leaf",
            OFlags::WRONLY | OFlags::CREATE | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o644),
        )
        .unwrap();
        drop(cur);
        let root = dir.path().to_path_buf();
        let tar = std::thread::Builder::new()
            .stack_size(192 * 1024)
            .spawn(move || walk_tar(&root))
            .unwrap()
            .join()
            .unwrap();
        let got = paths(&tar);
        assert_eq!(got.len(), DEPTH + 1);
        let deepest = format!("{}leaf", "d/".repeat(DEPTH));
        assert_eq!(got.last(), Some(&deepest));
        assert_eq!(got[DEPTH - 1], "d/".repeat(DEPTH));
    }

    #[test]
    fn xattrs_come_out_sorted_by_name() {
        let path = std::env::temp_dir().join(format!("kiln-seal-xattr-{}", std::process::id()));
        std::fs::write(&path, b"x").unwrap();
        let fd: OwnedFd = std::fs::File::open(&path).unwrap().into();
        let mut set = 0;
        for name in ["user.c", "user.a", "user.b"] {
            match rustix::fs::fsetxattr(&fd, name, name.as_bytes(), rustix::fs::XattrFlags::empty()) {
                Ok(()) => set += 1,
                Err(Errno::NOTSUP) => {
                    eprintln!("skipped: no user xattrs on {}", path.display());
                    std::fs::remove_file(&path).unwrap();
                    return;
                }
                Err(e) => panic!("{e}"),
            }
        }
        let got = xattrs(&fd, b"/f").unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(set, 3);
        let names: Vec<&[u8]> = got.iter().map(|(n, _)| n.as_slice()).collect();
        assert_eq!(names, [&b"user.a"[..], b"user.b", b"user.c"]);
        assert_eq!(got[0].1, b"user.a");
    }
}
