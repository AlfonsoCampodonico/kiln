//! Reading kiln-profile erofs images (spec §7.5).

use std::io::{self, Read, Seek, SeekFrom};

use crate::error::{Error, Result};
use crate::ondisk::{
    BLOCK_SIZE, COMPACT_INODE_LEN, DIRENT_LEN, DiskInode, EXTENDED_INODE_LEN, LAYOUT_FLAT_PLAIN, NAME_LEN_MAX, S_IFBLK,
    S_IFCHR, S_IFDIR, S_IFMT, SLOT_SIZE, SUPER_LEN, SUPER_OFFSET, SuperBlock, XATTR_ENTRY_HEADER_LEN,
    XATTR_IBODY_HEADER_LEN, decode_dirent, decode_rdev, xattr_entry_len, xattr_ibody_len,
};
use crate::path::components;
use crate::tree::{Timestamp, XattrKey, Xattrs};

/// A kiln-profile erofs image.
pub struct Image<R> {
    r: R,
    sb: SuperBlock,
}

/// Decoded inode attributes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InodeInfo {
    pub nid: u64,
    /// Full `st_mode` (type and permission bits).
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub nlink: u32,
    pub size: u64,
    pub mtime: Timestamp,
    /// `(major, minor)` for device inodes, else `(0, 0)`.
    pub rdev: (u32, u32),
    pub layout: u16,
    pub startblk: u32,
    /// On-disk inode size: 32 (compact) or 64 (extended).
    pub isize: usize,
    pub ibody_len: usize,
}

impl InodeInfo {
    pub fn is_dir(&self) -> bool {
        self.mode & S_IFMT == S_IFDIR
    }

    /// An overlayfs whiteout: a character device 0:0.
    pub fn is_whiteout(&self) -> bool {
        self.mode & S_IFMT == S_IFCHR && self.rdev == (0, 0)
    }
}

/// One directory entry other than `.` and `..`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: Vec<u8>,
    pub nid: u64,
    pub file_type: u8,
}

fn corrupt(what: String) -> Error {
    Error::Corrupt(what)
}

impl<R: Read + Seek> Image<R> {
    pub fn open(mut r: R) -> Result<Self> {
        let mut b = [0u8; SUPER_LEN];
        r.seek(SeekFrom::Start(SUPER_OFFSET))?;
        r.read_exact(&mut b)
            .map_err(|_| corrupt("image shorter than a superblock".into()))?;
        let sb = SuperBlock::decode(&b)?;
        let len = r.seek(SeekFrom::End(0))?;
        if len < u64::from(sb.blocks) * BLOCK_SIZE {
            return Err(corrupt(format!(
                "image is {len} bytes but declares {} blocks",
                sb.blocks
            )));
        }
        Ok(Self { r, sb })
    }

    pub fn superblock(&self) -> &SuperBlock {
        &self.sb
    }

    pub fn root_nid(&self) -> u64 {
        u64::from(self.sb.root_nid)
    }

    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<()> {
        self.r.seek(SeekFrom::Start(off))?;
        self.r
            .read_exact(buf)
            .map_err(|_| corrupt(format!("read past the end of the image at offset {off}")))
    }

    fn iloc(&self, nid: u64) -> Result<u64> {
        let base = u64::from(self.sb.meta_blkaddr)
            .checked_mul(BLOCK_SIZE)
            .ok_or_else(|| corrupt(format!("nid {nid} out of range")))?;
        let offset = nid
            .checked_mul(SLOT_SIZE)
            .ok_or_else(|| corrupt(format!("nid {nid} out of range")))?;
        let result = base
            .checked_add(offset)
            .ok_or_else(|| corrupt(format!("nid {nid} out of range")))?;
        let max = u64::from(self.sb.blocks)
            .checked_mul(BLOCK_SIZE)
            .ok_or_else(|| corrupt(format!("nid {nid} out of range")))?;
        if result >= max {
            return Err(corrupt(format!("nid {nid} out of range")));
        }
        Ok(result)
    }

