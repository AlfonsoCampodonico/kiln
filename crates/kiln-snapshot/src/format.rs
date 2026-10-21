//! The kiln snapshot format, schema version 1 (design §5.2, normative in
//! `docs/format.md`), and the checks a reader runs on it (design §5.4).

use kiln_oci::Descriptor;
use kiln_store::{Digest, MAX_METADATA_BLOB};
use serde::{Deserialize, Serialize};

use crate::error::{Result, SnapshotError};

/// The manifest's `artifactType`.
pub const SNAPSHOT_ARTIFACT: &str = "application/vnd.kiln.snapshot.v1";
/// The config blob's media type.
pub const SNAPSHOT_CONFIG: &str = "application/vnd.kiln.snapshot.config.v1+json";
/// Every layer's media type: one distinct non-zero chunk.
pub const SNAPSHOT_CHUNK: &str = "application/vnd.kiln.snapshot.chunk.v1";
pub const SCHEMA_VERSION: u32 = 1;

/// The vmkit bundle's own description, carried in the config's `bundle`.
pub const BUNDLE_JSON: &str = "bundle.json";
/// The chunk index a lazy pull writes (design §6.2).
pub const LAZY_JSON: &str = "lazy.json";
/// The vmkit bundle format this version carries.
pub const BUNDLE_VERSION: u64 = 1;

/// 1 MiB: 256 pages, close to the kernel's readahead window (design §5.3).
pub const DEFAULT_CHUNK_SIZE: u64 = 1 << 20;

/// The bounds a reader enforces on untrusted snapshots (design §5.4, T3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    pub max_files: usize,
    pub max_file_size: u64,
    pub max_total_size: u64,
    pub min_chunk_size: u64,
    pub max_chunk_size: u64,
    /// The config blob's size.
    pub max_config: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_files: 64,
            max_file_size: 1 << 40,
            max_total_size: 4 << 40,
            min_chunk_size: 64 << 10,
            max_chunk_size: 16 << 20,
            max_config: MAX_METADATA_BLOB,
        }
    }
}

impl Limits {
    /// A chunk size must be a power of two within the bounds.
    pub fn check_chunk_size(&self, size: u64) -> Result<()> {
        if size.is_power_of_two() && (self.min_chunk_size..=self.max_chunk_size).contains(&size) {
            Ok(())
        } else {
            Err(SnapshotError::BadChunkSize {
                size,
                min: self.min_chunk_size,
                max: self.max_chunk_size,
            })
        }
    }
}

/// The snapshot config blob.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SnapshotConfig {
    pub schema_version: u32,
    pub chunk_size: u64,
    /// `bundle.json`, verbatim as a JSON value. vmkit checks it fully at restore.
    pub bundle: serde_json::Value,
    /// Every file of the bundle but `bundle.json`, sorted by name.
    pub files: Vec<ChunkedFile>,
}

/// One bundle file as a list of chunks. `chunks[i]` covers bytes
/// `[i·chunkSize, min((i+1)·chunkSize, size))`; `None` means all zero.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChunkedFile {
    pub name: String,
    pub size: u64,
    pub chunks: Vec<Option<Digest>>,
}

impl ChunkedFile {
    /// The number of bytes chunk `i` covers: `chunk_size`, except for a short last chunk.
    pub fn chunk_len(&self, i: usize, chunk_size: u64) -> u64 {
        let start = (i as u64).saturating_mul(chunk_size);
        self.size.saturating_sub(start).min(chunk_size)
    }
}

/// Whether `name` may be a bundle file: `[a-z0-9][a-z0-9._-]{0,63}`, and not one
/// of the names kiln writes itself. Such a name cannot leave its directory.
pub fn check_name(name: &str) -> Result<()> {
    let b = name.as_bytes();
    let ok = (1..=64).contains(&b.len())
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b[1..]
            .iter()
            .all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"._-".contains(&c))
        && name != BUNDLE_JSON
        && name != LAZY_JSON;
    if ok {
        Ok(())
    } else {
        Err(SnapshotError::BadName(name.to_string()))
    }
}

