//! The init layer (spec §6.5): a tiny erofs holding `/kiln-init` and the empty
//! mount points `/proc`, `/sys`, `/dev` and `/kiln`. It boots as `vda`.

use std::collections::BTreeMap;
use std::io::Cursor;
use std::path::Path;

use kiln_erofs::{LayerWriter, Limits};

use crate::error::Result;

/// Builds the init layer around a static `kiln-init` binary. Deterministic: every
/// entry is owned by root with mtime 0.
pub fn init_layer(kiln_init: &[u8], spill_dir: &Path) -> Result<Vec<u8>> {
    let mut tar = tar::Builder::new(Vec::new());
    let entry = |kind: tar::EntryType, mode: u32, size: u64| {
        let mut h = tar::Header::new_ustar();
        h.set_entry_type(kind);
        h.set_mode(mode);
        h.set_size(size);
        h.set_uid(0);
        h.set_gid(0);
        h.set_mtime(0);
        h
    };
    for dir in ["./", "dev/", "kiln/", "proc/", "sys/"] {
        let mut h = entry(tar::EntryType::Directory, 0o755, 0);
        tar.append_data(&mut h, dir, std::io::empty())?;
    }
    let mut h = entry(tar::EntryType::Regular, 0o755, kiln_init.len() as u64);
    tar.append_data(&mut h, "kiln-init", kiln_init)?;
    let tar = tar.into_inner()?;
    let mut writer = LayerWriter::new(Cursor::new(Vec::new()), spill_dir, Limits::default())?;
    writer.append_tar(tar.as_slice())?;
    let (out, _) = writer.finish(&BTreeMap::new())?;
    Ok(out.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_erofs::Image;

    #[test]
    fn holds_init_and_the_mount_points_and_is_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let bin = b"\x7fELF fake init".repeat(1000);
        let a = init_layer(&bin, dir.path()).unwrap();
        assert_eq!(a, init_layer(&bin, dir.path()).unwrap());
        let mut img = Image::open(Cursor::new(a)).unwrap();
        let root = img.root_nid();
        let names: Vec<Vec<u8>> = img
            .read_dir(root)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .filter(|n| n != b"." && n != b"..")
            .collect();
        assert_eq!(names, [&b"dev"[..], b"kiln", b"kiln-init", b"proc", b"sys"]);
        let init = img.lookup(b"kiln-init").unwrap().unwrap();
        assert_eq!(img.inode(init).unwrap().mode & 0o7777, 0o755);
        assert_eq!(img.read_data(init).unwrap(), bin);
    }
}
