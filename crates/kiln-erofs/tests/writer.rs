use std::collections::BTreeMap;
use std::io::Cursor;

use kiln_erofs::ondisk::{BLOCK_SIZE, SUPER_OFFSET, SuperBlock};
use kiln_erofs::testtar::{Opts, TarBuilder};
use kiln_erofs::{Error, LayerWriter, Limits};

fn write(tar: &[u8]) -> kiln_erofs::Result<Vec<u8>> {
    let dir = tempfile::tempdir().unwrap();
    let mut w = LayerWriter::new(Cursor::new(Vec::new()), dir.path(), Limits::default())?;
    w.append_tar(tar)?;
    let (out, _) = w.finish(&BTreeMap::new())?;
    Ok(out.into_inner())
}

fn sb(img: &[u8]) -> SuperBlock {
    SuperBlock::decode(&img[SUPER_OFFSET as usize..]).unwrap()
}

#[test]
fn empty_layer_is_two_blocks() {
    let img = write(&TarBuilder::new().finish()).unwrap();
    assert_eq!(img.len(), 8192);
    let s = sb(&img);
    assert_eq!(
        (s.root_nid, s.inos, s.blocks, s.meta_blkaddr, s.xattr_blkaddr, s.epoch),
        (1, 1, 2, 1, 0, 0)
    );
}

#[test]
fn image_is_block_aligned_and_counts_inodes() {
    let tar = TarBuilder::new()
        .dir("etc", &Opts::default().mode(0o755))
        .file("etc/a", &[1u8; 10], &Opts::default())
        .file("etc/b", &[2u8; 5000], &Opts::default())
        .symlink("etc/c", "a", &Opts::default())
        .finish();
    let img = write(&tar).unwrap();
    let s = sb(&img);
    assert_eq!(s.inos, 5);
    assert_eq!(img.len() as u64, u64::from(s.blocks) * BLOCK_SIZE);
}

#[test]
fn data_blocks_hold_file_bytes() {
    let tar = TarBuilder::new().file("f", &[0xAB; 8192], &Opts::default()).finish();
    let img = write(&tar).unwrap();
    assert!(img[4096..4096 + 8192].iter().all(|&b| b == 0xAB));
}

#[test]
fn same_input_same_bytes_across_spill_dirs() {
    let tar = TarBuilder::new()
        .dir("d", &Opts::default().xattr("user.k", b"v"))
        .file("d/f", &[3u8; 7000], &Opts::default().xattr("user.k", b"v"))
        .finish();
    assert_eq!(write(&tar).unwrap(), write(&tar).unwrap());
}

#[test]
fn truncated_tar_is_malformed() {
    let full = TarBuilder::new().file("f", &[1u8; 5000], &Opts::default()).finish();
    assert!(matches!(write(&full[..512 + 1000]), Err(Error::MalformedTar(_))));
}

#[test]
fn stops_at_end_of_archive() {
    let mut tar = TarBuilder::new().file("f", b"x", &Opts::default()).finish();
    tar.extend_from_slice(b"TRAILER");
    let mut rest: &[u8] = &tar;
    let dir = tempfile::tempdir().unwrap();
    let mut w = LayerWriter::new(Cursor::new(Vec::new()), dir.path(), Limits::default()).unwrap();
    w.append_tar(&mut rest).unwrap();
    assert!(
        rest.ends_with(b"TRAILER"),
        "the bytes after the archive must stay unread"
    );
    assert!(rest.len() <= 512 + 7, "at most the second end block may remain unread");
    let (_, summary) = w.finish(&BTreeMap::new()).unwrap();
    assert_eq!(summary.tar_bytes, (tar.len() - rest.len()) as u64);
}

#[test]
fn implicit_dirs_are_reported_before_finish() {
    let tar = TarBuilder::new().file("a/b/f", b"", &Opts::default()).finish();
    let dir = tempfile::tempdir().unwrap();
    let mut w = LayerWriter::new(Cursor::new(Vec::new()), dir.path(), Limits::default()).unwrap();
    w.append_tar(tar.as_slice()).unwrap();
    assert_eq!(w.implicit_dirs(), vec![b"a".to_vec(), b"a/b".to_vec()]);
    let (_, summary) = w.finish(&BTreeMap::new()).unwrap();
    assert_eq!(summary.implicit_dirs, vec![b"a".to_vec(), b"a/b".to_vec()]);
}

