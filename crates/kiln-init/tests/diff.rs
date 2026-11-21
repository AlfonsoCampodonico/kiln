//! The sealed diff's format, read back by kiln's own converter (kiln-erofs), as
//! `kiln convert` and potter's ingest read it: whiteouts, opaque directories, hard
//! links, long names and link targets, large ids, extended attributes.

use std::collections::BTreeMap;
use std::io::Cursor;

use kiln_erofs::{Image, LayerWriter, Limits};
use kiln_init::diff::{Kind, Node, TarWriter, opaque, whiteout};

fn node(path: &str, kind: Kind) -> Node {
    Node {
        kind,
        mode: 0o755,
        ..Node::marker(path.as_bytes().to_vec())
    }
}

#[test]
fn a_diff_converts_with_its_whiteouts_links_and_long_names() {
    let long_dir = "d".repeat(200);
    let long_file = format!("{long_dir}/{}", "f".repeat(255));
    let long_target = format!("/{}", "t".repeat(300));
    let mut t = TarWriter::new(Vec::new());
    let mut entries = vec![
        (node("app", Kind::Dir), None),
        (node("app/a", Kind::File(5)), Some(b"hello".to_vec())),
        (node("app/hard", Kind::HardLink(b"app/a".to_vec())), None),
        (Node::marker(whiteout(b"app", b"gone")), Some(Vec::new())),
        (node("cache", Kind::Dir), None),
        (Node::marker(opaque(b"cache")), Some(Vec::new())),
        (node(&long_dir, Kind::Dir), None),
        (node(&long_file, Kind::File(3)), Some(b"abc".to_vec())),
        (node("link", Kind::Symlink(long_target.clone().into_bytes())), None),
    ];
    let mut big = node("big", Kind::File(0));
    big.uid = 5_000_000;
    big.gid = 3_000_000;
    big.xattrs = vec![
        (b"security.capability".to_vec(), CAP_NET_BIND_SERVICE.to_vec()),
        (b"user.k".to_vec(), b"v".to_vec()),
    ];
    entries.push((big, Some(Vec::new())));
    for (n, data) in &entries {
        let mut r: Option<&[u8]> = data.as_deref();
        t.entry(n, r.as_mut().map(|r| r as &mut dyn std::io::Read)).unwrap();
    }
    let (tar, written) = t.finish(4096).unwrap();
    assert_eq!(written.entries, entries.len() as u64);
    assert_eq!(written.len as usize, tar.len());
    assert_eq!(tar.len() % 4096, 0);

    let dir = tempfile::tempdir().unwrap();
    let mut w = LayerWriter::new(Cursor::new(Vec::new()), dir.path(), Limits::default()).unwrap();
    w.append_tar(tar.as_slice()).unwrap();
    let (out, _) = w.finish(&BTreeMap::new()).unwrap();
    let mut img = Image::open(Cursor::new(out.into_inner())).unwrap();
    let a = img.lookup(b"app/a").unwrap().unwrap();
    assert_eq!(img.read_data(a).unwrap(), b"hello");
    assert_eq!(
        img.lookup(b"app/hard").unwrap(),
        Some(a),
        "a hard link shares the inode"
    );
    let gone = img.lookup(b"app/gone").unwrap().unwrap();
    assert!(img.inode(gone).unwrap().is_whiteout());
    let cache = img.lookup(b"cache").unwrap().unwrap();
    let xattrs: Vec<Vec<u8>> = img.xattrs(cache).unwrap().into_keys().map(|k| k.full_name()).collect();
    assert!(xattrs.iter().any(|k| k.ends_with(b"overlay.opaque")), "{xattrs:?}");
    let f = img.lookup(long_file.as_bytes()).unwrap().unwrap();
    assert_eq!(img.read_data(f).unwrap(), b"abc");
    let l = img.lookup(b"link").unwrap().unwrap();
    assert_eq!(img.readlink(l).unwrap(), long_target.as_bytes());
    let b = img.lookup(b"big").unwrap().unwrap();
    let info = img.inode(b).unwrap();
    assert_eq!((info.uid, info.gid), (5_000_000, 3_000_000));
    let bx: std::collections::BTreeSet<(Vec<u8>, Vec<u8>)> = img
        .xattrs(b)
        .unwrap()
        .into_iter()
        .map(|(k, v)| (k.full_name(), v))
        .collect();
    assert_eq!(
        bx,
        [
            (b"security.capability".to_vec(), CAP_NET_BIND_SERVICE.to_vec()),
            (b"user.k".to_vec(), b"v".to_vec())
        ]
        .into(),
        "file capabilities (setcap) and user attributes survive"
    );
}

/// `setcap cap_net_bind_service=+ep`: a `vfs_cap_data` revision 2 (magic and
/// effective flag, then permitted and inheritable, low and high words).
const CAP_NET_BIND_SERVICE: [u8; 20] = [1, 0, 0, 2, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
