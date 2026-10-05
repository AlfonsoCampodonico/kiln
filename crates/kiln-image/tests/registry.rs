//! Registry convert, pull and push against the in-process test registry.
mod common;

use common::*;
use kiln_image::types::{KILN_ARTIFACT, KILN_LAYER};
use kiln_image::{
    ConvertOptions, ImageError, convert_local, convert_registry, load, pull_image, push_image, resolve_name,
};
use kiln_oci::media::OCI_INDEX;
use kiln_oci::{ImageIndex, ImageManifest, OciError, canonical_json};
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
    assert!(
        matches!(err, ImageError::Oci(OciError::UnsupportedMediaType(_))),
        "{err}"
    );
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

/// The manifest tagged `app` in `reg`, re-published as `tag` after `edit`.
fn republish(reg: &kiln_registry::testregistry::TestRegistry, tag: &str, edit: impl FnOnce(&mut ImageManifest)) {
    let c = client(&reg.host());
    let mut m: ImageManifest = serde_json::from_slice(&c.get_manifest("x", "app").unwrap().bytes).unwrap();
    edit(&mut m);
    reg.put_manifest(tag, OCI_MANIFEST, &canonical_json(&m));
}

fn layer_requests(reg: &kiln_registry::testregistry::TestRegistry, d: &kiln_store::Digest) -> usize {
    reg.log().iter().filter(|l| l.path.ends_with(&d.to_string())).count()
}

#[test]
fn a_layer_declaring_more_than_the_limit_is_refused_before_any_request() {
    let src = tempfile::tempdir().unwrap();
    let path = layout(src.path(), &[arm()], &[gz(&base(0o755))]);
    let reg = serve(&path);
    let mut huge = None;
    republish(&reg, "big", |m| {
        m.layers[0].size = 1 << 40;
        huge = Some(m.layers[0].digest.clone());
    });
    let huge = huge.unwrap();
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    let err = convert_registry(
        &store,
        &client(&reg.host()),
        &reference(&reg.host(), "x:big"),
        &registry_req(&[arm()], "big"),
        &ConvertOptions::default(),
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            ImageError::LimitExceeded {
                what: "compressed layer size",
                ..
            }
        ),
        "{err}"
    );
    assert_eq!(layer_requests(&reg, &huge), 0, "the layer was never requested");
    assert_eq!(store.get_ref("big").unwrap(), None);
    assert_eq!(cache_entries(&store), 0);
}

#[test]
fn compressed_layers_are_bounded_over_the_whole_image() {
    let src = tempfile::tempdir().unwrap();
    let path = layout(src.path(), &[arm()], &[gz(&base(0o755)), gz(&top())]);
    let reg = serve(&path);
    let max_image: u64 = 2 << 20;
    let mut second = None;
    republish(&reg, "big", |m| {
        // Each layer fits the image limit alone; together they do not.
        m.layers[1].size = max_image - m.layers[0].size + 1;
        second = Some(m.layers[1].digest.clone());
    });
    let second = second.unwrap();
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    let opts = ConvertOptions {
        max_image_bytes: max_image,
        jobs: 1,
        ..Default::default()
    };
    let err = convert_registry(
        &store,
        &client(&reg.host()),
        &reference(&reg.host(), "x:big"),
        &registry_req(&[arm()], "big"),
        &opts,
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            ImageError::LimitExceeded {
                what: "compressed image size",
                ..
            }
        ),
        "{err}"
    );
    assert_eq!(layer_requests(&reg, &second), 0, "the second layer was never requested");
    assert_eq!(store.get_ref("big").unwrap(), None);
}

/// A two-platform kiln image pushed to a fresh registry, with the store it came from.
fn pushed_two_platform_image() -> (
    kiln_registry::testregistry::TestRegistry,
    Store,
    tempfile::TempDir,
    kiln_store::Digest,
) {
    let src = tempfile::tempdir().unwrap();
    let path = layout(src.path(), &[arm(), amd()], &[gz(&base(0o1777))]);
    let reg = kiln_registry::testregistry::TestRegistry::start(Default::default());
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    let out = convert_local(&store, &path, &req(&[arm(), amd()], "two"), &ConvertOptions::default()).unwrap();
    push_image(
        &store,
        &client(&reg.host()),
        "two",
        &reference(&reg.host(), "kiln/two:v1"),
    )
    .unwrap();
    (reg, store, home, out.digest)
}

