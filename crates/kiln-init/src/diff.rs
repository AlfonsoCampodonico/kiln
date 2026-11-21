//! The OCI tar diff a build step is sealed into (spec §9.8 build mode): ustar
//! headers, with PAX records for what ustar cannot hold (long paths and link
//! targets, large ids, sizes and times, extended attributes). The walker over the
//! overlay's upper directory (`linux::seal`) turns overlay whiteouts into
//! `.wh.<name>` and opaque directories into `.wh..wh..opq` before they get here.
//!
//! Paths are relative (no leading `/`, never `..`); directories end in `/`.
//! `entries` counts the logical entries, not the PAX headers. The archive ends
//! with two zero blocks, then zeros up to the alignment the output disk asks for,
//! so the range a reader is told about holds nothing after the tar's end.

use std::io::{self, Read, Write};

const BLOCK: usize = 512;

/// One entry's type and type-specific data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Dir,
    /// A regular file of this many bytes; its data follows the header.
    File(u64),
    Symlink(Vec<u8>),
    /// A hard link to an earlier entry of the same archive (its path).
    HardLink(Vec<u8>),
    Char(u32, u32),
    Block(u32, u32),
    Fifo,
}

/// One entry: everything but a file's data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// Relative, `/`-separated, without a trailing `/`.
    pub path: Vec<u8>,
    pub kind: Kind,
    /// Permission bits with setuid, setgid and sticky (`0o7777`).
    pub mode: u32,
    pub uid: u64,
    pub gid: u64,
    /// Seconds since the epoch.
    pub mtime: i64,
    /// Full names (`user.foo`, `security.capability`) and raw values.
    pub xattrs: Vec<(Vec<u8>, Vec<u8>)>,
}

/// Whether a sealed diff keeps the extended attribute `name`: `user.*` (but
/// `user.overlay.*`, which an overlay mounted with `userxattr`, as potter's
/// namespace executor mounts its lowers, would read as its own) and
/// `security.capability` (file capabilities, as `setcap` sets them). Everything
/// else is dropped: the overlay's `trusted.overlay.*`, other `trusted.*` and
/// `security.*` attributes (SELinux labels), and `system.*` (POSIX ACLs).
pub fn keep_xattr(name: &[u8]) -> bool {
    (name.starts_with(b"user.") && !name.starts_with(b"user.overlay.")) || name == b"security.capability"
}

/// The mtime a diff records: at most `epoch` when one is set (`SOURCE_DATE_EPOCH`).
pub fn clamp_mtime(mtime: i64, epoch: Option<u64>) -> i64 {
    match epoch {
        Some(e) => mtime.min(i64::try_from(e).unwrap_or(i64::MAX)),
        None => mtime,
    }
}

impl Node {
    /// An empty regular file (a whiteout or opaque marker).
    pub fn marker(path: Vec<u8>) -> Self {
        Node {
            path,
            kind: Kind::File(0),
            mode: 0o644,
            uid: 0,
            gid: 0,
            mtime: 0,
            xattrs: Vec::new(),
        }
    }
}

/// Writes a tar archive and counts its entries and bytes.
pub struct TarWriter<W: Write> {
    out: W,
    entries: u64,
    len: u64,
}

/// What a finished archive holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Written {
    pub entries: u64,
    /// Bytes written, alignment padding included.
    pub len: u64,
}

const MAX_7: u64 = 0o7777777;
const MAX_11: u64 = 0o77777777777;

/// A PAX record: `<len> <key>=<value>\n`, where `len` counts itself.
fn pax_record(key: &[u8], value: &[u8]) -> Vec<u8> {
    let body = key.len() + value.len() + 3;
    let mut n = body + 1;
    while (body + n.to_string().len()) != n {
        n = body + n.to_string().len();
    }
    let mut r = format!("{n} ").into_bytes();
    r.extend_from_slice(key);
    r.push(b'=');
    r.extend_from_slice(value);
    r.push(b'\n');
    r
}

fn octal(field: &mut [u8], v: u64) {
    let digits = field.len() - 1;
    let s = format!("{v:0digits$o}");
    field[..digits].copy_from_slice(&s.as_bytes()[s.len() - digits..]);
    field[digits] = 0;
}

