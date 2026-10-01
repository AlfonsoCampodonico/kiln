# kiln — Design Spec

**Date:** 2026-09-30
**Status:** Draft, awaiting review
**Scope:** `kiln` (OCI image / Dockerfile → bootable microVM image builder) and the v1 scope of `vmkit` (shared VMM crate)
**Related:** `../../../../ROADMAP.md` (projects #1 snapshot registry and #2 agent sandbox)

---

## 1. Goals

`kiln` turns an OCI image (or a Dockerfile, via BuildKit) into a bootable microVM image that runs on Firecracker and Cloud Hypervisor, on aarch64 and x86_64, with per-layer caching fast enough that an unchanged rebuild is near-instant.

It is intended to be a serious open-source tool (stable format, real docs, polished CLI), built in a way that exercises the internals (native erofs writer, custom init) rather than wrapping existing tools.

### Success criteria

1. `kiln convert php:8.4-cli` produces an image that boots and runs `php -v` on both VMMs and both architectures.
2. `kiln build -f Dockerfile .` produces the same kind of image via BuildKit.
3. Performance targets (measured by `kiln bench`, see §9):
   - Warm, unchanged image: **< 200 ms**
   - One changed top layer (~50 MB): **< 2 s**
   - Cold `php:8.4-cli`, excluding network download: **< 5 s**
4. Builds are deterministic: identical inputs produce byte-identical erofs blobs and manifests on any host.
5. `kiln` builds images natively on macOS (no root, no Linux VM). Running requires Linux + KVM.
6. `docs/format.md` documents the image format well enough for a third party to produce or consume `kiln` images.

### Non-goals (v1)

- A native Dockerfile engine (BuildKit does this).
- Per-workload kernel tuning or local kernel builds.
- erofs compression (LZ4 is the planned follow-up).
- Auto-delegating `kiln run` to a Lima VM on macOS.
- Snapshots, forking, lazy restore (owned by projects #2 and #1).
- Windows hosts, GPUs, non-Linux guests.

---

## 2. Decisions (with rationale)

| Decision | Choice | Why |
|---|---|---|
| Language | Rust, all components | Matches Firecracker/Cloud Hypervisor/rust-vmm; no-GC suits future UFFD work; one toolchain across all three projects. |
| VMMs | Firecracker **and** Cloud Hypervisor from day one, behind `vmkit` | Real portability; abstraction is designed against two concrete backends rather than retrofitted. |
| Architectures | aarch64 and x86_64 from day one | aarch64 is native on the dev machine (M4 Pro); x86_64 is the common production target. Retrofitting kernel configs is painful. |
| Input | OCI images are the core; Dockerfiles go through `docker buildx` / `buildctl` | Effort goes into conversion, not a BuildKit clone. |
| Rootfs format | One read-only erofs per OCI layer, overlayfs in the guest, writable ext4 scratch | True per-layer caching; layers dedupe across images; erofs is compact and fast to mount. |
| erofs writer | Native Rust, streams tar → erofs without extraction | Runs on macOS (APFS cannot faithfully hold Linux ownership, device nodes, or case-distinct names); fastest path; core learning piece. |
| Kernel | Prebuilt, versioned kernel profiles built in CI, fetched by digest | Kernel builds take minutes; prebuilt profiles keep builds in seconds and behaviour consistent across VMMs. |
| Running | Minimal `kiln run` on top of a shared `vmkit` crate | Images must be bootable to be testable; VMM drivers are written once and reused by #1 and #2. |
| License | Apache-2.0 | Matches the Firecracker / Cloud Hypervisor ecosystem. |

---

## 3. Repositories and components

Two repositories. `vmkit` is the only code shared between the microVM projects; all other coupling is through documented file formats.

### 3.1 `vmkit` (separate repo, single crate)

VMM-neutral VM lifecycle.

- **`Vmm` trait**: `create(VmSpec) -> Vm`, and on `Vm`: `start`, `pause`, `resume`, `shutdown`, `wait -> GuestExit`, `snapshot(dest)`, `restore(src)`, `capabilities() -> Capabilities`.
- **`VmSpec`**: kernel path, base cmdline, ordered block devices (path, read-only flag), network interfaces (tap name, MAC), vCPUs, memory MiB, optional vsock (CID, UDS path), serial console output sink.
- **Drivers**: `firecracker` and `cloud-hypervisor`. Each spawns the VMM binary, drives its REST API over a Unix socket, and translates `VmSpec` to the backend's config.
- **Backend-specific details hidden by drivers**:
  - Console device and kernel cmdline additions (e.g. `ttyS0` vs `ttyAMA0` on Cloud Hypervisor aarch64; `reboot=k`, `pci=off` where required on Firecracker x86_64).
  - Configuring the VMM to exit on guest reboot, and normalising "guest exited" into `GuestExit`.
- **`Capabilities`**: includes `max_block_devices`, `supports_diff_snapshot`, `supports_balloon`, `supports_vsock`. Callers check capabilities instead of matching on backend type.
- **`net` module**: create/destroy tap devices, allocate a /30 from a configurable pool (default `172.30.0.0/16`), install/remove nftables NAT rules. Requires `CAP_NET_ADMIN`.
- **Binary discovery**: `$VMKIT_FIRECRACKER`, `$VMKIT_CLOUD_HYPERVISOR`, else `PATH`. Minimum supported versions are pinned in the crate and checked at `create`.

`snapshot` / `restore` are in the v1 trait (and covered by the contract suite, §9.2) because #2 depends on them, even though `kiln` does not call them.

### 3.2 `kiln` (this repo, Cargo workspace)

| Crate | Responsibility | Depends on |
|---|---|---|
| `kiln-oci` | Resolve refs; pull manifests and blobs per platform (`linux/arm64`, `linux/amd64`); read OCI image layouts and Docker archive tarballs; registry auth via Docker config. | `oci-client`, `kiln-store` |
| `kiln-store` | Content-addressed blob store, layer cache index, refs, atomic writes, GC. | — |
| `kiln-erofs` | Tar stream → erofs image. Pure: `Read` in, `Write + Seek` out. Whiteout translation. Deterministic. | — |
| `kiln-init` | Static musl PID 1 for the guest. | — (guest-only) |
| `kiln-kernel` | Resolve kernel profile `name@version` per arch; fetch; verify against pinned digests. | `kiln-store` |
| `kiln-image` | `kiln` image manifest/config types; assembly; OCI artifact export and push. | `kiln-oci`, `kiln-erofs`, `kiln-kernel`, `kiln-store` |
| `kiln` (bin) | CLI: `build`, `convert`, `pull`, `run`, `inspect`, `ls`, `push`, `gc`, `bench`. | all of the above, `vmkit` |

Additional repo contents:

- `kernels/` — per-arch config fragments, pinned kernel version, CI build script.
- `lima/kiln.yaml` — Lima template for macOS dev: nested virtualization, Firecracker and Cloud Hypervisor installed, `$KILN_HOME` mounted.
- `xtask/` — `cargo xtask build-init` cross-compiles `kiln-init` for both Linux targets (via `cargo-zigbuild`) so it can be embedded in the `kiln` binary.
- `docs/format.md`, `docs/architecture.md`.

---

## 4. Image format

A `kiln` image is an OCI artifact and can be pushed to any OCI-compliant registry.

### 4.1 Structure

- **Multi-arch**: an OCI image index with one entry per platform, each pointing to a `kiln` manifest.
- **Manifest**: OCI image manifest with `artifactType: application/vnd.kiln.image.v1`.
- **Config blob** — `application/vnd.kiln.image.config.v1+json`:
  - `schemaVersion` (integer, starts at 1)
  - `architecture` (`arm64` | `amd64`)
  - `process`: `entrypoint`, `cmd`, `env`, `workingDir`, `user` (copied from the source OCI config)
  - `kernel`: `{ profile, version, digest }`
  - `cmdline`: default kernel cmdline fragment (backend-specific parts are added by `vmkit` at run time)
  - `source`: `{ reference, manifestDigest }` of the input OCI image
  - `builder`: `{ kilnVersion, erofsFormatVersion }`
- **Layers, in boot order**:
  1. Kernel — `application/vnd.kiln.kernel.v1`
  2. Init layer — `application/vnd.kiln.init.v1.erofs`
  3. App layers, lowest first — `application/vnd.kiln.layer.v1.erofs`, each annotated with `dev.kiln.source.digests` (the OCI layer digest(s) it was built from; more than one if squashed).

`docs/format.md` is the normative version of this section. Breaking changes bump `schemaVersion`; readers reject unknown major versions.

### 4.2 Local store

Root: `$KILN_HOME`, default `~/.local/share/kiln` on all platforms.

```
$KILN_HOME/
  blobs/sha256/<hex>                      # all content, by digest
  cache/layers/<src-digest>@<fmt>         # file containing erofs digest for a source layer
  cache/squash/<sha256-of-ordered-src-digests>@<fmt>
  refs/<escaped-reference>                # tag → manifest/index digest
  run/<run-id>/                           # per-run state for cleanup (sockets, scratch, tap name)
  tmp/                                    # staging for atomic writes (same filesystem as blobs/)
```

- **Atomic writes**: write to `tmp/`, `fsync`, `rename` into place. A cache index entry is written only after its blob is committed.
- **GC**: `kiln gc` marks from `refs/`, sweeps unreferenced blobs and dangling cache entries. Does not touch `run/` entries belonging to live processes.

---

## 5. Build pipeline

`kiln convert <ref|path>` runs steps 1–6. `kiln build -f Dockerfile [--platform ...] <context>` first runs `docker buildx build --output type=oci,dest=<tmp>` (falling back to `buildctl` if Docker is absent; actionable error if neither is present), then runs `convert` on the resulting OCI layout.

1. **Resolve** — reference → OCI index → per-platform OCI manifest (registry, local OCI layout, or Docker archive). If a requested platform is missing, error lists the available platforms.
2. **Convert layers (parallel)** — cache key: compressed OCI layer digest + `kiln-erofs` format version.
   - **Hit**: read erofs digest from `cache/layers/` (no decompression).
   - **Miss**: stream blob → decompress (gzip or zstd, by media type) → tar reader → `kiln-erofs` writer → store → write cache entry.
3. **Squash** — if app layer count exceeds `--max-layers` (default **12**), merge the bottom `N − max + 1` layers into one erofs, applying whiteouts in memory. Cache key: SHA-256 of the ordered source digests + format version. Default of 12 is chosen so that 3 fixed devices + 12 layers + network + vsock stays within per-VMM device limits; see §10.
4. **Init layer** — the `kiln-init` binary for the target arch is embedded in the `kiln` binary at build time; wrapped in a tiny erofs (contains `/kiln-init` and the empty directories `/proc`, `/sys`, `/dev`, `/run`, `/kiln`), cached by digest.
5. **Kernel** — resolve profile (default `base`) for the arch, fetch if absent, verify against the pinned digest table.
6. **Commit** — write config blob, manifest, and (multi-arch) index; update the ref.

### 5.1 Determinism rules

- erofs output depends only on the input tar byte stream and the format version.
- Directory entries are sorted by name; inode numbering follows a defined traversal order.
- Timestamps, ownership, and modes come from tar headers; no build-time values are written anywhere in blobs.
- JSON blobs are serialized with stable key order and no insignificant whitespace.
- Verified by golden tests (§9.1).

---

## 6. `kiln-erofs` writer

### 6.1 Strategy

Single pass over the tar stream, no extraction:

- Regular file data larger than the inline threshold is written to block-aligned data extents in the output as it is read.
- An in-memory metadata tree (paths, inode attributes, xattrs, extent locations, small-file tails) is built as entries arrive. Later entries for the same path replace earlier ones (tar semantics).
- After the stream ends, the metadata area (inodes, directories, shared xattrs) is written after the data, then the superblock at offset 1024 is written last.

### 6.2 v1 feature set

- Block size 4096; uncompressed; tail-packing inline data for small files and directories.
- Compact inodes where fields fit; extended inodes when they don't (uid/gid > 65535, size > 4 GiB, or mtime differs from the superblock's base time). The superblock base time is the minimum mtime in the layer, keeping output deterministic.
- Entry types: regular files, directories, symlinks, hardlinks (shared inode, correct `nlink`), char/block devices, FIFOs. Sockets are skipped with a warning.
- Xattrs: inline and shared, including `security.capability` and PAX `SCHILY.xattr.*` records.
- PAX extended headers and GNU long names/links.
- **Whiteouts** (OCI → overlayfs):
  - `.wh.<name>` → character device 0:0 named `<name>`.
  - `.wh..wh..opq` → `trusted.overlay.opaque=y` xattr on the containing directory.
- Errors (typed, with the offending path): unsupported tar entry type, path escaping root, malformed headers.

### 6.3 Test reader

A minimal erofs reader lives in the crate behind a `test-reader` feature. It is used only for round-trip tests; it is not a supported API.

---

## 7. Boot and runtime

### 7.1 Block device order (fixed)

| Device | Content | Mode |
|---|---|---|
| `vda` | Init erofs; kernel boots with `root=/dev/vda ro rootfstype=erofs init=/kiln-init` | ro |
| `vdb` | Runtime config disk (§7.3) | rw |
| `vdc` | Scratch ext4 (overlay upper + work) | rw |
| `vdd…` | App layers, lowest first | ro |

### 7.2 Scratch disk

- The `kiln` binary embeds a small, empty ext4 template image created with `meta_bg` (so online growth is not capped by reserved GDT blocks).
- `kiln run` copies the template to a sparse file in `run/<id>/` and extends it to `--disk` (default 4 GiB).
- `kiln-init` grows the filesystem to the device size with `EXT4_IOC_RESIZE_FS` before mounting.
- Deleted on exit unless `--persist <path>` is given (then reused on subsequent runs).

### 7.3 Config disk layout

Fixed size 64 KiB, little-endian:

| Offset | Size | Field |
|---|---|---|
| 0 | 8 | Magic `KILNCFG1` |
| 8 | 4 | Config format version (u32) |
| 12 | 4 | JSON length (u32) |
| 512 | 512 | **Status sector** (written by guest) |
| 4096 | ≤ 60 KiB | Runtime config JSON |

Status sector:

| Offset | Size | Field |
|---|---|---|
| 0 | 8 | Magic `KILNSTAT` |
| 8 | 4 | Kind (u32): 0 = not reached, 1 = app exited, 2 = app killed by signal, 3 = init failed |
| 12 | 4 | Code (i32): exit code, signal number, or init stage id |
| 16 | 496 | UTF-8 message, NUL-padded (init failure detail) |

Runtime config JSON: `schemaVersion`, process overrides (`entrypoint`, `cmd`, `env`, `workingDir`, `user`), `hostname`, `interfaces[]` (`name`, `mac`, `address/prefix`, `gateway`, `mtu`), `dns[]`, `layers` (count), `scratch` (`device`, `resize`: bool).

### 7.4 `kiln-init` sequence

Each step is a numbered stage; a failure writes kind 3 with the stage id and message, prints `kiln-init: <stage>: <error>` to the console, and powers off.

1. Mount `/proc`, `/sys`, `/dev` (devtmpfs), `/run` (tmpfs).
2. Read and validate the config disk.
3. Mount a tmpfs at `/kiln` and create mount points in it (the init root is read-only); mount app layers (`erofs`, ro) at `/kiln/layers/<n>`; resize and mount scratch at `/kiln/rw`; mount overlay (`lowerdir` = layers top→bottom, `upperdir`, `workdir`) at `/kiln/root`.
4. Move `/proc`, `/sys`, `/dev`, `/run` into the new root; `pivot_root`; detach the old root.
5. Write hostname, `/etc/hosts`, `/etc/resolv.conf`; bring up `lo`; configure interfaces over netlink (static address, gateway, MTU).
6. Resolve `user` against the image's `/etc/passwd` / `/etc/group` (numeric uid:gid accepted directly); `chdir`; set env; spawn entrypoint + cmd as a child.
7. PID 1 loop: reap all children; forward `SIGTERM`/`SIGINT`/`SIGHUP` to the main child; when the main child exits, write status (kind 1 or 2), `sync`, unmount, `reboot()`.

`kiln-init` has no knowledge of which VMM it runs under.

### 7.5 `kiln run`

```
kiln run <ref> [--vmm firecracker|cloud-hypervisor] [--cpus N] [--memory MiB]
               [--disk SIZE] [--persist PATH] [--net] [--env K=V]... [--boot-timeout SECS]
               [-- CMD ARGS...]
```

- Foreground; guest serial console attached to stdout/stdin.
- Ctrl-C sends a graceful shutdown; a second Ctrl-C force-kills the VMM.
- Exit code: the app's exit code (kind 1), `128 + signal` (kind 2), or a `kiln` error with the failed init stage reported (kind 3 / kind 0 / boot timeout).
- `--net`: `vmkit::net` creates a tap, allocates a /30, configures NAT; the guest gets the static config via the config disk. Without `--net`, loopback only.
- All per-run resources are recorded in `run/<id>/` and cleaned up on exit; stale entries (dead PID) are cleaned on the next `kiln run` or `kiln gc`.
- On macOS, `run` exits with an error explaining how to use the bundled Lima template.

---

## 8. Kernel profiles

- Kernel version pinned to the current LTS at implementation time; recorded in `kernels/VERSION`.
- Config = upstream Firecracker recommended guest config for the arch + `kiln` fragments (erofs, overlayfs, virtio-blk/net, vsock, balloon, ext4 online resize, Cloud Hypervisor-required options).
- v1 ships one profile, `base`. Additional profiles (e.g. `netfilter` for Docker-in-VM) are added as fragments later.
- CI builds each profile per arch, boot-tests it on both VMMs with the init layer and a busybox fixture, then publishes:
  - GitHub release assets, and
  - an OCI artifact `ghcr.io/<project-org>/kiln-kernels/<profile>:<version>-<arch>`, where `<project-org>` is the GitHub org/user hosting the `kiln` repo (fixed at first release and baked into the pinned table).
- `kiln-kernel` contains a pinned table `(profile, version, arch) → digest`; fetches are verified against it. `--kernel <path>` bypasses the table (recorded in the manifest as `profile: "custom"`).

---

## 9. Testing

### 9.1 `kiln-erofs`

- Unit tests per entry type and whiteout form.
- Property tests (`proptest`): random tar trees (nested dirs, hardlinks, xattrs, long paths, whiteouts, duplicate paths) → write → read back via the test reader → compare to the expected tree.
- Golden determinism tests: fixed tar fixtures → committed expected digests; run on macOS and Linux in CI.
- Linux CI only: `fsck.erofs` on all generated images; kernel loop-mount and diff against a plain tar extraction.
- `criterion` benchmarks for throughput.

### 9.2 `vmkit` contract suite

One parametrized suite run against both backends: boot to init, exit-code propagation, pause/resume, snapshot/restore smoke, graceful and forced shutdown, resource cleanup.

### 9.3 End-to-end matrix

{Firecracker, Cloud Hypervisor} × {aarch64, x86_64} × fixtures:

- `alpine`, `debian`, `php:8.4-cli`, a distroless image
- An image with > 12 layers (squash path)
- Whiteout and opaque-directory cases
- A non-root `USER` image
- A Dockerfile build (BuildKit path)

### 9.4 Performance

`kiln bench` runs the three §1 targets and emits JSON. CI records results per commit; regressions are reported, not failing, until targets are first met.

### 9.5 CI hosts

- x86_64: hosted Linux runners with `/dev/kvm`.
- aarch64: hosted arm64 Linux runners if they expose `/dev/kvm`; otherwise a self-hosted runner, with the Lima template as the local equivalent. (Verification item, §10.)

---

## 10. Verification items for planning

These are facts to confirm early in implementation; each has a defined fallback, so none blocks the design.

| Item | Fallback if false |
|---|---|
| Hosted arm64 Linux CI runners expose `/dev/kvm`. | Self-hosted aarch64 runner; Lima locally. |
| Block-device limits per VMM/arch accommodate 3 + 12 layers + net + vsock. | Lower the default `--max-layers`; `kiln run` re-squashes at run time if `Capabilities.max_block_devices` is exceeded. |
| ext4 template with `meta_bg` grows online from template size to ≥ 64 GiB. | Embed several template sizes and pick the nearest. |
| Both VMMs can be configured to exit (not reset) on guest `reboot()` on both arches. | Per-backend shutdown path inside `vmkit` (e.g. ACPI power-off where supported). |
| Crate names `kiln-*` and `vmkit` are available on crates.io. | Rename before first publish; binary name unaffected. |

---

## 11. Error handling summary

- Libraries: `thiserror` typed errors. CLI: `miette` diagnostics with remediation hints.
- Unsupported inputs (Docker schema v1, unknown media types, missing platform) are rejected with specific messages.
- Store mutations are atomic; interrupted builds never leave a cache entry pointing at a missing or partial blob.
- Guest failures are reported by init stage via the status sector, not only by exit code.
- `--boot-timeout` (default 30 s) bounds time to reach stage 6.

---

## 12. Release and docs

- Release binaries: macOS arm64/x86_64, Linux arm64/x86_64 (static musl). The release pipeline builds `kiln-init` first and embeds it.
- Docs: `README.md` (quickstart: build on Mac, run in Lima), `docs/format.md` (normative image + config disk format), `docs/architecture.md`.
- License: Apache-2.0.
