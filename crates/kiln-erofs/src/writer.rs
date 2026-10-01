//! The layer writer: streams data, then lays out metadata (spec §7.1–§7.3).

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::apply::{Entry, EntryKind, LayerBuilder};
use crate::error::{Error, Result};
use crate::layout::{assign_nids, dir_size, encode_dir, fits_compact, plan_xattrs, tail_fits_inline};
use crate::limits::Limits;
use crate::ondisk::{
    BLOCK_SIZE, COMPACT_INODE_LEN, DiskInode, EXTENDED_INODE_LEN, FT_BLKDEV, FT_CHRDEV, FT_DIR, FT_FIFO, FT_REG_FILE,
    FT_SYMLINK, LAYOUT_FLAT_INLINE, LAYOUT_FLAT_PLAIN, S_IFBLK, S_IFCHR, S_IFDIR, S_IFIFO, S_IFLNK, S_IFREG, SLOT_SIZE,
    SUPER_OFFSET, SuperBlock, encode_rdev, xattr_icount_for,
};
use crate::tarstream::read_tar;
use crate::tree::{Data, DirAttrs, FileData, Kind, NodeId, TailRef, Timestamp, Tree, Xattrs};

const ZEROS: [u8; BLOCK_SIZE as usize] = [0; BLOCK_SIZE as usize];

fn too_many_blocks() -> Error {
    Error::LimitExceeded {
        limit: "image blocks",
        max: u64::from(u32::MAX),
        path: String::new(),
    }
}

fn read_exact(r: &mut dyn Read, buf: &mut [u8]) -> Result<()> {
    r.read_exact(buf).map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            Error::MalformedTar("entry data truncated".into())
        } else {
            Error::Io(e)
        }
    })
}

fn copy_exact(r: &mut dyn Read, w: &mut impl Write, mut n: u64) -> Result<()> {
    let mut buf = vec![0u8; 128 * 1024];
    while n > 0 {
        let want = n.min(buf.len() as u64) as usize;
        read_exact(r, &mut buf[..want])?;
        w.write_all(&buf[..want])?;
        n -= want as u64;
    }
    Ok(())
}

fn write_zeros(w: &mut impl Write, mut n: u64) -> Result<()> {
    while n > 0 {
        let k = n.min(BLOCK_SIZE);
        w.write_all(&ZEROS[..k as usize])?;
        n -= k;
    }
    Ok(())
}

/// The output plus a spill file for inline tails. Between calls the output
/// position is always `next_blk * BLOCK_SIZE`.
pub(crate) struct DataStore<W> {
    out: W,
    next_blk: u32,
    spill: File,
    spill_len: u64,
}

