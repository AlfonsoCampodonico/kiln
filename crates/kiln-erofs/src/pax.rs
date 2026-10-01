//! PAX extended header parsing (POSIX.1-2001 `x`/`g` records).

use crate::error::{Error, Result};
use crate::tree::Timestamp;

fn malformed(what: &str) -> Error {
    Error::MalformedTar(format!("PAX header: {what}"))
}

/// Parses `"<len> <key>=<value>\n"` records. Values may be binary.
pub(crate) fn parse_records(data: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut out = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        if rest.iter().all(|&b| b == 0) {
            break;
        }
        let sp = rest
            .iter()
            .position(|&b| b == b' ')
            .ok_or_else(|| malformed("missing length"))?;
        let len: usize = std::str::from_utf8(&rest[..sp])
            .ok()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| malformed("bad length"))?;
        if len <= sp + 1 || len > rest.len() {
            return Err(malformed("record length out of range"));
        }
        let rec = rest[sp + 1..len]
            .strip_suffix(b"\n")
            .ok_or_else(|| malformed("record missing newline"))?;
        let eq = rec
            .iter()
            .position(|&b| b == b'=')
            .ok_or_else(|| malformed("record missing '='"))?;
        out.push((rec[..eq].to_vec(), rec[eq + 1..].to_vec()));
        rest = &rest[len..];
    }
    Ok(out)
}

fn parse_u64(v: &[u8], what: &str) -> Result<u64> {
    std::str::from_utf8(v)
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| malformed(what))
}

/// Parses a PAX time such as `1700000000.123456789` or `-1.5`.
pub(crate) fn parse_timestamp(v: &[u8]) -> Result<Timestamp> {
    let s = std::str::from_utf8(v).map_err(|_| malformed("mtime"))?;
    let (neg, s) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    if int.is_empty() || !int.bytes().all(|b| b.is_ascii_digit()) || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return Err(malformed("mtime"));
    }
    let sec: i64 = int.parse().map_err(|_| malformed("mtime"))?;
    let mut digits: String = frac.chars().take(9).collect();
    while digits.len() < 9 {
        digits.push('0');
    }
    let nsec: u32 = digits.parse().map_err(|_| malformed("mtime"))?;
    Ok(match (neg, nsec) {
        (false, _) => Timestamp { sec, nsec },
        (true, 0) => Timestamp { sec: -sec, nsec: 0 },
        (true, n) => Timestamp {
            sec: -sec - 1,
            nsec: 1_000_000_000 - n,
        },
    })
}

/// Accumulated PAX overrides for the next entry.
#[derive(Debug, Default, Clone)]
pub(crate) struct PaxState {
    pub path: Option<Vec<u8>>,
    pub linkpath: Option<Vec<u8>>,
    pub uid: Option<u64>,
    pub gid: Option<u64>,
    pub size: Option<u64>,
    pub mtime: Option<Timestamp>,
    /// Full xattr names (`user.foo`) with raw values, in record order.
    pub xattrs: Vec<(Vec<u8>, Vec<u8>)>,
    pub sparse: bool,
    /// Xattr names kiln cannot represent (`LIBARCHIVE.xattr.*`).
    pub dropped: Vec<Vec<u8>>,
}

impl PaxState {
    /// An empty `path`, `linkpath`, `uid`, `gid`, `size` or `mtime` value means the
    /// record is absent: like Go's `archive/tar`, the ustar header value is kept.
    pub fn apply(&mut self, records: Vec<(Vec<u8>, Vec<u8>)>) -> Result<()> {
        for (k, v) in records {
            let ustar_field = matches!(
                k.as_slice(),
                b"path" | b"linkpath" | b"uid" | b"gid" | b"size" | b"mtime"
            );
            if ustar_field && v.is_empty() {
                continue;
            }
            match k.as_slice() {
                b"path" => self.path = Some(v),
                b"linkpath" => self.linkpath = Some(v),
                b"uid" => self.uid = Some(parse_u64(&v, "uid")?),
                b"gid" => self.gid = Some(parse_u64(&v, "gid")?),
                b"size" => self.size = Some(parse_u64(&v, "size")?),
                b"mtime" => self.mtime = Some(parse_timestamp(&v)?),
                _ if k.starts_with(b"SCHILY.xattr.") => self.xattrs.push((k[13..].to_vec(), v)),
                _ if k.starts_with(b"GNU.sparse.") => self.sparse = true,
                _ if k.starts_with(b"LIBARCHIVE.xattr.") => self.dropped.push(k[17..].to_vec()),
                _ => {}
            }
        }
        Ok(())
    }

