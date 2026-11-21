//! The OCI tar diff a build step is sealed into (spec §9.8 build mode): ustar
//! headers, with PAX records for what ustar cannot hold (long paths and link
//! targets, large ids, sizes and times, extended attributes). The walker over the
//! overlay's upper directory (`linux::seal`) turns overlay whiteouts into
//! `.wh.<name>` and opaque directories into `.wh..wh..opq` before they get here.
//!
//! Paths are relative (no leading `/`, never `..`); directories end in `/`.
//! [`TarWriter::entry`] refuses a path that is not, a hard link target that is
//! not, and an extended attribute name that holds `=` or NUL (a PAX key ends at
//! the first `=`). A path or link target that is not UTF-8 is written as it is
//! (its raw bytes in the ustar field, or in a PAX `path` or `linkpath` record),
//! and a PAX `hdrcharset=BINARY` record, as POSIX asks, goes only with a `path`
//! or `linkpath` record that holds such bytes: it has no effect anywhere else.
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

/// `what` must be a relative path: not empty, no NUL, no leading `/`, no `..` component.
fn check_path(what: &str, p: &[u8]) -> io::Result<()> {
    let bad = |why: &str| {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{what} {:?} {why}", String::from_utf8_lossy(p)),
        ))
    };
    if p.is_empty() {
        return bad("is empty");
    }
    if p.contains(&0) {
        return bad("holds a NUL");
    }
    if p[0] == b'/' {
        return bad("is absolute");
    }
    if p.split(|&b| b == b'/').any(|c| c == b"..") {
        return bad("holds a `..` component");
    }
    Ok(())
}

