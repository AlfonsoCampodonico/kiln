//! Media types kiln reads, and the layer policy of spec §6.1.

use crate::error::{OciError, Result};

pub const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
pub const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
pub const OCI_CONFIG: &str = "application/vnd.oci.image.config.v1+json";
pub const OCI_LAYER_TAR: &str = "application/vnd.oci.image.layer.v1.tar";
pub const OCI_LAYER_GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
pub const OCI_LAYER_ZSTD: &str = "application/vnd.oci.image.layer.v1.tar+zstd";
pub const DOCKER_MANIFEST_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
pub const DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";
pub const DOCKER_CONFIG: &str = "application/vnd.docker.container.image.v1+json";
pub const DOCKER_LAYER_GZIP: &str = "application/vnd.docker.image.rootfs.diff.tar.gzip";
pub const DOCKER_LAYER_TAR: &str = "application/vnd.docker.image.rootfs.diff.tar";

/// How a layer blob is compressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    None,
    Gzip,
    Zstd,
}

pub fn is_index(media_type: &str) -> bool {
    media_type == OCI_INDEX || media_type == DOCKER_MANIFEST_LIST
}

pub fn is_manifest(media_type: &str) -> bool {
    media_type == OCI_MANIFEST || media_type == DOCKER_MANIFEST
}

pub fn is_config(media_type: &str) -> bool {
    media_type == OCI_CONFIG || media_type == DOCKER_CONFIG
}

/// The compression of a supported layer; rejects foreign, non-distributable and
/// unknown layer types before anything is fetched.
pub fn layer_compression(media_type: &str) -> Result<Compression> {
    match media_type {
        OCI_LAYER_TAR | DOCKER_LAYER_TAR => Ok(Compression::None),
        OCI_LAYER_GZIP | DOCKER_LAYER_GZIP => Ok(Compression::Gzip),
        OCI_LAYER_ZSTD => Ok(Compression::Zstd),
        m if m.contains("foreign") || m.contains("nondistributable") => Err(OciError::ForeignLayer(m.to_string())),
        m => Err(OciError::UnsupportedMediaType(m.to_string())),
    }
}

/// Sniffs compression from a blob's first bytes (docker archives carry no media types).
pub fn sniff_compression(head: &[u8]) -> Compression {
    if head.starts_with(&[0x1f, 0x8b]) {
        Compression::Gzip
    } else if head.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        Compression::Zstd
    } else {
        Compression::None
    }
}

pub fn oci_layer_media_type(c: Compression) -> &'static str {
    match c {
        Compression::None => OCI_LAYER_TAR,
        Compression::Gzip => OCI_LAYER_GZIP,
        Compression::Zstd => OCI_LAYER_ZSTD,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layer_policy() {
        assert_eq!(layer_compression(OCI_LAYER_GZIP).unwrap(), Compression::Gzip);
        assert_eq!(layer_compression(DOCKER_LAYER_GZIP).unwrap(), Compression::Gzip);
        assert_eq!(layer_compression(OCI_LAYER_ZSTD).unwrap(), Compression::Zstd);
        assert_eq!(layer_compression(OCI_LAYER_TAR).unwrap(), Compression::None);
        for foreign in [
            "application/vnd.docker.image.rootfs.foreign.diff.tar.gzip",
            "application/vnd.oci.image.layer.nondistributable.v1.tar+gzip",
        ] {
            assert!(matches!(layer_compression(foreign), Err(OciError::ForeignLayer(_))));
        }
        assert!(matches!(
            layer_compression("text/plain"),
            Err(OciError::UnsupportedMediaType(_))
        ));
    }

    #[test]
    fn sniffing() {
        assert_eq!(sniff_compression(&[0x1f, 0x8b, 8, 0]), Compression::Gzip);
        assert_eq!(sniff_compression(&[0x28, 0xb5, 0x2f, 0xfd]), Compression::Zstd);
        assert_eq!(sniff_compression(b"usr/"), Compression::None);
    }
}
