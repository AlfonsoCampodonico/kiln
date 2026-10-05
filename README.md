# kiln

kiln turns OCI container images into microVM images: one deterministic erofs filesystem per layer, stacked with overlayfs inside the guest. It runs natively on macOS and Linux, without root.

Status: milestone M3a. `kiln convert` works on registry images and local inputs, and `kiln pull`/`kiln push` move kiln images through registries. The guest side of running an image (`kiln-init`, the control protocol and the scratch disk) boots under Firecracker and Cloud Hypervisor in kiln's boot tests; `kiln run` comes next (M3b).

## Quickstart

```bash
cargo install --path crates/kiln

# From a registry (Docker Hub here): only layers that are not cached are downloaded.
kiln convert php:8.4-cli --platform linux/arm64 --platform linux/amd64
kiln inspect php:8.4-cli

# Or a local OCI layout or `docker save` archive (Docker 25+ writes OCI-in-tar;
# drop --platform if your Docker's `save` doesn't have it).
docker save --platform linux/arm64 php:8.4-cli -o php.tar
kiln convert --platform linux/arm64 --tag php:8.4-cli php.tar

kiln ls
kiln inspect php:8.4-cli
kiln gc
```

`convert` options:
- `--platform os/arch[/variant]`: repeatable; the default is the host's. Several platforms produce a multi-arch index.
- `--tag NAME`: the name in the store (default: the normalised reference, such as `docker.io/library/php:8.4-cli`, or `<file name>:latest` for a path).
- `--ref NAME`: which image to take when a local source holds several. References compare normalised, so `php:8.4-cli` matches `docker.io/library/php:8.4-cli`.
- `--max-layers N` (default 10): more app layers than this squashes the bottom ones into one.
- `--json`: a machine-readable summary.

An argument that names an existing path is a local source; anything else must be an image reference. An argument starting with `./`, `../`, `/` or `~`, or ending in `.tar`, `.tar.gz` or `.tgz`, is always taken as a local path, and a full reference such as `docker.io/library/php` forces the registry. A second convert of the same image is served from the layer cache and costs the registry one manifest HEAD, which Docker Hub does not count against its pull limit. Changing a base layer reconverts only the layers that inherit attributes from it.

## Registries

- `kiln push NAME REF` uploads a kiln image (blobs the registry already has are skipped) and `kiln pull REF [--tag NAME]` downloads one, verifying every blob. Digests survive the round trip. `pull` accepts only kiln images (an index of at most 8 platforms); use `convert` for OCI images.
- An anonymous request that a registry still refuses after handing out a token usually means the repository does not exist (Docker Hub answers so), or that it is private and needs `docker login`. An empty username in the Docker config or from a credential helper means no credentials, as in Docker.
- Credentials come from Docker's config (`$DOCKER_CONFIG/config.json`, else `~/.docker/config.json`): `auths`, `credsStore` and `credHelpers` (`docker-credential-*` on `PATH`), as `docker login` writes them. They are sent only to the registry's own origin and its token realm, never to redirect targets.
- Registries are spoken to over https. A registry on `localhost`, `127.0.0.0/8` or `::1` is always spoken to over plain http, as Docker does; a TLS registry reached through a localhost tunnel or port forward is therefore not supported.
- A tag is resolved with a manifest HEAD; manifests and configs already in the store are not downloaded again (they are content-addressed and were verified when stored), so a warm convert makes that single request. A cold one adds a GET of the manifest by the digest the HEAD reported.
- kiln contacts only the registry: descriptor `urls` are ignored, and redirects (at most 5), token realms and upload locations must be https and must not lead to loopback, link-local (cloud metadata), private, CGNAT, unspecified or reserved addresses (including site-local, 6to4 and local-use NAT64 IPv6 ranges). The exception is a registry with no public address at all: what it hands out may use the address classes it is in itself. Proxy settings are not used.
- Downloads are bounded by per-layer and per-image size limits.
- Commands that accept a name (`inspect`, `push`) also find an image by its normalised reference: `kiln inspect php:8.4-cli`.

