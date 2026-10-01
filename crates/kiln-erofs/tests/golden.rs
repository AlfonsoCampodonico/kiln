mod common;

use sha2::{Digest, Sha256};

fn hex(b: &[u8]) -> String {
    Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect()
}

fn golden_path() -> String {
    format!(
        "{}/tests/golden/digests-v{}.txt",
        env!("CARGO_MANIFEST_DIR"),
        kiln_erofs::FORMAT_VERSION
    )
}

#[test]
fn golden_digests() {
    let actual: String = common::fixtures()
        .iter()
        .map(|(name, tar)| format!("{name} {}\n", hex(&common::convert(tar).0)))
        .collect();
    if std::env::var_os("KILN_UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(format!("{}/tests/golden", env!("CARGO_MANIFEST_DIR"))).unwrap();
        std::fs::write(golden_path(), &actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(golden_path())
        .unwrap_or_else(|_| panic!("missing {}; create it with KILN_UPDATE_GOLDEN=1", golden_path()));
    assert_eq!(
        actual, expected,
        "erofs output changed: bump FORMAT_VERSION and add a new golden file instead of editing this one"
    );
}
