//! Raw tar stream decoding into `Entry` events (spec §7.4, §7.6).

use std::cell::Cell;
use std::io::{self, Read};
use std::rc::Rc;

use tar::{Archive, EntryType, Header};

use crate::apply::{Entry, EntryKind};
use crate::error::{Error, Result, lossy};
use crate::limits::Limits;
use crate::path::normalize;
use crate::pax::{PaxState, parse_records};
use crate::tree::{Meta, Timestamp, XattrKey, Xattrs};

/// Counts consumed bytes and fails reads past the layer byte limit.
struct Counting<R> {
    inner: R,
    count: Rc<Cell<u64>>,
    limit: u64,
    exceeded: Rc<Cell<bool>>,
}

impl<R: Read> Read for Counting<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        let total = self.count.get() + n as u64;
        self.count.set(total);
        if total > self.limit {
            self.exceeded.set(true);
            return Err(io::Error::other("kiln: layer byte limit exceeded"));
        }
        Ok(n)
    }
}

fn tar_err(e: io::Error) -> Error {
    match e.kind() {
        io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof | io::ErrorKind::Other => {
            Error::MalformedTar(e.to_string())
        }
        _ => Error::Io(e),
    }
}

/// Check if a global PAX header contains any recognized keys that would cause unbounded amplification.
/// Uses raw byte matching (no UTF-8 conversion) to prevent bypass via non-UTF-8 keys.
fn has_recognized_pax_keys(records: &[(Vec<u8>, Vec<u8>)]) -> bool {
    for (key, _) in records {
        if matches!(
            key.as_slice(),
            b"path" | b"linkpath" | b"uid" | b"gid" | b"size" | b"mtime"
        ) || key.starts_with(b"SCHILY.xattr.")
            || key.starts_with(b"GNU.sparse.")
            || key.starts_with(b"LIBARCHIVE.xattr.")
        {
            return true;
        }
    }
    false
}

/// Truncate a string to 128 bytes on a char boundary.
fn truncate_to_128(s: &str) -> String {
    let bytes = s.as_bytes();
    if bytes.len() <= 128 {
        return s.to_string();
    }
    // Find the last valid UTF-8 boundary before 128
    let mut end = 128;
    while end > 0 && !std::str::from_utf8(&bytes[..end]).is_ok() {
        end -= 1;
    }
    std::str::from_utf8(&bytes[..end]).unwrap_or("").to_string()
}

/// Add a warning with a maximum of 100 messages; further warnings are counted.
fn add_warning(warnings: &mut Vec<String>, suppressed: &mut usize, msg: String) {
    if warnings.len() < 100 {
        warnings.push(msg);
    } else {
        *suppressed += 1;
    }
}

/// Reads entries up to the end-of-archive marker; returns tar bytes consumed.
pub(crate) fn read_tar<R: Read>(
    reader: R,
    limits: &Limits,
    warnings: &mut Vec<String>,
    f: &mut dyn FnMut(Entry, &mut dyn Read) -> Result<()>,
) -> Result<u64> {
    let count = Rc::new(Cell::new(0u64));
    let exceeded = Rc::new(Cell::new(false));
    let mut archive = Archive::new(Counting {
        inner: reader,
        count: Rc::clone(&count),
        limit: limits.max_layer_bytes,
        exceeded: Rc::clone(&exceeded),
    });
    let mut suppressed_count = 0usize;
    match walk(&mut archive, limits, warnings, &mut suppressed_count, f) {
        Err(_) if exceeded.get() => Err(Error::LimitExceeded {
            limit: "uncompressed bytes per layer",
            max: limits.max_layer_bytes,
            path: String::new(),
        }),
        Err(e) => Err(e),
        Ok(()) => {
            if suppressed_count > 0 {
                warnings.push(format!("{} more warnings suppressed", suppressed_count));
            }
            Ok(count.get())
        }
    }
}

