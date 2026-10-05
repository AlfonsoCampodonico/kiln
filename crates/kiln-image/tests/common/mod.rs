//! Shared helpers: build OCI layouts and inspect results.
#![allow(dead_code)]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use flate2::write::GzEncoder;
use kiln_erofs::Image;
use kiln_erofs::testtar::{Opts, TarBuilder};
use kiln_image::{LocalRequest, RegistryRequest};
use kiln_oci::testlayout::{LayoutBuilder, TestLayer};
use kiln_oci::{ContainerConfig, Descriptor, ImageIndex, ImageManifest, Platform, media};
use kiln_registry::testregistry::{Config, TestRegistry};
use kiln_registry::{Client, DockerConfig, Reference};
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

pub fn amd() -> Platform {
    Platform::parse("linux/amd64").unwrap()
}

pub fn registry_req<'a>(platforms: &'a [Platform], tag: &'a str) -> RegistryRequest<'a> {
    RegistryRequest {
        platforms,
        tag: Some(tag),
    }
}

/// Serves a layout made by [`layout`] (tagged `app`).
pub fn serve(dir: &Path) -> TestRegistry {
    TestRegistry::serve_layout(dir, Config::default())
}

pub fn client(host: &str) -> Client {
    Client::new(host, DockerConfig::anonymous()).unwrap()
}

/// `<host>/<path>` as a reference.
pub fn reference(host: &str, path: &str) -> Reference {
    Reference::parse(&format!("{host}/{path}")).unwrap()
}

/// Seeds a registry from a layout with the client's low-level push: every blob,
/// child manifests by digest, then each tagged `index.json` entry by `tag`.
pub fn push_layout(c: &Client, dir: &Path, repo: &str, tag: &str) {
    let blob = |d: &Digest| dir.join("blobs/sha256").join(d.hex());
    fn push_tree(c: &Client, repo: &str, d: &Descriptor, blob: &dyn Fn(&Digest) -> PathBuf) {
        let bytes = fs::read(blob(&d.digest)).unwrap();
        if media::is_index(&d.media_type) {
            let idx: ImageIndex = serde_json::from_slice(&bytes).unwrap();
            for m in &idx.manifests {
                push_tree(c, repo, m, blob);
                let child = fs::read(blob(&m.digest)).unwrap();
                c.put_manifest(repo, &m.digest.to_string(), &m.media_type, &child)
                    .unwrap();
            }
        } else {
            let m: ImageManifest = serde_json::from_slice(&bytes).unwrap();
            for l in std::iter::once(&m.config).chain(&m.layers) {
                c.push_blob(repo, &l.digest, &blob(&l.digest)).unwrap();
            }
        }
    }
    let index: ImageIndex = serde_json::from_slice(&fs::read(dir.join("index.json")).unwrap()).unwrap();
    for d in &index.manifests {
        push_tree(c, repo, d, &blob);
        let bytes = fs::read(blob(&d.digest)).unwrap();
        c.put_manifest(repo, tag, &d.media_type, &bytes).unwrap();
    }
}