impl<W: Read + Write + Seek> DataStore<W> {
    /// `out` must be empty, readable (relocation reads data back) and not in append
    /// mode. The first two are checked here.
    pub fn new(mut out: W, spill_dir: &Path) -> Result<Self> {
        if out.seek(SeekFrom::End(0))? != 0 {
            return Err(Error::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "output must be empty",
            )));
        }
        out.write_all(&ZEROS)?;
        out.seek(SeekFrom::Start(0))?;
        out.read_exact(&mut [0u8; 1]).map_err(|_| {
            Error::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "output must be readable (open read+write)",
            ))
        })?;
        out.seek(SeekFrom::Start(BLOCK_SIZE))?;
        Ok(Self {
            out,
            next_blk: 1,
            spill: tempfile::tempfile_in(spill_dir)?,
            spill_len: 0,
        })
    }

    /// Streams `size` bytes from `r`. Full blocks go to the data area; a partial
    /// tail goes to the spill file when `inline_tail`, else to a zero-padded block.
    pub fn write_file(&mut self, r: &mut dyn Read, size: u64, inline_tail: bool) -> Result<FileData> {
        let tail_len = size % BLOCK_SIZE;
        let full = size - tail_len;
        let start = self.next_blk;
        copy_exact(r, &mut self.out, full)?;
        let mut blocks = full / BLOCK_SIZE;
        let mut tail = None;
        if tail_len > 0 {
            let mut buf = vec![0u8; tail_len as usize];
            read_exact(r, &mut buf)?;
            if inline_tail {
                self.spill.seek(SeekFrom::Start(self.spill_len))?;
                self.spill.write_all(&buf)?;
                tail = Some(TailRef {
                    offset: self.spill_len,
                    len: tail_len as u32,
                });
                self.spill_len += tail_len;
            } else {
                self.out.write_all(&buf)?;
                self.out.write_all(&ZEROS[..(BLOCK_SIZE - tail_len) as usize])?;
                blocks += 1;
            }
        }
        let blocks = u32::try_from(blocks).map_err(|_| too_many_blocks())?;
        self.next_blk = self.next_blk.checked_add(blocks).ok_or_else(too_many_blocks)?;
        Ok(FileData {
            start_blk: if blocks == 0 { 0 } else { start },
            blocks,
            tail,
        })
    }

    /// Writes `bytes` padded to whole blocks and returns the first block.
    pub fn write_blocks(&mut self, bytes: &[u8]) -> Result<u32> {
        Ok(self.write_file(&mut &bytes[..], bytes.len() as u64, false)?.start_blk)
    }

    /// Moves a file with a spilled tail into plain contiguous blocks at the end
    /// of the data area, zero-padding the tail to a block. Used when later xattrs
    /// leave no room for the tail in the inode record.
    pub fn relocate(&mut self, fd: FileData) -> Result<FileData> {
        let tail = fd.tail.map(|t| self.read_tail(t)).transpose()?.unwrap_or_default();
        let blocks = fd.blocks.checked_add(1).ok_or_else(too_many_blocks)?;
        let start = self.next_blk;
        let end = start.checked_add(blocks).ok_or_else(too_many_blocks)?;
        let mut buf = vec![0u8; 128 * 1024];
        let total = u64::from(fd.blocks) * BLOCK_SIZE;
        let mut done = 0u64;
        while done < total {
            let want = (total - done).min(buf.len() as u64) as usize;
            self.out
                .seek(SeekFrom::Start(u64::from(fd.start_blk) * BLOCK_SIZE + done))?;
            self.out.read_exact(&mut buf[..want])?;
            self.out.seek(SeekFrom::Start(u64::from(start) * BLOCK_SIZE + done))?;
            self.out.write_all(&buf[..want])?;
            done += want as u64;
        }
        self.out.seek(SeekFrom::Start(u64::from(start) * BLOCK_SIZE + total))?;
        self.out.write_all(&tail)?;
        write_zeros(&mut self.out, BLOCK_SIZE - tail.len() as u64)?;
        self.next_blk = end;
        // The invariant: the output position is `next_blk * BLOCK_SIZE`.
        debug_assert_eq!(self.out.stream_position()?, u64::from(end) * BLOCK_SIZE);
        Ok(FileData {
            start_blk: start,
            blocks,
            tail: None,
        })
    }

    pub fn read_tail(&mut self, t: TailRef) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; t.len as usize];
        self.spill.seek(SeekFrom::Start(t.offset))?;
        self.spill.read_exact(&mut buf)?;
        Ok(buf)
    }
}

/// Source of file bytes that still live in other images (squash).
pub(crate) trait ExternalData {
    fn open(&mut self, layer: usize, nid: u64) -> Result<Box<dyn Read + '_>>;
}

pub(crate) struct NoExternal;

impl ExternalData for NoExternal {
    fn open(&mut self, _layer: usize, _nid: u64) -> Result<Box<dyn Read + '_>> {
        Err(Error::Corrupt("external data in a streamed layer".into()))
    }
}

pub(crate) struct EmitStats {
    pub inodes: u64,
    pub bytes: u64,
}

struct InodeRec {
    extended: bool,
    size: u64,
    nlink: u32,
    isize: usize,
    inline_len: usize,
}

fn file_type(k: &Kind) -> u8 {
    match k {
        Kind::Dir { .. } => FT_DIR,
        Kind::File { .. } => FT_REG_FILE,
        Kind::Symlink { .. } => FT_SYMLINK,
        Kind::CharDev { .. } => FT_CHRDEV,
        Kind::BlockDev { .. } => FT_BLKDEV,
        Kind::Fifo => FT_FIFO,
    }
}

