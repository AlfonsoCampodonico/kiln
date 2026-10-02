//! Shared helpers: build OCI layouts and inspect results.
#![allow(dead_code)]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use flate2::write::GzEncoder;
use kiln_erofs::Image;
use kiln_erofs::testtar::{Opts, TarBuilder};
use kiln_image::LocalRequest;
use kiln_oci::testlayout::{LayoutBuilder, TestLayer};
use kiln_oci::{ContainerConfig, Platform, media};
use kiln_store::{Digest, Store};

pub fn gz(tar: &[u8]) -> TestLayer {
    let mut e = GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(tar).unwrap();
    TestLayer {
        media_type: media::OCI_LAYER_GZIP.into(),
        blob: e.finish().unwrap(),
        diff_id: Digest::of(tar),
    }
}

pub fn zst(tar: &[u8]) -> TestLayer {
    TestLayer {
        media_type: media::OCI_LAYER_ZSTD.into(),
        blob: zstd::encode_all(tar, 3).unwrap(),
        diff_id: Digest::of(tar),
    }
}

pub fn arm() -> Platform {
    Platform::parse("linux/arm64").unwrap()
}

/// Base: `tmp/` 1777 owned by 0, `etc/os-release`. Top: `tmp/x` with no `tmp/` header.
pub fn base(tmp_mode: u32) -> Vec<u8> {
    TarBuilder::new()
        .dir("tmp", &Opts::default().mode(tmp_mode))
        .dir("etc", &Opts::default().mode(0o755))
        .file("etc/os-release", b"ID=test\n", &Opts::default())
        .finish()
}

pub fn top() -> Vec<u8> {
    TarBuilder::new().file("tmp/x", b"x", &Opts::default()).finish()
}

pub fn layout(dir: &Path, platforms: &[Platform], layers: &[TestLayer]) -> PathBuf {
    let mut b = LayoutBuilder::new(dir);
    let cfg = ContainerConfig {
        cmd: Some(vec!["php".into(), "-v".into()]),
        working_dir: Some("/app".into()),
        ..Default::default()
    };
    let descs: Vec<_> = platforms.iter().map(|p| b.image(p, layers, cfg.clone())).collect();
    let top = if let [one] = descs.as_slice() {
        one.clone()
    } else {
        b.multiarch(descs)
    };
    b.add(top, Some("app")).finish()
}

pub fn req<'a>(platforms: &'a [Platform], tag: &'a str) -> LocalRequest<'a> {
    LocalRequest {
        source_ref: None,
        platforms,
        tag: Some(tag),
    }
}

pub fn cache_entries(store: &Store) -> usize {
    ["cache/layers", "cache/layers-ctx", "cache/squash"]
        .iter()
        .map(|d| fs::read_dir(store.root().join(d)).unwrap().count())
        .sum()
}

pub fn mode_of(store: &Store, erofs: &Digest, path: &[u8]) -> u32 {
    let mut img = Image::open(store.open_blob(erofs).unwrap()).unwrap();
    let nid = img.lookup(path).unwrap().expect("path present");
    img.inode(nid).unwrap().mode & 0o7777
}
