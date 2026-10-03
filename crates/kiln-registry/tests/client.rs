//! The client against the in-process test registry: verification, auth flows,
//! redirects (T2) and pushes.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use base64::Engine as _;
use kiln_registry::testregistry::{Bearer, Config, TestRegistry};
use kiln_registry::{Client, DockerConfig, OCI_INDEX, OCI_MANIFEST, RegistryError};
use kiln_store::{Digest, Store, StoreError};
use serde_json::json;

struct Image {
    dir: tempfile::TempDir,
    manifest: Digest,
    config: Digest,
    layer: Digest,
    layer_size: u64,
}

fn blob(dir: &Path, bytes: &[u8]) -> Digest {
    let d = Digest::of(bytes);
    fs::write(dir.join("blobs/sha256").join(d.hex()), bytes).unwrap();
    d
}

/// A one-layer OCI layout tagged `v1`.
fn image() -> Image {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("blobs/sha256")).unwrap();
    fs::write(dir.path().join("oci-layout"), br#"{"imageLayoutVersion":"1.0.0"}"#).unwrap();
    let layer_bytes = b"not really a tar, the client does not care".to_vec();
    let layer = blob(dir.path(), &layer_bytes);
    let config_bytes = serde_json::to_vec(&json!({"architecture": "arm64", "os": "linux"})).unwrap();
    let config = blob(dir.path(), &config_bytes);
    let manifest_bytes = serde_json::to_vec(&json!({
        "schemaVersion": 2,
        "mediaType": OCI_MANIFEST,
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": config.to_string(), "size": config_bytes.len()},
        "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar", "digest": layer.to_string(), "size": layer_bytes.len()}],
    }))
    .unwrap();
    let manifest = blob(dir.path(), &manifest_bytes);
    let index = json!({"schemaVersion": 2, "manifests": [{
        "mediaType": OCI_MANIFEST, "digest": manifest.to_string(), "size": manifest_bytes.len(),
        "annotations": {"org.opencontainers.image.ref.name": "v1"},
    }]});
    fs::write(dir.path().join("index.json"), serde_json::to_vec(&index).unwrap()).unwrap();
    Image {
        dir,
        manifest,
        config,
        layer,
        layer_size: layer_bytes.len() as u64,
    }
}

fn serve(img: &Image, config: Config) -> TestRegistry {
    TestRegistry::serve_layout(img.dir.path(), config)
}

fn client(reg: &TestRegistry, docker: DockerConfig) -> Client {
    Client::new(&reg.host(), docker).unwrap()
}

fn anonymous(reg: &TestRegistry) -> Client {
    client(reg, DockerConfig::anonymous())
}

fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path()).unwrap();
    (dir, s)
}

fn docker_config(json: serde_json::Value) -> (tempfile::TempDir, DockerConfig) {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("config.json"), serde_json::to_vec(&json).unwrap()).unwrap();
    let c = DockerConfig::from_dir(dir.path());
    (dir, c)
}

fn auth(user: &str, pass: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"))
}

#[test]
fn pulls_manifests_by_tag_and_digest_and_blobs_into_the_store() {
    let img = image();
    let reg = serve(&img, Config::default());
    let c = anonymous(&reg);
    let by_tag = c.get_manifest("any/repo", "v1").unwrap();
    assert_eq!(by_tag.digest, img.manifest);
    assert_eq!(by_tag.media_type, OCI_MANIFEST);
    assert_eq!(c.get_manifest("any/repo", &img.manifest.to_string()).unwrap(), by_tag);
    let head = c.head_manifest("any/repo", "v1").unwrap().unwrap();
    assert_eq!(head.digest, Some(img.manifest.clone()));
    assert_eq!(head.size, Some(by_tag.bytes.len() as u64));
    assert_eq!(c.head_manifest("any/repo", "nope").unwrap(), None);
    assert!(matches!(
        c.get_manifest("any/repo", "nope"),
        Err(RegistryError::NotFound { what: "manifest", .. })
    ));

    let (_d, s) = store();
    assert!(c.has_blob("any/repo", &img.layer).unwrap());
    assert!(!c.has_blob("any/repo", &Digest::of(b"absent")).unwrap());
    assert!(c.fetch_blob("any/repo", &img.layer, img.layer_size, &s).unwrap());
    assert!(s.has_blob(&img.layer));
    let gets = reg.blob_requests("GET");
    assert!(
        !c.fetch_blob("any/repo", &img.layer, img.layer_size, &s).unwrap(),
        "already stored"
    );
    assert_eq!(reg.blob_requests("GET"), gets, "a stored blob is not fetched again");
    assert!(matches!(
        c.fetch_blob("any/repo", &img.layer, img.layer_size + 1, &s),
        Err(RegistryError::Store(StoreError::SizeMismatch { .. }))
    ));
}

