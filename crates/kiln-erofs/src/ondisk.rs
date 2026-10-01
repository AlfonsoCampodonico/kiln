//! erofs on-disk structures (Linux `fs/erofs/erofs_fs.h`), restricted to kiln's profile.

use crate::error::{Error, Result};

pub const BLOCK_SIZE: u64 = 4096;
pub const BLKSZ_BITS: u8 = 12;
pub const SUPER_OFFSET: u64 = 1024;
pub const SUPER_MAGIC: u32 = 0xE0F5_E1E2;
pub const SUPER_LEN: usize = 128;
pub const SLOT_SIZE: u64 = 32;
pub const COMPACT_INODE_LEN: usize = 32;
pub const EXTENDED_INODE_LEN: usize = 64;
pub const DIRENT_LEN: usize = 12;
pub const XATTR_IBODY_HEADER_LEN: usize = 12;
pub const XATTR_ENTRY_HEADER_LEN: usize = 4;
pub const NAME_LEN_MAX: usize = 255;
pub const MAX_SHARED_XATTRS: usize = 255;

pub const LAYOUT_FLAT_PLAIN: u16 = 0;
pub const LAYOUT_FLAT_INLINE: u16 = 2;

pub const FT_REG_FILE: u8 = 1;
pub const FT_DIR: u8 = 2;
pub const FT_CHRDEV: u8 = 3;
pub const FT_BLKDEV: u8 = 4;
pub const FT_FIFO: u8 = 5;
pub const FT_SYMLINK: u8 = 7;

pub const S_IFMT: u32 = 0o170000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFREG: u32 = 0o100000;
pub const S_IFLNK: u32 = 0o120000;
pub const S_IFCHR: u32 = 0o020000;
pub const S_IFBLK: u32 = 0o060000;
pub const S_IFIFO: u32 = 0o010000;

pub const XATTR_INDEX_USER: u8 = 1;
pub const XATTR_INDEX_POSIX_ACL_ACCESS: u8 = 2;
pub const XATTR_INDEX_POSIX_ACL_DEFAULT: u8 = 3;
pub const XATTR_INDEX_TRUSTED: u8 = 4;
pub const XATTR_INDEX_SECURITY: u8 = 6;

fn put_u16(b: &mut [u8], off: usize, v: u16) {
    b[off..off + 2].copy_from_slice(&v.to_le_bytes());
}
fn put_u32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
fn put_u64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}
fn get_u16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(b[off..off + 2].try_into().expect("2 bytes"))
}
fn get_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().expect("4 bytes"))
}
fn get_u64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().expect("8 bytes"))
}

pub fn round_up(v: u64, align: u64) -> u64 {
    v.div_ceil(align) * align
}

/// The fields of `struct erofs_super_block` that kiln writes; all others are zero.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SuperBlock {
    pub root_nid: u16,
    pub inos: u64,
    pub epoch: u64,
    pub fixed_nsec: u32,
    pub blocks: u32,
    pub meta_blkaddr: u32,
    pub xattr_blkaddr: u32,
}

impl SuperBlock {
    pub fn encode(&self) -> [u8; SUPER_LEN] {
        let mut b = [0u8; SUPER_LEN];
        put_u32(&mut b, 0, SUPER_MAGIC);
        b[12] = BLKSZ_BITS;
        put_u16(&mut b, 14, self.root_nid);
        put_u64(&mut b, 16, self.inos);
        put_u64(&mut b, 24, self.epoch);
        put_u32(&mut b, 32, self.fixed_nsec);
        put_u32(&mut b, 36, self.blocks);
        put_u32(&mut b, 40, self.meta_blkaddr);
        put_u32(&mut b, 44, self.xattr_blkaddr);
        b
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        if b.len() < SUPER_LEN {
            return Err(Error::Corrupt("superblock truncated".into()));
        }
        let violation = |what: String| Err(Error::ProfileViolation(what));
        if get_u32(b, 0) != SUPER_MAGIC {
            return violation("bad superblock magic".into());
        }
        if get_u32(b, 8) != 0 {
            return violation(format!("feature_compat {:#x}", get_u32(b, 8)));
        }
        if b[12] != BLKSZ_BITS {
            return violation(format!("block size 2^{}", b[12]));
        }
        if get_u32(b, 80) != 0 {
            return violation(format!("feature_incompat {:#x}", get_u32(b, 80)));
        }
        if get_u16(b, 84) != 0 || get_u16(b, 86) != 0 || b[90] != 0 {
            return violation("compression, extra devices or dirblkbits".into());
        }
        Ok(Self {
            root_nid: get_u16(b, 14),
            inos: get_u64(b, 16),
            epoch: get_u64(b, 24),
            fixed_nsec: get_u32(b, 32),
            blocks: get_u32(b, 36),
            meta_blkaddr: get_u32(b, 40),
            xattr_blkaddr: get_u32(b, 44),
        })
    }
}