fn walk<R: Read>(
    archive: &mut Archive<R>,
    limits: &Limits,
    warnings: &mut Vec<String>,
    suppressed_count: &mut usize,
    f: &mut dyn FnMut(Entry, &mut dyn Read) -> Result<()>,
) -> Result<()> {
    let mut global = PaxState::default();
    let mut local = PaxState::default();
    let mut long_name: Option<Vec<u8>> = None;
    let mut long_link: Option<Vec<u8>> = None;
    let mut seen: u64 = 0;
    let mut seen_local_pax = false;
    let mut seen_gnu_longname = false;
    let mut seen_gnu_longlink = false;
    for item in archive.entries().map_err(tar_err)?.raw(true) {
        let mut ent = item.map_err(tar_err)?;
        let header = ent.header().clone();
        let et = header.entry_type();
        if et.is_pax_local_extensions() || et.is_pax_global_extensions() || et.is_gnu_longname() || et.is_gnu_longlink()
        {
            let data = read_record(&mut ent, &header, limits)?;
            if et.is_pax_local_extensions() {
                if seen_local_pax {
                    return Err(Error::MalformedTar("duplicate x header before one entry".into()));
                }
                seen_local_pax = true;
                local.apply(parse_records(&data)?)?;
            } else if et.is_pax_global_extensions() {
                let records = parse_records(&data)?;
                if has_recognized_pax_keys(&records) {
                    return Err(Error::UnsupportedEntry {
                        path: lossy(&header.path_bytes()),
                        kind: "global PAX header override".into(),
                    });
                }
                global.apply(records)?;
            } else if et.is_gnu_longname() {
                if seen_gnu_longname {
                    return Err(Error::MalformedTar("duplicate L header before one entry".into()));
                }
                seen_gnu_longname = true;
                let trimmed = trim_nul(data);
                long_name = Some(trimmed);
            } else {
                if seen_gnu_longlink {
                    return Err(Error::MalformedTar("duplicate K header before one entry".into()));
                }
                seen_gnu_longlink = true;
                let trimmed = trim_nul(data);
                long_link = Some(trimmed);
            }
            continue;
        }
        // Reset per-entry flags for the next filesystem entry
        seen_local_pax = false;
        seen_gnu_longname = false;
        seen_gnu_longlink = false;

        let pax = PaxState::overlay(&global, std::mem::take(&mut local));
        // Go's archive/tar and kiln would pick different names for these.
        if (pax.path.is_some() && long_name.is_some()) || (pax.linkpath.is_some() && long_link.is_some()) {
            return Err(Error::UnsupportedEntry {
                path: lossy(pax.path.as_deref().unwrap_or(&header.path_bytes())),
                kind: "conflicting long-name and PAX path".into(),
            });
        }
        let raw_path = match pax.path.clone().or(long_name.take()) {
            Some(p) => p,
            None => header_name(&header)?,
        };
        let raw_link = pax
            .linkpath
            .clone()
            .or(long_link.take())
            .or_else(|| header.link_name_bytes().map(|c| c.into_owned()));
        seen += 1;
        if seen > limits.max_entries {
            return Err(Error::LimitExceeded {
                limit: "entries per layer",
                max: limits.max_entries,
                path: lossy(&raw_path),
            });
        }
        let entry = build_entry(
            &header,
            et,
            &raw_path,
            raw_link,
            &pax,
            limits,
            warnings,
            suppressed_count,
        )?;
        f(entry, &mut ent)?;
    }
    Ok(())
}

fn read_record(ent: &mut dyn Read, header: &Header, limits: &Limits) -> Result<Vec<u8>> {
    let size = header.entry_size().map_err(tar_err)?;
    if size > limits.max_header_record {
        return Err(Error::LimitExceeded {
            limit: "tar header record",
            max: limits.max_header_record,
            path: lossy(&header.path_bytes()),
        });
    }
    let mut data = Vec::with_capacity(size as usize);
    ent.take(size).read_to_end(&mut data).map_err(tar_err)?;
    if data.len() as u64 != size {
        return Err(Error::MalformedTar("truncated header record".into()));
    }
    Ok(data)
}

fn trim_nul(mut v: Vec<u8>) -> Vec<u8> {
    while v.last() == Some(&0) {
        v.pop();
    }
    v
}

/// How Go's `archive/tar` (containerd's parser) classifies a header block
/// (`getFormat` in reader.go). The `tar` crate's own rules differ, so names and
/// device numbers are read from the raw block with Go's rules instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GoFormat {
    V7,
    Ustar,
    Star,
    Gnu,
}

fn go_format(b: &[u8]) -> GoFormat {
    let (magic, version, trailer) = (&b[257..263], &b[263..265], &b[508..512]);
    match () {
        _ if magic == b"ustar\0" && trailer == b"tar\0" => GoFormat::Star,
        // Go does not check the version for USTAR, unlike the `tar` crate.
        _ if magic == b"ustar\0" => GoFormat::Ustar,
        _ if magic == b"ustar " && version == b" \0" => GoFormat::Gnu,
        _ => GoFormat::V7,
    }
}

/// A NUL-terminated field (Go's `parseString`).
fn c_string(f: &[u8]) -> &[u8] {
    &f[..f.iter().position(|&c| c == 0).unwrap_or(f.len())]
}

/// Go's `parseNumeric`: base-256 when the high bit is set, else octal with NULs
/// and spaces trimmed (empty is 0). `None` where Go reports a header error.
fn go_numeric(f: &[u8]) -> Option<i64> {
    if f.first().is_some_and(|&b| b & 0x80 != 0) {
        let inv = if f[0] & 0x40 != 0 { 0xff } else { 0 };
        let mut x: u64 = 0;
        for (i, &c) in f.iter().enumerate() {
            let c = if i == 0 { (c ^ inv) & 0x7f } else { c ^ inv };
            if x >> 56 > 0 {
                return None;
            }
            x = (x << 8) | c as u64;
        }
        if x >> 63 > 0 {
            return None;
        }
        return Some(if inv == 0xff { !(x as i64) } else { x as i64 });
    }
    let s = std::str::from_utf8(f).ok()?.trim_matches([' ', '\0']);
    if s.is_empty() {
        return Some(0);
    }
    u64::from_str_radix(s, 8).ok().and_then(|v| i64::try_from(v).ok())
}