fn type_bits(k: &Kind) -> u32 {
    match k {
        Kind::Dir { .. } => S_IFDIR,
        Kind::File { .. } => S_IFREG,
        Kind::Symlink { .. } => S_IFLNK,
        Kind::CharDev { .. } => S_IFCHR,
        Kind::BlockDev { .. } => S_IFBLK,
        Kind::Fifo => S_IFIFO,
    }
}

/// Lays out `tree` after the data already in `store` and writes the image.
pub(crate) fn emit<W: Read + Write + Seek>(
    mut tree: Tree,
    base: Timestamp,
    mut store: DataStore<W>,
    ext: &mut dyn ExternalData,
) -> Result<(W, EmitStats)> {
    // 1. Number inodes breadth-first in name order; count links; record parents.
    let mut index_of: Vec<Option<usize>> = vec![None; tree.nodes.len()];
    let mut order: Vec<NodeId> = vec![tree.root];
    let mut links: Vec<u32> = vec![1];
    let mut parent: Vec<usize> = vec![0];
    index_of[tree.root] = Some(0);
    let mut i = 0;
    while i < order.len() {
        if let Some(children) = tree.children(order[i]) {
            for &child in children.values() {
                match index_of[child] {
                    Some(ci) => links[ci] += 1,
                    None => {
                        index_of[child] = Some(order.len());
                        order.push(child);
                        links.push(1);
                        parent.push(i);
                    }
                }
            }
        }
        i += 1;
    }
    let n = order.len();

    // 2. Copy external (squash) file data, in inode order.
    for &id in &order {
        if let Kind::File {
            size,
            data: Data::External { layer, nid },
        } = &tree.nodes[id].kind
        {
            let (size, layer, nid) = (*size, *layer, *nid);
            let inline = tail_fits_inline(size % BLOCK_SIZE, &tree.nodes[id].xattrs);
            let written = {
                let mut r = ext.open(layer, nid)?;
                store.write_file(&mut r, size, inline)?
            };
            if let Kind::File { data, .. } = &mut tree.nodes[id].kind {
                *data = Data::Written(written);
            }
        }
    }

    // 3. Xattr plan and the size of every inode record.
    let plan = {
        let refs: Vec<&Xattrs> = order.iter().map(|&id| &tree.nodes[id].xattrs).collect();
        plan_xattrs(&refs)?
    };
    // A hardlink header can add xattrs to a file whose inline tail was decided
    // while streaming. Where the record no longer fits a block, move the file to
    // plain blocks (in inode order, so the output stays deterministic).
    for (ix, &id) in order.iter().enumerate() {
        let node = &tree.nodes[id];
        let Kind::File {
            size,
            data: Data::Written(fd),
        } = &node.kind
        else {
            continue;
        };
        let Some(tail) = fd.tail else { continue };
        let (size, fd) = (*size, *fd);
        let isize = if fits_compact(&node.meta, links[ix], size, base) {
            COMPACT_INODE_LEN
        } else {
            EXTENDED_INODE_LEN
        };
        if tail.len as usize + isize + plan.ibodies[ix].len() > BLOCK_SIZE as usize {
            let moved = store.relocate(fd)?;
            if let Kind::File { data, .. } = &mut tree.nodes[id].kind {
                *data = Data::Written(moved);
            }
        }
    }
    let mut recs = Vec::with_capacity(n);
    for (ix, &id) in order.iter().enumerate() {
        let node = &tree.nodes[id];
        let ibody = plan.ibodies[ix].len();
        let (nlink, size, streamed_tail) = match &node.kind {
            Kind::Dir { children, .. } => {
                let subdirs = children.values().filter(|&&c| tree.nodes[c].is_dir()).count() as u32;
                let mut names: Vec<&[u8]> = vec![&b"."[..], &b".."[..]];
                names.extend(children.keys().map(Vec::as_slice));
                names.sort();
                (2 + subdirs, dir_size(&names), None)
            }
            Kind::Symlink { target } => (links[ix], target.len() as u64, None),
            Kind::File {
                size,
                data: Data::Written(fd),
            } => (links[ix], *size, Some(fd.tail.map_or(0, |t| t.len as usize))),
            Kind::File { .. } => unreachable!("external data was copied in step 2"),
            _ => (links[ix], 0, Some(0)),
        };
        let extended = !fits_compact(&node.meta, nlink, size, base);
        let isize = if extended {
            EXTENDED_INODE_LEN
        } else {
            COMPACT_INODE_LEN
        };
        let inline_len = streamed_tail.unwrap_or_else(|| {
            let t = (size % BLOCK_SIZE) as usize;
            if t > 0 && t + isize + ibody <= BLOCK_SIZE as usize {
                t
            } else {
                0
            }
        });
        recs.push(InodeRec {
            extended,
            size,
            nlink,
            isize,
            inline_len,
        });
    }
    let lens: Vec<usize> = recs
        .iter()
        .zip(&plan.ibodies)
        .map(|(r, b)| r.isize + b.len() + r.inline_len)
        .collect();
    let (nids, meta_len) = assign_nids(&lens);

    // 4. Directory and symlink bodies (they need nids); record file locations.
    let mut placed = vec![
        FileData {
            start_blk: 0,
            blocks: 0,
            tail: None
        };
        n
    ];
    for (ix, &id) in order.iter().enumerate() {
        let node = &tree.nodes[id];
        let body = match &node.kind {
            Kind::Dir { children, .. } => {
                let mut ents: Vec<(&[u8], u64, u8)> =
                    vec![(&b"."[..], nids[ix], FT_DIR), (&b".."[..], nids[parent[ix]], FT_DIR)];
                for (name, &c) in children {
                    let ci = index_of[c].expect("every child is numbered");
                    ents.push((name.as_slice(), nids[ci], file_type(&tree.nodes[c].kind)));
                }
                ents.sort_by(|a, b| a.0.cmp(b.0));
                encode_dir(&ents)
            }
            Kind::Symlink { target } => target.clone(),
            Kind::File {
                data: Data::Written(fd),
                ..
            } => {
                placed[ix] = *fd;
                continue;
            }
            _ => continue,
        };
        debug_assert_eq!(body.len() as u64, recs[ix].size);
        placed[ix] = store.write_file(&mut body.as_slice(), body.len() as u64, recs[ix].inline_len > 0)?;
    }

    // 5. Shared xattr table.
    let xattr_blkaddr = if plan.table.is_empty() {
        0
    } else {
        store.write_blocks(&plan.table)?
    };

    // 6. Inode records, in nid order.
    let meta_blkaddr = store.next_blk;
    let mut written = 0u64;
    for ix in 0..n {
        let node = &tree.nodes[order[ix]];
        let rec = &recs[ix];
        let off = nids[ix] * SLOT_SIZE;
        write_zeros(&mut store.out, off - written)?;
        let i_u = match node.kind {
            Kind::CharDev { major, minor } | Kind::BlockDev { major, minor } => encode_rdev(major, minor),
            Kind::Fifo => 0,
            _ => placed[ix].start_blk,
        };
        let (mtime, mtime_nsec) = if rec.extended {
            (node.meta.mtime.sec as u64, node.meta.mtime.nsec)
        } else {
            (0, 0)
        };
        let inode = DiskInode {
            extended: rec.extended,
            layout: if rec.inline_len > 0 {
                LAYOUT_FLAT_INLINE
            } else {
                LAYOUT_FLAT_PLAIN
            },
            xattr_icount: xattr_icount_for(plan.ibodies[ix].len()),
            mode: (type_bits(&node.kind) | (node.meta.mode & 0o7777)) as u16,
            nlink: rec.nlink,
            size: rec.size,
            mtime,
            mtime_nsec,
            i_u,
            ino: (ix + 1) as u32,
            uid: node.meta.uid,
            gid: node.meta.gid,
        };
        let mut bytes = inode.encode();
        bytes.extend_from_slice(&plan.ibodies[ix]);
        if rec.inline_len > 0 {
            let tail = placed[ix].tail.expect("an inline record has a spilled tail");
            bytes.extend(store.read_tail(tail)?);
        }
        store.out.write_all(&bytes)?;
        written = off + bytes.len() as u64;
    }
    write_zeros(&mut store.out, meta_len - written)?;

    // 7. Superblock, written last.
    let blocks = u32::try_from(u64::from(meta_blkaddr) + meta_len / BLOCK_SIZE).map_err(|_| too_many_blocks())?;
    let sb = SuperBlock {
        root_nid: u16::try_from(nids[0]).expect("the root is the first inode"),
        inos: n as u64,
        epoch: base.sec as u64,
        fixed_nsec: base.nsec,
        blocks,
        meta_blkaddr,
        xattr_blkaddr,
    };
    let mut out = store.out;
    out.seek(SeekFrom::Start(SUPER_OFFSET))?;
    out.write_all(&sb.encode())?;
    let bytes = u64::from(blocks) * BLOCK_SIZE;
    out.seek(SeekFrom::Start(bytes))?;
    out.flush()?;
    Ok((
        out,
        EmitStats {
            inodes: n as u64,
            bytes,
        },
    ))
}