#[test]
fn pull_validates_every_platform_before_fetching_any_layer() {
    let (reg, store, _home, digest) = pushed_two_platform_image();
    let mut index: ImageIndex = serde_json::from_slice(&store.read_metadata(&digest).unwrap()).unwrap();
    assert_eq!(index.manifests.len(), 2);
    // The second platform's layer claims to be an OCI tar layer.
    let mut m: ImageManifest =
        serde_json::from_slice(&store.read_metadata(&index.manifests[1].digest).unwrap()).unwrap();
    m.layers[0].media_type = kiln_oci::media::OCI_LAYER_GZIP.into();
    let bytes = canonical_json(&m);
    index.manifests[1].digest = reg.put_manifest("evil-child", OCI_MANIFEST, &bytes);
    index.manifests[1].size = bytes.len() as u64;
    reg.put_manifest("evil", OCI_INDEX, &canonical_json(&index));

    let fresh = tempfile::tempdir().unwrap();
    let fresh = Store::open(fresh.path()).unwrap();
    let before = reg.blob_requests("GET");
    let err = pull_image(
        &fresh,
        &client(&reg.host()),
        &reference(&reg.host(), "kiln/two:evil"),
        "evil",
    )
    .unwrap_err();
    assert!(
        matches!(err, ImageError::Oci(OciError::UnsupportedMediaType(_))),
        "{err}"
    );
    assert_eq!(
        reg.blob_requests("GET"),
        before,
        "no config or layer of any platform fetched"
    );
    assert_eq!(fresh.get_ref("evil").unwrap(), None);
}

#[test]
fn pull_refuses_a_declared_layer_over_the_limit_before_fetching_it() {
    let (reg, store, _home, digest) = pushed_two_platform_image();
    let mut index: ImageIndex = serde_json::from_slice(&store.read_metadata(&digest).unwrap()).unwrap();
    let mut m: ImageManifest =
        serde_json::from_slice(&store.read_metadata(&index.manifests[0].digest).unwrap()).unwrap();
    m.layers[0].size = 1 << 40;
    let bytes = canonical_json(&m);
    index.manifests[0].digest = reg.put_manifest("huge-child", OCI_MANIFEST, &bytes);
    index.manifests[0].size = bytes.len() as u64;
    reg.put_manifest("huge", OCI_INDEX, &canonical_json(&index));

    let fresh = tempfile::tempdir().unwrap();
    let fresh = Store::open(fresh.path()).unwrap();
    let before = reg.blob_requests("GET");
    let err = pull_image(
        &fresh,
        &client(&reg.host()),
        &reference(&reg.host(), "kiln/two:huge"),
        "huge",
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            ImageError::LimitExceeded {
                what: "compressed layer size",
                ..
            }
        ),
        "{err}"
    );
    assert_eq!(reg.blob_requests("GET"), before, "nothing fetched");
    assert_eq!(fresh.get_ref("huge").unwrap(), None);
}

#[test]
fn pull_refuses_an_empty_index() {
    let reg = kiln_registry::testregistry::TestRegistry::start(Default::default());
    let index = ImageIndex {
        schema_version: 2,
        media_type: Some(OCI_INDEX.into()),
        artifact_type: Some(KILN_ARTIFACT.into()),
        manifests: vec![],
        annotations: None,
    };
    reg.put_manifest("empty", OCI_INDEX, &canonical_json(&index));
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    let err = pull_image(
        &store,
        &client(&reg.host()),
        &reference(&reg.host(), "x:empty"),
        "empty",
    )
    .unwrap_err();
    assert!(matches!(err, ImageError::NotKilnRemote(_)), "{err}");
    assert_eq!(store.get_ref("empty").unwrap(), None);
}