/// The name a header block carries by itself (before GNU long names or PAX
/// paths), with Go's prefix rules: USTAR prefixes are 155 bytes and STAR's 131;
/// GNU headers have none, except that Go falls back to the 155-byte field when
/// the GNU atime/ctime fields do not parse and it holds ASCII (golang.org/issue/12594).
fn header_name(header: &Header) -> Result<Vec<u8>> {
    let b = header.as_bytes();
    let name = c_string(&b[0..100]);
    let prefix: &[u8] = match go_format(b) {
        GoFormat::V7 => b"",
        GoFormat::Ustar => c_string(&b[345..500]),
        GoFormat::Star => {
            if go_numeric(&b[476..488]).is_none() || go_numeric(&b[488..500]).is_none() {
                return Err(Error::MalformedTar(format!(
                    "STAR header for {:?} has invalid access or change times",
                    lossy(name)
                )));
            }
            c_string(&b[345..476])
        }
        GoFormat::Gnu => {
            let unparsable = |f: &[u8]| f[0] != 0 && go_numeric(f).is_none();
            let prefix = c_string(&b[345..500]);
            if (unparsable(&b[345..357]) || unparsable(&b[357..369])) && prefix.is_ascii() {
                prefix
            } else {
                b""
            }
        }
    };
    Ok(if prefix.is_empty() {
        name.to_vec()
    } else {
        [prefix, b"/", name].concat()
    })
}

/// Device numbers as Go reads them: parsed (and validated) for every non-V7
/// header, whatever its entry type; V7 headers have none.
fn header_devices(header: &Header, path: &[u8]) -> Result<(i64, i64)> {
    let b = header.as_bytes();
    if go_format(b) == GoFormat::V7 {
        return Ok((0, 0));
    }
    match (go_numeric(&b[329..337]), go_numeric(&b[337..345])) {
        (Some(major), Some(minor)) => Ok((major, minor)),
        _ => Err(Error::MalformedTar(format!(
            "invalid device number fields for {:?}",
            lossy(path)
        ))),
    }
}

/// A numeric header field at `range`. Go's archive/tar (containerd) reads a field of
/// only NULs and spaces as 0, where the tar crate fails; match Go.
fn numeric<T: Default>(header: &Header, range: std::ops::Range<usize>, parsed: io::Result<T>) -> Result<T> {
    if header.as_bytes()[range].iter().all(|&b| b == 0 || b == b' ') {
        return Ok(T::default());
    }
    parsed.map_err(tar_err)
}

fn to_u32(v: u64, what: &'static str, path: &[u8]) -> Result<u32> {
    u32::try_from(v).map_err(|_| Error::UnsupportedEntry {
        path: lossy(path),
        kind: format!("{what} {v} exceeds 32 bits"),
    })
}

#[allow(clippy::too_many_arguments)]
fn build_entry(
    header: &Header,
    et: EntryType,
    raw_path: &[u8],
    raw_link: Option<Vec<u8>>,
    pax: &PaxState,
    limits: &Limits,
    warnings: &mut Vec<String>,
    suppressed_count: &mut usize,
) -> Result<Entry> {
    let unsupported = |kind: &str| Error::UnsupportedEntry {
        path: lossy(raw_path),
        kind: kind.to_string(),
    };
    if pax.sparse || et.is_gnu_sparse() {
        return Err(unsupported("sparse file"));
    }
    let path = normalize(raw_path, limits)?;
    let typeflag = header.as_bytes()[156];
    let size = header.entry_size().map_err(tar_err)?;
    if pax.size.is_some_and(|s| s != size) {
        return Err(unsupported("PAX size override (entries larger than 8 GiB)"));
    }
    // Check for parser differential: header-only types with nonzero size
    if (matches!(
        et,
        EntryType::Link
            | EntryType::Symlink
            | EntryType::Char
            | EntryType::Block
            | EntryType::Directory
            | EntryType::Fifo
    ) || (typeflag == 0 && raw_path.ends_with(b"/")))
        && size != 0
    {
        return Err(unsupported("header-only entry with nonzero size"));
    }
    let uid = match pax.uid {
        Some(v) => v,
        None => numeric(header, 108..116, header.uid())?,
    };
    let gid = match pax.gid {
        Some(v) => v,
        None => numeric(header, 116..124, header.gid())?,
    };
    let (uid, gid) = (to_u32(uid, "uid", &path)?, to_u32(gid, "gid", &path)?);
    let mtime = match pax.mtime {
        Some(t) => t,
        None => Timestamp {
            sec: numeric(header, 136..148, header.mtime())? as i64,
            nsec: 0,
        },
    };
    let meta = Meta {
        mode: numeric(header, 100..108, header.mode())? & 0o7777,
        uid,
        gid,
        mtime,
    };
    let xattrs = convert_xattrs(pax, &path, limits, warnings, suppressed_count)?;
    let (dev_major, dev_minor) = header_devices(header, &path)?;
    let device = || -> Result<(u32, u32)> {
        if !(0..=0xfff).contains(&dev_major) || !(0..=0xf_ffff).contains(&dev_minor) {
            return Err(unsupported("device number out of range"));
        }
        Ok((dev_major as u32, dev_minor as u32))
    };
    let kind = match et {
        _ if typeflag == 0 && raw_path.ends_with(b"/") => EntryKind::Dir,
        EntryType::Regular | EntryType::Continuous => EntryKind::File { size },
        EntryType::Directory => EntryKind::Dir,
        EntryType::Symlink => {
            let target = raw_link.unwrap_or_default();
            if target.is_empty() {
                return Err(unsupported("empty symlink target"));
            }
            if target.contains(&0) || target.len() > limits.max_path_len {
                return Err(Error::InvalidPath {
                    path: lossy(&path),
                    reason: "symlink target has NUL or is too long",
                });
            }
            EntryKind::Symlink { target }
        }
        EntryType::Link => {
            let target = raw_link.ok_or_else(|| Error::InvalidHardlink {
                path: lossy(&path),
                target: String::new(),
                reason: "missing target",
            })?;
            EntryKind::Hardlink {
                target: normalize(&target, limits)?,
            }
        }
        EntryType::Char => {
            let (major, minor) = device()?;
            EntryKind::CharDev { major, minor }
        }
        EntryType::Block => {
            let (major, minor) = device()?;
            EntryKind::BlockDev { major, minor }
        }
        EntryType::Fifo => EntryKind::Fifo,
        other => return Err(unsupported(&format!("tar entry type {other:?}"))),
    };
    Ok(Entry {
        path,
        kind,
        meta,
        xattrs,
    })
}

