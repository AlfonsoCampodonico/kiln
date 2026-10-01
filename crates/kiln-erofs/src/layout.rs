//! Layout decisions shared by the streaming writer and squash (spec §7.1–§7.3).

use std::collections::HashMap;
use std::ops::Range;

use crate::error::{Error, Result};
use crate::ondisk::{
    BLOCK_SIZE, DIRENT_LEN, EXTENDED_INODE_LEN, MAX_SHARED_XATTRS, SLOT_SIZE, XATTR_IBODY_HEADER_LEN, encode_dirent,
    encode_xattr_entry, encode_xattr_ibody_header, round_up, xattr_entry_len,
};
use crate::tree::{Meta, Timestamp, XattrKey, Xattrs};

/// Size of the xattr body if every xattr were inline (sharing only shrinks it).
pub(crate) fn xattr_ibody_bound(x: &Xattrs) -> usize {
    if x.is_empty() {
        return 0;
    }
    XATTR_IBODY_HEADER_LEN
        + x.iter()
            .map(|(k, v)| xattr_entry_len(k.name.len(), v.len()))
            .sum::<usize>()
}

/// Whether a tail of `tail_len` bytes fits inline whatever the final inode layout.
pub(crate) fn tail_fits_inline(tail_len: u64, xattrs: &Xattrs) -> bool {
    tail_len > 0 && tail_len as usize + EXTENDED_INODE_LEN + xattr_ibody_bound(xattrs) <= BLOCK_SIZE as usize
}

pub(crate) fn fits_compact(meta: &Meta, nlink: u32, size: u64, base: Timestamp) -> bool {
    meta.uid <= 0xffff && meta.gid <= 0xffff && nlink <= 0xffff && size < (1 << 32) && meta.mtime == base
}

/// Greedily packs sorted names into directory blocks.
pub(crate) fn pack_dir(names: &[&[u8]]) -> Vec<Range<usize>> {
    let mut blocks = Vec::new();
    let mut start = 0;
    let mut used = 0usize;
    for (i, name) in names.iter().enumerate() {
        let need = DIRENT_LEN + name.len();
        if used + need > BLOCK_SIZE as usize {
            blocks.push(start..i);
            start = i;
            used = 0;
        }
        used += need;
    }
    blocks.push(start..names.len());
    blocks
}

/// Byte length of the encoded directory (`i_size`).
pub(crate) fn dir_size(names: &[&[u8]]) -> u64 {
    let blocks = pack_dir(names);
    let last = blocks.last().expect("at least one block");
    let used: usize = names[last.clone()].iter().map(|n| DIRENT_LEN + n.len()).sum();
    (blocks.len() as u64 - 1) * BLOCK_SIZE + used as u64
}

/// Encodes `(name, nid, file_type)` entries, which must be sorted by name.
pub(crate) fn encode_dir(entries: &[(&[u8], u64, u8)]) -> Vec<u8> {
    let names: Vec<&[u8]> = entries.iter().map(|e| e.0).collect();
    let blocks = pack_dir(&names);
    let mut out = Vec::new();
    for (bi, range) in blocks.iter().enumerate() {
        let block_start = out.len();
        let mut nameoff = range.len() * DIRENT_LEN;
        for &(name, nid, ft) in &entries[range.clone()] {
            encode_dirent(nid, nameoff as u16, ft, &mut out);
            nameoff += name.len();
        }
        for &(name, _, _) in &entries[range.clone()] {
            out.extend_from_slice(name);
        }
        if bi + 1 < blocks.len() {
            out.resize(block_start + BLOCK_SIZE as usize, 0);
        }
    }
    out
}

pub(crate) struct XattrPlan {
    /// Shared xattr table (entries 4-byte aligned; id = offset / 4).
    pub table: Vec<u8>,
    /// Encoded xattr body per inode, in inode order (empty when none).
    pub ibodies: Vec<Vec<u8>>,
}

