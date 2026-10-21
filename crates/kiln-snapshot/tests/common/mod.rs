//! Bundles and stores for the tests.
#![allow(dead_code)]

use std::path::Path;

use kiln_store::Store;

/// The smallest valid chunk size, so tests stay small.
pub const K: u64 = 64 << 10;

pub fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("home")).unwrap();
    (dir, s)
}

/// A bundle.json as vmkit writes it (only `version` matters to kiln).
pub fn bundle_json() -> serde_json::Value {
    serde_json::json!({
        "arch": "aarch64",
        "disks": [{"captured": true, "read_only": false, "size": 4 * K}],
        "files": [
            {"name": "vmstate", "role": "state"},
            {"name": "memory", "role": "memory"},
            {"name": "disk-0", "role": "disk"}
        ],
        "memory_mib": 256,
        "net": false,
        "vcpus": 1,
        "version": 1,
        "vmm": {"name": "firecracker", "version": "1.17.0"},
        "vsock": {"guest_cid": 3}
    })
}

/// Writes `bundle.json` and `files` into `dir`.
pub fn bundle(dir: &Path, files: &[(&str, Vec<u8>)]) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("bundle.json"), serde_json::to_vec(&bundle_json()).unwrap()).unwrap();
    for (name, bytes) in files {
        std::fs::write(dir.join(name), bytes).unwrap();
    }
}

/// `len` non-zero, non-repeating bytes that differ by `seed`.
pub fn pattern(len: u64, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| {
            let x = (i ^ (u64::from(seed) << 40)).wrapping_mul(0x9e37_79b9_7f4a_7c15);
            (x >> 56) as u8 | 1
        })
        .collect()
}

pub fn zeros(len: u64) -> Vec<u8> {
    vec![0; len as usize]
}