    pub fn inode(&mut self, nid: u64) -> Result<InodeInfo> {
        let mut b = [0u8; EXTENDED_INODE_LEN];
        let at = self.iloc(nid)?;
        self.read_at(at, &mut b[..COMPACT_INODE_LEN])?;
        if b[0] & 1 == 1 {
            self.read_at(at + COMPACT_INODE_LEN as u64, &mut b[COMPACT_INODE_LEN..])?;
        }
        let di = DiskInode::decode(&b)?;
        let mtime = if di.extended {
            Timestamp {
                sec: di.mtime as i64,
                nsec: di.mtime_nsec,
            }
        } else {
            Timestamp {
                sec: (self.sb.epoch as i64).wrapping_add(di.mtime as i64),
                nsec: self.sb.fixed_nsec,
            }
        };
        let mode = u32::from(di.mode);
        let ft = mode & S_IFMT;
        let rdev = if ft == S_IFCHR || ft == S_IFBLK {
            decode_rdev(di.i_u)
        } else {
            (0, 0)
        };
        Ok(InodeInfo {
            nid,
            mode,
            uid: di.uid,
            gid: di.gid,
            nlink: di.nlink,
            size: di.size,
            mtime,
            rdev,
            layout: di.layout,
            startblk: di.i_u,
            isize: di.encoded_len(),
            ibody_len: xattr_ibody_len(di.xattr_icount),
        })
    }

    pub fn xattrs(&mut self, nid: u64) -> Result<Xattrs> {
        let info = self.inode(nid)?;
        let mut out = Xattrs::new();
        if info.ibody_len == 0 {
            return Ok(out);
        }
        let mut body = vec![0u8; info.ibody_len];
        let iloc_val = self.iloc(nid)?;
        self.read_at(iloc_val + info.isize as u64, &mut body)?;
        let shared = usize::from(body[4]);
        let mut pos = XATTR_IBODY_HEADER_LEN;
        if pos + 4 * shared > body.len() {
            return Err(corrupt(format!(
                "xattr body of nid {nid} too short for {shared} shared ids"
            )));
        }
        for _ in 0..shared {
            let id = u32::from_le_bytes(body[pos..pos + 4].try_into().expect("4 bytes"));
            pos += 4;
            let (k, v) = self.shared_xattr(id)?;
            out.insert(k, v);
        }
        while pos + XATTR_ENTRY_HEADER_LEN <= body.len() {
            let (k, v, len) = parse_xattr_entry(&body[pos..])?;
            out.insert(k, v);
            pos += len;
        }
        Ok(out)
    }

    fn shared_xattr(&mut self, id: u32) -> Result<(XattrKey, Vec<u8>)> {
        let off = u64::from(self.sb.xattr_blkaddr) * BLOCK_SIZE + u64::from(id) * 4;
        let mut h = [0u8; XATTR_ENTRY_HEADER_LEN];
        self.read_at(off, &mut h)?;
        let name_len = usize::from(h[0]);
        let value_len = usize::from(u16::from_le_bytes([h[2], h[3]]));
        let mut rest = vec![0u8; name_len + value_len];
        self.read_at(off + XATTR_ENTRY_HEADER_LEN as u64, &mut rest)?;
        Ok((
            XattrKey {
                index: h[1],
                name: rest[..name_len].to_vec(),
            },
            rest[name_len..].to_vec(),
        ))
    }

    fn segments(&mut self, info: &InodeInfo) -> Result<Vec<(u64, u64)>> {
        if info.size == 0 {
            return Ok(Vec::new());
        }
        let start = u64::from(info.startblk) * BLOCK_SIZE;
        if info.layout == LAYOUT_FLAT_PLAIN {
            return Ok(vec![(start, info.size)]);
        }
        let full = (info.size.div_ceil(BLOCK_SIZE) - 1) * BLOCK_SIZE;
        let iloc_val = self.iloc(info.nid)?;
        let tail_off = iloc_val + (info.isize + info.ibody_len) as u64;
        let tail_len = info.size - full;
        if tail_off % BLOCK_SIZE + tail_len > BLOCK_SIZE {
            return Err(corrupt(format!(
                "inline data of nid {} crosses a block boundary",
                info.nid
            )));
        }
        let mut segs = Vec::new();
        if full > 0 {
            segs.push((start, full));
        }
        segs.push((tail_off, tail_len));
        Ok(segs)
    }

