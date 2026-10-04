//! The scratch disk (spec §9.4): the committed ext4 template, decompressed into a
//! sparse file and extended. The guest grows the filesystem online at boot.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::error::{ImageError, Result};

/// `crates/kiln-image/assets/ext4-template.img.zst`, made by the repository's
/// `assets/make-ext4-template.sh`. It lives in this crate so that it is packaged with it.
pub const TEMPLATE_ZST: &[u8] = include_bytes!("../assets/ext4-template.img.zst");
/// The template's size: the smallest scratch disk.
pub const TEMPLATE_BYTES: u64 = 64 << 20;
/// The template's filesystem UUID (fixed, so the template is reproducible).
pub const TEMPLATE_UUID: [u8; 16] = *b"kilnscratch\0\0\0\0\x01";

const BLOCK: usize = 4096;

/// Creates the scratch disk at `path` (which must not exist) with `size` bytes: a
/// multiple of 4096, at least [`TEMPLATE_BYTES`]. Zero blocks stay holes, so the
/// file takes about the template's non-zero data on disk.
pub fn create_scratch(path: &Path, size: u64) -> Result<()> {
    if size < TEMPLATE_BYTES || !size.is_multiple_of(BLOCK as u64) {
        return Err(ImageError::BadOption(format!(
            "scratch disk size {size}: must be a multiple of 4096 and at least {TEMPLATE_BYTES}"
        )));
    }
    let mut file = create_new(path)?;
    let written = write_sparse(&mut file, size);
    if written.is_err() {
        let _ = std::fs::remove_file(path);
    }
    written
}

fn create_new(path: &Path) -> Result<File> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    Ok(opts.open(path)?)
}

fn write_sparse(file: &mut File, size: u64) -> Result<()> {
    let mut template = zstd::stream::read::Decoder::new(TEMPLATE_ZST)?;
    let mut block = vec![0u8; BLOCK];
    let mut offset = 0u64;
    loop {
        let n = read_block(&mut template, &mut block)?;
        if n == 0 {
            break;
        }
        if offset + n as u64 > TEMPLATE_BYTES {
            return Err(corrupt("larger than 64 MiB"));
        }
        if block[..n].iter().any(|&b| b != 0) {
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(&block[..n])?;
        }
        offset += n as u64;
    }
    if offset != TEMPLATE_BYTES {
        return Err(corrupt("shorter than 64 MiB"));
    }
    file.set_len(size)?;
    Ok(())
}

/// Reads up to one block; less only at the end of the stream.
fn read_block(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

fn corrupt(what: &str) -> ImageError {
    ImageError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("embedded ext4 template is {what}"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_template_is_ext4_with_the_fixed_uuid() {
        let raw = zstd::decode_all(TEMPLATE_ZST).unwrap();
        assert_eq!(raw.len() as u64, TEMPLATE_BYTES);
        let sb = &raw[1024..2048];
        assert_eq!(&sb[0x38..0x3a], &[0x53, 0xef], "ext4 magic");
        assert_eq!(
            u32::from_le_bytes(sb[0x18..0x1c].try_into().unwrap()),
            2,
            "4 KiB blocks"
        );
        assert_eq!(&sb[0x68..0x78], &TEMPLATE_UUID);
        let compat = u32::from_le_bytes(sb[0x5c..0x60].try_into().unwrap());
        let incompat = u32::from_le_bytes(sb[0x60..0x64].try_into().unwrap());
        assert_eq!(compat & 0x10, 0, "no resize_inode");
        assert_ne!(incompat & 0x10, 0, "meta_bg");
        assert_eq!(
            u32::from_le_bytes(sb[0x160..0x164].try_into().unwrap()),
            2,
            "unsigned dir hash"
        );
    }

    #[test]
    fn creates_a_sparse_disk_of_the_requested_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scratch.img");
        create_scratch(&path, 1 << 30).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 1 << 30);
        let mut head = vec![0u8; TEMPLATE_BYTES as usize];
        File::open(&path).unwrap().read_exact(&mut head).unwrap();
        assert_eq!(head, zstd::decode_all(TEMPLATE_ZST).unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let meta = std::fs::metadata(&path).unwrap();
            assert!(
                meta.blocks() * 512 < 16 << 20,
                "{} bytes allocated",
                meta.blocks() * 512
            );
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn refuses_bad_sizes_and_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scratch.img");
        for size in [0, TEMPLATE_BYTES - 4096, TEMPLATE_BYTES + 1] {
            assert!(matches!(create_scratch(&path, size), Err(ImageError::BadOption(_))));
            assert!(!path.exists());
        }
        std::fs::write(&path, b"keep").unwrap();
        assert!(create_scratch(&path, TEMPLATE_BYTES).is_err());
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"keep",
            "an existing file is never touched"
        );
    }
}