#[test]
fn wrong_bytes_for_a_digest_never_enter_the_store() {
    let img = image();
    let (_d, s) = store();
    let reg = serve(
        &img,
        Config {
            corrupt: Some(img.layer.clone()),
            ..Default::default()
        },
    );
    let err = anonymous(&reg)
        .fetch_blob("r", &img.layer, img.layer_size, &s)
        .unwrap_err();
    assert!(
        matches!(err, RegistryError::Store(StoreError::DigestMismatch { .. })),
        "{err}"
    );
    assert!(!s.has_blob(&img.layer));
    assert_eq!(fs::read_dir(s.tmp_dir()).unwrap().count(), 0);

    let reg = serve(
        &img,
        Config {
            corrupt: Some(img.manifest.clone()),
            ..Default::default()
        },
    );
    let c = anonymous(&reg);
    for target in ["v1".to_string(), img.manifest.to_string()] {
        let err = c.get_manifest("r", &target).unwrap_err();
        assert!(matches!(err, RegistryError::DigestMismatch { .. }), "{target}: {err}");
    }
}

#[test]
fn a_disagreeing_digest_header_is_an_error() {
    let img = image();
    let reg = serve(
        &img,
        Config {
            wrong_digest_header: true,
            ..Default::default()
        },
    );
    let err = anonymous(&reg).get_manifest("r", "v1").unwrap_err();
    assert!(matches!(err, RegistryError::DigestMismatch { .. }), "{err}");
}

#[test]
fn schema1_and_oversized_manifests_are_refused() {
    let img = image();
    let reg = serve(&img, Config::default());
    reg.put_manifest(
        "old",
        "application/vnd.docker.distribution.manifest.v1+prettyjws",
        br#"{"schemaVersion":1,"name":"x","tag":"old","fsLayers":[]}"#,
    );
    reg.put_manifest("big", OCI_INDEX, &vec![b' '; (4 << 20) + 1]);
    let c = anonymous(&reg);
    assert!(matches!(c.get_manifest("r", "old"), Err(RegistryError::Schema1)));
    assert!(matches!(
        c.get_manifest("r", "big"),
        Err(RegistryError::TooLarge { what: "manifest", .. })
    ));
}

#[test]
fn basic_auth_uses_docker_config_credentials() {
    let img = image();
    let reg = serve(
        &img,
        Config {
            basic: Some(("alice".into(), "s3cret".into())),
            ..Default::default()
        },
    );
    let err = anonymous(&reg).get_manifest("r", "v1").unwrap_err();
    assert!(matches!(err, RegistryError::Unauthorized { .. }), "{err}");
    let (_c, wrong) = docker_config(json!({"auths": {reg.host(): {"auth": auth("alice", "nope")}}}));
    assert!(matches!(
        client(&reg, wrong).get_manifest("r", "v1"),
        Err(RegistryError::Unauthorized { .. })
    ));
    let (_c, right) = docker_config(json!({"auths": {reg.host(): {"auth": auth("alice", "s3cret")}}}));
    let c = client(&reg, right);
    assert_eq!(c.get_manifest("r", "v1").unwrap().digest, img.manifest);
    let (_d, s) = store();
    c.fetch_blob("r", &img.layer, img.layer_size, &s).unwrap();
    let unauthenticated = reg.log().iter().filter(|l| l.authorization.is_none()).count();
    assert_eq!(
        unauthenticated, 3,
        "two refused clients, then one challenge before basic auth is used"
    );
}

#[test]
fn anonymous_bearer_tokens_are_fetched_once_and_reused() {
    let img = image();
    let reg = serve(
        &img,
        Config {
            bearer: Some(Bearer::default()),
            ..Default::default()
        },
    );
    let c = anonymous(&reg);
    let (_d, s) = store();
    c.get_manifest("r", "v1").unwrap();
    c.fetch_blob(
        "r",
        &img.config,
        fs::metadata(img.dir.path().join("blobs/sha256").join(img.config.hex()))
            .unwrap()
            .len(),
        &s,
    )
    .unwrap();
    c.fetch_blob("r", &img.layer, img.layer_size, &s).unwrap();
    assert_eq!(reg.requests("/token"), 1);
}