    pub fn data_reader(&mut self, nid: u64) -> Result<DataReader<'_, R>> {
        let info = self.inode(nid)?;
        let segs = self.segments(&info)?;
        Ok(DataReader {
            img: self,
            segs,
            seg: 0,
            pos: 0,
        })
    }

    pub fn read_data(&mut self, nid: u64) -> Result<Vec<u8>> {
        let mut v = Vec::new();
        self.data_reader(nid)?
            .read_to_end(&mut v)
            .map_err(|e| corrupt(format!("reading data of nid {nid}: {e}")))?;
        Ok(v)
    }

    pub fn readlink(&mut self, nid: u64) -> Result<Vec<u8>> {
        self.read_data(nid)
    }

    pub fn read_dir(&mut self, nid: u64) -> Result<Vec<DirEntry>> {
        if !self.inode(nid)?.is_dir() {
            return Err(corrupt(format!("nid {nid} is not a directory")));
        }
        let data = self.read_data(nid)?;
        let bad = || corrupt(format!("bad directory block in nid {nid}"));
        let mut all_entries = Vec::new();
        for block in data.chunks(BLOCK_SIZE as usize) {
            if block.len() < DIRENT_LEN {
                return Err(bad());
            }
            let nameoff0 = usize::from(decode_dirent(block).1);
            if nameoff0 == 0 || nameoff0 % DIRENT_LEN != 0 || nameoff0 > block.len() {
                return Err(bad());
            }
            let count = nameoff0 / DIRENT_LEN;
            for i in 0..count {
                let (child, nameoff, ft) = decode_dirent(&block[i * DIRENT_LEN..]);
                let start = usize::from(nameoff);
                let end = if i + 1 < count {
                    usize::from(decode_dirent(&block[(i + 1) * DIRENT_LEN..]).1)
                } else {
                    let tail = block.get(start..).ok_or_else(bad)?;
                    start + tail.iter().position(|&b| b == 0).unwrap_or(tail.len())
                };
                if start >= end || end > block.len() || end - start > NAME_LEN_MAX {
                    return Err(bad());
                }
                let name = block[start..end].to_vec();
                all_entries.push((name.clone(), child, ft));
            }
        }
        // Check sort order on ALL entries including "." and ".."
        if !all_entries.windows(2).all(|w| w[0].0 < w[1].0) {
            return Err(corrupt(format!("directory nid {nid} is not strictly sorted")));
        }
        // Filter out "." and ".." before returning
        let out: Vec<DirEntry> = all_entries
            .into_iter()
            .filter(|(name, _, _)| name != b"." && name != b"..")
            .map(|(name, nid, file_type)| DirEntry { name, nid, file_type })
            .collect();
        Ok(out)
    }

    /// Resolves a normalized path (`b""` is the root).
    pub fn lookup(&mut self, path: &[u8]) -> Result<Option<u64>> {
        let mut cur = self.root_nid();
        for c in components(path) {
            if !self.inode(cur)?.is_dir() {
                return Ok(None);
            }
            match self.read_dir(cur)?.into_iter().find(|e| e.name == c) {
                Some(e) => cur = e.nid,
                None => return Ok(None),
            }
        }
        Ok(Some(cur))
    }
}

fn parse_xattr_entry(b: &[u8]) -> Result<(XattrKey, Vec<u8>, usize)> {
    let name_len = usize::from(b[0]);
    let value_len = usize::from(u16::from_le_bytes([b[2], b[3]]));
    let end = XATTR_ENTRY_HEADER_LEN + name_len + value_len;
    if end > b.len() {
        return Err(corrupt("inline xattr entry overruns the xattr body".into()));
    }
    let name = b[XATTR_ENTRY_HEADER_LEN..XATTR_ENTRY_HEADER_LEN + name_len].to_vec();
    let value = b[XATTR_ENTRY_HEADER_LEN + name_len..end].to_vec();
    Ok((
        XattrKey { index: b[1], name },
        value,
        xattr_entry_len(name_len, value_len),
    ))
}

/// Sequential reader over one inode's data.
pub struct DataReader<'a, R> {
    img: &'a mut Image<R>,
    segs: Vec<(u64, u64)>,
    seg: usize,
    pos: u64,
}

impl<R: Read + Seek> Read for DataReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while let Some(&(off, len)) = self.segs.get(self.seg) {
            if self.pos >= len {
                self.seg += 1;
                self.pos = 0;
                continue;
            }
            let want = (len - self.pos).min(buf.len() as u64) as usize;
            self.img.r.seek(SeekFrom::Start(off + self.pos))?;
            self.img.r.read_exact(&mut buf[..want])?;
            self.pos += want as u64;
            return Ok(want);
        }
        Ok(0)
    }
}