/// An attribute name is the tail of a PAX key `SCHILY.xattr.<name>=<value>`.
fn check_xattr_name(name: &[u8]) -> io::Result<()> {
    if name.is_empty() || name.contains(&b'=') || name.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "the attribute name {:?} is empty or holds `=` or NUL",
                String::from_utf8_lossy(name)
            ),
        ));
    }
    Ok(())
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
        check_path("the path", &node.path)?;
        if let Kind::HardLink(t) = &node.kind {
            check_path("the hard link target", t)?;
        }
        for (k, _) in &node.xattrs {
            check_xattr_name(k)?;
        }
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
        // `hdrcharset` governs the values of the `path` and `linkpath` records only.
        let binary = (name.len() > 100 && std::str::from_utf8(&name).is_err())
            || (link.len() > 100 && std::str::from_utf8(link).is_err());
        if binary {
            pax.extend(pax_record(b"hdrcharset", b"BINARY"));
        }
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
        if align == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "an alignment of 0"));
        }
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

    /// Keeps the first `limit` bytes written, then fails: what an entry's headers
    /// look like without writing its (possibly enormous) data.
    struct Capture {
        buf: Vec<u8>,
        limit: usize,
    }

    impl Write for Capture {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            if self.buf.len() >= self.limit {
                return Err(io::Error::other("enough"));
            }
            let n = b.len().min(self.limit - self.buf.len());
            self.buf.extend_from_slice(&b[..n]);
            Ok(n)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn field(h: &[u8], a: usize, b: usize) -> u64 {
        let s: String = h[a..b].iter().take_while(|&&c| c != 0).map(|&c| c as char).collect();
        if s.trim().is_empty() {
            0
        } else {
            u64::from_str_radix(s.trim(), 8).unwrap()
        }
    }

    /// The records of the PAX header at the start of `tar`.
    fn pax(tar: &[u8]) -> Vec<(String, Vec<u8>)> {
        assert_eq!(tar[156], b'x', "a PAX header comes first");
        let size = field(&tar[..512], 124, 136) as usize;
        let mut data = &tar[512..512 + size];
        let mut out = Vec::new();
        while !data.is_empty() {
            let sp = data.iter().position(|&b| b == b' ').unwrap();
            let n: usize = std::str::from_utf8(&data[..sp]).unwrap().parse().unwrap();
            let rec = &data[sp + 1..n - 1];
            assert_eq!(data[n - 1], b'\n');
            let eq = rec.iter().position(|&b| b == b'=').unwrap();
            out.push((String::from_utf8_lossy(&rec[..eq]).into_owned(), rec[eq + 1..].to_vec()));
            data = &data[n..];
        }
        out
    }

    fn first_headers(node: &Node) -> Vec<u8> {
        let mut cap = Capture {
            buf: Vec::new(),
            limit: 4096,
        };
        let mut t = TarWriter::new(&mut cap);
        let _ = t.entry(node, Some(&mut io::repeat(0)));
        cap.buf
    }

    fn refused(node: &Node) -> String {
        let mut out = Vec::new();
        let e = TarWriter::new(&mut out)
            .entry(node, Some(&mut io::empty()))
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{e}");
        assert!(out.is_empty(), "nothing is written for a refused entry");
        e.to_string()
    }

    #[test]
    fn a_path_that_is_not_relative_is_refused() {
        for bad in [&b"/etc/passwd"[..], b"a/../b", b"..", b"../x", b"a/..", b"", b"a\0b"] {
            refused(&Node::marker(bad.to_vec()));
        }
        // A hard link names an entry of the archive: the same rule.
        for bad in [&b"/etc/passwd"[..], b"../x", b""] {
            let node = Node {
                kind: Kind::HardLink(bad.to_vec()),
                ..Node::marker(b"l".to_vec())
            };
            let why = refused(&node);
            assert!(why.contains("hard link target"), "{why}");
        }
        // Dots in a name are not `..`; a symlink's target is the image's own business.
        let mut t = TarWriter::new(Vec::new());
        for ok in [&b"..a"[..], b"a..", b"a/..b/c", b".wh..wh..opq"] {
            t.entry(&Node::marker(ok.to_vec()), Some(&mut io::empty())).unwrap();
        }
        let link = Node {
            kind: Kind::Symlink(b"/abs/../x".to_vec()),
            ..Node::marker(b"s".to_vec())
        };
        t.entry(&link, None).unwrap();
    }

    #[test]
    fn an_attribute_name_with_equals_or_nul_is_refused() {
        for bad in [&b"user.a=b"[..], b"user.a\0b", b"=", b""] {
            let node = Node {
                xattrs: vec![(bad.to_vec(), b"v".to_vec())],
                ..Node::marker(b"f".to_vec())
            };
            let why = refused(&node);
            assert!(why.contains("attribute name"), "{why}");
        }
        // The value may hold anything, `=` and NUL included.
        let node = Node {
            xattrs: vec![(b"user.k".to_vec(), b"a=b\0c\n".to_vec())],
            ..Node::marker(b"f".to_vec())
        };
        let h = first_headers(&node);
        assert!(pax(&h).contains(&("SCHILY.xattr.user.k".into(), b"a=b\0c\n".to_vec())));
    }

    #[test]
    fn a_long_path_or_link_that_is_not_utf8_gets_a_binary_charset_record() {
        let mut long = b"x".repeat(150);
        long[3] = 0xfe;
        let h = first_headers(&Node::marker(long.clone()));
        let records = pax(&h);
        assert_eq!(records[0], ("hdrcharset".into(), b"BINARY".to_vec()));
        assert_eq!(records[1], ("path".into(), long));

        let mut target = b"t".repeat(150);
        target[0] = 0xc0;
        let link = Node {
            kind: Kind::Symlink(target.clone()),
            ..Node::marker(b"l".to_vec())
        };
        let records = pax(&first_headers(&link));
        assert_eq!(records[0], ("hdrcharset".into(), b"BINARY".to_vec()));
        assert_eq!(records[1], ("linkpath".into(), target));
    }

    #[test]
    fn short_binary_names_and_attribute_names_get_no_charset_record() {
        // Raw bytes in the ustar name: no PAX header at all.
        let short = Node::marker(b"d\xffir/f\xfe".to_vec());
        let h = first_headers(&short);
        assert_eq!(h[156], b'0', "no PAX header");
        assert_eq!(&h[..7], b"d\xffir/f\xfe");
        let link = Node {
            kind: Kind::Symlink(b"t\xc0".to_vec()),
            ..Node::marker(b"l".to_vec())
        };
        assert_eq!(first_headers(&link)[156], b'2', "no PAX header");
        // A binary attribute name: the key is raw bytes, with no charset record.
        let attr = Node {
            xattrs: vec![(b"user.\xff".to_vec(), b"v".to_vec())],
            ..Node::marker(b"f".to_vec())
        };
        let h = first_headers(&attr);
        let records = pax(&h);
        assert_eq!(records.len(), 1, "{records:?}");
        assert!(records[0].0.starts_with("SCHILY.xattr.user."));
        assert!(h.windows(19).any(|w| w == b"SCHILY.xattr.user.\xff"), "the raw name");
    }

    #[test]
    fn utf8_names_get_no_charset_record() {
        let h = first_headers(&Node::marker("caf\u{e9}".as_bytes().to_vec()));
        assert_eq!(h[156], b'0', "no PAX header");
    }

    #[test]
    fn a_time_before_the_epoch_or_past_eleven_octal_digits_is_a_pax_record() {
        for (mtime, record) in [(-5, "-5"), (MAX_11 as i64 + 1, "8589934592")] {
            let node = Node {
                mtime,
                ..Node::marker(b"f".to_vec())
            };
            let h = first_headers(&node);
            assert_eq!(pax(&h), [("mtime".into(), record.as_bytes().to_vec())], "{mtime}");
            // The entry's own header holds 0 or the field's largest value.
            let header = &h[1024..1536];
            assert_eq!(field(header, 136, 148), if mtime < 0 { 0 } else { MAX_11 });
        }
        for mtime in [0, 1_700_000_000, MAX_11 as i64] {
            let h = first_headers(&Node {
                mtime,
                ..Node::marker(b"f".to_vec())
            });
            assert_eq!(h[156], b'0', "{mtime} fits the header");
            assert_eq!(field(&h[..512], 136, 148), mtime as u64);
        }
    }

    #[test]
    fn a_size_past_eight_gib_is_a_pax_record() {
        let big = MAX_11 + 1;
        let node = Node {
            kind: Kind::File(big),
            ..Node::marker(b"huge".to_vec())
        };
        let h = first_headers(&node);
        assert_eq!(pax(&h), [("size".into(), b"8589934592".to_vec())]);
        let header = &h[1024..1536];
        assert_eq!((header[156], field(header, 124, 136)), (b'0', MAX_11));
        // The largest size the header holds needs no record.
        let node = Node {
            kind: Kind::File(MAX_11),
            ..Node::marker(b"huge".to_vec())
        };
        let h = first_headers(&node);
        assert_eq!((h[156], field(&h[..512], 124, 136)), (b'0', MAX_11));
    }

    #[test]
    fn devices_and_fifos_are_typed_entries_without_data() {
        let mut t = TarWriter::new(Vec::new());
        let nodes = [
            (Kind::Char(1, 3), b'3'),
            (Kind::Block(8, 16), b'4'),
            (Kind::Block(259, 1_048_575), b'4'),
            (Kind::Fifo, b'6'),
        ];
        for (i, (kind, _)) in nodes.iter().enumerate() {
            let node = Node {
                kind: kind.clone(),
                mode: 0o640,
                ..Node::marker(format!("n{i}").into_bytes())
            };
            t.entry(&node, None).unwrap();
        }
        let (tar, w) = t.finish(512).unwrap();
        assert_eq!(w.entries, 4);
        assert_eq!(tar.len(), 6 * 512, "four headers, two end blocks");
        for (i, (kind, flag)) in nodes.iter().enumerate() {
            let h = &tar[i * 512..(i + 1) * 512];
            assert_eq!(h[156], *flag, "{kind:?}");
            assert_eq!(field(h, 124, 136), 0, "{kind:?}: no data");
            assert_eq!(field(h, 100, 108), 0o640);
            let (ma, mi) = match kind {
                Kind::Char(a, b) | Kind::Block(a, b) => (u64::from(*a), u64::from(*b)),
                _ => (0, 0),
            };
            assert_eq!((field(h, 329, 337), field(h, 337, 345)), (ma, mi), "{kind:?}");
            assert_eq!(&h[257..263], b"ustar\0");
        }
    }

    #[test]
    fn the_padding_follows_the_alignment_asked_for() {
        // Not a multiple of the tar block: the range still ends on `align`.
        let mut t = TarWriter::new(Vec::new());
        t.entry(&Node::marker(b"a".to_vec()), Some(&mut io::empty())).unwrap();
        let (tar, w) = t.finish(1000).unwrap();
        assert_eq!(w.len, 2000);
        assert_eq!(tar.len(), 2000);
        assert!(tar[3 * 512..].iter().all(|&b| b == 0));
        // Already a multiple: no padding.
        let (tar, w) = TarWriter::new(Vec::new()).finish(512).unwrap();
        assert_eq!((w.len, tar.len()), (1024, 1024));
        // No alignment to speak of.
        let (_, w) = TarWriter::new(Vec::new()).finish(1).unwrap();
        assert_eq!(w.len, 1024);
        assert!(TarWriter::new(Vec::new()).finish(0).is_err());
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
