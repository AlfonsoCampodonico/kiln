//! `resolve_registry` against the in-process test registry.

use kiln_oci::testlayout::{LayoutBuilder, TestLayer};
use kiln_oci::{ContainerConfig, Descriptor, OciError, Platform, media, resolve_registry};
use kiln_registry::testregistry::{Config, TestRegistry};
use kiln_registry::{Client, DockerConfig, Reference, RegistryError};
use kiln_store::{Digest, Store, StoreError};

fn arm() -> Platform {
    Platform::parse("linux/arm64").unwrap()
}

fn amd() -> Platform {
    Platform::parse("linux/amd64").unwrap()
}

fn layer(name: &str) -> TestLayer {
    let mut b = tar::Builder::new(Vec::new());
    let mut h = tar::Header::new_ustar();
    h.set_path(name).unwrap();
    h.set_size(3);
    h.set_mode(0o644);
    h.set_cksum();
    b.append(&h, &b"abc"[..]).unwrap();
    TestLayer::tar(b.into_inner().unwrap())
}

fn cfg() -> ContainerConfig {
    ContainerConfig {
        cmd: Some(vec!["sh".into()]),
        ..Default::default()
    }
}

struct Fixture {
    _src: tempfile::TempDir,
    _home: tempfile::TempDir,
    reg: TestRegistry,
    store: Store,
    client: Client,
}

impl Fixture {
    /// Serves a layout whose top entry is tagged `v1`.
    fn new(build: impl FnOnce(&mut LayoutBuilder) -> Descriptor) -> Self {
        let src = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let mut b = LayoutBuilder::new(src.path());
        let top = build(&mut b);
        b.add(top, Some("v1")).finish();
        let reg = TestRegistry::serve_layout(src.path(), Config::default());
        let client = Client::new(&reg.host(), DockerConfig::anonymous()).unwrap();
        Self {
            store: Store::open(home.path()).unwrap(),
            _src: src,
            _home: home,
            reg,
            client,
        }
    }

    fn reference(&self, target: &str) -> Reference {
        Reference::parse(&format!("{}/team/app{target}", self.reg.host())).unwrap()
    }

    fn resolve(&self, platforms: &[Platform]) -> kiln_oci::Result<Vec<kiln_oci::ResolvedImage>> {
        resolve_registry(&self.store, &self.client, &self.reference(":v1"), platforms)
    }
}

#[test]
fn resolves_manifest_and_config_but_fetches_no_layers() {
    let mut layer_digest = None;
    let f = Fixture::new(|b| {
        let l = layer("a");
        layer_digest = Some(Digest::of(&l.blob));
        b.image(&arm(), &[l], cfg())
    });
    let imgs = f.resolve(&[arm()]).unwrap();
    let img = &imgs[0];
    assert_eq!(img.ref_name.as_deref(), Some(&*format!("{}/team/app:v1", f.reg.host())));
    assert!(f.store.has_blob(&img.manifest_digest));
    assert!(f.store.has_blob(&img.config_digest));
    assert!(
        !f.store.has_blob(&layer_digest.unwrap()),
        "layers are fetched by the convert"
    );
    assert_eq!(f.reg.blob_requests("GET"), 1, "only the config");

    // Warm: only the tag is asked for again.
    let before = f.reg.log().len();
    let again = f.resolve(&[arm()]).unwrap();
    assert_eq!(again[0].manifest_digest, img.manifest_digest);
    let after = f.reg.log();
    assert_eq!(after.len() - before, 1);
    assert!(after.last().unwrap().path.ends_with("/manifests/v1"));
}

#[test]
fn picks_platforms_from_an_index_and_by_digest() {
    let mut children = Vec::new();
    let f = Fixture::new(|b| {
        let ma = b.image(&arm(), &[layer("arm")], cfg());
        let mx = b.image(&amd(), &[layer("amd")], cfg());
        children = vec![ma.digest.clone(), mx.digest.clone()];
        b.multiarch(vec![ma, mx])
    });
    let imgs = f.resolve(&[amd(), arm()]).unwrap();
    assert_eq!(
        imgs.iter().map(|i| i.manifest_digest.clone()).collect::<Vec<_>>(),
        vec![children[1].clone(), children[0].clone()]
    );
    let err = f.resolve(&[Platform::parse("linux/riscv64").unwrap()]).unwrap_err();
    match err {
        OciError::MissingPlatform { available, .. } => assert_eq!(available, vec!["linux/arm64", "linux/amd64"]),
        e => panic!("{e}"),
    }
    let by_digest = f.reference(&format!("@{}", children[0]));
    let img = resolve_registry(&f.store, &f.client, &by_digest, &[arm()]).unwrap();
    assert_eq!(img[0].manifest_digest, children[0]);
}

#[test]
fn skips_attestations_in_an_index() {
    let mut image = None;
    let f = Fixture::new(|b| {
        let m = b.image(&arm(), &[layer("a")], cfg());
        image = Some(m.digest.clone());
        let att_bytes = br#"{"schemaVersion":2,"config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855","size":0},"layers":[]}"#;
        let mut att = Descriptor::new(media::OCI_MANIFEST, b.blob(att_bytes), att_bytes.len() as u64);
        att.platform = Some(Platform::parse("unknown/unknown").unwrap());
        att.annotations = Some(
            [(
                "vnd.docker.reference.type".to_string(),
                "attestation-manifest".to_string(),
            )]
            .into(),
        );
        b.multiarch(vec![m, att])
    });
    assert_eq!(f.resolve(&[arm()]).unwrap()[0].manifest_digest, image.unwrap());
}

#[test]
fn foreign_layers_are_rejected_before_the_config_is_fetched() {
    let f = Fixture::new(|b| {
        let mut l = layer("a");
        l.media_type = "application/vnd.docker.image.rootfs.foreign.diff.tar.gzip".into();
        b.image(&arm(), &[l], cfg())
    });
    assert!(matches!(f.resolve(&[arm()]), Err(OciError::ForeignLayer(_))));
    assert_eq!(f.reg.blob_requests("GET"), 0);
}

#[test]
fn an_index_entry_with_the_wrong_size_is_rejected() {
    let f = Fixture::new(|b| {
        let mut m = b.image(&arm(), &[layer("a")], cfg());
        m.size += 1;
        b.multiarch(vec![m])
    });
    assert!(matches!(
        f.resolve(&[arm()]),
        Err(OciError::Store(StoreError::SizeMismatch { .. }))
    ));
}

#[test]
fn schema1_is_refused() {
    let f = Fixture::new(|b| b.image(&arm(), &[layer("a")], cfg()));
    f.reg.put_manifest(
        "v1",
        "application/vnd.docker.distribution.manifest.v1+prettyjws",
        br#"{"schemaVersion":1,"fsLayers":[]}"#,
    );
    assert!(matches!(
        f.resolve(&[arm()]),
        Err(OciError::Registry(RegistryError::Schema1))
    ));
}

#[test]
fn a_config_for_another_platform_is_refused() {
    let f = Fixture::new(|b| {
        let mut m = b.image(&amd(), &[layer("a")], cfg());
        // The index claims arm64; the config says amd64.
        m.platform = Some(arm());
        b.multiarch(vec![m])
    });
    assert!(matches!(f.resolve(&[arm()]), Err(OciError::MissingPlatform { .. })));
}