#[test]
fn push_checks_every_blob_before_uploading_any() {
    let src = tempfile::tempdir().unwrap();
    let path = layout(src.path(), &[arm()], &[gz(&base(0o755)), gz(&top())]);
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    let out = convert_local(&store, &path, &req(&[arm()], "app"), &ConvertOptions::default()).unwrap();
    let m: ImageManifest = serde_json::from_slice(&store.read_metadata(&out.digest).unwrap()).unwrap();
    // The last blob to be uploaded goes missing, after earlier ones would have been sent.
    let last = m.layers.last().unwrap();
    std::fs::remove_file(store.blob_path(&last.digest)).unwrap();
    let reg = kiln_registry::testregistry::TestRegistry::start(Default::default());
    let err = push_image(
        &store,
        &client(&reg.host()),
        "app",
        &reference(&reg.host(), "kiln/app:v1"),
    )
    .unwrap_err();
    assert!(matches!(err, ImageError::Store(_)), "{err}");
    assert!(!reg.has_blob(&m.config.digest), "nothing was uploaded");
    assert!(
        reg.log().iter().all(|l| l.method == "GET" || l.method == "HEAD"),
        "no upload or manifest request: {:?}",
        reg.log()
    );
}

/// Requests for any of `digests` (manifest or blob, any method) after the first `from`.
fn requests_for(
    reg: &kiln_registry::testregistry::TestRegistry,
    from: usize,
    digests: &[&kiln_store::Digest],
) -> usize {
    reg.log()[from..]
        .iter()
        .filter(|l| digests.iter().any(|d| l.path.ends_with(&d.to_string())))
        .count()
}

#[test]
fn pull_refuses_an_index_of_more_than_eight_manifests_before_fetching_any() {
    let (reg, store, _home, digest) = pushed_two_platform_image();
    let mut index: ImageIndex = serde_json::from_slice(&store.read_metadata(&digest).unwrap()).unwrap();
    let children: Vec<_> = index.manifests.iter().map(|d| d.digest.clone()).collect();
    index.manifests = (0..9).map(|i| index.manifests[i % 2].clone()).collect();
    reg.put_manifest("many", OCI_INDEX, &canonical_json(&index));

    let fresh = tempfile::tempdir().unwrap();
    let fresh = Store::open(fresh.path()).unwrap();
    let before = reg.log().len();
    let err = pull_image(
        &fresh,
        &client(&reg.host()),
        &reference(&reg.host(), "kiln/two:many"),
        "many",
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            ImageError::LimitExceeded {
                what: "manifests in a kiln index",
                max: 8
            }
        ),
        "{err}"
    );
    assert_eq!(
        requests_for(&reg, before, &children.iter().collect::<Vec<_>>()),
        0,
        "no child manifest requested: {:?}",
        &reg.log()[before..]
    );
    assert_eq!(fresh.get_ref("many").unwrap(), None);
}

#[test]
fn pull_refuses_an_index_entry_that_is_not_a_manifest_before_fetching_any() {
    let (reg, store, _home, digest) = pushed_two_platform_image();
    let mut index: ImageIndex = serde_json::from_slice(&store.read_metadata(&digest).unwrap()).unwrap();
    let children: Vec<_> = index.manifests.iter().map(|d| d.digest.clone()).collect();
    // The first entry is a valid kiln manifest; the second claims to be an index.
    index.manifests[1].media_type = OCI_INDEX.into();
    reg.put_manifest("nested", OCI_INDEX, &canonical_json(&index));

    let fresh = tempfile::tempdir().unwrap();
    let fresh = Store::open(fresh.path()).unwrap();
    let before = reg.log().len();
    let err = pull_image(
        &fresh,
        &client(&reg.host()),
        &reference(&reg.host(), "kiln/two:nested"),
        "nested",
    )
    .unwrap_err();
    assert!(matches!(err, ImageError::NotKilnRemote(_)), "{err}");
    assert_eq!(
        requests_for(&reg, before, &children.iter().collect::<Vec<_>>()),
        0,
        "neither entry requested: {:?}",
        &reg.log()[before..]
    );
}

