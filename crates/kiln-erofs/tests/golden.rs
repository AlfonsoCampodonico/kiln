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

fn squash_fixture() -> Vec<u8> {
    let l0 = {
        let mut b = kiln_erofs::testtar::TarBuilder::new();
        b.dir("etc", &kiln_erofs::testtar::Opts::default())
            .file("etc/a", b"a", &kiln_erofs::testtar::Opts::default())
            .file("etc/b", b"b", &kiln_erofs::testtar::Opts::default())
            .file("keep", b"keep", &kiln_erofs::testtar::Opts::default())
            .finish()
    };
    let l1 = {
        let mut b = kiln_erofs::testtar::TarBuilder::new();
        b.whiteout("etc/a")
            .file("etc/c", b"c", &kiln_erofs::testtar::Opts::default())
            .finish()
    };
    common::squash_all(&common::convert_stack(&[l0, l1]))
}

#[test]
fn golden_digests() {
    let mut actual: String = common::fixtures()
        .iter()
        .map(|(name, tar)| format!("{name} {}\n", hex(&common::convert(tar).0)))
        .collect();
    actual.push_str(&format!("squash {}\n", hex(&squash_fixture())));
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