/// Decides shared vs inline xattrs for inodes given in inode-numbering order.
pub(crate) fn plan_xattrs(inodes: &[&Xattrs]) -> Result<XattrPlan> {
    let mut counts: HashMap<(&XattrKey, &[u8]), u32> = HashMap::new();
    for x in inodes {
        for (k, v) in x.iter() {
            *counts.entry((k, v.as_slice())).or_default() += 1;
        }
    }
    let mut ids: HashMap<(&XattrKey, &[u8]), u32> = HashMap::new();
    let mut table = Vec::new();
    let mut ibodies = Vec::with_capacity(inodes.len());
    for x in inodes {
        if x.is_empty() {
            ibodies.push(Vec::new());
            continue;
        }
        let force = xattr_ibody_bound(x) > BLOCK_SIZE as usize - EXTENDED_INODE_LEN;
        let mut shared = Vec::new();
        let mut inline = Vec::new();
        for (k, v) in x.iter() {
            let key = (k, v.as_slice());
            if force || counts[&key] >= 2 {
                let id = *ids.entry(key).or_insert_with(|| {
                    let id = (table.len() / 4) as u32;
                    encode_xattr_entry(k.index, &k.name, v, &mut table);
                    id
                });
                shared.push(id);
            } else {
                inline.push((k, v));
            }
        }
        if shared.len() > MAX_SHARED_XATTRS {
            return Err(Error::TooManyXattrs);
        }
        let mut body = Vec::new();
        encode_xattr_ibody_header(shared.len() as u8, &mut body);
        for id in shared {
            body.extend_from_slice(&id.to_le_bytes());
        }
        for (k, v) in inline {
            encode_xattr_entry(k.index, &k.name, v, &mut body);
        }
        ibodies.push(body);
    }
    Ok(XattrPlan { table, ibodies })
}

/// Assigns nids to inode records of the given byte lengths, in order. Slot 0 is
/// reserved and no record crosses a block. Returns nids and the metadata area length.
pub(crate) fn assign_nids(record_lens: &[usize]) -> (Vec<u64>, u64) {
    let mut off = SLOT_SIZE;
    let mut nids = Vec::with_capacity(record_lens.len());
    for &len in record_lens {
        let len = len as u64;
        assert!(len <= BLOCK_SIZE, "inode record of {len} bytes exceeds a block");
        if off % BLOCK_SIZE + len > BLOCK_SIZE {
            off = round_up(off, BLOCK_SIZE);
        }
        nids.push(off / SLOT_SIZE);
        off += round_up(len, SLOT_SIZE);
    }
    (nids, round_up(off, BLOCK_SIZE))
}

#[cfg(test)]
mod tests {
    use super::{assign_nids, dir_size, encode_dir, fits_compact, pack_dir, plan_xattrs, tail_fits_inline};
    use crate::ondisk::{FT_DIR, FT_REG_FILE, XATTR_INDEX_USER, decode_dirent};
    use crate::tree::{Meta, Timestamp, XattrKey, Xattrs};

    fn x(pairs: &[(&str, &str)]) -> Xattrs {
        pairs
            .iter()
            .map(|(n, v)| {
                (
                    XattrKey {
                        index: XATTR_INDEX_USER,
                        name: n.as_bytes().to_vec(),
                    },
                    v.as_bytes().to_vec(),
                )
            })
            .collect()
    }

    #[test]
    fn tail_inline_boundaries() {
        let none = Xattrs::new();
        assert!(!tail_fits_inline(0, &none));
        assert!(tail_fits_inline(4032, &none));
        assert!(!tail_fits_inline(4033, &none));
        let one = x(&[("a", "1")]); // bound = 12 + 8
        assert!(tail_fits_inline(4012, &one));
        assert!(!tail_fits_inline(4013, &one));
    }

    #[test]
    fn compact_rules() {
        let base = Timestamp { sec: 100, nsec: 5 };
        let m = Meta {
            mode: 0o644,
            uid: 65_535,
            gid: 0,
            mtime: base,
        };
        assert!(fits_compact(&m, 1, (1 << 32) - 1, base));
        assert!(!fits_compact(&m, 1, 1 << 32, base));
        assert!(!fits_compact(&m, 65_536, 0, base));
        assert!(!fits_compact(
            &Meta {
                uid: 65_536,
                ..m.clone()
            },
            1,
            0,
            base
        ));
        assert!(!fits_compact(
            &Meta {
                mtime: Timestamp { sec: 100, nsec: 6 },
                ..m
            },
            1,
            0,
            base
        ));
    }