fn convert_xattrs(
    pax: &PaxState,
    path: &[u8],
    limits: &Limits,
    warnings: &mut Vec<String>,
    suppressed_count: &mut usize,
) -> Result<Xattrs> {
    let mut out = Xattrs::new();
    let path_str = truncate_to_128(&lossy(path));
    for name in &pax.dropped {
        let name_str = truncate_to_128(&lossy(name));
        let msg = format!("dropping LIBARCHIVE xattr {:?} on {:?}", name_str, path_str);
        add_warning(warnings, suppressed_count, msg);
    }
    for (name, value) in &pax.xattrs {
        if value.len() as u64 > limits.max_header_record {
            return Err(Error::LimitExceeded {
                limit: "xattr value",
                max: limits.max_header_record,
                path: lossy(path),
            });
        }
        let Some(key) = XattrKey::from_full_name(name) else {
            let name_str = truncate_to_128(&lossy(name));
            let msg = format!(
                "dropping xattr {:?} on {:?}: namespace not representable in erofs",
                name_str, path_str
            );
            add_warning(warnings, suppressed_count, msg);
            continue;
        };
        if key.is_overlay() {
            // Only kiln's own markers (from `.wh.` entries) may steer the overlay.
            let name_str = truncate_to_128(&lossy(name));
            let msg = format!("dropping overlay xattr {:?} on {:?}", name_str, path_str);
            add_warning(warnings, suppressed_count, msg);
            continue;
        }
        let bad = |reason| Error::XattrUnencodable {
            path: lossy(path),
            name: lossy(name),
            reason,
        };
        if key.name.len() > 255 {
            return Err(bad("name longer than 255 bytes"));
        }
        if value.len() > 65_535 {
            return Err(bad("value larger than 65535 bytes"));
        }
        out.insert(key, value.clone());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ondisk::{XATTR_INDEX_SECURITY, XATTR_INDEX_USER};
    use crate::testtar::{Opts, TarBuilder};

    /// Entries with their data, warnings, and bytes consumed.
    type Collected = (Vec<(Entry, Vec<u8>)>, Vec<String>, u64);

    fn collect(tar: &[u8], limits: &Limits) -> Result<Collected> {
        let mut out = Vec::new();
        let mut warnings = Vec::new();
        let n = read_tar(tar, limits, &mut warnings, &mut |e: Entry, r: &mut dyn Read| {
            let mut data = Vec::new();
            if matches!(e.kind, EntryKind::File { .. }) {
                r.read_to_end(&mut data)?;
            }
            out.push((e, data));
            Ok(())
        })?;
        Ok((out, warnings, n))
    }

    /// A raw header block (fields as in Go's reader.go), for format-detection tests.
    struct Raw(Vec<u8>);

    impl Raw {
        fn new(name: &[u8], magic: &[u8; 6], version: &[u8; 2]) -> Self {
            let mut b = vec![0u8; 512];
            b[..name.len()].copy_from_slice(name);
            b[100..108].copy_from_slice(b"0000644\0");
            b[108..116].copy_from_slice(b"0000000\0");
            b[116..124].copy_from_slice(b"0000000\0");
            b[124..136].copy_from_slice(b"00000000000\0");
            b[136..148].copy_from_slice(b"14000000000\0");
            b[156] = b'0';
            b[257..263].copy_from_slice(magic);
            b[263..265].copy_from_slice(version);
            Raw(b)
        }
        fn ustar(name: &[u8]) -> Self {
            Self::new(name, b"ustar\0", b"00")
        }
        fn gnu(name: &[u8]) -> Self {
            Self::new(name, b"ustar ", b" \0")
        }
        fn at(mut self, off: usize, data: &[u8]) -> Self {
            self.0[off..off + data.len()].copy_from_slice(data);
            self
        }
        fn tar(mut self) -> Vec<u8> {
            self.0[148..156].copy_from_slice(b"        ");
            let sum: u32 = self.0.iter().map(|&b| b as u32).sum();
            self.0[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
            let mut t = self.0;
            t.resize(512 + 1024, 0);
            t
        }
    }

    fn first_path(tar: Vec<u8>) -> Result<Vec<u8>> {
        Ok(collect(&tar, &Limits::default())?.0.remove(0).0.path)
    }

    // Expected values below were checked against Go's archive/tar (containerd's parser).

    #[test]
    fn ustar_magic_with_any_version_uses_the_prefix() {
        let t = Raw::ustar(b"file").at(263, b"xx").at(345, b"dir").tar();
        assert_eq!(first_path(t).unwrap(), b"dir/file");
    }

    #[test]
    fn star_trailer_limits_the_prefix_to_131_bytes() {
        let t = Raw::ustar(b"file").at(345, &[b'p'; 131]).at(508, b"tar\0").tar();
        let mut want = vec![b'p'; 131];
        want.extend_from_slice(b"/file");
        assert_eq!(first_path(t).unwrap(), want);
    }

    #[test]
    fn star_header_with_unparseable_times_is_rejected() {
        let t = Raw::ustar(b"file")
            .at(345, b"dir")
            .at(476, b"zzzz")
            .at(508, b"tar\0")
            .tar();
        assert!(matches!(first_path(t), Err(Error::MalformedTar(_))));
    }

    #[test]
    fn gnu_header_uses_an_ascii_prefix_only_when_its_times_do_not_parse() {
        let text = Raw::gnu(b"file").at(345, b"dir").tar();
        assert_eq!(first_path(text).unwrap(), b"dir/file");
        let octal = Raw::gnu(b"file")
            .at(345, b"00000000001\0")
            .at(357, b"00000000002\0")
            .tar();
        assert_eq!(first_path(octal).unwrap(), b"file");
        let non_ascii = Raw::gnu(b"file").at(345, b"d\xffr").tar();
        assert_eq!(first_path(non_ascii).unwrap(), b"file");
    }

    #[test]
    fn v7_header_never_has_a_prefix() {
        let t = Raw::new(b"file", &[0; 6], &[0; 2])
            .at(345, b"dir")
            .at(329, b"zz\0")
            .tar();
        assert_eq!(first_path(t).unwrap(), b"file", "and V7 device fields are not parsed");
    }

    #[test]
    fn device_numbers_are_read_for_ustar_magic_with_any_version() {
        let t = Raw::ustar(b"dev/x")
            .at(263, b"xx")
            .at(156, b"3")
            .at(329, b"0000001\0")
            .at(337, b"0000003\0")
            .tar();
        let es = collect(&t, &Limits::default()).unwrap().0;
        assert!(matches!(es[0].0.kind, EntryKind::CharDev { major: 1, minor: 3 }));
    }

    #[test]
    fn garbage_device_fields_reject_any_non_v7_entry() {
        for t in [
            Raw::ustar(b"file").at(329, b"zz\0").tar(),
            Raw::gnu(b"file").at(329, b"zz\0").tar(),
        ] {
            assert!(matches!(collect(&t, &Limits::default()), Err(Error::MalformedTar(_))));
        }
    }

    #[test]
    fn empty_numeric_fields_read_as_zero_like_go() {
        let mut h = tar::Header::new_ustar();
        h.set_path("dev/x").unwrap();
        h.set_size(0);
        h.set_entry_type(tar::EntryType::Char);
        for r in [100..108, 108..116, 116..124, 136..148, 329..337, 337..345] {
            h.as_mut_bytes()[r].fill(0);
        }
        h.as_mut_bytes()[108..116].copy_from_slice(b"        ");
        h.set_cksum();
        let mut tar = h.as_bytes().to_vec();
        tar.resize(tar.len() + 1024, 0);
        let (es, _, _) = collect(&tar, &Limits::default()).unwrap();
        let m = &es[0].0.meta;
        assert_eq!((m.mode, m.uid, m.gid, m.mtime.sec), (0, 0, 0, 0));
        assert!(matches!(es[0].0.kind, EntryKind::CharDev { major: 0, minor: 0 }));
    }

    #[test]
    fn malformed_numeric_fields_are_still_rejected() {
        let mut h = tar::Header::new_ustar();
        h.set_path("f").unwrap();
        h.set_size(0);
        h.as_mut_bytes()[108..116].copy_from_slice(b"12x4567\0");
        h.set_cksum();
        let mut tar = h.as_bytes().to_vec();
        tar.resize(tar.len() + 1024, 0);
        assert!(matches!(collect(&tar, &Limits::default()), Err(Error::MalformedTar(_))));
    }

    #[test]
    fn decodes_every_kind() {
        let mut b = TarBuilder::new();
        b.dir("./etc/", &Opts::default().mode(0o755))
            .file("etc/hostname", b"kiln\n", &Opts::default().uid(1000).gid(1001))
            .symlink("etc/link", "../usr/x", &Opts::default())
            .hardlink("etc/hard", "./etc/hostname")
            .chardev("dev/null", 1, 3, &Opts::default().mode(0o666))
            .fifo("run/pipe", &Opts::default());
        let (es, _, _) = collect(&b.finish(), &Limits::default()).unwrap();
        assert_eq!(es[0].0.path, b"etc");
        assert!(matches!(es[0].0.kind, EntryKind::Dir));
        assert_eq!(es[0].0.meta.mode, 0o755);
        assert_eq!(es[1].1, b"kiln\n");
        assert_eq!((es[1].0.meta.uid, es[1].0.meta.gid), (1000, 1001));
        assert!(matches!(&es[2].0.kind, EntryKind::Symlink { target } if target == b"../usr/x"));
        assert!(matches!(&es[3].0.kind, EntryKind::Hardlink { target } if target == b"etc/hostname"));
        assert!(matches!(es[4].0.kind, EntryKind::CharDev { major: 1, minor: 3 }));
        assert!(matches!(es[5].0.kind, EntryKind::Fifo));
    }

    #[test]
    fn pax_overrides_and_xattrs() {
        let long = format!("{}/file", "d".repeat(150));
        let o = Opts::default()
            .pax("path", long.as_bytes())
            .pax("uid", b"70000")
            .pax("mtime", b"1700000000.25")
            .xattr("user.a", b"1")
            .xattr("security.capability", &[1, 0, 0, 2])
            .xattr("com.apple.quarantine", b"q")
            .pax("LIBARCHIVE.xattr.user.b", b"eA==");
        let mut b = TarBuilder::new();
        b.file("short", b"x", &o);
        let (es, warnings, _) = collect(&b.finish(), &Limits::default()).unwrap();
        let e = &es[0].0;
        assert_eq!(e.path, long.as_bytes());
        assert_eq!(e.meta.uid, 70000);
        assert_eq!(
            e.meta.mtime,
            Timestamp {
                sec: 1_700_000_000,
                nsec: 250_000_000
            }
        );
        let keys: Vec<(u8, Vec<u8>)> = e.xattrs.keys().map(|k| (k.index, k.name.clone())).collect();
        assert_eq!(
            keys,
            vec![
                (XATTR_INDEX_USER, b"a".to_vec()),
                (XATTR_INDEX_SECURITY, b"capability".to_vec())
            ]
        );
        assert_eq!(warnings.len(), 2, "{warnings:?}");
    }

    #[test]
    fn drops_tar_supplied_overlay_xattrs() {
        let o = Opts::default()
            .xattr("trusted.overlay.opaque", b"y")
            .xattr("trusted.overlay.redirect", b"/x")
            .xattr("trusted.other", b"kept");
        let mut b = TarBuilder::new();
        b.dir("d", &o);
        let (es, warnings, _) = collect(&b.finish(), &Limits::default()).unwrap();
        let keys: Vec<Vec<u8>> = es[0].0.xattrs.keys().map(XattrKey::full_name).collect();
        assert_eq!(keys, vec![b"trusted.other".to_vec()]);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(
            warnings.iter().all(|w| w.starts_with("dropping overlay xattr")),
            "{warnings:?}"
        );
    }

    #[test]
    fn empty_pax_path_keeps_the_ustar_name() {
        let mut b = TarBuilder::new();
        b.file("short", b"x", &Opts::default().pax("path", b"").pax("uid", b""));
        let (es, _, _) = collect(&b.finish(), &Limits::default()).unwrap();
        assert_eq!(es[0].0.path, b"short");
        assert_eq!(es[0].0.meta.uid, 0);
    }

    #[test]
    fn rejects_gnu_long_name_together_with_pax_path() {
        use crate::testtar::pax_record;
        let conflict = |e: Result<Collected>| match e {
            Err(Error::UnsupportedEntry { kind, .. }) => kind == "conflicting long-name and PAX path",
            _ => false,
        };
        let mut b = TarBuilder::new();
        b.entry(b"././@LongLink", b'L', b"gnu-name\0", b"", (0, 0), &Opts::default())
            .entry(
                b"././@PaxHeader",
                b'x',
                &pax_record(b"path", b"pax-name"),
                b"",
                (0, 0),
                &Opts::default(),
            )
            .file("f", b"", &Opts::default());
        assert!(conflict(collect(&b.finish(), &Limits::default())), "L + PAX path");
        let mut b = TarBuilder::new();
        b.entry(b"././@LongLink", b'K', b"gnu-target\0", b"", (0, 0), &Opts::default())
            .entry(
                b"././@PaxHeader",
                b'x',
                &pax_record(b"linkpath", b"pax-target"),
                b"",
                (0, 0),
                &Opts::default(),
            )
            .symlink("s", "t", &Opts::default());
        assert!(conflict(collect(&b.finish(), &Limits::default())), "K + PAX linkpath");
        // Either form alone, or a long name with a PAX linkpath, is fine.
        let mut b = TarBuilder::new();
        b.entry(b"././@LongLink", b'L', b"gnu-name\0", b"", (0, 0), &Opts::default())
            .entry(
                b"././@PaxHeader",
                b'x',
                &pax_record(b"linkpath", b"pax-target"),
                b"",
                (0, 0),
                &Opts::default(),
            )
            .symlink("s", "t", &Opts::default());
        let (es, _, _) = collect(&b.finish(), &Limits::default()).unwrap();
        assert_eq!(es[0].0.path, b"gnu-name");
        assert!(matches!(&es[0].0.kind, EntryKind::Symlink { target } if target == b"pax-target"));
    }

    #[test]
    fn gnu_long_names_and_links() {
        let long = format!("{}/f", "x".repeat(120));
        let target = format!("{}/t", "y".repeat(130));
        let mut b = TarBuilder::new();
        b.file(&long, b"", &Opts::default())
            .symlink("s", &target, &Opts::default());
        let (es, _, _) = collect(&b.finish(), &Limits::default()).unwrap();
        assert_eq!(es[0].0.path, long.as_bytes());
        assert!(matches!(&es[1].0.kind, EntryKind::Symlink { target: t } if t == target.as_bytes()));
    }

    #[test]
    fn v7_directory_with_trailing_slash() {
        let mut b = TarBuilder::new();
        b.entry(b"olddir/", 0, b"", b"", (0, 0), &Opts::default());
        let (es, _, _) = collect(&b.finish(), &Limits::default()).unwrap();
        assert!(matches!(es[0].0.kind, EntryKind::Dir));
        assert_eq!(es[0].0.path, b"olddir");
    }

    #[test]
    fn rejects_sparse_bad_devices_and_empty_symlinks() {
        let l = Limits::default();
        let mut b = TarBuilder::new();
        b.entry(b"s", b'S', b"", b"", (0, 0), &Opts::default());
        assert!(matches!(collect(&b.finish(), &l), Err(Error::UnsupportedEntry { .. })));
        let mut b = TarBuilder::new();
        b.file("p", b"", &Opts::default().pax("GNU.sparse.major", b"1"));
        assert!(matches!(collect(&b.finish(), &l), Err(Error::UnsupportedEntry { .. })));
        let mut b = TarBuilder::new();
        b.chardev("c", 4096, 0, &Opts::default());
        assert!(matches!(collect(&b.finish(), &l), Err(Error::UnsupportedEntry { .. })));
        let mut b = TarBuilder::new();
        b.symlink("s", "", &Opts::default());
        assert!(matches!(collect(&b.finish(), &l), Err(Error::UnsupportedEntry { .. })));
        let mut b = TarBuilder::new();
        b.entry(b"v", b'V', b"", b"", (0, 0), &Opts::default());
        assert!(matches!(collect(&b.finish(), &l), Err(Error::UnsupportedEntry { .. })));
    }

    #[test]
    fn enforces_limits() {
        let mut b = TarBuilder::new();
        b.file("a", b"", &Opts::default())
            .file("b", b"", &Opts::default())
            .file("c", b"", &Opts::default());
        let l = Limits {
            max_entries: 2,
            ..Limits::default()
        };
        assert!(matches!(
            collect(&b.finish(), &l),
            Err(Error::LimitExceeded {
                limit: "entries per layer",
                ..
            })
        ));

        let mut b = TarBuilder::new();
        b.file("a", b"", &Opts::default().pax("comment", &[b'c'; 200]));
        let l = Limits {
            max_header_record: 64,
            ..Limits::default()
        };
        assert!(matches!(
            collect(&b.finish(), &l),
            Err(Error::LimitExceeded {
                limit: "tar header record",
                ..
            })
        ));

        let mut b = TarBuilder::new();
        b.file("big", &[7u8; 8192], &Opts::default());
        let l = Limits {
            max_layer_bytes: 4096,
            ..Limits::default()
        };
        assert!(matches!(
            collect(&b.finish(), &l),
            Err(Error::LimitExceeded {
                limit: "uncompressed bytes per layer",
                ..
            })
        ));

        let mut b = TarBuilder::new();
        b.file("x", b"", &Opts::default().xattr("user.big", &vec![1u8; 70_000]));
        assert!(matches!(
            collect(&b.finish(), &Limits::default()),
            Err(Error::XattrUnencodable { .. })
        ));
    }

    #[test]
    fn consumes_through_the_first_end_block() {
        let mut b = TarBuilder::new();
        b.file("a", b"hello", &Opts::default());
        let (_, _, n) = collect(&b.finish(), &Limits::default()).unwrap();
        let body = b.bytes().len() as u64;
        assert!(n >= body + 512 && n <= body + 1024, "consumed {n}, body {body}");
    }

    #[test]
    fn rejects_header_only_with_nonzero_size() {
        let l = Limits::default();
        // Hardlink with nonzero size
        let mut b = TarBuilder::new();
        b.entry(b"hard", b'1', &[0u8; 8], b"target", (0, 0), &Opts::default());
        match collect(&b.finish(), &l) {
            Err(Error::UnsupportedEntry { kind, .. }) => {
                assert!(kind.contains("header-only"));
            }
            e => panic!("expected UnsupportedEntry, got {:?}", e),
        }

        // Directory with nonzero size
        let mut b = TarBuilder::new();
        b.entry(b"dir", b'5', &[0u8; 8], b"", (0, 0), &Opts::default());
        match collect(&b.finish(), &l) {
            Err(Error::UnsupportedEntry { kind, .. }) => {
                assert!(kind.contains("header-only"));
            }
            e => panic!("expected UnsupportedEntry, got {:?}", e),
        }
    }

    #[test]
    fn rejects_global_pax_with_recognized_keys() {
        use crate::testtar::pax_record;

        // Global PAX with recognized key (SCHILY.xattr.*) should be rejected
        let mut payload = Vec::new();
        payload.extend(pax_record(b"SCHILY.xattr.user.a", b"value"));
        let mut b = TarBuilder::new();
        b.entry(b"././@PaxGlobal", b'g', &payload, b"", (0, 0), &Opts::default());
        let l = Limits::default();
        match collect(&b.finish(), &l) {
            Err(Error::UnsupportedEntry { kind, .. }) => {
                assert!(kind.contains("global PAX"));
            }
            e => panic!("expected UnsupportedEntry, got {:?}", e),
        }

        // Global PAX with ignorable key should be accepted
        let mut payload = Vec::new();
        payload.extend(pax_record(b"comment", b"x"));
        let mut b = TarBuilder::new();
        b.entry(b"././@PaxGlobal", b'g', &payload, b"", (0, 0), &Opts::default())
            .file("f", b"", &Opts::default());
        let (entries, _, _) = collect(&b.finish(), &l).unwrap();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn rejects_duplicate_pax_headers() {
        // Two consecutive x (PAX local) headers should be rejected
        let mut b = TarBuilder::new();
        let pax1 = vec![(b"path".to_vec(), b"p1".to_vec())];
        let pax2 = vec![(b"path".to_vec(), b"p2".to_vec())];
        b.pax_header(&pax1).pax_header(&pax2).file("f", b"", &Opts::default());
        let l = Limits::default();
        match collect(&b.finish(), &l) {
            Err(Error::MalformedTar(msg)) => {
                assert!(msg.contains("duplicate x header"));
            }
            e => panic!("expected MalformedTar, got {:?}", e),
        }
    }

    #[test]
    fn bounds_warnings_to_100() {
        // Create 150 files each with one unmappable xattr
        let mut b = TarBuilder::new();
        for i in 0..150 {
            let name = format!("f{}", i);
            b.file(&name, b"", &Opts::default().xattr("com.apple.x", b"v"));
        }
        let (_, warnings, _) = collect(&b.finish(), &Limits::default()).unwrap();
        // Should have 100 warnings + 1 suppression message = 101 total
        assert_eq!(warnings.len(), 101, "{warnings:?}");
        assert!(warnings[100].contains("50 more warnings suppressed"));
    }

    #[test]
    fn rejects_global_pax_with_non_utf8_key() {
        use crate::testtar::pax_record;

        // Global PAX with non-UTF-8 key that matches SCHILY.xattr. pattern
        // This tests the byte-based matching, not UTF-8 conversion bypass
        let mut payload = Vec::new();
        payload.extend(pax_record(b"SCHILY.xattr.user.\xff", b"v"));
        let mut b = TarBuilder::new();
        b.entry(b"././@PaxGlobal", b'g', &payload, b"", (0, 0), &Opts::default());
        let l = Limits::default();
        match collect(&b.finish(), &l) {
            Err(Error::UnsupportedEntry { kind, .. }) => {
                assert!(kind.contains("global PAX"));
            }
            e => panic!("expected UnsupportedEntry, got {:?}", e),
        }
    }

    #[test]
    fn rejects_duplicate_gnu_longname() {
        // Two consecutive L (GNU longname) headers should be rejected
        let mut b = TarBuilder::new();
        b.entry(b"././@LongLink", b'L', b"name1\0", b"", (0, 0), &Opts::default())
            .entry(b"././@LongLink", b'L', b"name2\0", b"", (0, 0), &Opts::default())
            .file("f", b"", &Opts::default());
        let l = Limits::default();
        match collect(&b.finish(), &l) {
            Err(Error::MalformedTar(msg)) => {
                assert!(msg.contains("duplicate L header"));
            }
            e => panic!("expected MalformedTar, got {:?}", e),
        }
    }

    #[test]
    fn rejects_duplicate_gnu_longlink() {
        // Two consecutive K (GNU longlink) headers should be rejected
        let mut b = TarBuilder::new();
        b.entry(b"././@LongLink", b'K', b"link1\0", b"", (0, 0), &Opts::default())
            .entry(b"././@LongLink", b'K', b"link2\0", b"", (0, 0), &Opts::default())
            .file("f", b"", &Opts::default());
        let l = Limits::default();
        match collect(&b.finish(), &l) {
            Err(Error::MalformedTar(msg)) => {
                assert!(msg.contains("duplicate K header"));
            }
            e => panic!("expected MalformedTar, got {:?}", e),
        }
    }

    #[test]
    fn rejects_v7_directory_with_size() {
        // V7 typeflag-0 trailing-slash directory with nonzero size
        let mut b = TarBuilder::new();
        b.entry(b"dir/", 0, &[0u8; 8], b"", (0, 0), &Opts::default());
        let l = Limits::default();
        match collect(&b.finish(), &l) {
            Err(Error::UnsupportedEntry { kind, .. }) => {
                assert!(kind.contains("header-only"));
            }
            e => panic!("expected UnsupportedEntry, got {:?}", e),
        }
    }

    #[test]
    fn rejects_symlink_with_size() {
        // Symlink (EntryType::Symlink) with nonzero size
        let mut b = TarBuilder::new();
        b.entry(b"link", b'2', &[0u8; 8], b"target", (0, 0), &Opts::default());
        let l = Limits::default();
        match collect(&b.finish(), &l) {
            Err(Error::UnsupportedEntry { kind, .. }) => {
                assert!(kind.contains("header-only"));
            }
            e => panic!("expected UnsupportedEntry, got {:?}", e),
        }
    }
}