/// Result of converting one layer.
#[derive(Debug, Clone)]
pub struct LayerSummary {
    pub inodes: u64,
    pub image_bytes: u64,
    /// Tar bytes consumed, up to and including the end-of-archive marker.
    pub tar_bytes: u64,
    /// Implicit directories, sorted, as reported before `finish`.
    pub implicit_dirs: Vec<Vec<u8>>,
    pub warnings: Vec<String>,
}

/// Converts one layer tar into one erofs image (spec §6.2).
pub struct LayerWriter<W: Read + Write + Seek> {
    store: DataStore<W>,
    builder: LayerBuilder,
    limits: Limits,
    warnings: Vec<String>,
    tar_bytes: u64,
}

impl<W: Read + Write + Seek> LayerWriter<W> {
    /// `spill_dir` holds a temporary file for inline tails; it should be on local disk.
    ///
    /// `out` must be empty, readable as well as writable (a file opened read+write:
    /// the writer reads data back when it relocates a file), and not in append mode
    /// (the writer seeks and overwrites). An output that is not empty or not readable
    /// is rejected with `Error::Io` of kind `InvalidInput`.
    pub fn new(out: W, spill_dir: &Path, limits: Limits) -> Result<Self> {
        Ok(Self {
            store: DataStore::new(out, spill_dir)?,
            builder: LayerBuilder::new(limits.max_entries),
            limits,
            warnings: Vec::new(),
            tar_bytes: 0,
        })
    }