/// A compact (32-byte) or extended (64-byte) on-disk inode.
///
/// For compact inodes `mtime` is the offset from the superblock epoch (kiln always
/// writes 0) and `mtime_nsec` is unused; for extended inodes both are absolute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskInode {
    pub extended: bool,
    pub layout: u16,
    pub xattr_icount: u16,
    pub mode: u16,
    pub nlink: u32,
    pub size: u64,
    pub mtime: u64,
    pub mtime_nsec: u32,
    pub i_u: u32,
    pub ino: u32,
    pub uid: u32,
    pub gid: u32,
}

impl DiskInode {
    pub fn encoded_len(&self) -> usize {
        if self.extended {
            EXTENDED_INODE_LEN
        } else {
            COMPACT_INODE_LEN
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let format = u16::from(self.extended) | (self.layout << 1);
        let mut b = vec![0u8; self.encoded_len()];
        put_u16(&mut b, 0, format);
        put_u16(&mut b, 2, self.xattr_icount);
        put_u16(&mut b, 4, self.mode);
        if self.extended {
            put_u64(&mut b, 8, self.size);
            put_u32(&mut b, 16, self.i_u);
            put_u32(&mut b, 20, self.ino);
            put_u32(&mut b, 24, self.uid);
            put_u32(&mut b, 28, self.gid);
            put_u64(&mut b, 32, self.mtime);
            put_u32(&mut b, 40, self.mtime_nsec);
            put_u32(&mut b, 44, self.nlink);
        } else {
            put_u16(&mut b, 6, self.nlink as u16);
            put_u32(&mut b, 8, self.size as u32);
            put_u32(&mut b, 12, self.mtime as u32);
            put_u32(&mut b, 16, self.i_u);
            put_u32(&mut b, 20, self.ino);
            put_u16(&mut b, 24, self.uid as u16);
            put_u16(&mut b, 26, self.gid as u16);
        }
        b
    }

    /// Decodes an inode; `b` must hold 32 bytes, or 64 for an extended inode.
    pub fn decode(b: &[u8]) -> Result<Self> {
        if b.len() < COMPACT_INODE_LEN {
            return Err(Error::Corrupt("inode truncated".into()));
        }
        let format = get_u16(b, 0);
        if format & !0x0F != 0 {
            return Err(Error::ProfileViolation(format!("i_format {format:#x}")));
        }
        let extended = format & 1 == 1;
        let layout = (format >> 1) & 0x7;
        if layout != LAYOUT_FLAT_PLAIN && layout != LAYOUT_FLAT_INLINE {
            return Err(Error::ProfileViolation(format!("data layout {layout}")));
        }
        let common = (get_u16(b, 2), get_u16(b, 4));
        if extended {
            if b.len() < EXTENDED_INODE_LEN {
                return Err(Error::Corrupt("extended inode truncated".into()));
            }
            Ok(Self {
                extended,
                layout,
                xattr_icount: common.0,
                mode: common.1,
                nlink: get_u32(b, 44),
                size: get_u64(b, 8),
                mtime: get_u64(b, 32),
                mtime_nsec: get_u32(b, 40),
                i_u: get_u32(b, 16),
                ino: get_u32(b, 20),
                uid: get_u32(b, 24),
                gid: get_u32(b, 28),
            })
        } else {
            Ok(Self {
                extended,
                layout,
                xattr_icount: common.0,
                mode: common.1,
                nlink: u32::from(get_u16(b, 6)),
                size: u64::from(get_u32(b, 8)),
                mtime: u64::from(get_u32(b, 12)),
                mtime_nsec: 0,
                i_u: get_u32(b, 16),
                ino: get_u32(b, 20),
                uid: u32::from(get_u16(b, 24)),
                gid: u32::from(get_u16(b, 26)),
            })
        }
    }
}

pub fn encode_dirent(nid: u64, nameoff: u16, file_type: u8, out: &mut Vec<u8>) {
    out.extend_from_slice(&nid.to_le_bytes());
    out.extend_from_slice(&nameoff.to_le_bytes());
    out.push(file_type);
    out.push(0);
}

/// Returns `(nid, nameoff, file_type)`; `b` must hold at least 12 bytes.
pub fn decode_dirent(b: &[u8]) -> (u64, u16, u8) {
    (get_u64(b, 0), get_u16(b, 8), b[10])
}

pub fn xattr_entry_len(name_len: usize, value_len: usize) -> usize {
    (XATTR_ENTRY_HEADER_LEN + name_len + value_len).div_ceil(4) * 4
}

/// Appends one xattr entry, zero-padded to a 4-byte boundary.
pub fn encode_xattr_entry(index: u8, name: &[u8], value: &[u8], out: &mut Vec<u8>) {
    let start = out.len();
    out.push(name.len() as u8);
    out.push(index);
    out.extend_from_slice(&(value.len() as u16).to_le_bytes());
    out.extend_from_slice(name);
    out.extend_from_slice(value);
    out.resize(start + xattr_entry_len(name.len(), value.len()), 0);
}

/// Appends `erofs_xattr_ibody_header` (name filter 0, as kiln sets no filter feature).
pub fn encode_xattr_ibody_header(shared_count: u8, out: &mut Vec<u8>) {
    out.extend_from_slice(&[0u8; 4]);
    out.push(shared_count);
    out.extend_from_slice(&[0u8; 7]);
}

pub fn xattr_ibody_len(icount: u16) -> usize {
    if icount == 0 {
        0
    } else {
        XATTR_IBODY_HEADER_LEN + 4 * (usize::from(icount) - 1)
    }
}

pub fn xattr_icount_for(ibody_len: usize) -> u16 {
    if ibody_len == 0 {
        0
    } else {
        ((ibody_len - XATTR_IBODY_HEADER_LEN) / 4 + 1) as u16
    }
}

/// Linux `new_encode_dev`.
pub fn encode_rdev(major: u32, minor: u32) -> u32 {
    (minor & 0xff) | (major << 8) | ((minor & !0xff) << 12)
}

/// Linux `new_decode_dev`, returning `(major, minor)`.
pub fn decode_rdev(dev: u32) -> (u32, u32) {
    ((dev & 0xfff00) >> 8, (dev & 0xff) | ((dev >> 12) & 0xfff00))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn superblock_round_trips_and_has_kernel_offsets() {
        let sb = SuperBlock {
            root_nid: 1,
            inos: 7,
            epoch: 1_700_000_000,
            fixed_nsec: 5,
            blocks: 9,
            meta_blkaddr: 3,
            xattr_blkaddr: 2,
        };
        let b = sb.encode();
        assert_eq!(&b[0..4], &[0xE2, 0xE1, 0xF5, 0xE0]);
        assert_eq!(b[12], 12);
        assert_eq!(u16::from_le_bytes([b[14], b[15]]), 1);
        assert_eq!(u32::from_le_bytes(b[36..40].try_into().unwrap()), 9);
        assert_eq!(&b[48..80], &[0u8; 32], "uuid and volume name must be zero");
        assert_eq!(SuperBlock::decode(&b).unwrap(), sb);
    }

    #[test]
    fn superblock_rejects_features_outside_profile() {
        let mut b = SuperBlock::default().encode();
        b[80] = 1; // feature_incompat
        assert!(matches!(SuperBlock::decode(&b), Err(Error::ProfileViolation(_))));
        let mut b = SuperBlock::default().encode();
        b[8] = 1; // feature_compat (checksum)
        assert!(matches!(SuperBlock::decode(&b), Err(Error::ProfileViolation(_))));
        let mut b = SuperBlock::default().encode();
        b[0] = 0;
        assert!(matches!(SuperBlock::decode(&b), Err(Error::ProfileViolation(_))));
    }

    fn sample(extended: bool) -> DiskInode {
        DiskInode {
            extended,
            layout: LAYOUT_FLAT_INLINE,
            xattr_icount: 3,
            mode: (S_IFREG | 0o644) as u16,
            nlink: 2,
            size: 10_000,
            mtime: if extended { 1_700_000_123 } else { 0 },
            mtime_nsec: if extended { 77 } else { 0 },
            i_u: 42,
            ino: 5,
            uid: 1000,
            gid: 1001,
        }
    }

    #[test]
    fn compact_inode_offsets() {
        let b = sample(false).encode();
        assert_eq!(b.len(), 32);
        assert_eq!(u16::from_le_bytes([b[0], b[1]]), LAYOUT_FLAT_INLINE << 1);
        assert_eq!(u16::from_le_bytes([b[6], b[7]]), 2, "nlink");
        assert_eq!(u32::from_le_bytes(b[8..12].try_into().unwrap()), 10_000);
        assert_eq!(u32::from_le_bytes(b[16..20].try_into().unwrap()), 42);
        assert_eq!(u16::from_le_bytes([b[24], b[25]]), 1000);
        assert_eq!(u16::from_le_bytes([b[26], b[27]]), 1001);
        assert_eq!(DiskInode::decode(&b).unwrap(), sample(false));
    }

    #[test]
    fn extended_inode_offsets() {
        let b = sample(true).encode();
        assert_eq!(b.len(), 64);
        assert_eq!(u16::from_le_bytes([b[0], b[1]]), 1 | (LAYOUT_FLAT_INLINE << 1));
        assert_eq!(u64::from_le_bytes(b[8..16].try_into().unwrap()), 10_000);
        assert_eq!(u64::from_le_bytes(b[32..40].try_into().unwrap()), 1_700_000_123);
        assert_eq!(u32::from_le_bytes(b[40..44].try_into().unwrap()), 77);
        assert_eq!(u32::from_le_bytes(b[44..48].try_into().unwrap()), 2, "nlink");
        assert_eq!(DiskInode::decode(&b).unwrap(), sample(true));
    }

    #[test]
    fn inode_rejects_bits_outside_profile() {
        let mut b = sample(false).encode();
        b[0] |= 1 << 4;
        assert!(matches!(DiskInode::decode(&b), Err(Error::ProfileViolation(_))));
        let mut b = sample(false).encode();
        b[0] = 1 << 1; // layout 1: compressed
        assert!(matches!(DiskInode::decode(&b), Err(Error::ProfileViolation(_))));
    }

    #[test]
    fn rdev_matches_linux_new_encode_dev() {
        assert_eq!(encode_rdev(8, 1), 0x801);
        assert_eq!(decode_rdev(encode_rdev(259, 65_536)), (259, 65_536));
        assert_eq!(decode_rdev(encode_rdev(0, 0)), (0, 0));
    }

    #[test]
    fn xattr_sizes() {
        assert_eq!(xattr_entry_len(0, 0), 4);
        assert_eq!(xattr_entry_len(3, 1), 8);
        assert_eq!(xattr_entry_len(5, 0), 12);
        assert_eq!(xattr_ibody_len(0), 0);
        assert_eq!(xattr_ibody_len(1), 12);
        assert_eq!(xattr_ibody_len(3), 20);
        for len in [0usize, 12, 16, 40] {
            assert_eq!(xattr_ibody_len(xattr_icount_for(len)), len);
        }
        let mut v = vec![];
        encode_xattr_entry(XATTR_INDEX_USER, b"abc", b"z", &mut v);
        assert_eq!(v, vec![3, 1, 1, 0, b'a', b'b', b'c', b'z']);
    }

    #[test]
    fn dirent_round_trips() {
        let mut v = vec![];
        encode_dirent(0x1122_3344_5566, 24, FT_DIR, &mut v);
        assert_eq!(v.len(), DIRENT_LEN);
        assert_eq!(decode_dirent(&v), (0x1122_3344_5566, 24, FT_DIR));
    }
}