    /// Combines global (`g`) and local (`x`) state; local values win.
    pub fn overlay(global: &PaxState, local: PaxState) -> PaxState {
        let mut xattrs = global.xattrs.clone();
        xattrs.extend(local.xattrs);
        let mut dropped = global.dropped.clone();
        dropped.extend(local.dropped);
        PaxState {
            path: local.path.or_else(|| global.path.clone()),
            linkpath: local.linkpath.or_else(|| global.linkpath.clone()),
            uid: local.uid.or(global.uid),
            gid: local.gid.or(global.gid),
            size: local.size.or(global.size),
            mtime: local.mtime.or(global.mtime),
            xattrs,
            sparse: global.sparse || local.sparse,
            dropped,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testtar::pax_record;

    #[test]
    fn parses_records_including_binary_values() {
        let mut data = pax_record(b"path", b"usr/bin/php");
        data.extend(pax_record(b"SCHILY.xattr.security.capability", &[1, 0, 0, 2, b'\n', 0]));
        data.extend([0u8; 7]); // trailing NUL padding is tolerated
        let recs = parse_records(&data).unwrap();
        assert_eq!(recs[0], (b"path".to_vec(), b"usr/bin/php".to_vec()));
        assert_eq!(recs[1].1, vec![1, 0, 0, 2, b'\n', 0]);
    }

    #[test]
    fn rejects_malformed_records() {
        // record missing '=' - valid length, valid newline, but no '='
        // "6 abc\n" = 6 bytes: '6' (1) + ' ' (1) + 'a' (1) + 'b' (1) + 'c' (1) + '\n' (1) = 6
        assert!(matches!(
            parse_records(b"6 abc\n"),
            Err(Error::MalformedTar(m)) if m.contains("record missing '='")
        ));

        // record missing newline - valid length, valid '=', but no newline
        // "5 a=b" = 5 bytes: '5' (1) + ' ' (1) + 'a' (1) + '=' (1) + 'b' (1) = 5
        assert!(matches!(
            parse_records(b"5 a=b"),
            Err(Error::MalformedTar(m)) if m.contains("record missing newline")
        ));

        // missing length - no space found at all
        assert!(matches!(
            parse_records(b"12"),
            Err(Error::MalformedTar(m)) if m.contains("missing length")
        ));

        // record length out of range: len <= sp + 1
        // "2 x" = sp=1, len=2, and 2 <= 1+1 is true (guard fails)
        assert!(matches!(
            parse_records(b"2 x"),
            Err(Error::MalformedTar(m)) if m.contains("record length out of range")
        ));
        // "1 x" = sp=1, len=1, and 1 <= 1+1 is true (guard fails)
        assert!(matches!(
            parse_records(b"1 x"),
            Err(Error::MalformedTar(m)) if m.contains("record length out of range")
        ));

        // record length out of range: len > rest.len()
        // Claims 99 bytes but only 8 bytes available
        assert!(matches!(
            parse_records(b"99 k=v\n"),
            Err(Error::MalformedTar(m)) if m.contains("record length out of range")
        ));

        // bad length - can't parse as usize
        assert!(matches!(
            parse_records(b"x path=x\n"),
            Err(Error::MalformedTar(m)) if m.contains("bad length")
        ));

        // overflow - very large number that overflows usize parsing
        assert!(matches!(
            parse_records(b"99999999999999999999999 k=v\n"),
            Err(Error::MalformedTar(m)) if m.contains("bad length")
        ));

        // valid first record followed by truncated second record (incomplete)
        let mut data = pax_record(b"a", b"b");
        data.extend_from_slice(b"10"); // incomplete second record (claims 10 bytes but not enough data)
        assert!(matches!(
            parse_records(&data),
            Err(Error::MalformedTar(m)) if m.contains("missing length")
        ));
    }

    #[test]
    fn timestamps() {
        assert_eq!(
            parse_timestamp(b"1700000000").unwrap(),
            Timestamp {
                sec: 1_700_000_000,
                nsec: 0
            }
        );
        assert_eq!(
            parse_timestamp(b"1700000000.5").unwrap(),
            Timestamp {
                sec: 1_700_000_000,
                nsec: 500_000_000
            }
        );
        assert_eq!(
            parse_timestamp(b"1.1234567891").unwrap(),
            Timestamp {
                sec: 1,
                nsec: 123_456_789
            }
        );
        assert_eq!(
            parse_timestamp(b"-1.5").unwrap(),
            Timestamp {
                sec: -2,
                nsec: 500_000_000
            }
        );
        assert_eq!(parse_timestamp(b"-3").unwrap(), Timestamp { sec: -3, nsec: 0 });
        assert!(parse_timestamp(b"abc").is_err());
    }

    #[test]
    fn empty_values_mean_absent() {
        let mut s = PaxState::default();
        s.apply(
            [&b"path"[..], b"linkpath", b"uid", b"gid", b"size", b"mtime"]
                .into_iter()
                .map(|k| (k.to_vec(), Vec::new()))
                .collect(),
        )
        .unwrap();
        assert_eq!(
            (s.path, s.linkpath, s.uid, s.gid, s.size, s.mtime),
            (None, None, None, None, None, None),
            "Go's archive/tar keeps the ustar value for an empty record"
        );
    }

    #[test]
    fn state_applies_known_keys_and_local_overrides_global() {
        let mut global = PaxState::default();
        global
            .apply(vec![
                (b"uid".to_vec(), b"5".to_vec()),
                (b"SCHILY.xattr.user.a".to_vec(), b"g".to_vec()),
            ])
            .unwrap();
        let mut local = PaxState::default();
        local
            .apply(vec![
                (b"uid".to_vec(), b"7".to_vec()),
                (b"GNU.sparse.major".to_vec(), b"1".to_vec()),
                (b"LIBARCHIVE.xattr.user.b".to_vec(), b"x".to_vec()),
                (b"SCHILY.xattr.user.a".to_vec(), b"l".to_vec()),
            ])
            .unwrap();
        let s = PaxState::overlay(&global, local);
        assert_eq!(s.uid, Some(7));
        assert!(s.sparse);
        assert_eq!(s.dropped, vec![b"user.b".to_vec()]);
        assert_eq!(s.xattrs.last().unwrap(), &(b"user.a".to_vec(), b"l".to_vec()));
    }
}
