mod common;

use std::fs;

use common::*;
use kiln_image::{ConvertOptions, ImageError, convert_local, import_image, load};
use kiln_store::Store;

#[test]
fn imports_between_stores_verifying_blobs() {
    let src = tempfile::tempdir().unwrap();
    let (h1, h2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = Store::open(h1.path()).unwrap();
    let path = layout(src.path(), &[arm()], &[gz(&base(0o1777)), gz(&top())]);
    let out = convert_local(&a, &path, &req(&[arm()], "app:1"), &ConvertOptions::default()).unwrap();
    let ro = Store::open_read_only(h1.path()).unwrap();
    let b = Store::open(h2.path()).unwrap();
    let r = import_image(&b, &ro, "app:1", "app:1").unwrap();
    assert_eq!(r.digest, out.digest);
    assert_eq!(r.blobs_copied, 4, "manifest, config, two layers");
    assert_eq!(load(&b, &out.digest).unwrap().entries.len(), 1);
    assert_eq!(cache_entries(&b), 0, "imported layers never enter the caches");
    assert_eq!(import_image(&b, &ro, "app:1", "app:2").unwrap().blobs_copied, 0);

    // A tampered layer in the source store is refused and nothing is tagged.
    let c = Store::open(tempfile::tempdir().unwrap().keep()).unwrap();
    let layer = &out.images[0].layers[1].erofs;
    fs::write(a.blob_path(layer), b"tampered").unwrap();
    let err = import_image(&c, &ro, "app:1", "app:1").unwrap_err();
    assert!(
        matches!(err, ImageError::Store(kiln_store::StoreError::DigestMismatch { .. })),
        "{err}"
    );
    assert_eq!(c.get_ref("app:1").unwrap(), None);
}
