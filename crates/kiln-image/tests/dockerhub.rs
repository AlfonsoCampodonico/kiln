//! Real Docker Hub images (spec §11.7), run nightly: set `KILN_TEST_DOCKERHUB=1`.
//! Credentials come from the Docker config (`$DOCKER_CONFIG`), if any.

use kiln_image::{ConvertOptions, RegistryRequest, convert_registry, load};
use kiln_oci::Platform;
use kiln_registry::{Client, DockerConfig, Reference};
use kiln_store::Store;

fn enabled() -> bool {
    let on = std::env::var("KILN_TEST_DOCKERHUB").is_ok_and(|v| v == "1");
    if !on {
        eprintln!("skipped: set KILN_TEST_DOCKERHUB=1 to run");
    }
    on
}

/// Converts `reference` cold, then warm; the warm run must download no layer.
fn convert_twice(reference: &str, platforms: &[Platform]) {
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    let r = Reference::parse(reference).unwrap();
    let client = Client::new(r.registry(), DockerConfig::from_env()).unwrap();
    let req = RegistryRequest {
        platforms,
        tag: Some("hub"),
    };
    let opts = ConvertOptions::default();
    let cold = convert_registry(&store, &client, &r, &req, &opts).unwrap();
    assert!(cold.layers_downloaded > 0);
    let loaded = load(&store, &cold.digest).unwrap();
    assert_eq!(loaded.entries.len(), platforms.len());
    for (_, m) in &loaded.entries {
        assert_eq!(m.config.source.reference.as_deref(), Some(&*r.to_string()));
    }
    let warm = convert_registry(&store, &client, &r, &req, &opts).unwrap();
    assert_eq!(warm.digest, cold.digest, "{reference}");
    assert_eq!(
        warm.layers_downloaded, 0,
        "{reference}: a warm convert downloads no layer"
    );
    assert!(warm.images.iter().all(|c| c.layers.iter().all(|l| l.cached)));
}

#[test]
fn alpine_for_the_host_platform() {
    if enabled() {
        convert_twice("alpine:3.20", &[Platform::host()]);
    }
}

#[test]
fn php_cli_for_both_architectures() {
    if enabled() {
        let platforms = [
            Platform::parse("linux/arm64").unwrap(),
            Platform::parse("linux/amd64").unwrap(),
        ];
        convert_twice("php:8.4-cli", &platforms);
    }
}