/// A hardlink header's xattrs land on the shared inode after its inline-tail
/// decision was made, so the record can no longer hold the tail: the writer must
/// relocate the file to plain blocks instead of panicking.
#[test]
fn hardlink_xattrs_that_overflow_an_inline_tail_relocate_the_file() {
    for len in [4096 + 4030, 4030] {
        let content: Vec<u8> = (0..len).map(|i| (i * 7 + 3) as u8).collect();
        let mut tb = TarBuilder::new();
        tb.file("a", &content, &Opts::default());
        tb.entry(
            b"b",
            b'1',
            b"",
            b"a",
            (0, 0),
            &Opts::default().xattr("user.k", &[1u8; 64]),
        );
        let img = write(&tb.finish()).expect("the layer must convert");
        assert_eq!(img.len() % BLOCK_SIZE as usize, 0);
        assert!(
            img.windows(content.len()).any(|w| w == content),
            "the file's content must be contiguous in the image (len {len})"
        );
    }
}

fn write_limited(tar: &[u8], max_entries: u64) -> kiln_erofs::Result<Vec<u8>> {
    let dir = tempfile::tempdir().unwrap();
    let limits = Limits {
        max_entries,
        ..Limits::default()
    };
    let mut w = LayerWriter::new(Cursor::new(Vec::new()), dir.path(), limits)?;
    w.append_tar(tar)?;
    let (out, _) = w.finish(&BTreeMap::new())?;
    Ok(out.into_inner())
}

/// `n` files, each under its own chain of `depth - 1` implicit directories.
fn deep_paths(n: usize, depth: usize) -> Vec<u8> {
    let mut b = TarBuilder::new();
    for i in 0..n {
        let dirs: Vec<String> = (1..depth).map(|d| format!("{i}-{d}")).collect();
        b.file(&format!("{}/f", dirs.join("/")), b"", &Opts::default());
    }
    b.finish()
}

/// T3: implicit directories count against `max_entries`, so a small tar of deep
/// paths cannot expand into far more inodes than it has headers.
#[test]
fn implicit_directories_count_against_max_entries() {
    // 20 headers, 20 * 10 = 200 tree entries.
    let tar = deep_paths(20, 10);
    assert!(matches!(
        write_limited(&tar, 199),
        Err(Error::LimitExceeded {
            limit: "entries per layer",
            max: 199,
            ..
        })
    ));
    let img = write_limited(&tar, 200).expect("a layer exactly at the limit converts");
    assert_eq!(sb(&img).inos, 201, "200 entries plus the root");
    let flat: Vec<u8> = {
        let mut b = TarBuilder::new();
        for i in 0..50 {
            b.file(&format!("f{i}"), b"", &Opts::default());
        }
        b.finish()
    };
    write_limited(&flat, 50).expect("50 flat entries fit a limit of 50");
    assert!(write_limited(&flat, 49).is_err());
}

#[test]
fn output_must_be_empty() {
    let dir = tempfile::tempdir().unwrap();
    let err = LayerWriter::new(Cursor::new(vec![1u8]), dir.path(), Limits::default())
        .err()
        .expect("a non-empty output is rejected");
    assert!(
        matches!(&err, Error::Io(e) if e.kind() == std::io::ErrorKind::InvalidInput && e.to_string() == "output must be empty"),
        "{err:?}"
    );
    let mut imgs: Vec<kiln_erofs::Image<Cursor<&[u8]>>> = Vec::new();
    let err = kiln_erofs::squash(&mut imgs, Cursor::new(vec![0u8; 4096]), dir.path())
        .expect_err("squash rejects a non-empty output too");
    assert!(
        matches!(&err, Error::Io(e) if e.to_string() == "output must be empty"),
        "{err:?}"
    );
}

/// Seekable and writable, but every read fails, like a file opened write-only.
struct WriteOnly(Cursor<Vec<u8>>);

impl std::io::Read for WriteOnly {
    fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
        Err(std::io::Error::from_raw_os_error(9)) // EBADF
    }
}

impl std::io::Write for WriteOnly {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl std::io::Seek for WriteOnly {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        self.0.seek(pos)
    }
}

#[test]
fn output_must_be_readable() {
    let dir = tempfile::tempdir().unwrap();
    let err = LayerWriter::new(WriteOnly(Cursor::new(Vec::new())), dir.path(), Limits::default())
        .err()
        .expect("a write-only output is rejected up front");
    assert!(
        matches!(&err, Error::Io(e) if e.to_string() == "output must be readable (open read+write)"),
        "{err:?}"
    );
}
