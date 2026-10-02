//! Builds OCI image layouts and `docker save` archives for tests and fixtures.
//! Not for production use. Callers supply already-compressed layer blobs.

use std::fs;
use std::path::{Path, PathBuf};

use kiln_store::Digest;

use crate::media::{DOCKER_MANIFEST, OCI_CONFIG, OCI_INDEX, OCI_MANIFEST};
use crate::platform::Platform;
use crate::resolve::REF_NAME;
use crate::types::{ContainerConfig, Descriptor, ImageConfig, ImageIndex, ImageManifest, RootFs, canonical_json};

/// One layer: its media type, blob bytes (as stored) and uncompressed tar digest.
#[derive(Debug, Clone)]
pub struct TestLayer {
    pub media_type: String,
    pub blob: Vec<u8>,
    pub diff_id: Digest,
}

impl TestLayer {
    /// An uncompressed tar layer.
    pub fn tar(tar: Vec<u8>) -> Self {
        Self {
            media_type: crate::media::OCI_LAYER_TAR.into(),
            diff_id: Digest::of(&tar),
            blob: tar,
        }
    }
}

/// Writes an OCI image layout directory.
pub struct LayoutBuilder {
    dir: PathBuf,
    index: Vec<Descriptor>,
}

impl LayoutBuilder {
    pub fn new(dir: &Path) -> Self {
        fs::create_dir_all(dir.join("blobs/sha256")).unwrap();
        fs::write(dir.join("oci-layout"), br#"{"imageLayoutVersion":"1.0.0"}"#).unwrap();
        Self {
            dir: dir.to_path_buf(),
            index: Vec::new(),
        }
    }

    /// Writes a blob and returns its digest.
    pub fn blob(&self, bytes: &[u8]) -> Digest {
        let d = Digest::of(bytes);
        fs::write(self.dir.join("blobs/sha256").join(d.hex()), bytes).unwrap();
        d
    }

    /// Writes config and manifest for one platform; returns the manifest descriptor.
    pub fn image(&self, platform: &Platform, layers: &[TestLayer], config: ContainerConfig) -> Descriptor {
        let cfg = ImageConfig {
            architecture: platform.architecture.clone(),
            os: platform.os.clone(),
            variant: platform.variant.clone(),
            config: Some(config),
            rootfs: RootFs {
                fs_type: "layers".into(),
                diff_ids: layers.iter().map(|l| l.diff_id.clone()).collect(),
            },
        };
        let cfg_bytes = canonical_json(&cfg);
        let config_desc = Descriptor::new(OCI_CONFIG, self.blob(&cfg_bytes), cfg_bytes.len() as u64);
        let layer_descs = layers
            .iter()
            .map(|l| Descriptor::new(&l.media_type, self.blob(&l.blob), l.blob.len() as u64))
            .collect();
        let manifest = ImageManifest {
            schema_version: 2,
            media_type: Some(OCI_MANIFEST.into()),
            artifact_type: None,
            config: config_desc,
            layers: layer_descs,
            annotations: None,
        };
        let bytes = canonical_json(&manifest);
        let mut d = Descriptor::new(OCI_MANIFEST, self.blob(&bytes), bytes.len() as u64);
        d.platform = Some(platform.clone());
        d
    }

    /// Adds a top-level `index.json` entry, optionally named.
    pub fn add(&mut self, mut desc: Descriptor, ref_name: Option<&str>) -> &mut Self {
        if let Some(n) = ref_name {
            desc.annotations = Some([(REF_NAME.to_string(), n.to_string())].into());
        }
        self.index.push(desc);
        self
    }

    /// Writes a nested multi-platform index; returns its descriptor.
    pub fn multiarch(&self, manifests: Vec<Descriptor>) -> Descriptor {
        let idx = ImageIndex {
            schema_version: 2,
            media_type: Some(OCI_INDEX.into()),
            artifact_type: None,
            manifests,
            annotations: None,
        };
        let bytes = canonical_json(&idx);
        Descriptor::new(OCI_INDEX, self.blob(&bytes), bytes.len() as u64)
    }

    /// Writes `index.json`; returns the layout directory.
    pub fn finish(&self) -> PathBuf {
        let idx = ImageIndex {
            schema_version: 2,
            media_type: Some(OCI_INDEX.into()),
            artifact_type: None,
            manifests: self.index.clone(),
            annotations: None,
        };
        fs::write(self.dir.join("index.json"), canonical_json(&idx)).unwrap();
        self.dir.clone()
    }
}

/// Writes a legacy `docker save` archive (`manifest.json`, config file, `<id>/layer.tar`).
pub fn docker_legacy_archive(
    path: &Path,
    platform: &Platform,
    layer_tars: &[Vec<u8>],
    config: ContainerConfig,
    tag: &str,
) {
    let cfg = ImageConfig {
        architecture: platform.architecture.clone(),
        os: platform.os.clone(),
        variant: None,
        config: Some(config),
        rootfs: RootFs {
            fs_type: "layers".into(),
            diff_ids: layer_tars.iter().map(|t| Digest::of(t)).collect(),
        },
    };
    let cfg_bytes = canonical_json(&cfg);
    let cfg_name = format!("{}.json", Digest::of(&cfg_bytes).hex());
    let mut b = tar::Builder::new(Vec::new());
    let mut add = |name: &str, data: &[u8]| {
        let mut h = tar::Header::new_ustar();
        h.set_path(name).unwrap();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        b.append(&h, data).unwrap();
    };
    add(&cfg_name, &cfg_bytes);
    let mut layers = Vec::new();
    for (i, t) in layer_tars.iter().enumerate() {
        let name = format!("{i:064}/layer.tar");
        add(&name, t);
        layers.push(name);
    }
    let manifest = serde_json::json!([{ "Config": cfg_name, "RepoTags": [tag], "Layers": layers }]);
    add("manifest.json", &serde_json::to_vec(&manifest).unwrap());
    fs::write(path, b.into_inner().unwrap()).unwrap();
    let _ = DOCKER_MANIFEST;
}