#[test]
fn bearer_tokens_with_basic_credentials_identity_tokens_and_helpers() {
    let img = image();
    let reg = serve(
        &img,
        Config {
            bearer: Some(Bearer {
                credentials: Some(("bob".into(), "pw".into())),
                refresh_token: Some("refresh-me".into()),
            }),
            ..Default::default()
        },
    );
    let err = anonymous(&reg).get_manifest("r", "v1").unwrap_err();
    assert!(matches!(err, RegistryError::Unauthorized { .. }), "{err}");

    let (_c, basic) = docker_config(json!({"auths": {reg.host(): {"auth": auth("bob", "pw")}}}));
    assert_eq!(
        client(&reg, basic).get_manifest("r", "v1").unwrap().digest,
        img.manifest
    );

    let (_c, identity) = docker_config(json!({"auths": {reg.host(): {"identitytoken": "refresh-me"}}}));
    assert_eq!(
        client(&reg, identity).get_manifest("r", "v1").unwrap().digest,
        img.manifest
    );
    assert!(reg.log().iter().any(|l| l.method == "POST" && l.path == "/token"));

    let bin = tempfile::tempdir().unwrap();
    let helper = bin.path().join("docker-credential-kilntest");
    fs::write(
        &helper,
        "#!/bin/sh\nread server\nprintf '{\"ServerURL\":\"%s\",\"Username\":\"bob\",\"Secret\":\"pw\"}' \"$server\"\n",
    )
    .unwrap();
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();
    let (_c, helped) = docker_config(json!({"credHelpers": {reg.host(): "kilntest"}}));
    let c = client(&reg, helped.with_helper_path(bin.path()));
    assert_eq!(c.get_manifest("r", "v1").unwrap().digest, img.manifest);
}

#[test]
fn redirects_are_followed_without_leaking_credentials_to_other_origins() {
    let img = image();
    let cdn = serve(&img, Config::default());
    let reg = serve(
        &img,
        Config {
            bearer: Some(Bearer::default()),
            redirect_blobs: Some(format!(
                "http://localhost:{}/redirected/",
                cdn.host().rsplit(':').next().unwrap()
            )),
            ..Default::default()
        },
    );
    let (_d, s) = store();
    anonymous(&reg).fetch_blob("r", &img.layer, img.layer_size, &s).unwrap();
    assert!(s.has_blob(&img.layer));
    let hops: Vec<_> = cdn
        .log()
        .into_iter()
        .filter(|l| l.path.starts_with("/redirected/"))
        .collect();
    assert_eq!(hops.len(), 1);
    assert_eq!(hops[0].authorization, None, "the bearer token stays with the registry");
}

#[test]
fn redirects_to_private_link_local_or_plain_http_destinations_are_refused() {
    let img = image();
    for target in [
        "https://169.254.169.254/latest/meta-data/?sig=SECRET&d=",
        "http://169.254.169.254/latest/meta-data/?sig=SECRET&d=",
        "https://10.0.0.1/blobs/?sig=SECRET&d=",
        "https://192.168.1.1/?sig=SECRET&d=",
        "https://[fd00:ec2::254]/?sig=SECRET&d=",
        "http://user:SECRET@localhost/?d=",
    ] {
        let reg = serve(
            &img,
            Config {
                redirect_blobs: Some(target.into()),
                ..Default::default()
            },
        );
        let (_d, s) = store();
        let err = anonymous(&reg)
            .fetch_blob("r", &img.layer, img.layer_size, &s)
            .unwrap_err();
        assert!(matches!(err, RegistryError::Refused { .. }), "{target}: {err}");
        assert!(!err.to_string().contains("SECRET"), "{err}");
        assert!(!s.has_blob(&img.layer));
    }
}

#[test]
fn redirect_loops_stop_after_five_hops() {
    let img = image();
    // A relative Location: every blob GET redirects to another blob path on the registry.
    let reg = serve(
        &img,
        Config {
            redirect_blobs: Some("/v2/loop/blobs/".into()),
            ..Default::default()
        },
    );
    let (_d, s) = store();
    let err = anonymous(&reg)
        .fetch_blob("r", &img.layer, img.layer_size, &s)
        .unwrap_err();
    assert!(matches!(err, RegistryError::TooManyRedirects { max: 5, .. }), "{err}");
    assert_eq!(reg.blob_requests("GET"), 6, "the first request and five redirects");
}

#[test]
fn errors_never_show_query_strings() {
    let img = image();
    // A closed port: the hop fails to connect.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let reg = serve(
        &img,
        Config {
            redirect_blobs: Some(format!("http://127.0.0.1:{port}/b?X-Amz-Signature=SECRET&d=")),
            ..Default::default()
        },
    );
    let (_d, s) = store();
    let err = anonymous(&reg)
        .fetch_blob("r", &img.layer, img.layer_size, &s)
        .unwrap_err();
    assert!(matches!(err, RegistryError::Http { .. }), "{err}");
    let mut chain = err.to_string();
    let mut src = std::error::Error::source(&err);
    while let Some(e) = src {
        chain.push_str(&e.to_string());
        src = e.source();
    }
    assert!(!chain.contains("SECRET") && !chain.contains("X-Amz"), "{chain}");
}

