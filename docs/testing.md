# Testing

kiln's tests come in tiers. The default tier runs anywhere and needs no network; the others need Linux, root, KVM or a registry and are skipped unless their environment variables are set. CI (`.github/workflows/ci.yml`) runs all of them.

| Tier | Command | Needs | What it covers |
|---|---|---|---|
| Default | `cargo test --all` | Nothing (Linux or macOS) | erofs writer and reader (golden digests, round trips, model-based property tests), conversion from local layouts and `docker save` archives, hostile inputs, the store, the registry client against an in-process test registry, the control protocol, run preparation and the CLI |
| Lint | `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings` | Nothing | Formatting and lints |
| Kernel | `KILN_KERNEL_TESTS=1 cargo test -p kiln-erofs --test kernel`, then `KILN_KERNEL_TESTS=1 scripts/run-root-test.sh kernel` | Linux, `erofs-utils`, root for the mount cases | `fsck.erofs` accepts every image; the kernel's erofs mount matches kiln's reader; overlay over the layers equals the squash |
| Oracle | `KILN_ORACLE=<binary> scripts/run-root-test.sh oracle` | Linux, root, Go to build `tools/oracle` | The tree kiln's layers produce under overlayfs matches containerd's applier (`archive.Apply`) for the same tars |
| Registries | `KILN_TEST_REGISTRIES=localhost:5000,localhost:5001 cargo test -p kiln-image --test registries` | Real registries (CI uses `registry:3` and zot) | Convert, push and pull against real registry implementations |
| Pinned kernel | `KILN_TEST_NETWORK=1 cargo test -p kiln --test cli pinned_boot_layers_are_fetched_from_the_registry` | Internet | The pinned kernel is fetched from ghcr.io by digest |
| Docker Hub | `KILN_TEST_DOCKERHUB=1 cargo test --release -p kiln-image --test dockerhub` | Internet, optionally Docker Hub credentials | Converting real Docker Hub images (nightly in `.github/workflows/dockerhub.yml`) |
| Boot | `scripts/boot-tests.sh <kernel>` (below) | Linux with KVM, the VMMs, vmkit's sandbox helper | Real guests on both VMMs |

Two CI checks guard reproducibility:
- Golden digests in `crates/kiln-erofs/tests/golden/` are immutable: a pull request may add a new `digests-vN.txt` with a new `kiln_erofs::FORMAT_VERSION`, never change an existing one.
- `assets/make-ext4-template.sh` regenerates the scratch disk template byte for byte with Docker, and CI compares it with the committed `crates/kiln-image/assets/ext4-template.img.zst`.

## Building kiln-init

`kiln-init` is a static musl binary, for aarch64 and x86_64:

```bash
rustup target add aarch64-unknown-linux-musl x86_64-unknown-linux-musl
# rust-lld links the musl binaries without a C cross toolchain.
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld cargo build --release --target aarch64-unknown-linux-musl -p kiln-init
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld cargo build --release --target x86_64-unknown-linux-musl -p kiln-init
```

The binary is `target/<target>/release/kiln-init`; pass it to `kiln run --init PATH --allow-custom-init`.

## Boot tests

The boot tests (`crates/kiln-run/tests/boot.rs`, on the same session code `kiln run` uses, and `crates/kiln/tests/run.rs` for the CLI) boot real guests through vmkit on both VMMs: exit codes, stdio, `-t`, shutdown and signals, users, many layers, scratch growth, networking and protocol abuse, including a hostile PID 1 (`crates/kiln-testguest`). They need Linux with KVM and the tools vmkit's own contract suite needs, which come with the vmkit revision kiln pins:

```bash
vmkit=$(dirname "$(cargo metadata --format-version 1 | jq -r '.packages[] | select(.name == "vmkit") | .manifest_path')")
"$vmkit/scripts/install-vmms.sh"                  # Firecracker and Cloud Hypervisor, into ~/.local/bin
"$vmkit/kernels/build.sh" "$(uname -m)" out       # the guest kernel
cargo build --manifest-path "$vmkit/Cargo.toml" --bin vmkit-sandbox --target-dir target
"$vmkit/scripts/install-apparmor.sh" "$PWD/target/debug/vmkit-sandbox"   # Ubuntu 23.10+ only; uses sudo
sudo apt-get install busybox-static passt nftables jq python3 flex bison bc libelf-dev libssl-dev
export PATH="$HOME/.local/bin:$PATH"
KILN_TEST_NET=1 scripts/boot-tests.sh out/vmlinux-*-"$(uname -m)" -- --test-threads=4
```

`scripts/boot-tests.sh` builds `kiln-init` and the hostile test guest for the host's musl target and runs the suite; without its environment the tests are skipped.

- `KILN_TEST_NET=1` adds the networking cases (pasta and nft; the `kiln run` egress case also needs the internet).
- `KILN_TEST_KEEP=1` keeps each run directory with its console log (the `kiln run` cases' through `KILN_KEEP_RUN_DIR=1`; CI uploads their logs when the job fails).
- `KILN_TEST_VCPUS` sets the guests' vCPUs (default 1).
- Cargo arguments go after the kernel, for example `firecracker::` to run one VMM's boot cases; `KILN_TEST_VMM` (default `firecracker`) picks the VMM of the `kiln run` cases.

In CI the boot suite runs on x86_64 KVM. Hosted arm64 runners have no `/dev/kvm`, so aarch64 runs in the Lima VM ([running.md](running.md#on-macos-the-lima-vm)).

## Benchmarks

`kiln bench LAYOUT` measures cold, warm and changed-top-layer conversions of a local OCI layout and prints JSON.