impl<W: Write> TarWriter<W> {
    pub fn new(out: W) -> Self {
        Self {
            out,
            entries: 0,
            len: 0,
        }
    }

    fn put(&mut self, b: &[u8]) -> io::Result<()> {
        self.out.write_all(b)?;
        self.len += b.len() as u64;
        Ok(())
    }

    fn pad(&mut self, n: u64) -> io::Result<()> {
        let rem = (n % BLOCK as u64) as usize;
        if rem != 0 {
            self.put(&[0u8; BLOCK][..BLOCK - rem])?;
        }
        Ok(())
    }

    fn header(&mut self, name: &[u8], typeflag: u8, size: u64, node: &Node, link: &[u8]) -> io::Result<()> {
        let mut h = [0u8; BLOCK];
        let n = name.len().min(100);
        h[..n].copy_from_slice(&name[..n]);
        octal(&mut h[100..108], u64::from(node.mode & 0o7777));
        octal(&mut h[108..116], node.uid.min(MAX_7));
        octal(&mut h[116..124], node.gid.min(MAX_7));
        octal(&mut h[124..136], size.min(MAX_11));
        octal(&mut h[136..148], u64::try_from(node.mtime).unwrap_or(0).min(MAX_11));
        h[156] = typeflag;
        let l = link.len().min(100);
        h[157..157 + l].copy_from_slice(&link[..l]);
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        if let Kind::Char(ma, mi) | Kind::Block(ma, mi) = node.kind {
            octal(&mut h[329..337], u64::from(ma));
            octal(&mut h[337..345], u64::from(mi));
        }
        h[148..156].fill(b' ');
        let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
        let s = format!("{sum:06o}\0 ");
        h[148..156].copy_from_slice(s.as_bytes());
        self.put(&h)
    }

    /// Writes one entry; a regular file's data comes from `data` (exactly its size).
    pub fn entry(&mut self, node: &Node, data: Option<&mut dyn Read>) -> io::Result<()> {
        let mut name = node.path.clone();
        if node.kind == Kind::Dir {
            name.push(b'/');
        }
        let (typeflag, size, link): (u8, u64, &[u8]) = match &node.kind {
            Kind::Dir => (b'5', 0, b""),
            Kind::File(n) => (b'0', *n, b""),
            Kind::Symlink(t) => (b'2', 0, t),
            Kind::HardLink(t) => (b'1', 0, t),
            Kind::Char(..) => (b'3', 0, b""),
            Kind::Block(..) => (b'4', 0, b""),
            Kind::Fifo => (b'6', 0, b""),
        };
        let mut pax = Vec::new();
        if name.len() > 100 {
            pax.extend(pax_record(b"path", &name));
        }
        if link.len() > 100 {
            pax.extend(pax_record(b"linkpath", link));
        }
        for (key, v) in [(&b"uid"[..], node.uid), (b"gid", node.gid)] {
            if v > MAX_7 {
                pax.extend(pax_record(key, v.to_string().as_bytes()));
            }
        }
        if size > MAX_11 {
            pax.extend(pax_record(b"size", size.to_string().as_bytes()));
        }
        if node.mtime < 0 || node.mtime as u64 > MAX_11 {
            pax.extend(pax_record(b"mtime", node.mtime.to_string().as_bytes()));
        }
        for (k, v) in &node.xattrs {
            let mut key = b"SCHILY.xattr.".to_vec();
            key.extend_from_slice(k);
            pax.extend(pax_record(&key, v));
        }
        if !pax.is_empty() {
            let mut pname = b"PaxHeaders/".to_vec();
            pname.extend_from_slice(&name[name.len().saturating_sub(80)..]);
            let header = Node {
                mode: 0o644,
                uid: 0,
                gid: 0,
                mtime: 0,
                ..node.clone()
            };
            self.header(&pname, b'x', pax.len() as u64, &header, b"")?;
            self.put(&pax)?;
            self.pad(pax.len() as u64)?;
        }
        self.header(&name, typeflag, size, node, link)?;
        if let (Kind::File(n), Some(r)) = (&node.kind, data) {
            let copied = io::copy(&mut r.take(*n), &mut CountingWrite(self))?;
            if copied != *n {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!(
                        "{} changed size while it was sealed",
                        String::from_utf8_lossy(&node.path)
                    ),
                ));
            }
            self.pad(*n)?;
        } else if size != 0 {
            return Err(io::Error::other("a file entry without its data"));
        }
        self.entries += 1;
        Ok(())
    }

    /// Ends the archive (two zero blocks), then pads with zeros to a multiple of `align`.
    pub fn finish(mut self, align: u64) -> io::Result<(W, Written)> {
        self.put(&[0u8; 2 * BLOCK])?;
        let rem = self.len % align;
        if rem != 0 {
            let zeros = vec![0u8; (align - rem) as usize];
            self.put(&zeros)?;
        }
        self.out.flush()?;
        Ok((
            self.out,
            Written {
                entries: self.entries,
                len: self.len,
            },
        ))
    }
}