    #[test]
    fn dir_packing() {
        let ones: Vec<Vec<u8>> = (0..316).map(|i| vec![b'a' + (i % 26) as u8]).collect();
        let refs: Vec<&[u8]> = ones.iter().map(|v| v.as_slice()).collect();
        assert_eq!(pack_dir(&refs), vec![0..315, 315..316]);
        let twenties: Vec<Vec<u8>> = (0..129).map(|i| format!("{i:020}").into_bytes()).collect();
        let refs: Vec<&[u8]> = twenties.iter().map(|v| v.as_slice()).collect();
        assert_eq!(
            pack_dir(&refs[..128]),
            vec![0..128],
            "12*128 + 20*128 == 4096 fits exactly"
        );
        assert_eq!(dir_size(&refs[..128]), 4096);
        assert_eq!(pack_dir(&refs), vec![0..128, 128..129]);
        assert_eq!(dir_size(&refs), 4096 + 32);
        assert_eq!(dir_size(&[&b"."[..], &b".."[..]]), 27);
    }

    #[test]
    fn dir_encoding() {
        let enc = encode_dir(&[
            (&b"."[..], 1, FT_DIR),
            (&b".."[..], 1, FT_DIR),
            (&b"a"[..], 2, FT_REG_FILE),
        ]);
        assert_eq!(enc.len(), 3 * 12 + 4);
        assert_eq!(decode_dirent(&enc[0..]), (1, 36, FT_DIR));
        assert_eq!(decode_dirent(&enc[12..]), (1, 37, FT_DIR));
        assert_eq!(decode_dirent(&enc[24..]), (2, 39, FT_REG_FILE));
        assert_eq!(&enc[36..], b"...a");

        let names: Vec<Vec<u8>> = (0..129).map(|i| format!("{i:020}").into_bytes()).collect();
        let ents: Vec<(&[u8], u64, u8)> = names.iter().map(|n| (n.as_slice(), 9, FT_REG_FILE)).collect();
        let enc = encode_dir(&ents);
        assert_eq!(enc.len(), 4096 + 32);
        assert_eq!(
            decode_dirent(&enc[4096..]).1,
            12,
            "second block starts its own name area"
        );
    }

    #[test]
    fn nid_assignment_never_crosses_blocks() {
        let (nids, meta_len) = assign_nids(&[32, 64, 4000, 100]);
        assert_eq!(nids, vec![1, 2, 128, 256]);
        assert_eq!(meta_len, 12_288);
    }

    #[test]
    fn xattr_sharing() {
        let a = x(&[("x", "1"), ("y", "2")]);
        let b = x(&[("x", "1")]);
        let c = Xattrs::new();
        let plan = plan_xattrs(&[&a, &b, &c]).unwrap();
        assert_eq!(plan.table, vec![1, XATTR_INDEX_USER, 1, 0, b'x', b'1', 0, 0]);
        assert_eq!(plan.ibodies[0].len(), 12 + 4 + 8);
        assert_eq!(plan.ibodies[0][4], 1, "shared count");
        assert_eq!(&plan.ibodies[0][12..16], &0u32.to_le_bytes());
        assert_eq!(plan.ibodies[1].len(), 16);
        assert!(plan.ibodies[2].is_empty());
    }

    #[test]
    fn oversized_inline_xattrs_are_forced_into_the_table() {
        let mut big = Xattrs::new();
        big.insert(
            XattrKey {
                index: XATTR_INDEX_USER,
                name: b"k".to_vec(),
            },
            vec![7u8; 4030],
        );
        let plan = plan_xattrs(&[&big]).unwrap();
        assert_eq!(plan.ibodies[0].len(), 16);
        assert_eq!(plan.table.len(), crate::ondisk::xattr_entry_len(1, 4030));
    }
}
