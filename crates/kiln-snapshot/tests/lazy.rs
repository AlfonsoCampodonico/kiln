//! Lazy restore end to end (design §6.3): a vmkit test guest is snapshotted, pushed to the
//! in-process test registry, pulled lazily into an empty store, and restored through vmkit
//! with chunks fetched from the registry on demand. Linux with KVM only.
//!
//! Needs vmkit's test guest and the VMMs: VMKIT_TEST_KERNEL=<vmlinux or Image>
//! VMKIT_TEST_INITRAMFS=<vmkit's testguest initramfs> and VMKIT_SANDBOX (vmkit's
//! scripts/boot-tests.sh documents them). Skipped without them, unless
//! KILN_REQUIRE_KVM_TESTS=1 makes that a failure.
#![cfg(target_os = "linux")]

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use kiln_registry::testregistry::{Config, TestRegistry};
use kiln_registry::{Client, DockerConfig, Reference};
use kiln_snapshot::{Fetch, StoreSource, chunk_bundle, pull, push, tag, write_lazy};
use vmkit::{Backend, Disk, RestoreDisk, RestoreSpec, SnapshotBundle, VmSpec, VsockSpec};

fn env() -> Option<(PathBuf, PathBuf)> {
    let get = |k: &str| std::env::var_os(k).map(PathBuf::from);
    match (get("VMKIT_TEST_KERNEL"), get("VMKIT_TEST_INITRAMFS")) {
        (Some(k), Some(i)) => Some((k, i)),
        _ => {
            assert!(
                std::env::var_os("KILN_REQUIRE_KVM_TESTS").is_none_or(|v| v != "1"),
                "KILN_REQUIRE_KVM_TESTS is set but VMKIT_TEST_KERNEL/VMKIT_TEST_INITRAMFS are not"
            );
            None
        }
    }
}

fn counts(console: &str) -> Vec<u64> {
    console
        .lines()
        .filter_map(|l| l.split("VMKIT-COUNT ").nth(1))
        .filter_map(|r| r.split_whitespace().next()?.parse().ok())
        .collect()
}

fn await_counts(console: &std::path::Path, n: usize) -> Vec<u64> {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let text = std::fs::read_to_string(console).unwrap_or_default();
        let seen = counts(&text);
        if seen.len() >= n {
            return seen;
        }
        assert!(Instant::now() < deadline, "{} counts; console:\n{text}", seen.len());
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_snapshot_pulled_lazily_restores_from_the_registry() {
    let Some((kernel, initramfs)) = env() else { return };
    let vmm = Backend::Firecracker.discover().expect("firecracker");
    let exit = match vmm.capabilities().guest_exit {
        vmkit::GuestExit::Reboot => "vmkit.exit=reboot",
        vmkit::GuestExit::Poweroff => "vmkit.exit=poweroff",
    };
    let run = tempfile::tempdir().unwrap();
    let disk = run.path().join("disk.img");
    std::fs::File::create(&disk).unwrap().set_len(16 << 20).unwrap();
    let spec = VmSpec {
        kernel,
        initramfs: Some(initramfs),
        cmdline: ["panic=-1", "rdinit=/init", "vmkit.test=count", "vmkit.mark=kiln", exit]
            .map(String::from)
            .to_vec(),
        disks: vec![Disk {
            path: disk,
            read_only: false,
        }],
        vcpus: 1,
        memory_mib: 256,
        vsock: Some(VsockSpec { guest_cid: 3 }),
        net: None,
        console_log: run.path().join("console.log"),
        run_dir: run.path().to_path_buf(),
    };
    let mut vm = vmm.create(&spec).unwrap();
    vm.start().unwrap();
    await_counts(&spec.console_log, 5);
    let bundle_dir = run.path().join("bundle");
    vm.snapshot(&bundle_dir).unwrap();
    let at = *counts(&std::fs::read_to_string(&spec.console_log).unwrap())
        .last()
        .unwrap();
    vm.kill().unwrap();

    // Push from one store, pull lazily into another.
    let reg = TestRegistry::start(Config::default());
    let client = || Client::new(&reg.host(), DockerConfig::anonymous()).unwrap();
    let reference = Reference::parse(&format!("{}/team/vm:1", reg.host())).unwrap();
    let (_h, here) = common::store();
    let built = chunk_bundle(&here, &bundle_dir, 1 << 20).unwrap();
    tag(&here, "vm", &built.manifest_digest).unwrap();
    push(&here, &client(), "vm", &reference).unwrap();
    let (_t, there) = common::store();
    let pulled = pull(&there, &client(), &reference, "vm", Fetch::Lazy).unwrap();
    let lazy_dir = run.path().join("lazy");
    write_lazy(&pulled.loaded, &lazy_dir).unwrap();
    assert_eq!(
        there.list_blobs().unwrap().len(),
        2,
        "a lazy pull stores only the manifest and config"
    );

    let restored = tempfile::tempdir().unwrap();
    let source = StoreSource::new(there.clone(), Some((client(), reference.repository().to_string())));
    let rs = RestoreSpec {
        disks: vec![RestoreDisk::Captured],
        console_log: restored.path().join("console.log"),
        run_dir: restored.path().to_path_buf(),
        net: None,
        chunks: Some(Arc::new(source)),
        scratch_dir: None,
    };
    let mut vm = vmm
        .restore(&SnapshotBundle { dir: lazy_dir }, &rs)
        .expect("lazy restore");
    vm.resume().unwrap();
    let seen = await_counts(&rs.console_log, 5);
    assert!(seen[0] == at || seen[0] == at + 1, "{at}: {seen:?}");
    let console = std::fs::read_to_string(&rs.console_log).unwrap();
    assert!(console.contains("VMKIT-MARK-kiln"), "{console}");
    let chunks = pulled.loaded.manifest.layers.len();
    let stored = there.list_blobs().unwrap().len() - 2;
    eprintln!("{stored} of {chunks} chunks fetched");
    assert!(stored > 0 && stored <= chunks, "{stored} of {chunks}");
    vm.kill().unwrap();
}