struct CountingWrite<'a, W: Write>(&'a mut TarWriter<W>);

impl<W: Write> Write for CountingWrite<'_, W> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        let n = self.0.out.write(b)?;
        self.0.len += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.out.flush()
    }
}

/// `<dir>/.wh.<name>`: the entry that deletes `name`.
pub fn whiteout(parent: &[u8], name: &[u8]) -> Vec<u8> {
    join(parent, &[b".wh.", name].concat())
}

/// `<dir>/.wh..wh..opq`: hides everything below `dir` in the layers beneath.
pub fn opaque(dir: &[u8]) -> Vec<u8> {
    join(dir, b".wh..wh..opq")
}

pub fn join(parent: &[u8], name: &[u8]) -> Vec<u8> {
    if parent.is_empty() {
        name.to_vec()
    } else {
        [parent, b"/", name].concat()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_user_attributes_and_file_capabilities_are_kept() {
        for keep in [&b"user.k"[..], b"user.mime_type", b"security.capability"] {
            assert!(keep_xattr(keep), "{}", String::from_utf8_lossy(keep));
        }
        for drop in [
            &b"trusted.overlay.opaque"[..],
            b"trusted.overlay.origin",
            b"trusted.x",
            b"user.overlay.opaque",
            b"security.selinux",
            b"security.ima",
            b"system.posix_acl_access",
            b"capability",
        ] {
            assert!(!keep_xattr(drop), "{}", String::from_utf8_lossy(drop));
        }
    }

    #[test]
    fn source_date_epoch_clamps_later_times_only() {
        assert_eq!(clamp_mtime(1_800_000_000, Some(1_700_000_000)), 1_700_000_000);
        assert_eq!(clamp_mtime(5, Some(1_700_000_000)), 5);
        assert_eq!(clamp_mtime(-1, Some(0)), -1);
        assert_eq!(clamp_mtime(1_800_000_000, None), 1_800_000_000);
        assert_eq!(clamp_mtime(7, Some(u64::MAX)), 7);
    }

    #[test]
    fn pax_records_count_their_own_length() {
        assert_eq!(pax_record(b"path", b"a"), b"9 path=a\n");
        let r = pax_record(b"path", &[b'x'; 95]);
        assert_eq!(r.len(), 105);
        assert!(r.starts_with(b"105 path="));
    }

    #[test]
    fn the_archive_is_block_aligned_and_padded_with_zeros() {
        let mut t = TarWriter::new(Vec::new());
        t.entry(&Node::marker(b"a".to_vec()), Some(&mut &b""[..])).unwrap();
        let mut data = &b"hello"[..];
        let file = Node {
            kind: Kind::File(5),
            ..Node::marker(b"b".to_vec())
        };
        t.entry(&file, Some(&mut data)).unwrap();
        let (bytes, w) = t.finish(4096).unwrap();
        assert_eq!(w, Written { entries: 2, len: 4096 });
        assert_eq!(bytes.len(), 4096);
        // a, b, b's data block, two end blocks; zeros after.
        assert!(bytes[5 * 512..].iter().all(|&b| b == 0));
        assert_eq!(&bytes[3 * 512..3 * 512 + 5], &[0; 5], "the end blocks follow the data");
        assert_eq!(&bytes[2 * 512..2 * 512 + 5], b"hello");
    }

    #[test]
    fn a_file_that_shrank_is_an_error() {
        let mut t = TarWriter::new(Vec::new());
        let file = Node {
            kind: Kind::File(10),
            ..Node::marker(b"f".to_vec())
        };
        assert!(t.entry(&file, Some(&mut &b"short"[..])).is_err());
    }
}