## Other commands

- `kiln import --from-store DIR NAME [--as NAME]` copies an image from another store (for example the macOS store mounted read-only into a Lima VM), verifying every blob.
- `kiln bench LAYOUT` measures cold, warm and changed-top-layer conversions and prints JSON.
- `--store DIR` (or `$KILN_HOME`) selects the store; the default is `~/.local/share/kiln`.

## Docs
## The guest

`kiln-init` is PID 1 of a kiln microVM. It stacks the image's layers with overlayfs on a scratch disk, sets the guest up as Docker sets up a container (mounts, `/etc/hosts`, users, environment), runs the image's process and relays its stdio over vsock; `docs/format.md` specifies the guest and the control protocol. It is a static musl binary, for aarch64 and x86_64:

```bash
rustup target add aarch64-unknown-linux-musl x86_64-unknown-linux-musl
cargo build --release --target aarch64-unknown-linux-musl -p kiln-init
# rust-lld links musl binaries for the other architecture without a C cross toolchain.
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld cargo build --release --target x86_64-unknown-linux-musl -p kiln-init
```

The boot tests (`crates/kiln-init/tests/boot.rs`) boot real guests through vmkit on both VMMs: exit codes, stdio, `-t`, shutdown and signals, users, many layers, scratch growth, networking and protocol abuse. They need Linux with KVM and the tools vmkit's own contract suite needs, which come with the vmkit revision kiln pins:

```bash
vmkit=$(dirname "$(cargo metadata --format-version 1 | jq -r '.packages[] | select(.name == "vmkit") | .manifest_path')")
"$vmkit/scripts/install-vmms.sh"                  # Firecracker and Cloud Hypervisor, into ~/.local/bin
"$vmkit/kernels/build.sh" "$(uname -m)" out       # the guest kernel
cargo build --manifest-path "$vmkit/Cargo.toml" --bin vmkit-sandbox --target-dir target
"$vmkit/scripts/install-apparmor.sh" "$PWD/target/debug/vmkit-sandbox"   # Ubuntu 23.10+ only; uses sudo
sudo apt-get install busybox-static passt nftables
KILN_TEST_NET=1 scripts/boot-tests.sh out/vmlinux-*-"$(uname -m)" -- --test-threads=4
```

`scripts/boot-tests.sh` builds `kiln-init` and the hostile test guest for the host's musl target and runs the suite; without its environment the tests are skipped. `KILN_TEST_NET=1` adds the networking case (pasta and nft), `KILN_TEST_KEEP=1` keeps each run directory with its console log, and `KILN_TEST_VCPUS` sets the guests' vCPUs (default 1). Cargo arguments go after the kernel, for example `firecracker::` to run one VMM.

The scratch disk starts from an ext4 template embedded in `kiln-image`. `assets/make-ext4-template.sh` regenerates it with Docker, byte for byte; CI checks that it does.

Known limitations:
- Cloud Hypervisor 53 cannot grow the scratch disk online beyond about 8 GiB (the resize hangs), and under nested virtualization (Lima on Apple Silicon) growth is unreliable at any size. Firecracker grows it to 64 GiB and is the reference VMM. kiln's default disk is 4 GiB.
- Signals are 1 to 31. An image whose `STOPSIGNAL` is a real-time signal is refused.
- Under nested virtualization (Lima on Apple Silicon), Cloud Hypervisor 53 guests sometimes stall, more often in vsock-heavy cases (stdio streaming, protocol abuse); rerun the failed cases there. Firecracker does not.

## Docs

- `docs/format.md`: the normative image format, erofs profile, guest and control protocol.
- `docs/superpowers/specs/2026-09-30-kiln-design.md`: the design.

kiln prints image-supplied strings (command lines, environment, error paths) with control characters removed.

License: Apache-2.0.