    /// Reads one (decompressed) layer tar up to its end-of-archive marker and
    /// leaves any following bytes unread. Call once per writer.
    pub fn append_tar<R: Read>(&mut self, tar: R) -> Result<()> {
        let Self {
            store,
            builder,
            limits,
            warnings,
            tar_bytes,
        } = self;
        let consumed = read_tar(tar, limits, warnings, &mut |entry: Entry,
                                                             data: &mut dyn Read|
         -> Result<()> {
            let file = match entry.kind {
                EntryKind::File { size } => {
                    let inline = tail_fits_inline(size % BLOCK_SIZE, &entry.xattrs);
                    Some(Data::Written(store.write_file(data, size, inline)?))
                }
                _ => None,
            };
            builder.apply(entry, file)
        })?;
        *tar_bytes += consumed;
        Ok(())
    }

    /// Paths whose attributes the caller should resolve from lower layers.
    pub fn implicit_dirs(&self) -> Vec<Vec<u8>> {
        self.builder.implicit_dirs()
    }

    /// Writes metadata and the superblock. `inherited` maps implicit directory
    /// paths to attributes from the merged lower layers (`resolve_inherited`).
    pub fn finish(self, inherited: &BTreeMap<Vec<u8>, DirAttrs>) -> Result<(W, LayerSummary)> {
        let (tree, base, implicit_dirs) = self.builder.finalize(inherited);
        let (out, stats) = emit(tree, base, self.store, &mut NoExternal)?;
        Ok((
            out,
            LayerSummary {
                inodes: stats.inodes,
                image_bytes: stats.bytes,
                tar_bytes: self.tar_bytes,
                implicit_dirs,
                warnings: self.warnings,
            },
        ))
    }
}
