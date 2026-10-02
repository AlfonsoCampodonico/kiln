//! Reading kiln images back from the store (`inspect`, `ls`, `import`).

use kiln_oci::media::OCI_INDEX;
use kiln_oci::{ImageIndex, ImageManifest, Platform};
use kiln_store::{Digest, Store};

use crate::error::{ImageError, Result, json};
use crate::types::{KILN_ARTIFACT, KILN_CONFIG, KilnConfig, SCHEMA_VERSION};

/// One platform's kiln manifest and config.
#[derive(Debug, Clone)]
pub struct KilnManifest {
    pub digest: Digest,
    pub manifest: ImageManifest,
    pub config: KilnConfig,
}

/// A stored kiln image: one manifest, or an index of per-platform manifests.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub digest: Digest,
    pub is_index: bool,
    pub entries: Vec<(Platform, KilnManifest)>,
}

/// A ref name, or a digest of a blob in the store.
pub fn resolve_name(store: &Store, name: &str) -> Result<Digest> {
    if let Ok(d) = Digest::parse(name)
        && store.has_blob(&d)
    {
        return Ok(d);
    }
    store
        .get_ref(name)?
        .ok_or_else(|| ImageError::RefNotFound(name.to_string()))
}

pub fn load_manifest(store: &Store, digest: &Digest) -> Result<KilnManifest> {
    let manifest: ImageManifest = json("kiln manifest", &store.read_metadata(digest)?)?;
    if manifest.artifact_type.as_deref() != Some(KILN_ARTIFACT) || manifest.config.media_type != KILN_CONFIG {
        return Err(ImageError::NotAKilnImage(digest.clone()));
    }
    let config: KilnConfig = json("kiln config", &store.read_metadata(&manifest.config.digest)?)?;
    if config.schema_version != SCHEMA_VERSION {
        return Err(ImageError::UnknownSchema(config.schema_version));
    }
    Ok(KilnManifest {
        digest: digest.clone(),
        manifest,
        config,
    })
}

pub fn load(store: &Store, digest: &Digest) -> Result<Loaded> {
    let bytes = store.read_metadata(digest)?;
    let probe: serde_json::Value = json("kiln image", &bytes)?;
    if probe.get("mediaType").and_then(|m| m.as_str()) != Some(OCI_INDEX) {
        let m = load_manifest(store, digest)?;
        let platform = Platform {
            os: "linux".into(),
            architecture: m.config.architecture.clone(),
            variant: None,
        };
        return Ok(Loaded {
            digest: digest.clone(),
            is_index: false,
            entries: vec![(platform, m)],
        });
    }
    let index: ImageIndex = json("kiln index", &bytes)?;
    if index.artifact_type.as_deref() != Some(KILN_ARTIFACT) {
        return Err(ImageError::NotAKilnImage(digest.clone()));
    }
    let entries = index
        .manifests
        .iter()
        .map(|d| {
            let platform = d
                .platform
                .clone()
                .ok_or_else(|| ImageError::NotAKilnImage(digest.clone()))?;
            Ok((platform, load_manifest(store, &d.digest)?))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Loaded {
        digest: digest.clone(),
        is_index: true,
        entries,
    })
}