/// `bundle.version` must be [`BUNDLE_VERSION`]; vmkit checks the rest.
pub(crate) fn check_bundle(bundle: &serde_json::Value) -> Result<()> {
    match bundle.get("version") {
        Some(v) if v.as_u64() == Some(BUNDLE_VERSION) => Ok(()),
        Some(v) => Err(SnapshotError::UnknownBundleVersion(v.to_string())),
        None => Err(SnapshotError::UnknownBundleVersion("<none>".into())),
    }
}

impl SnapshotConfig {
    /// Checks the config against the manifest's `layers` and `limits` (design §5.4):
    /// versions, chunk size, names, sizes, chunk counts, and that `layers` are
    /// exactly the distinct chunks, each with the size it covers.
    pub fn check(&self, layers: &[Descriptor], limits: &Limits) -> Result<()> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(SnapshotError::UnknownSchema(self.schema_version.into()));
        }
        check_bundle(&self.bundle)?;
        limits.check_chunk_size(self.chunk_size)?;
        if self.files.len() > limits.max_files {
            return Err(SnapshotError::LimitExceeded {
                what: "files in a snapshot",
                max: limits.max_files as u64,
            });
        }
        // Layers: chunks only, each once.
        let mut sizes = std::collections::HashMap::with_capacity(layers.len());
        for l in layers {
            if l.media_type != SNAPSHOT_CHUNK {
                return Err(SnapshotError::BadLayerType {
                    digest: l.digest.clone(),
                    media_type: l.media_type.clone(),
                });
            }
            if sizes.insert(&l.digest, (l.size, false)).is_some() {
                return Err(SnapshotError::DuplicateLayer(l.digest.clone()));
            }
        }
        let mut names = std::collections::HashSet::with_capacity(self.files.len());
        let mut total = 0u64;
        for f in &self.files {
            check_name(&f.name)?;
            if !names.insert(f.name.as_str()) {
                return Err(SnapshotError::DuplicateName(f.name.clone()));
            }
            if f.size > limits.max_file_size {
                return Err(SnapshotError::LimitExceeded {
                    what: "snapshot file size",
                    max: limits.max_file_size,
                });
            }
            total = total.saturating_add(f.size);
            if total > limits.max_total_size {
                return Err(SnapshotError::LimitExceeded {
                    what: "snapshot total size",
                    max: limits.max_total_size,
                });
            }
            let want = f.size.div_ceil(self.chunk_size);
            if f.chunks.len() as u64 != want {
                return Err(SnapshotError::BadFile {
                    name: f.name.clone(),
                    reason: format!("{} chunks for {} bytes, expected {want}", f.chunks.len(), f.size),
                });
            }
            for (i, c) in f.chunks.iter().enumerate() {
                let Some(d) = c else { continue };
                let len = f.chunk_len(i, self.chunk_size);
                match sizes.get_mut(d) {
                    None => {
                        return Err(SnapshotError::BadFile {
                            name: f.name.clone(),
                            reason: format!("chunk {i} ({d}) is not in the manifest's layers"),
                        });
                    }
                    Some((size, _)) if *size != len => {
                        return Err(SnapshotError::BadFile {
                            name: f.name.clone(),
                            reason: format!("chunk {i} ({d}) covers {len} bytes, but its layer has {size}"),
                        });
                    }
                    Some((_, used)) => *used = true,
                }
            }
        }
        if let Some(l) = layers.iter().find(|l| !sizes[&l.digest].1) {
            return Err(SnapshotError::UnusedLayer(l.digest.clone()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const K: u64 = 64 << 10;

    fn d(s: &str) -> Digest {
        Digest::of(s.as_bytes())
    }

    fn chunk(s: &str, size: u64) -> Descriptor {
        Descriptor::new(SNAPSHOT_CHUNK, d(s), size)
    }

    /// `memory`: 2.5 chunks (a, zero, short b); `disk-0`: one chunk (a again).
    fn valid() -> (SnapshotConfig, Vec<Descriptor>) {
        let config = SnapshotConfig {
            schema_version: 1,
            chunk_size: K,
            bundle: json!({"version": 1, "vmm": {"name": "firecracker"}}),
            files: vec![
                ChunkedFile {
                    name: "disk-0".into(),
                    size: K,
                    chunks: vec![Some(d("a"))],
                },
                ChunkedFile {
                    name: "memory".into(),
                    size: 2 * K + 100,
                    chunks: vec![Some(d("a")), None, Some(d("b"))],
                },
            ],
        };
        (config, vec![chunk("a", K), chunk("b", 100)])
    }

    fn refused(config: &SnapshotConfig, layers: &[Descriptor]) -> SnapshotError {
        config.check(layers, &Limits::default()).unwrap_err()
    }

    #[test]
    fn a_valid_config_passes() {
        let (c, l) = valid();
        c.check(&l, &Limits::default()).unwrap();
    }

    #[test]
    fn config_serialises_canonically() {
        let (c, _) = valid();
        let s = String::from_utf8(kiln_oci::canonical_json(&c)).unwrap();
        assert!(
            s.starts_with(
                r#"{"bundle":{"version":1,"vmm":{"name":"firecracker"}},"chunkSize":65536,"files":[{"chunks":["sha256:"#
            ),
            "{s}"
        );
        assert!(s.contains(r#",null,"#), "zero chunks are null: {s}");
        assert!(s.ends_with(r#""schemaVersion":1}"#), "{s}");
        assert!(!s.contains(' ') && !s.contains('\n'));
        let back: SnapshotConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn unknown_fields_are_refused() {
        let (c, _) = valid();
        let mut v = serde_json::to_value(&c).unwrap();
        v["prefetch"] = json!([0]);
        assert!(serde_json::from_value::<SnapshotConfig>(v).is_err());
        let mut v = serde_json::to_value(&c).unwrap();
        v["files"][0]["layer"] = json!("sha256:00");
        assert!(serde_json::from_value::<SnapshotConfig>(v).is_err());
    }

    #[test]
    fn the_schema_version_must_be_1() {
        let (mut c, l) = valid();
        c.schema_version = 2;
        assert!(matches!(refused(&c, &l), SnapshotError::UnknownSchema(2)));
    }

    #[test]
    fn the_bundle_version_must_be_1() {
        let (mut c, l) = valid();
        for bundle in [json!({"version": 2}), json!({"version": "1"}), json!({}), json!([1])] {
            c.bundle = bundle;
            assert!(matches!(refused(&c, &l), SnapshotError::UnknownBundleVersion(_)));
        }
    }

    #[test]
    fn chunk_size_must_be_a_power_of_two_in_range() {
        let (mut c, l) = valid();
        for size in [0, 1, 32 << 10, 96 << 10, 32 << 20] {
            c.chunk_size = size;
            assert!(matches!(refused(&c, &l), SnapshotError::BadChunkSize { .. }), "{size}");
        }
        let limits = Limits::default();
        for size in [64 << 10, 1 << 20, 16 << 20] {
            limits.check_chunk_size(size).unwrap();
        }
    }

    #[test]
    fn names_cannot_escape_the_bundle() {
        for bad in [
            "",
            "..",
            ".",
            "../x",
            "a/b",
            "/etc",
            ".hidden",
            "Memory",
            "a b",
            "-x",
            "_x",
            "bundle.json",
            "lazy.json",
            &"a".repeat(65),
            "mem\u{0}",
            "mémoire",
        ] {
            assert!(matches!(check_name(bad), Err(SnapshotError::BadName(_))), "{bad:?}");
            let (mut c, l) = valid();
            c.files[0].name = bad.to_string();
            assert!(matches!(refused(&c, &l), SnapshotError::BadName(_)), "{bad:?}");
        }
        for good in [
            "memory",
            "disk-0",
            "vmstate",
            "config.json",
            "memory-ranges",
            "0",
            &"a".repeat(64),
        ] {
            check_name(good).unwrap();
        }
    }

    #[test]
    fn duplicate_names_are_refused() {
        let (mut c, l) = valid();
        c.files[1].name = "disk-0".into();
        assert!(matches!(refused(&c, &l), SnapshotError::DuplicateName(_)));
    }

    #[test]
    fn too_many_files_are_refused() {
        let (mut c, l) = valid();
        let limits = Limits {
            max_files: 1,
            ..Limits::default()
        };
        assert!(matches!(
            c.check(&l, &limits),
            Err(SnapshotError::LimitExceeded {
                what: "files in a snapshot",
                max: 1
            })
        ));
        c.files.extend((0..64).map(|i| ChunkedFile {
            name: format!("f{i}"),
            size: 0,
            chunks: vec![],
        }));
        assert!(matches!(refused(&c, &l), SnapshotError::LimitExceeded { max: 64, .. }));
    }

    #[test]
    fn file_and_total_sizes_are_bounded() {
        let (c, l) = valid();
        let limits = Limits {
            max_file_size: K,
            ..Limits::default()
        };
        assert!(matches!(
            c.check(&l, &limits),
            Err(SnapshotError::LimitExceeded {
                what: "snapshot file size",
                ..
            })
        ));
        let limits = Limits {
            max_total_size: 3 * K,
            ..Limits::default()
        };
        assert!(matches!(
            c.check(&l, &limits),
            Err(SnapshotError::LimitExceeded {
                what: "snapshot total size",
                ..
            })
        ));
        // Sizes that would overflow a sum are refused, not wrapped.
        let (mut c, l) = valid();
        c.files[0].size = u64::MAX;
        assert!(matches!(refused(&c, &l), SnapshotError::LimitExceeded { .. }));
    }

    #[test]
    fn the_chunk_count_must_match_the_size() {
        let (mut c, l) = valid();
        c.files[1].chunks.push(None);
        assert!(matches!(refused(&c, &l), SnapshotError::BadFile { .. }));
        let (mut c, l) = valid();
        c.files[1].chunks.pop();
        assert!(matches!(refused(&c, &l), SnapshotError::BadFile { .. }));
        // An empty file has no chunks.
        let (mut c, l) = valid();
        c.files.push(ChunkedFile {
            name: "empty".into(),
            size: 0,
            chunks: vec![],
        });
        c.check(&l, &Limits::default()).unwrap();
        c.files[2].chunks.push(None);
        assert!(matches!(refused(&c, &l), SnapshotError::BadFile { .. }));
    }

    #[test]
    fn a_chunk_missing_from_layers_is_refused() {
        let (mut c, l) = valid();
        c.files[0].chunks[0] = Some(d("elsewhere"));
        let e = refused(&c, &l);
        assert!(e.to_string().contains("not in the manifest's layers"), "{e}");
    }

    #[test]
    fn a_short_middle_chunk_is_refused() {
        // `b` (100 bytes) used in the middle of `memory`, where a chunk covers 64 KiB.
        let (mut c, l) = valid();
        c.files[1].chunks = vec![Some(d("b")), None, Some(d("b"))];
        c.files[0].chunks = vec![Some(d("a"))];
        let e = refused(&c, &l);
        assert!(
            e.to_string().contains("covers 65536 bytes, but its layer has 100"),
            "{e}"
        );
    }

    #[test]
    fn a_long_last_chunk_is_refused() {
        let (c, mut l) = valid();
        l[1].size = K;
        assert!(matches!(refused(&c, &l), SnapshotError::BadFile { .. }));
    }

    #[test]
    fn an_unused_layer_is_refused() {
        let (c, mut l) = valid();
        l.push(chunk("spare", 10));
        assert!(matches!(refused(&c, &l), SnapshotError::UnusedLayer(_)));
    }

    #[test]
    fn a_duplicate_layer_is_refused() {
        let (c, mut l) = valid();
        l.push(chunk("a", K));
        assert!(matches!(refused(&c, &l), SnapshotError::DuplicateLayer(_)));
    }

    #[test]
    fn a_layer_of_another_type_is_refused() {
        let (c, mut l) = valid();
        l[0].media_type = "application/vnd.oci.image.layer.v1.tar".into();
        assert!(matches!(refused(&c, &l), SnapshotError::BadLayerType { .. }));
    }
}