#[test]
fn pushes_blobs_once_and_manifests_by_tag_and_digest() {
    let img = image();
    let reg = TestRegistry::start(Config::default());
    let c = anonymous(&reg);
    let blobs = img.dir.path().join("blobs/sha256");
    for d in [&img.layer, &img.config] {
        assert!(c.push_blob("out", d, &blobs.join(d.hex())).unwrap());
        assert!(reg.has_blob(d));
    }
    assert!(
        !c.push_blob("out", &img.layer, &blobs.join(img.layer.hex())).unwrap(),
        "skipped"
    );
    assert_eq!(reg.log().iter().filter(|l| l.method == "POST").count(), 2);
    let bytes = fs::read(blobs.join(img.manifest.hex())).unwrap();
    assert_eq!(c.put_manifest("out", "v2", OCI_MANIFEST, &bytes).unwrap(), img.manifest);
    assert_eq!(
        c.put_manifest("out", &img.manifest.to_string(), OCI_MANIFEST, &bytes)
            .unwrap(),
        img.manifest
    );
    let got = c.get_manifest("out", "v2").unwrap();
    assert_eq!(
        (got.digest, got.media_type.as_str()),
        (img.manifest.clone(), OCI_MANIFEST)
    );
}

#[test]
fn pushes_authenticate_for_the_push_scope() {
    let img = image();
    let reg = TestRegistry::start(Config {
        bearer: Some(Bearer::default()),
        ..Default::default()
    });
    let c = anonymous(&reg);
    let path = img.dir.path().join("blobs/sha256").join(img.layer.hex());
    assert!(c.push_blob("out", &img.layer, &path).unwrap());
    assert!(reg.has_blob(&img.layer));
}

#[test]
fn a_token_realm_at_a_refused_destination_is_refused() {
    let img = image();
    let reg = serve(
        &img,
        Config {
            bearer: Some(Bearer::default()),
            bearer_realm: Some("https://169.254.169.254/token?sig=SECRET".into()),
            ..Default::default()
        },
    );
    let err = anonymous(&reg).get_manifest("r", "v1").unwrap_err();
    assert!(matches!(err, RegistryError::Refused { .. }), "{err}");
    assert!(!err.to_string().contains("SECRET"), "{err}");
    assert_eq!(reg.requests("/token"), 0);
}

#[test]
fn an_upload_location_at_a_refused_destination_is_refused() {
    let img = image();
    let path = img.dir.path().join("blobs/sha256").join(img.layer.hex());
    for location in [
        "https://169.254.169.254/v2/out/blobs/uploads/1?sig=SECRET",
        "http://example.com/v2/out/blobs/uploads/1?sig=SECRET",
        "https://user:SECRET@example.com/up",
    ] {
        let reg = TestRegistry::start(Config {
            upload_location: Some(location.into()),
            ..Default::default()
        });
        let err = anonymous(&reg).push_blob("out", &img.layer, &path).unwrap_err();
        assert!(matches!(err, RegistryError::Refused { .. }), "{location}: {err}");
        assert!(!err.to_string().contains("SECRET"), "{err}");
        assert!(!reg.has_blob(&img.layer));
        assert_eq!(reg.log().iter().filter(|l| l.method == "PUT").count(), 0);
    }
}

#[test]
fn a_content_type_that_disagrees_with_the_document_is_refused() {
    let img = image();
    let reg = serve(&img, Config::default());
    let manifest = serde_json::to_vec(&json!({"schemaVersion": 2, "mediaType": OCI_MANIFEST, "layers": []})).unwrap();
    reg.put_manifest("lying", OCI_INDEX, &manifest);
    let err = anonymous(&reg).get_manifest("r", "lying").unwrap_err();
    assert!(matches!(err, RegistryError::BadResponse { .. }), "{err}");
    // The same bytes served under their own type are fine.
    reg.put_manifest("honest", OCI_MANIFEST, &manifest);
    assert_eq!(
        anonymous(&reg).get_manifest("r", "honest").unwrap().media_type,
        OCI_MANIFEST
    );
}

#[test]
fn a_document_that_is_both_an_index_and_a_manifest_is_refused() {
    let img = image();
    let reg = serve(&img, Config::default());
    let both = serde_json::to_vec(&json!({"schemaVersion": 2, "manifests": [], "layers": []})).unwrap();
    reg.put_manifest("both", OCI_INDEX, &both);
    let err = anonymous(&reg).get_manifest("r", "both").unwrap_err();
    assert!(matches!(err, RegistryError::BadResponse { .. }), "{err}");
    assert!(err.to_string().contains("both manifests and layers"), "{err}");
}
