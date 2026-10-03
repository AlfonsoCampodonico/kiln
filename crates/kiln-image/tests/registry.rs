//! Registry convert, pull and push against the in-process test registry.
mod common;

use common::*;
use kiln_image::types::KILN_LAYER;
use kiln_image::{
    ConvertOptions, ImageError, convert_local, convert_registry, load, pull_image, push_image, resolve_name,
};
use kiln_oci::canonical_json;
use kiln_registry::OCI_MANIFEST;
use kiln_store::Store;

fn erofs_layers(out: &kiln_image::Output) -> Vec<Vec<kiln_store::Digest>> {
    out.images
        .iter()
        .map(|c| c.layers.iter().map(|l| l.erofs.clone()).collect())
        .collect()
}

#[test]
fn converts_like_a_local_layout_and_records_the_reference() {
    let src = tempfile::tempdir().unwrap();
    let path = layout(src.path(), &[arm(), amd()], &[gz(&base(0o1777)), zst(&top())]);
    let reg = serve(&path);
    let r = reference(&reg.host(), "team/app:app");
    let (h1, h2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (remote, local) = (Store::open(h1.path()).unwrap(), Store::open(h2.path()).unwrap());
    let opts = ConvertOptions::default();
    let out = convert_registry(
        &remote,
        &client(&reg.host()),
        &r,
        &registry_req(&[arm(), amd()], "t"),
        &opts,
    )
    .unwrap();
    let want = convert_local(&local, &path, &req(&[arm(), amd()], "t"), &opts).unwrap();
    assert_eq!(
        erofs_layers(&out),
        erofs_layers(&want),
        "same app layers as a local convert"
    );
    assert_ne!(out.digest, want.digest, "the config records the reference");
    assert_eq!(out.layers_downloaded, 2, "the two layers, shared by both platforms");
    for (_, m) in load(&remote, &out.digest).unwrap().entries {
        assert_eq!(m.config.source.reference.as_deref(), Some(&*r.to_string()));
    }
    assert_eq!(remote.get_ref("t").unwrap(), Some(out.digest.clone()));
    assert_eq!(resolve_name(&remote, "t").unwrap(), out.digest);
}

#[test]
fn a_warm_convert_downloads_no_layers_even_with_inherited_parents() {
    let src = tempfile::tempdir().unwrap();
    let path = layout(src.path(), &[arm()], &[gz(&base(0o1777)), gz(&top())]);
    let reg = serve(&path);
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    let c = client(&reg.host());
    let r = reference(&reg.host(), "app:app");
    let opts = ConvertOptions::default();
    let cold = convert_registry(&store, &c, &r, &registry_req(&[arm()], "a"), &opts).unwrap();
    assert!(cold.images[0].layers[1].inherits);
    let blob_gets = reg.blob_requests("GET");
    let warm = convert_registry(&store, &c, &r, &registry_req(&[arm()], "a"), &opts).unwrap();
    assert_eq!(warm.digest, cold.digest);
    assert_eq!(warm.layers_downloaded, 0);
    assert_eq!(reg.blob_requests("GET"), blob_gets, "no blob is fetched at all");

    // GC frees the source blobs; a re-convert fetches the config again, but no layer.
    store.gc().unwrap();
    let after_gc = convert_registry(&store, &c, &r, &registry_req(&[arm()], "a"), &opts).unwrap();
    assert_eq!(after_gc.digest, cold.digest);
    assert_eq!(after_gc.layers_downloaded, 0);
    assert_eq!(reg.blob_requests("GET"), blob_gets + 1);
}

#[test]
fn a_changed_base_downloads_only_what_must_be_reconverted() {
    let (s1, s2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let p1 = layout(s1.path(), &[arm()], &[gz(&base(0o1777)), gz(&top())]);
    let p2 = layout(s2.path(), &[arm()], &[gz(&base(0o755)), gz(&top())]);
    let (r1, r2) = (serve(&p1), serve(&p2));
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    let opts = ConvertOptions::default();
    convert_registry(
        &store,
        &client(&r1.host()),
        &reference(&r1.host(), "x:app"),
        &registry_req(&[arm()], "a"),
        &opts,
    )
    .unwrap();
    store.gc().unwrap();
    let b = convert_registry(
        &store,
        &client(&r2.host()),
        &reference(&r2.host(), "x:app"),
        &registry_req(&[arm()], "b"),
        &opts,
    )
    .unwrap();
    // The new base converts; the shared top layer inherits from it, so it converts too.
    assert_eq!(b.layers_downloaded, 2);
    assert!(b.images[0].layers.iter().all(|l| !l.cached));
}

#[test]
fn push_then_pull_round_trips_digests_and_pulled_layers_skip_the_caches() {
    let src = tempfile::tempdir().unwrap();
    let path = layout(src.path(), &[arm(), amd()], &[gz(&base(0o1777)), gz(&top())]);
    let reg = serve(&path);
    let c = client(&reg.host());
    let (h1, h2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a, b) = (Store::open(h1.path()).unwrap(), Store::open(h2.path()).unwrap());
    let out = convert_local(&a, &path, &req(&[arm(), amd()], "app:1"), &ConvertOptions::default()).unwrap();
    let dest = reference(&reg.host(), "kiln/app:v1");
    let pushed = push_image(&a, &c, "app:1", &dest).unwrap();
    assert_eq!(pushed.digest, out.digest);
    let blobs = pushed.blobs;
    assert!(blobs >= 4, "two configs and at least two layers: {pushed:?}");
    let again = push_image(&a, &c, "app:1", &dest).unwrap();
    assert_eq!(
        (again.blobs, again.skipped),
        (0, blobs),
        "the second push uploads nothing"
    );

    let pulled = pull_image(&b, &c, &dest, "copy").unwrap();
    assert_eq!(pulled.digest, out.digest);
    assert_eq!(b.get_ref("copy").unwrap(), Some(out.digest.clone()));
    assert_eq!(cache_entries(&b), 0, "pulled layers never enter the caches");
    let loaded = load(&b, &out.digest).unwrap();
    assert_eq!(loaded.entries.len(), 2);
    for (_, m) in &loaded.entries {
        for l in &m.manifest.layers {
            assert!(b.has_blob(&l.digest));
        }
    }
    // By digest too, and a single-platform image.
    let one = convert_local(&a, &path, &req(&[arm()], "one"), &ConvertOptions::default()).unwrap();
    push_image(&a, &c, "one", &reference(&reg.host(), "kiln/one:v1")).unwrap();
    let by_digest = reference(&reg.host(), &format!("kiln/one@{}", one.digest));
    assert_eq!(pull_image(&b, &c, &by_digest, "one").unwrap().digest, one.digest);
}

#[test]
fn pull_refuses_images_that_are_not_kiln_images() {
    let src = tempfile::tempdir().unwrap();
    let path = layout(src.path(), &[arm()], &[gz(&base(0o755))]);
    let reg = serve(&path);
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    let err = pull_image(&store, &client(&reg.host()), &reference(&reg.host(), "x:app"), "x").unwrap_err();
    assert!(matches!(err, ImageError::NotKilnRemote(_)), "{err}");
    assert!(err.to_string().contains("kiln convert"), "{err}");
    assert_eq!(store.get_ref("x").unwrap(), None);
}

#[test]
fn pull_refuses_unexpected_layer_types_before_fetching_layers() {
    let src = tempfile::tempdir().unwrap();
    let path = layout(src.path(), &[arm()], &[gz(&base(0o755))]);
    let reg = serve(&path);
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    let out = convert_local(&store, &path, &req(&[arm()], "k"), &ConvertOptions::default()).unwrap();
    // A kiln manifest whose layer claims to be an OCI tar layer.
    let mut m: kiln_oci::ImageManifest = serde_json::from_slice(&store.read_metadata(&out.digest).unwrap()).unwrap();
    assert_eq!(m.layers[0].media_type, KILN_LAYER);
    m.layers[0].media_type = kiln_oci::media::OCI_LAYER_GZIP.into();
    reg.put_manifest("evil", OCI_MANIFEST, &canonical_json(&m));
    let c = client(&reg.host());
    let config = &m.config;
    c.push_blob("x", &config.digest, &store.blob_path(&config.digest))
        .unwrap();
    let fresh = tempfile::tempdir().unwrap();
    let fresh = Store::open(fresh.path()).unwrap();
    let before = reg.blob_requests("GET");
    let err = pull_image(&fresh, &c, &reference(&reg.host(), "x:evil"), "evil").unwrap_err();
    assert!(matches!(err, ImageError::Oci(_)), "{err}");
    assert_eq!(reg.blob_requests("GET"), before, "nothing fetched");
    assert_eq!(fresh.get_ref("evil").unwrap(), None);
}

#[test]
fn inspect_finds_registry_images_by_their_short_name() {
    let src = tempfile::tempdir().unwrap();
    let path = layout(src.path(), &[arm()], &[gz(&base(0o755))]);
    let reg = serve(&path);
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    let r = reference(&reg.host(), "app:app");
    let tag = r.to_string();
    let out = convert_registry(
        &store,
        &client(&reg.host()),
        &r,
        &registry_req(&[arm()], &tag),
        &ConvertOptions::default(),
    )
    .unwrap();
    // `<host>/app` normalises to `<host>/app:latest`, a different tag: not found.
    assert!(resolve_name(&store, &format!("{}/app", reg.host())).is_err());
    assert_eq!(
        resolve_name(&store, &format!("{}/app:app", reg.host())).unwrap(),
        out.digest
    );
    store.set_ref("docker.io/library/php:8.4-cli", &out.digest).unwrap();
    assert_eq!(resolve_name(&store, "php:8.4-cli").unwrap(), out.digest);
    assert_eq!(resolve_name(&store, "library/php:8.4-cli").unwrap(), out.digest);
}
