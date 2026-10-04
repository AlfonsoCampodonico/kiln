#!/usr/bin/env bash
# Builds what the kiln-init boot tests need and runs them (Linux with KVM).
# Usage: scripts/boot-tests.sh <vmkit kernel> [cargo test args...]
# Needs the VMMs on PATH (vmkit's scripts/install-vmms.sh), a static busybox
# (`busybox-static`), vmkit's sandbox helper allowed to create user namespaces
# (vmkit's scripts/install-apparmor.sh), and the musl target for the host arch.
# Set KILN_TEST_NET=1 to include networking (needs pasta and nft).
set -euo pipefail
kernel=$(realpath "$1"); shift
root=$(cd "$(dirname "$0")/.." && pwd)
target=${CARGO_TARGET_DIR:-$root/target}
musl=$(uname -m)-unknown-linux-musl
cd "$root"
cargo build --release --target "$musl" -p kiln-init -p kiln-testguest
vmkit_dir=$(cargo metadata --format-version 1 | python3 -c 'import json,sys; print(next(p["manifest_path"] for p in json.load(sys.stdin)["packages"] if p["name"] == "vmkit"))')
cargo build --locked --manifest-path "$vmkit_dir" --bin vmkit-sandbox --target-dir "$target"
export VMKIT_SANDBOX=$target/debug/vmkit-sandbox
export KILN_TEST_KERNEL=$kernel
export KILN_TEST_INIT=$target/$musl/release/kiln-init
export KILN_TEST_HOSTILE=$target/$musl/release/hostile
export KILN_TEST_BUSYBOX=${KILN_TEST_BUSYBOX:-$(command -v busybox)}
export KILN_REQUIRE_KVM_TESTS=1
cargo test -p kiln-init --test boot --test host_driver "$@"
