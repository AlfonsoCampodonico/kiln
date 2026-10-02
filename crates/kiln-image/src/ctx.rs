//! Cache keys for layers whose output depends on inherited parent attributes (§6.3).

use std::collections::BTreeMap;

use kiln_erofs::DirAttrs;
use kiln_store::Digest;
use serde_json::json;

use crate::types::{hex, unhex};

/// `parents <json array of hex paths>`: the layer-cache value for a layer with implicit dirs.
pub fn parents_entry(paths: &[Vec<u8>]) -> String {
    let hexes: Vec<String> = paths.iter().map(|p| hex(p)).collect();
    format!("parents {}", serde_json::to_string(&hexes).expect("strings serialize"))
}

/// Parses a `parents [...]` entry back to paths; `None` if malformed.
pub fn parse_parents(entry: &str) -> Option<Vec<Vec<u8>>> {
    let list: Vec<String> = serde_json::from_str(entry.strip_prefix("parents ")?).ok()?;
    list.iter().map(|h| unhex(h)).collect()
}

/// SHA-256 (hex) of the canonical encoding of each implicit path and what it inherited.
pub fn ctx_hash(paths: &[Vec<u8>], inherited: &BTreeMap<Vec<u8>, DirAttrs>) -> String {
    let entries: Vec<serde_json::Value> = paths
        .iter()
        .map(|p| match inherited.get(p) {
            None => json!([hex(p), null]),
            Some(a) => {
                let xattrs: Vec<serde_json::Value> = a
                    .xattrs
                    .iter()
                    .map(|(k, v)| json!([k.index, hex(&k.name), hex(v)]))
                    .collect();
                json!([
                    hex(p),
                    [
                        a.meta.mode,
                        a.meta.uid,
                        a.meta.gid,
                        a.meta.mtime.sec,
                        a.meta.mtime.nsec,
                        xattrs
                    ]
                ])
            }
        })
        .collect();
    Digest::of(&serde_json::to_vec(&entries).expect("json serializes"))
        .hex()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_erofs::{Meta, Timestamp, XattrKey, Xattrs};

    fn attrs(mode: u32) -> DirAttrs {
        let mut x = Xattrs::new();
        x.insert(
            XattrKey {
                index: 1,
                name: b"k".to_vec(),
            },
            b"v".to_vec(),
        );
        DirAttrs {
            meta: Meta {
                mode,
                uid: 0,
                gid: 0,
                mtime: Timestamp { sec: 5, nsec: 0 },
            },
            xattrs: x,
        }
    }

    #[test]
    fn parents_round_trip_with_non_utf8_paths() {
        let paths = vec![b"tmp".to_vec(), b"caf\xe9".to_vec()];
        assert_eq!(parse_parents(&parents_entry(&paths)).unwrap(), paths);
        assert!(parse_parents("erofs sha256:00").is_none());
    }

    #[test]
    fn ctx_changes_with_inherited_attributes_only() {
        let paths = vec![b"tmp".to_vec()];
        let a = BTreeMap::from([(b"tmp".to_vec(), attrs(0o1777))]);
        let b = BTreeMap::from([(b"tmp".to_vec(), attrs(0o755))]);
        assert_eq!(ctx_hash(&paths, &a), ctx_hash(&paths, &a.clone()));
        assert_ne!(ctx_hash(&paths, &a), ctx_hash(&paths, &b));
        assert_ne!(
            ctx_hash(&paths, &a),
            ctx_hash(&paths, &BTreeMap::new()),
            "absent differs from present"
        );
    }
}
