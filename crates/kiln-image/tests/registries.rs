//! Convert, push and pull against real registries (spec §11.7).
//!
//! `KILN_TEST_REGISTRIES=host:port,...` names local registries (CI runs
//! `registry:3` and zot); without it only the in-process registry is used.
mod common;

use common::*;
use kiln_erofs::testtar::{Opts, TarBuilder};
use kiln_image::{ConvertOptions, convert_registry, load, pull_image, push_image};
use kiln_store::Store;

/// Seeds `<host>/kiln-test/src:<run>` with a two-platform OCI image, converts it,
/// pushes the kiln image to `<host>/kiln-test/out:<run>` and pulls it into a
/// fresh store: the digest must survive, and a warm re-convert downloads nothing.
fn round_trip(host: &str) {
    let run = format!("r{}", std::process::id());
    let src = tempfile::tempdir().unwrap();
    let extra = TarBuilder::new()
        .file(
            "usr/local/bin/tool",
            b"#!/bin/sh\necho hi\n",
            &Opts::default().mode(0o755),
        )
        .finish();
    let path = layout(
        src.path(),
        &[arm(), amd()],
        &[gz(&base(0o1777)), zst(&top()), gz(&extra)],
    );
    let c = client(host);
    push_layout(&c, &path, "kiln-test/src", &run);

    let (h1, h2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a, b) = (Store::open(h1.path()).unwrap(), Store::open(h2.path()).unwrap());
    let opts = ConvertOptions::default();
    let src_ref = reference(host, &format!("kiln-test/src:{run}"));
    let platforms = [arm(), amd()];
    let out = convert_registry(&a, &c, &src_ref, &registry_req(&platforms, "img"), &opts).unwrap();
    assert_eq!(out.images.len(), 2);
    assert_eq!(out.layers_downloaded, 3, "{host}: each layer once");
    let warm = convert_registry(&a, &c, &src_ref, &registry_req(&platforms, "img"), &opts).unwrap();
    assert_eq!(
        (warm.digest.clone(), warm.layers_downloaded),
        (out.digest.clone(), 0),
        "{host}"
    );

    let out_ref = reference(host, &format!("kiln-test/out:{run}"));
    let pushed = push_image(&a, &c, "img", &out_ref).unwrap();
    assert_eq!(pushed.digest, out.digest, "{host}");
    let pulled = pull_image(&b, &c, &out_ref, "pulled").unwrap();
    assert_eq!(pulled.digest, out.digest, "{host}: digest survives push and pull");
    assert_eq!(b.get_ref("pulled").unwrap(), Some(out.digest.clone()));
    assert_eq!(cache_entries(&b), 0);
    let (la, lb) = (load(&a, &out.digest).unwrap(), load(&b, &out.digest).unwrap());
    let digests = |l: &kiln_image::Loaded| {
        l.entries
            .iter()
            .flat_map(|(_, m)| m.manifest.layers.iter().map(|d| d.digest.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(digests(&la), digests(&lb));
}

#[test]
fn round_trip_in_process() {
    let reg = kiln_registry::testregistry::TestRegistry::start(Default::default());
    round_trip(&reg.host());
}

#[test]
fn round_trip_real_registries() {
    let Ok(list) = std::env::var("KILN_TEST_REGISTRIES") else {
        eprintln!("skipped: set KILN_TEST_REGISTRIES=host:port,... to run");
        return;
    };
    for host in list.split(',').map(str::trim).filter(|h| !h.is_empty()) {
        round_trip(host);
    }
}
