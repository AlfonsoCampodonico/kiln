//! `resolve_inherited` reads each lower directory once, however many implicit
//! paths are resolved under it (M1a carry-forward: it re-read them per path).

use std::collections::BTreeMap;
use std::io::Cursor;
use std::time::Instant;

use kiln_erofs::testtar::{Opts, TarBuilder};
use kiln_erofs::{Image, LayerWriter, Limits, resolve_inherited};

fn image(tar: &[u8]) -> Image<Cursor<Vec<u8>>> {
    let spill = tempfile::tempdir().unwrap();
    let mut w = LayerWriter::new(Cursor::new(Vec::new()), spill.path(), Limits::default()).unwrap();
    w.append_tar(tar).unwrap();
    let (out, _) = w.finish(&BTreeMap::new()).unwrap();
    Image::open(out).unwrap()
}

#[test]
fn many_paths_under_a_large_lower_directory() {
    let mut lower = TarBuilder::new();
    lower.dir("big", &Opts::default().mode(0o755));
    for i in 0..20_000 {
        lower.dir(&format!("big/d{i}"), &Opts::default().mode(0o700).uid(i));
    }
    let mut lowers = vec![image(&lower.finish())];
    let paths: Vec<Vec<u8>> = (0..20_000)
        .step_by(10)
        .map(|i| format!("big/d{i}").into_bytes())
        .collect();

    // Compare against resolving one path, which reads `big` once: the old code
    // re-read it per path (~2000x), the cache makes the whole call a small multiple.
    // A ratio, not a wall-clock bound, so slow CI machines don't flake.
    let one = (0..3)
        .map(|_| {
            let start = Instant::now();
            resolve_inherited(&mut lowers, &paths[..1]).unwrap();
            start.elapsed()
        })
        .min()
        .unwrap();
    let start = Instant::now();
    let got = resolve_inherited(&mut lowers, &paths).unwrap();
    let all = start.elapsed();

    assert_eq!(got.len(), paths.len());
    assert_eq!(got[b"big/d10".as_slice()].meta.uid, 10);
    assert_eq!(got[b"big/d10".as_slice()].meta.mode, 0o700);
    let ratio = all.as_secs_f64() / one.as_secs_f64().max(1e-6);
    assert!(
        ratio < 100.0,
        "{} paths took {all:?}, one took {one:?} ({ratio:.0}x)",
        paths.len()
    );
}