#[test]
fn a_warm_convert_by_tag_makes_one_manifest_head_and_no_get() {
    let src = tempfile::tempdir().unwrap();
    let path = layout(src.path(), &[arm(), amd()], &[gz(&base(0o1777))]);
    let reg = serve(&path);
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    let c = client(&reg.host());
    let r = reference(&reg.host(), "x:app");
    let opts = ConvertOptions::default();
    let platforms = [arm(), amd()];
    let cold = convert_registry(&store, &c, &r, &registry_req(&platforms, "a"), &opts).unwrap();
    // Cold: a HEAD for the tag, then the index by digest and both children.
    assert_eq!(reg.manifest_requests("HEAD"), 1);
    assert_eq!(reg.manifest_requests("GET"), 3);
    let index_get = reg
        .log()
        .into_iter()
        .find(|l| l.method == "GET" && l.path.contains("/manifests/"))
        .unwrap();
    assert!(
        index_get.path.contains("/manifests/sha256:"),
        "the index is fetched by the digest the HEAD reported, not by tag: {index_get:?}"
    );
    assert!(
        !reg.log()
            .iter()
            .any(|l| l.method == "GET" && l.path.ends_with("/manifests/app"))
    );

    let before = reg.log().len();
    let warm = convert_registry(&store, &c, &r, &registry_req(&platforms, "a"), &opts).unwrap();
    assert_eq!(warm.digest, cold.digest);
    let log = reg.log();
    let warm_log: Vec<_> = log[before..]
        .iter()
        .map(|l| (l.method.as_str(), l.path.as_str()))
        .collect();
    assert_eq!(
        warm_log,
        vec![("HEAD", "/v2/x/manifests/app")],
        "a warm convert is a single manifest HEAD"
    );
    assert_eq!(reg.manifest_requests("GET"), 3, "no manifest GET when warm");
}

#[test]
fn converting_a_kiln_image_says_to_pull_it() {
    let (reg, _store, _home, _digest) = pushed_two_platform_image();
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    let err = convert_registry(
        &store,
        &client(&reg.host()),
        &reference(&reg.host(), "kiln/two:v1"),
        &registry_req(&[arm()], "k"),
        &ConvertOptions::default(),
    )
    .unwrap_err();
    assert!(matches!(err, ImageError::KilnRemote(_)), "{err}");
    let msg = err.to_string();
    assert!(msg.contains("is a kiln image") && msg.contains("`kiln pull`"), "{msg}");
    assert_eq!(store.get_ref("k").unwrap(), None);
}

#[test]
fn a_layer_redirect_to_a_refused_destination_fails_the_convert() {
    let src = tempfile::tempdir().unwrap();
    let layer = gz(&base(0o755));
    let layer_digest = kiln_store::Digest::of(&layer.blob);
    let path = layout(src.path(), &[arm()], &[layer]);
    let reg = kiln_registry::testregistry::TestRegistry::serve_layout(
        &path,
        kiln_registry::testregistry::Config {
            redirect_blobs: Some("https://169.254.169.254/latest/?sig=SECRET&d=".into()),
            redirect_only: Some(layer_digest.clone()),
            ..Default::default()
        },
    );
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    let c = client(&reg.host());
    let m: ImageManifest = serde_json::from_slice(&c.get_manifest("x", "app").unwrap().bytes).unwrap();
    let err = convert_registry(
        &store,
        &c,
        &reference(&reg.host(), "x:app"),
        &registry_req(&[arm()], "a"),
        &ConvertOptions::default(),
    )
    .unwrap_err();
    assert!(
        matches!(err, ImageError::Registry(kiln_registry::RegistryError::Refused { .. })),
        "{err}"
    );
    assert!(!err.to_string().contains("SECRET"), "{err}");
    assert!(
        store.has_blob(&m.config.digest),
        "the config was fetched without a redirect"
    );
    assert!(!store.has_blob(&layer_digest));
    assert_eq!(
        layer_requests(&reg, &layer_digest),
        1,
        "one GET, answered with the redirect"
    );
    assert_eq!(store.get_ref("a").unwrap(), None);
}
