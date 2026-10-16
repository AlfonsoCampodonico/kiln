# M3b-2: Pinned Artifacts, `--persist`, Networking and the Lima VM Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** finish `kiln run` (milestone M3b): kiln trusts kernels and kiln-init only by pinned digest, and `convert` puts them in images; a scratch disk can outlive its run (`--persist`); a run can have a network with an egress policy and published ports (`--net`, `--egress`, `-p`), published on `127.0.0.1` unless told otherwise; and on macOS a Lima VM made from `lima/kiln.yaml` runs the images the Mac converted, with published ports on the Mac's `localhost`.

**Architecture:**
- **`kiln_image::artifacts`** (Task 1): the compiled-in pin table (empty until vmkit's kernel and kiln's kiln-init releases exist, with one marked place for the kernel entries), fetch by pinned digest (store first, then one verified blob GET), the init-layer cache `cache/init-layers`, and the GC roots that keep all of it through `kiln gc`.
- **Boot layers** (Task 2): `convert` adds a kernel and an init layer for each artifact the table pins (`BootLayers`; the CLI asks for `Pinned`, library callers get provisional images unless they ask). `kiln-run`'s `prepare` takes the table as a `Pins` trait: an image's kernel layer boots only when it is the pinned one, and the init layer is always the pinned kiln-init's (or `--init`'s).
- **`kiln_run::persist`** (Task 3): `--persist PATH` with its sidecar, lock, size rules and `--persist-reset`/`--persist-force`; the persisted disk replaces the run's scratch directory.
- **Networking** (Task 4): `--net`, `--egress`, `-p [ADDRESS:]HOST:GUEST[/tcp|udp]` onto `vmkit::NetSpec`; vmkit gains a bind address per forward (`PortForward::address`) and refuses an `allow` entry that is the host's outbound address. kiln-init's `/etc/resolv.conf` gets `nameserver 127.0.0.1` when there is no network.
- **Lima** (Task 5): `lima/kiln.yaml`, `KILN_IMPORT_FROM` (`prepare::import_from`), and the docs.
- **Tests:** pins against kiln-registry's in-process registry (Tasks 1, 2); `prepare` against temporary stores with test tables (Tasks 2, 5); `persist` unit tests (Task 3); CLI refusals (Tasks 3, 4); `kiln run` end to end for boot layers, `--persist`, published ports, the egress policy, the resolver and the import (Tasks 2–5).

**Tech Stack:**
- Rust 2024, in the existing workspace.
- Dependencies (all already in the lock, or added by cargo with the vmkit pin):
  - `kiln-run`: `kiln-registry` (dev in Task 2, normal from Task 5); vmkit repinned to the forward-bind-address change (Task 4; Global Constraints), which brings `seccompiler` 0.5.0 into the lock.
- vmkit change (made with this plan, Task 4 needs it): branch `forward-bind-address` of `AlfonsoCampodonico/vmkit`, based on PR #6's branch `sandbox-exec` (dbea230, the generalised sandbox, which also touches `net.rs`). One commit: `PortForward::address` (one `--tcp-ports ADDRESS/PORT` per forward for pasta), `NetSpec::check_allow` and `net::outbound_address` (an `allow` entry that is only the host's outbound address, the one pasta copies into the namespace, is `Error::InvalidSpec` at creation), `VmSpec::console_log` documented as allowed to be a FIFO; unit tests for the pasta arguments, the unchanged nft forwards and the refusal, and two network-suite cases on both backends (`a_forward_bound_to_loopback_is_reachable_only_there`: refused on the host's other addresses; `an_allow_entry_for_the_hosts_outbound_address_is_refused`).
- Tools (Tasks 2–5 on Linux): as M3b-1: Firecracker 1.17, Cloud Hypervisor 53, vmkit's 6.18.54 kernel, a static busybox (with `nc`, `httpd` and `nslookup`), pasta and nft, vmkit's `vmkit-sandbox` with its AppArmor profile; internet access for the egress case.

**Spec:** `docs/superpowers/specs/2026-09-30-kiln-design.md` (rev 2.9; rev 2.10, committed with this plan, records this plan's rulings). This plan implements the second half of milestone M3b (findings §7, T8–T12):
- §8.1 the pin table, fetch by digest, verification at run, and §6.5 the init and kernel layers at convert;
- §5.2 `cache/init-layers` and GC roots for pinned artifacts (ruling 3);
- §9.4 `--persist` (ruling 2);
- §9.3 `--net`, `--egress`, `-p` and the guest's addressing; the field report's rulings (bind address, listen-port contract, resolver, `allow=` refusal);
- §10 the Lima template and `KILN_IMPORT_FROM` (ruling 4);
- §11.4's `--persist` mismatch and `--disk` shrink cases, and §11.5's "kernel and init swap" against a pin table.

**Spec rev 2.10** (committed with this plan):
- `-p [ADDRESS:]HOST:GUEST[/tcp|udp]`: the host address defaults to `127.0.0.1`; `0.0.0.0` (or another of the host's addresses) publishes beyond the host. A host port is published once whatever its addresses. The image owns its listen port: the app must listen on `0.0.0.0` (or the guest's address); a forward to a guest listening on `127.0.0.1` ends at once (§9.3).
- Without a network (or with one that names no DNS server), `/etc/resolv.conf` adds `nameserver 127.0.0.1`, as Docker's `--network none` does, so lookups fail at once instead of tools refusing to start (§9.6, `docs/format.md`).
- pasta copies the host's outbound address (the one on its default route) into the VM's namespace, so the guest can never reach it: an `--egress allow=` entry that is only that address is refused before boot (and by vmkit), and a wider one that includes it is warned about. The host's other addresses can be reached through an exception (finding 4.1, §9.3).
- `--persist` reuses a disk whose sidecar lists the same app layers in the same order even when the manifest differs (another env or cmd), recording the new manifest with a note (ruling 2); `--persist-reset` and `--persist-force` exclude each other; the disk is locked for the run; a disk without a readable sidecar needs one of them; the persisted disk replaces the run's scratch directory and no cleanup touches it (§9.4).
- `kiln gc` keeps every blob the compiled pin table names and the init layers built from pinned kiln-init binaries (`cache/init-layers`), so offline runs survive it (ruling 3, finding 4.2, §5.2).
- `convert` adds each boot layer whose artifact is pinned for the architecture, independently: a kernel can be pinned before any kiln-init release. Without either it makes a provisional image, says what `kiln run` will need, and `inspect` shows a `boot:` line. `config.init` is informational (finding 4.3) (§5.1, §6.5, §8.1).
- Findings 4.4 (the device budget's remedy is `kiln convert --max-layers N`, §9.7) and 4.5 (`shutdownGraceSecs` in §9.5's table).
- §10: the Lima template, `KILN_IMPORT_FROM` importing and retagging on every run (ruling 4), and published ports on the Mac's `localhost`.

It builds on M3b-1 (`m3b1-run` at 109b648, PR #7) and on vmkit's forward-bind-address change (Global Constraints).

## Global Constraints

- **Repository:**
  - `/Users/alfonso/Github/Personal/playground/kiln` (GitHub `AlfonsoCampodonico/kiln`, private).
  - Work on a new branch `m3b2-run` created from `m3b1-run` (109b648).
  - Do not push; the controller asks the user.
- **Commits:** plain messages with no `Co-Authored-By`, `Claude-Session` or other attribution lines.
- **vmkit:** `kiln-run`'s Linux-only dependency is pinned by full revision: `vmkit = { git = "https://github.com/AlfonsoCampodonico/vmkit", rev = "4aa091361798dd6fa4e99d6494efb235bd727d20" }` until Task 4, which repins it. Its new revision is the placeholder `<VMKIT_REV>`, in exactly one place, Task 4's `crates/kiln-run/Cargo.toml` step: before dispatching Task 4 the controller replaces it with the full revision of the forward-bind-address change once it is merged into vmkit's `main` (local branch `forward-bind-address`, commit 1bc3cfb3a11773877bd8fd2348e449c756fd14c3, based on PR #6; pin the merge commit, or this commit if it is merged as is). No path dependency or `[patch]` anywhere.
- **Crates:** every crate root but `kiln-init`'s keeps `#![forbid(unsafe_code)]`; no new `unsafe`. No `expect()`, `unwrap()` or panicking index on the run path; tests may. `kiln-run`'s Linux code stays `#[cfg(target_os = "linux")]`, so `cargo test --all` works on macOS.
- **Pins:** `kiln_image::artifacts::PINS` stays empty (ruling 7). Tests never depend on its contents: they pass their own tables (`prepare::Table`, `BootLayers::Explicit`, `artifacts::gc_roots_in`). Library callers of `convert_*` get provisional images unless they ask for `BootLayers::Pinned`, so no test fetches from a real registry.
- **Exit codes** (spec §9.5, rev 2.9): unchanged; every refusal this plan adds happens before boot and exits 125.
- **Device budget:** `--net` adds a device; the cases here use one-layer images.
- **Environment:**
  - macOS for every task's checks.
  - Linux checks (every task) in the Lima VM `vmkit` (Ubuntu 26.04 aarch64, nested KVM, Rust 1.99): `limactl shell vmkit`, `cd` to the repository, `export CARGO_TARGET_DIR=$HOME/target-m3b2` (the AppArmor profile covers `/home/*/target-*/debug/vmkit-sandbox`). KVM runs need the `kvm` group (`sg kvm -c '…'`), Firecracker and Cloud Hypervisor on `PATH` (`$HOME/bin` there), `busybox-static`, `pasta`, `nft`, internet access, and the kernel `/Users/alfonso/Github/Personal/playground/vmkit/out/vmlinux-6.18.54-aarch64`.
  - From Task 4 on, the Linux build fetches the substituted vmkit revision from GitHub.
- **Formatting:** `rustfmt.toml` sets `max_width = 120`. The plan's code is already `cargo fmt`-clean. CI runs fmt, clippy (`-D warnings`) and the tests on ubuntu-24.04 and macos-15, and the boot job runs `scripts/boot-tests.sh` with `KILN_TEST_NET=1` on x86_64 KVM.
- **Validation:**
  - This plan was generated from task commits built on the validated prototype `m3b-proto` (findings §7's T8–T12, with rulings 2, 3, 4, 7, 10 and the field report's networking rulings) and replayed task by task, mechanically, on a fresh clone of `m3b1-run` (109b648) on macOS (Apple Silicon), the vmkit placeholder replaced by the local vmkit commit (1bc3cfb3a11773877bd8fd2348e449c756fd14c3). After every task the replayed tree, `Cargo.lock` included, is identical to the task branch it was generated from.
  - After every task, fmt, clippy (`-D warnings`) and the whole workspace's tests passed on macOS: 404, 411, 413, 415, 416 tests after Tasks 1–5.
  - After every task, fmt, clippy and the tests passed on Linux (Lima `vmkit`, Ubuntu 26.04 aarch64, Rust 1.99): 503, 511, 514, 520, 522 tests (the boot and `kiln run` cases skip themselves without their environment).
  - `kiln-init` and `hostile` build static for aarch64 (`statically linked`) and x86_64 (`static-pie linked`, rust-lld), clippy clean.
  - **vmkit** `forward-bind-address` (1bc3cfb, on PR #6's dbea230): fmt, clippy, 52 unit tests; the network suite 14/14 on both backends in two consecutive runs (both new cases included); the contract suite 22/22, the sandbox suite 11/11 and the exec suite 27/27 (the exec suite outside `sg kvm`, whose group change `newuidmap` refuses).
  - **Firecracker 1.17** (Task 5's tree, nested, 4 test threads, `KILN_TEST_NET=1`): the boot suite 23/23 and `kiln run` end to end 16/16 (the 11 of M3b-1 and the 5 new or changed: boot layers converted, `--persist`, published ports, egress and resolver, import).
  - **Cloud Hypervisor 53** (same): boot suite 14/23 and `kiln run` end to end 6/16 in the full run, every failure but one a stall (boot timeouts, a VM not ending after `InitFailed`). Serial reruns: `networking_from_config`, `init_failures_are_sanitised_on_the_host`, `user_groups_env_and_workdir`, `signals_are_forwarded` and `converted_boot_layers_are_replaced_or_refused_at_run` passed on their 1st try, `images_are_imported_from_kiln_import_from` and `image_boot_layers_are_verified_and_init_replaced` on their 2nd; `egress_policy_and_the_resolver_without_a_network` and `exit_codes_as_docker_reports_them` stalled at boot in both of two tries; `persisted_disks_are_reused_and_guarded` stalled at boot in three reruns, one of them after the reuse checks had passed.
  - The exception: in the full Cloud Hypervisor run, `persisted_disks_are_reused_and_guarded`'s second run did not find the file its first run wrote (the first had exited 0). Not reproduced in three reruns, nor on Firecracker. Suspected cause (Known gaps): kiln-init syncs the scratch disk after sending `Exited`, so a VM that stalls after `Exited` and is killed by the end bound loses unsynced writes.
  - **The Lima→macOS hop** (field report ruling 2): `kiln run --net -p 28473:8080` in the Lima VM, then `curl --http0.9 localhost:28473` on the Mac answered `hello-mac` through Lima's own forward (`limactl` listening on the Mac's `127.0.0.1:28473`); inside the VM the port refused connections on its non-loopback address `192.168.5.15`. So the template needs no `portForwards` entry. On port 18080 the Mac's `localhost` reached another program listening there over IPv6, hence the README's note.

## Review Focus

Inputs and conditions no other test targets that will bite a real user, most likely first. Each has a pinned test in the task named.

1. **`kiln run --net -p 8080:8080 app` and then `curl` from another machine** (or from the Mac when kiln runs in Lima). Expected: the port is published on `127.0.0.1` only (reachable from the Mac's `localhost` through Lima's forwarding); the host's other addresses refuse it; `-p 0.0.0.0:8080:8080` publishes it everywhere.
   - Pinned in Task 4 (`published_ports_bind_to_loopback_unless_told_otherwise`, and vmkit's `a_forward_bound_to_loopback_is_reachable_only_there` on both backends); the Lima hop verified by hand (Validation).
2. **An app that listens on `127.0.0.1` in the guest** (a dev server's default). Expected: the published port's connections end at once (no hang), and the README says the app must listen on `0.0.0.0`.
   - Pinned in Task 4 (the same test, its loopback-only `httpd`).
3. **A tool that needs a resolver in a run without `--net`** (the field report's `oha`). Expected: `/etc/resolv.conf` names `127.0.0.1`, lookups fail at once.
   - Pinned in Task 4 (`etc::tests`, the boot case `hostname_hosts_and_resolv_conf_replace_symlinks`, and `egress_policy_and_the_resolver_without_a_network`).
4. **`--persist` during development**: the image is rebuilt with a changed env or cmd but the same layers. Expected: the disk is reused with a note (ruling 2); a rebuild that changes a layer is refused with what to do.
   - Pinned in Task 3 (`persist::tests::decisions`, `persisted_disks_are_reused_and_guarded`).
5. **`kiln gc`, then a run without the network.** Expected: once the table pins artifacts, they and the init layer built from kiln-init survive GC.
   - Pinned in Task 1 (`roots_are_kept_with_their_cache_entries`, `gc_keeps_pinned_artifacts_and_their_init_layers`).
6. **`--egress allow=` naming the host's own address** (to reach a service on the host). Expected: for the address pasta copies into the namespace (the outbound one), refused before boot with why, instead of a rule that silently never works.
   - Pinned in Task 4 (`run_refuses_an_image_without_a_kernel_before_booting`'s outbound-address case, `net_options_map_onto_vmkit`), and in vmkit (`an_allow_entry_that_is_the_hosts_outbound_address_is_refused`, `an_allow_entry_for_the_hosts_outbound_address_is_refused`).

## Design decisions (rulings made while prototyping and planning)

Controller rulings applied here (`scratchpad/m3b/rulings.md`, `design-brief-m3b2.md`):
- **R2.** `--persist` with the same ordered app-layer digests but another manifest is allowed; the sidecar records the new manifest and one note is printed.
- **R3.** `kiln gc` keeps every blob the compiled table pins and the `cache/init-layers` entries (with their blobs) of pinned kiln-init binaries.
- **R4.** `KILN_IMPORT_FROM`: when the source store has the name, import (missing blobs only) and retag every time.
- **R7 (updated).** The table ships empty; the mechanism is tested with kiln-registry's in-process registry; one marked place takes the kernel entries (profile `base`, version `6.18.54`, repository `ghcr.io/alfonsocampodonico/vmkit-kernels/base`). Only explicit, warned overrides; no kiln-init next to the binary.
- **R8.** Findings 4.1–4.5 go into spec rev 2.10 (4.1 also as the vmkit refusal).
- **R10.** The vmkit change M3b-2 needs (the forward bind address) also carries the refusal of an `allow` entry for the host's outbound address and the FIFO console's documentation. `Vm::identity()` and a waitable end are not in it: they would not remove a kiln workaround cheaply (both touch every backend).
- **Networking (field report):** N1 `-p [IP:]HOST:GUEST[/proto]` with `127.0.0.1` by default; N2 the Lima→macOS hop verified; N3 the listen-port contract documented and its failure pinned; N4 `nameserver 127.0.0.1` without a network; N5 `--egress` values and the `allow=` refusal; N6 `Config.network` from vmkit's fixed `172.30.0.2/30`.

Rulings made while planning (not in the prototype):
- **P1. `BootLayers::Provisional` is the library default; `kiln convert` asks for `Pinned`.** The prototype defaulted to `Pinned`: once the table is filled, every test and library caller that converts (dozens of temporary stores) would fetch kernels from ghcr.io. Tests stay offline and deterministic; potter, which uses kiln's crates, asks for `Pinned` explicitly.
- **P2. Each boot layer is added when it is pinned, independently.** vmkit's kernel release comes before kiln-init's (M3c): with the kernel entries filled, images get their kernel layer and `kiln run` needs only `--init`. The prototype added both or neither.
- **P3. `prepare` takes `&dyn Pins` and `&dyn Fetcher`**; `CompiledPins` is the table, `Table(&[Pin])` any other. A provisional image after a kernel is pinned is refused with "convert it again" (it names no kernel to verify); an image's `custom` kernel keeps M3b-1's rule.
- **P4. GC roots are a closure run under GC's exclusive lock** (`Store::gc_with_roots`), so the init-layer cache read cannot race a convert; a cache entry survives when its blob is a root.
- **P5. `--persist` takes the disk's lock before recreating it** (the prototype removed it first, under a run that might be using it), refuses to create over a file that appeared meanwhile, and makes no scratch directory for the run; the persisted disk is never in `$KILN_HOME/scratch/`, so no stale-run sweep can touch it. With `--persist` the end bound is 60 s (D-3: the guest syncs a disk that outlives the run).
- **P6. The vmkit change is based on PR #6's `sandbox-exec` branch** (coordinator): PR #6 also changes `net.rs` (serde on `PortForward`, the tap-less egress ruleset). kiln uses only `sandbox::pid_file` and `cgroups_available` from vmkit's sandbox, which PR #6 keeps.
- **P7. The `allow=` refusal covers only the address pasta copies, and is checked twice.** The brief asked to refuse entries covering the host's addresses; validating found that only the outbound address is copied into the namespace (`pasta --config-net`), while vmkit's own network suite reaches the host's other addresses (its fixtures on `lo`) through `allow`. So an entry that is exactly the outbound address is refused, a wider one that includes it is warned about (its other addresses work), and the rest stay usable. kiln checks before anything is prepared (vmkit's `NetSpec::check_allow` with `net::outbound_address`, one rule), and vmkit again when the VM is created.
- **P8. `nameserver 127.0.0.1` also when a network names no DNS server** (only a non-kiln host could send that), for the same reason as without a network.
- **P9. The e2e busybox image gains `httpd` and `nslookup`** (a listener on the guest's loopback only, and a lookup) — busybox's `nc` cannot bind an address.

From the prototype (findings §1, §7), kept:
- **D1.** A pin names the blob itself (kernel image, static kiln-init) with its repository and size: one digest-verified blob GET; no tag or manifest is resolved.
- **D2.** `ensure` looks in the store first and builds no registry client when the blob is there (a client resolves its registry's name when built), so cached runs work offline.
- **D3.** The init layer is cached by the kiln-init digest and the erofs format version (spec §6.5); the pinned one boots from its blob in the store.
- **D4.** `--persist`'s sidecar is all kiln trusts about the disk; it never parses the guest-written ext4. The disk is never truncated, symlinks are refused, and the sidecar is written atomically.
- **D5.** `net_spec` maps kiln's options onto vmkit's one to one; kiln keeps its own copy of vmkit's guest addressing for the platform-independent `Config` builder, compared to vmkit's by a Linux test.
- **D6.** `KILN_IMPORT_FROM` imports before the store lock is taken; a failure is a warning, never fatal (the image may be in the local store already).

**Known gaps:**
- The pin table is empty. Filling the kernel entries is one commit in `artifacts.rs`; it will also change what some tests expect (`a_provisional_image_needs_the_custom_kernel_flags`, the CLI convert note), and `kiln convert` in the CLI tests will fetch the kernel (once per temporary store) — the CI then needs ghcr.io.
- The egress e2e case reaches `1.1.1.1:80` and needs the internet (an `allow=` exception for a host address other than the outbound one would do, but the CI host has no such fixture).
- **Writes after `Exited` with `--persist`:** kiln-init sends `Exited`, then syncs the scratch disk (M3a's order, spec §9.6). A VM that stalls after `Exited` (Cloud Hypervisor 53 nested) is killed by the end bound (60 s with `--persist`) and may lose the guest's last unsynced writes: suspected in one Cloud Hypervisor run (Validation), not reproduced. A fix (sync before `Exited`) changes kiln-init's supervision order and `docs/format.md`; it is left for the controller to rule on.
- Cloud Hypervisor 53 under nested virtualization stalls guests (spec §14): judged by pass rate with reruns; Firecracker is the reference.
- `run.json` does not record a `--persist` path (a field would make older kilns' stale cleanup skip the run as unreadable); the disk is found by its sidecar.
- vmkit asks still open (ruling 10): `Vm::identity()` and a waitable end (kiln still polls the VMM's end every 20 ms and reads vmkit's PID file).

## File Structure

```
crates/kiln-store/src/{cache,gc,lib}.rs        cache/init-layers; gc_with_roots              Task 1
crates/kiln-registry/src/testregistry.rs       put_blob                                     Task 1
crates/kiln-image/src/artifacts.rs             PINS, Pin, ensure, init_layer_blob, gc_roots  Task 1
crates/kiln-image/src/{error,lib}.rs, tests/artifacts.rs                                    Task 1 (+ 2)
crates/kiln-image/src/convert.rs               Boot, BootLayers                             Task 2
crates/kiln-run/src/prepare.rs                 Pins, CompiledPins, Table, InitLayer         Task 2 (+ 5: import_from)
crates/kiln-run/src/persist.rs                 Sidecar, decide, inspect, apply              Task 3
crates/kiln-run/src/options.rs                 persist flags (3); Egress, PortSpec, Cidr, NetOptions (4)
crates/kiln-run/src/config.rs                  GUEST, GATEWAY, PREFIX, Config.network       Task 4
crates/kiln-run/src/linux/run.rs               pins (2), persist (3), net_spec (4), import (5)
crates/kiln-run/src/lib.rs                     persist (3), LIMA_INSTRUCTIONS (5)
crates/kiln-run/tests/{prepare,boot}.rs                                                     Tasks 2, 4, 5
crates/kiln-init/src/etc.rs                    nameserver 127.0.0.1                          Task 4
crates/kiln/src/{main,run_cmd}.rs              gc roots (1), boot note and line (2), flags (3, 4)
crates/kiln/tests/{cli,run}.rs                                                              Tasks 2–5
lima/kiln.yaml                                                                              Task 5
README.md, docs/format.md                                                                   Tasks 1–5
```

Each step below gives a file's whole content, or exact replacements in it ("In `path`, replace … with …", each old text occurring exactly once at that point). Cargo updates `Cargo.lock` on the first build.

---

### Task 1: `kiln-image`: the pinned artifact table, fetch by pinned digest, the init-layer cache and GC roots

**Files:**
- Create: `crates/kiln-image/src/artifacts.rs`, `crates/kiln-image/tests/artifacts.rs`
- Modify: `crates/kiln-image/src/error.rs`, `crates/kiln-image/src/lib.rs`, `crates/kiln-registry/src/testregistry.rs`, `crates/kiln-store/src/cache.rs`, `crates/kiln-store/src/gc.rs`, `crates/kiln-store/src/lib.rs`, `crates/kiln/src/main.rs`, `docs/format.md`

**Interfaces:**
- Consumes: `kiln_registry::{Client::fetch_blob, DockerConfig, Reference}`, `kiln_registry::testregistry::TestRegistry` (tests), `kiln_store::{Store, Digest, CacheKind}`, `kiln_image::init_layer`, `kiln_erofs::FORMAT_VERSION`.
- Produces:
  - `kiln_store::CacheKind::InitLayers` (`cache/init-layers/<kiln-init digest>@<format version>` → the init layer's digest); `Store::gc_with_roots(impl FnOnce(&Store) -> Result<Vec<Digest>>) -> Result<GcReport>` (`gc()` is `gc_with_roots` with none).
  - `kiln_registry::testregistry::TestRegistry::put_blob(&[u8]) -> Digest`.
  - `kiln_image::artifacts`: `ArtifactKind::{Kernel, Init}`; `Pin { kind, name, version, arch, repository, digest, size }` (all `&'static str` but `size: u64`) with `digest() -> Result<Digest>` and `describe()`; `PINS` (empty), `KERNEL_PROFILE` (`base`), `KERNEL_VERSION` (`6.18.54`), `KERNEL_REPOSITORY`, `INIT_VERSION` (`CARGO_PKG_VERSION`), `CUSTOM_PROFILE` (`custom`); `kernel_pin(profile, version, arch)`, `init_pin(arch)`, `find(table, kind, name, version, arch)`; `trait Fetcher { fn fetch(&self, &Pin, &Store) -> Result<()> }`, `RegistryFetcher { config }` with `from_env()`; `ensure(&Store, &Pin, &dyn Fetcher) -> Result<Digest>`; `init_layer_blob(&Store, &Digest) -> Result<Digest>`; `gc_roots(&Store)`, `gc_roots_in(&[Pin], &Store) -> kiln_store::Result<Vec<Digest>>`.
  - `ImageError::Artifact { what, source }` (`fetching the pinned <what>: <source>`).
  - `kiln gc` keeps the roots.

The trusted-artifact mechanism (spec §8.1, T4), without any artifact yet: vmkit's kernel release and kiln's kiln-init release do not exist (ruling 7), so `PINS` is empty, with one marked place for the kernel entries. Everything is tested against kiln-registry's in-process registry with test pins.

Design notes:
- A pin names the blob itself (the kernel image, or the static kiln-init binary), its repository and its size: fetching is one blob GET by digest, verified by the store (D1). No tag or manifest is resolved, so nothing the registry says can change what boots. Releases must publish that blob's digest and size.
- `ensure` looks in the store first and builds no registry client when the blob is there (a client resolves its registry's name when it is built), so cached runs work offline; the size is checked even then (D2).
- `init_layer_blob` caches the init layer by the kiln-init digest and the erofs format version (spec §6.5, D3).
- GC roots (ruling 3): `kiln gc` keeps every blob the table pins and the init layers built from pinned kiln-init binaries; the cache entry naming a root survives with it. The roots are computed under GC's exclusive lock (P4).

- [ ] **Step 1: Create `crates/kiln-image/tests/artifacts.rs`**

Fetch by digest against the in-process registry: once, then from the store with a fetcher that fails if asked; a registry serving other bytes, a missing blob, a wrong pinned size; the init layer built once; GC keeping pinned artifacts and their init layers.

`crates/kiln-image/tests/artifacts.rs`:
```rust
//! Fetching pinned boot artifacts by digest (spec §8.1) against the in-process registry.

use std::cell::Cell;

use kiln_image::ImageError;
use kiln_image::artifacts::{ArtifactKind, Fetcher, Pin, RegistryFetcher, ensure, gc_roots_in, init_layer_blob};
use kiln_registry::DockerConfig;
use kiln_registry::testregistry::{Config, TestRegistry};
use kiln_store::{Digest, Store};

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

fn pin(reg: &TestRegistry, bytes: &[u8]) -> Pin {
    Pin {
        kind: ArtifactKind::Init,
        name: "kiln-init",
        version: "0.0.0-test",
        arch: "arm64",
        repository: leak(format!("{}/kiln/kiln-init", reg.host())),
        digest: leak(Digest::of(bytes).to_string()),
        size: bytes.len() as u64,
    }
}

fn fetcher() -> RegistryFetcher {
    RegistryFetcher {
        config: DockerConfig::anonymous(),
    }
}

/// Fails the test if asked to fetch: the store must have answered.
struct Offline(Cell<usize>);

impl Fetcher for Offline {
    fn fetch(&self, _: &Pin, _: &Store) -> kiln_image::Result<()> {
        self.0.set(self.0.get() + 1);
        Err(ImageError::BadOption("offline".into()))
    }
}

#[test]
fn fetches_by_digest_once_then_serves_from_the_store() {
    let reg = TestRegistry::start(Config::default());
    let bin = b"\x7fELF a static kiln-init".repeat(100);
    reg.put_blob(&bin);
    let p = pin(&reg, &bin);
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let d = ensure(&store, &p, &fetcher()).unwrap();
    assert_eq!(std::fs::read(store.blob_path(&d)).unwrap(), bin);
    assert_eq!(reg.blob_requests("GET"), 1);
    assert_eq!(reg.manifest_requests("GET") + reg.manifest_requests("HEAD"), 0);
    // Cached: no fetcher (so no registry client, no DNS) is used.
    let offline = Offline(Cell::new(0));
    assert_eq!(ensure(&store, &p, &offline).unwrap(), d);
    assert_eq!(offline.0.get(), 0);
}

#[test]
fn a_registry_serving_other_content_is_refused() {
    let bin = b"the real kiln-init".to_vec();
    let reg = TestRegistry::start(Config {
        corrupt: Some(Digest::of(&bin)),
        ..Config::default()
    });
    reg.put_blob(&bin);
    let p = pin(&reg, &bin);
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let err = ensure(&store, &p, &fetcher()).unwrap_err();
    assert!(
        err.to_string()
            .contains("fetching the pinned kiln-init kiln-init 0.0.0-test for arm64"),
        "{err}"
    );
    assert!(!store.has_blob(&p.digest().unwrap()));
    // A missing blob is an error too, and nothing is stored.
    let empty = TestRegistry::start(Config::default());
    let p = pin(&empty, &bin);
    assert!(ensure(&store, &p, &fetcher()).is_err());
    assert!(!store.has_blob(&p.digest().unwrap()));
}

#[test]
fn a_wrong_size_in_the_pin_is_refused_even_from_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let bin = b"kiln-init".to_vec();
    store.put_bytes(&bin).unwrap();
    let reg = TestRegistry::start(Config::default());
    let mut p = pin(&reg, &bin);
    p.size += 1;
    assert!(ensure(&store, &p, &Offline(Cell::new(0))).is_err());
}

#[test]
fn the_init_layer_is_built_once_per_binary() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let bin = store.put_bytes(&b"\x7fELF init".repeat(50)).unwrap();
    let a = init_layer_blob(&store, &bin).unwrap();
    let want = kiln_image::init_layer(&std::fs::read(store.blob_path(&bin)).unwrap(), dir.path()).unwrap();
    assert_eq!(a, Digest::of(&want));
    // A second call is a cache hit; a missing layer blob is rebuilt.
    assert_eq!(init_layer_blob(&store, &bin).unwrap(), a);
    std::fs::remove_file(store.blob_path(&a)).unwrap();
    assert_eq!(init_layer_blob(&store, &bin).unwrap(), a);
    assert!(store.has_blob(&a));
}

/// `kiln gc` keeps the pinned blobs and the init layers built from pinned
/// kiln-init binaries (ruling 3), so offline runs keep working after it.
#[test]
fn gc_keeps_pinned_artifacts_and_their_init_layers() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let reg = TestRegistry::start(Config::default());
    let (kernel, init) = (b"a kernel Image".to_vec(), b"\x7fELF kiln-init".to_vec());
    let mut kpin = pin(&reg, &kernel);
    (kpin.kind, kpin.name, kpin.version) = (ArtifactKind::Kernel, "base", "6.18.54");
    let ipin = pin(&reg, &init);
    let offline = Offline(Cell::new(0));
    store.put_bytes(&kernel).unwrap();
    store.put_bytes(&init).unwrap();
    let k = ensure(&store, &kpin, &offline).unwrap();
    let i = ensure(&store, &ipin, &offline).unwrap();
    let layer = init_layer_blob(&store, &i).unwrap();
    // An init layer of a kiln-init that is no longer pinned goes.
    let old = store.put_bytes(b"\x7fELF an older kiln-init").unwrap();
    let old_layer = init_layer_blob(&store, &old).unwrap();
    let table = [kpin, ipin];
    let roots = gc_roots_in(&table, &store).unwrap();
    assert_eq!(roots.len(), 3);
    store.gc_with_roots(|s| gc_roots_in(&table, s)).unwrap();
    for d in [&k, &i, &layer] {
        assert!(store.has_blob(d), "{d}");
    }
    assert!(!store.has_blob(&old) && !store.has_blob(&old_layer));
    // The cached init layer is still a hit: nothing is rebuilt.
    assert_eq!(init_layer_blob(&store, &i).unwrap(), layer);
    assert_eq!(offline.0.get(), 0);
}
```

- [ ] **Step 2: Run the tests to see them fail**

Run:
```bash
cargo test -q -p kiln-image --test artifacts
```
Expected: FAIL to compile: `kiln_image::artifacts` does not exist yet.

- [ ] **Step 3: Modify `crates/kiln-store/src/cache.rs`**

The new cache. `Warnings` stays last: GC decides its entries after the `Layers` entries.

In `crates/kiln-store/src/cache.rs`, replace:
```rust
    Warnings,
}
```
with:
```rust
    Warnings,
    /// `<kiln-init digest>@<fmt>` → digest of the init layer built around it (spec §6.5).
    InitLayers,
}
```

In `crates/kiln-store/src/cache.rs`, replace:
```rust
            CacheKind::Warnings => "cache/warnings",
        }
```
with:
```rust
            CacheKind::Warnings => "cache/warnings",
            CacheKind::InitLayers => "cache/init-layers",
        }
```

In `crates/kiln-store/src/cache.rs`, replace:
```rust
    pub(crate) const ALL: [CacheKind; 4] = [
        CacheKind::Layers,
        CacheKind::LayersCtx,
        CacheKind::Squash,
```
with:
```rust
    pub(crate) const ALL: [CacheKind; 5] = [
        CacheKind::Layers,
        CacheKind::LayersCtx,
        CacheKind::Squash,
        CacheKind::InitLayers,
```

- [ ] **Step 4: Modify `crates/kiln-store/src/lib.rs`**

`Store::open` creates its directory.

In `crates/kiln-store/src/lib.rs`, replace:
```rust
            "cache/warnings",
            "tmp",
```
with:
```rust
            "cache/warnings",
            "cache/init-layers",
            "tmp",
```

- [ ] **Step 5: Modify `crates/kiln-store/src/gc.rs`**

Roots: blobs kept although no ref reaches them. The test keeps a pinned kernel, a pinned kiln-init and its init layer, and drops an older init layer and its cache entry.

In `crates/kiln-store/src/gc.rs`, replace:
```rust
        let _lock = self.lock_exclusive()?;
        let live = self.live_blobs()?;
```
with:
```rust
        self.gc_with_roots(|_| Ok(Vec::new()))
    }

    /// [`Store::gc`], also keeping the blobs `roots` names (and the cache entries
    /// naming them) although no ref reaches them: kiln's pinned boot artifacts and
    /// the init layers built from them (spec §5.2). `roots` runs under the lock.
    pub fn gc_with_roots(&self, roots: impl FnOnce(&Store) -> Result<Vec<Digest>>) -> Result<GcReport> {
        let _lock = self.lock_exclusive()?;
        let mut live = self.live_blobs()?;
        live.extend(roots(self)?);
```

In `crates/kiln-store/src/gc.rs`, replace:
```rust
    }

    #[test]
    fn gc_waits_for_shared_holders() {
```
with:
```rust
    }

    /// Pinned artifacts and the cache entries naming them survive GC (ruling 3);
    /// other cache entries of the same kind do not.
    #[test]
    fn roots_are_kept_with_their_cache_entries() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let (kernel, init, layer) = (
            s.put_bytes(b"a pinned kernel").unwrap(),
            s.put_bytes(b"a pinned kiln-init").unwrap(),
            s.put_bytes(b"its init layer").unwrap(),
        );
        let old = s.put_bytes(b"an older init layer").unwrap();
        let key = |d: &Digest| format!("{d}@1");
        s.cache_put(CacheKind::InitLayers, &key(&init), &layer.to_string())
            .unwrap();
        let old_init = Digest::of(b"an older kiln-init");
        s.cache_put(CacheKind::InitLayers, &key(&old_init), &old.to_string())
            .unwrap();
        let roots = vec![kernel.clone(), init.clone(), layer.clone()];
        let r = s.gc_with_roots(|_| Ok(roots)).unwrap();
        assert_eq!((r.blobs_removed, r.cache_entries_removed), (1, 1));
        for d in [&kernel, &init, &layer] {
            assert!(s.has_blob(d), "{d}");
        }
        assert!(!s.has_blob(&old));
        assert_eq!(
            s.cache_get_blob(CacheKind::InitLayers, &key(&init)).unwrap(),
            Some(layer)
        );
        assert_eq!(s.cache_get(CacheKind::InitLayers, &key(&old_init)).unwrap(), None);
        // Without roots, all of it goes.
        assert_eq!(s.gc().unwrap().blobs_removed, 3);
    }

    #[test]
    fn gc_waits_for_shared_holders() {
```

- [ ] **Step 6: Modify `crates/kiln-registry/src/testregistry.rs`**

Pinned artifacts are bare blobs; the test registry can serve one.

In `crates/kiln-registry/src/testregistry.rs`, replace:
```rust
        self.state.content.lock().unwrap().blobs.remove(d);
    }
```
with:
```rust
        self.state.content.lock().unwrap().blobs.remove(d);
    }

    /// Stores a blob under its digest (in every repository).
    pub fn put_blob(&self, bytes: &[u8]) -> Digest {
        let d = Digest::of(bytes);
        self.state
            .content
            .lock()
            .unwrap()
            .blobs
            .insert(d.clone(), bytes.to_vec());
        d
    }
```

- [ ] **Step 7: Modify `crates/kiln-image/src/error.rs`**

Names which pinned artifact a fetch was for.

In `crates/kiln-image/src/error.rs`, replace:
```rust
    BadOption(String),
}
```
with:
```rust
    BadOption(String),
    #[error("fetching the pinned {what}: {source}")]
    Artifact { what: String, source: Box<ImageError> },
}
```

- [ ] **Step 8: Create `crates/kiln-image/src/artifacts.rs`**

The table, its lookups, fetching, the init-layer cache and the GC roots. `PINS`' doc comment says how to fill the kernel entries.

`crates/kiln-image/src/artifacts.rs`:
```rust
//! Pinned boot artifacts (spec §8.1, T4): the kernels and `kiln-init` binaries this
//! build of kiln trusts, by digest, and fetching them by that digest.
//!
//! The table is compiled in. Kernels come from vmkit's releases, published as OCI
//! artifacts at `ghcr.io/<org>/vmkit-kernels/<profile>:<version>-<arch>`; `kiln-init`
//! comes from kiln's own releases. A pin names the blob itself (the kernel image, or
//! the static `kiln-init` binary) and the repository holding it, so a fetch is one
//! verified blob GET: no tag or manifest is resolved, and nothing the registry says
//! can change what boots.
//!
//! Fetching looks in the store first and builds no registry client when the blob is
//! there, so cached runs work offline (a client resolves its registry's name when it
//! is built). `kiln gc` keeps every pinned blob and the init layers built from the
//! pinned `kiln-init` binaries ([`gc_roots`], spec §5.2), so they stay offline too.

use std::fmt;

use kiln_erofs::FORMAT_VERSION;
use kiln_registry::{Client, DockerConfig, Reference};
use kiln_store::{CacheKind, Digest, Store};

use crate::error::{ImageError, Result};
use crate::init::init_layer;

/// What a pin is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactKind {
    /// A kernel image (`Image` on arm64, PVH `vmlinux` on amd64), booted as is.
    Kernel,
    /// A static `kiln-init` binary, wrapped in the init layer at use.
    Init,
}

impl fmt::Display for ArtifactKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ArtifactKind::Kernel => "kernel",
            ArtifactKind::Init => "kiln-init",
        })
    }
}

/// One trusted artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    pub kind: ArtifactKind,
    /// The kernel profile (`base`), or `kiln-init`.
    pub name: &'static str,
    pub version: &'static str,
    /// `arm64` or `amd64`, as in kiln image configs.
    pub arch: &'static str,
    /// The registry repository holding the blob, e.g. `ghcr.io/org/vmkit-kernels/base`.
    pub repository: &'static str,
    /// `sha256:<hex>` of the blob.
    pub digest: &'static str,
    pub size: u64,
}

impl Pin {
    /// The pinned digest (the table's are checked by a test; others may be wrong).
    pub fn digest(&self) -> Result<Digest> {
        Ok(Digest::parse(self.digest)?)
    }

    /// What the pin is, for messages: `kernel base 6.18.54 for arm64`.
    pub fn describe(&self) -> String {
        format!("{} {} {} for {}", self.kind, self.name, self.version, self.arch)
    }
}

/// The kernel profile new images get.
pub const KERNEL_PROFILE: &str = "base";
/// The kernel version new images get: vmkit's pinned LTS.
pub const KERNEL_VERSION: &str = "6.18.54";
/// Where vmkit publishes the `base` kernels (spec §8.2).
pub const KERNEL_REPOSITORY: &str = "ghcr.io/alfonsocampodonico/vmkit-kernels/base";
/// The `kiln-init` this build boots: the one released with it.
pub const INIT_VERSION: &str = env!("CARGO_PKG_VERSION");
/// The profile of kernels the user supplies (`--kernel`); never pinned.
pub const CUSTOM_PROFILE: &str = "custom";

/// The compiled-in table (ruling 7).
///
/// Empty until the artifacts are released; until then images are converted without
/// boot layers and `kiln run` needs `--kernel` and `--init`.
/// - **Kernels:** vmkit's `kernels-6.18.54` release. Each arch gets one entry, from
///   that release's OCI artifact `KERNEL_REPOSITORY:6.18.54-<arch>`: the digest and
///   size of its kernel blob (the artifact's layer, not its manifest). For example:
///   `Pin { kind: ArtifactKind::Kernel, name: KERNEL_PROFILE, version: KERNEL_VERSION,
///   arch: "arm64", repository: KERNEL_REPOSITORY, digest: "sha256:<hex>", size: <bytes> },`
/// - **kiln-init:** kiln's own releases (milestone M3c), `name: "kiln-init"`,
///   `version: INIT_VERSION`.
pub const PINS: &[Pin] = &[
    // The kernel entries (vmkit `kernels-6.18.54`, arm64 and amd64) go here.
];

/// The pinned kernel `profile`/`version` for `arch`.
pub fn kernel_pin(profile: &str, version: &str, arch: &str) -> Option<&'static Pin> {
    find(PINS, ArtifactKind::Kernel, profile, version, arch)
}

/// The pinned `kiln-init` of this build for `arch`.
pub fn init_pin(arch: &str) -> Option<&'static Pin> {
    find(PINS, ArtifactKind::Init, "kiln-init", INIT_VERSION, arch)
}

/// The pin in `table` for `kind`, `name`, `version` and `arch`.
pub fn find<'a>(table: &'a [Pin], kind: ArtifactKind, name: &str, version: &str, arch: &str) -> Option<&'a Pin> {
    table
        .iter()
        .find(|p| p.kind == kind && p.name == name && p.version == version && p.arch == arch)
}

/// Gets a pinned blob into the store from its registry.
pub trait Fetcher {
    fn fetch(&self, pin: &Pin, store: &Store) -> Result<()>;
}

/// Fetches from the pin's registry with the Docker config's credentials. The
/// client is built per fetch, so a run that finds everything in the store never
/// resolves a registry name.
#[derive(Debug, Clone)]
pub struct RegistryFetcher {
    pub config: DockerConfig,
}

impl RegistryFetcher {
    pub fn from_env() -> Self {
        Self {
            config: DockerConfig::from_env(),
        }
    }
}

impl Fetcher for RegistryFetcher {
    fn fetch(&self, pin: &Pin, store: &Store) -> Result<()> {
        let r = Reference::parse(&format!("{}@{}", pin.repository, pin.digest))?;
        let client = Client::new(r.registry(), self.config.clone())?;
        client.fetch_blob(r.repository(), &pin.digest()?, pin.size, store)?;
        Ok(())
    }
}

/// Makes `pin`'s blob available in the store, verified against its digest and size
/// (T1): from the store when it is there, else from `fetcher`.
pub fn ensure(store: &Store, pin: &Pin, fetcher: &dyn Fetcher) -> Result<Digest> {
    let digest = pin.digest()?;
    if !store.has_blob(&digest) {
        fetcher.fetch(pin, store).map_err(|e| ImageError::Artifact {
            what: pin.describe(),
            source: Box::new(e),
        })?;
    }
    let size = store.blob_size(&digest)?;
    if size != pin.size {
        return Err(ImageError::Store(kiln_store::StoreError::SizeMismatch {
            digest,
            expected: pin.size,
            actual: size,
        }));
    }
    Ok(digest)
}

/// The `cache/init-layers` key of the init layer around the `kiln-init` binary `init`.
fn init_layer_key(init: &Digest) -> String {
    format!("{init}@{FORMAT_VERSION}")
}

/// The init layer around the `kiln-init` binary `init` (a blob in the store),
/// built once and cached by the binary's digest and the erofs format (spec §6.5).
pub fn init_layer_blob(store: &Store, init: &Digest) -> Result<Digest> {
    let key = init_layer_key(init);
    if let Some(d) = store.cache_get_blob(CacheKind::InitLayers, &key)? {
        return Ok(d);
    }
    let bin = std::fs::read(store.blob_path(init))?;
    let layer = init_layer(&bin, &store.tmp_dir())?;
    let d = store.put_bytes(&layer)?;
    store.cache_put(CacheKind::InitLayers, &key, &d.to_string())?;
    Ok(d)
}

/// What `kiln gc` keeps although no ref reaches it (ruling 3): every blob the
/// compiled table pins, and the init layers built from its `kiln-init` binaries.
pub fn gc_roots(store: &Store) -> kiln_store::Result<Vec<Digest>> {
    gc_roots_in(PINS, store)
}

/// [`gc_roots`] for another table (tests).
pub fn gc_roots_in(table: &[Pin], store: &Store) -> kiln_store::Result<Vec<Digest>> {
    let mut roots = Vec::new();
    for p in table {
        let d = Digest::parse(p.digest)?;
        if p.kind == ArtifactKind::Init
            && let Some(layer) = store.cache_get_blob(CacheKind::InitLayers, &init_layer_key(&d))?
        {
            roots.push(layer);
        }
        roots.push(d);
    }
    Ok(roots)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_digests_are_valid_and_unique() {
        for (i, p) in PINS.iter().enumerate() {
            p.digest().unwrap();
            assert!(p.arch == "arm64" || p.arch == "amd64", "{p:?}");
            assert!(p.size > 0, "{p:?}");
            assert!(
                PINS[..i]
                    .iter()
                    .all(|q| (q.kind, q.name, q.version, q.arch) != (p.kind, p.name, p.version, p.arch)),
                "{p:?} is pinned twice"
            );
        }
    }

    #[test]
    fn lookups_match_every_field() {
        let table = [Pin {
            kind: ArtifactKind::Kernel,
            name: "base",
            version: "6.18.54",
            arch: "arm64",
            repository: "ghcr.io/o/vmkit-kernels/base",
            digest: "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            size: 1,
        }];
        assert!(find(&table, ArtifactKind::Kernel, "base", "6.18.54", "arm64").is_some());
        assert!(find(&table, ArtifactKind::Kernel, "base", "6.18.54", "amd64").is_none());
        assert!(find(&table, ArtifactKind::Kernel, "base", "6.18.55", "arm64").is_none());
        assert!(find(&table, ArtifactKind::Init, "base", "6.18.54", "arm64").is_none());
        assert_eq!(table[0].describe(), "kernel base 6.18.54 for arm64");
        let bad = Pin {
            digest: "sha256:nothex",
            ..table[0].clone()
        };
        assert!(bad.digest().is_err());
    }
}
```

- [ ] **Step 9: Modify `crates/kiln-image/src/lib.rs`**

Export the module.

In `crates/kiln-image/src/lib.rs`, replace:
```rust

mod convert;
```
with:
```rust

pub mod artifacts;
mod convert;
```

- [ ] **Step 10: Modify `crates/kiln/src/main.rs`**

`kiln gc` keeps the pinned artifacts.

In `crates/kiln/src/main.rs`, replace:
```rust
            // a later convert re-verifies them from the source.
            let r = open_store(&cli)?.gc()?;
```
with:
```rust
            // a later convert re-verifies them from the source. Pinned kernels and
            // kiln-init stay, so cached runs keep working offline (spec §5.2).
            let r = open_store(&cli)?.gc_with_roots(kiln_image::artifacts::gc_roots)?;
```

- [ ] **Step 11: Modify `docs/format.md`**

The store's informative description gains the cache and the GC roots.

In `docs/format.md`, replace:
```markdown
`$KILN_HOME` (default `~/.local/share/kiln`) holds `blobs/sha256/<hex>`, `refs.json` (`{"refs": {"<name>": "<digest>"}}`), and four caches of small text files. Each entry is written only after the layer it describes has been verified and committed:
```
with:
```markdown
`$KILN_HOME` (default `~/.local/share/kiln`) holds `blobs/sha256/<hex>`, `refs.json` (`{"refs": {"<name>": "<digest>"}}`), and five caches of small text files. Each entry is written only after the layer it describes has been verified and committed:
```

In `docs/format.md`, replace:
```markdown
- `cache/warnings/<layer cache key>`: the JSON list of warnings converting that layer produced, so a cache hit reports them too. Removed by GC together with the `cache/layers/` entry of the same name.

```
with:
```markdown
- `cache/warnings/<layer cache key>`: the JSON list of warnings converting that layer produced, so a cache hit reports them too. Removed by GC together with the `cache/layers/` entry of the same name.
- `cache/init-layers/<kiln-init binary digest>@<format version>`: the digest of the init layer built around that `kiln-init` binary (see Init layer below).

GC keeps every blob reachable from `refs.json`, and also kiln's pinned boot artifacts (the kernel and `kiln-init` blobs its compiled table names) with the init layers built from the pinned `kiln-init` binaries, so runs that need no network keep working after `kiln gc`.

```

- [ ] **Step 12: Run the crate's tests**

Run:
```bash
cargo test -q -p kiln-image && cargo test -q -p kiln-store
```
Expected: all pass (71 tests on macOS, 71 on Linux) for kiln-image, with `tests/artifacts.rs`' 5 cases; kiln-store's 29 include `roots_are_kept_with_their_cache_entries`.

- [ ] **Step 13: Format, lint and run the whole workspace**

Run:
```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test --all -q
```
Expected: no clippy warnings; every test passes (404 tests).

- [ ] **Step 14: Lint and test on Linux**

In the Lima VM (`limactl shell vmkit`, then `cd` to the repository and `export CARGO_TARGET_DIR=$HOME/target-m3b2`):

Run (Linux):
```bash
cargo clippy --all-targets -- -D warnings && cargo test --all -q
```
Expected: no clippy warnings; every test passes (503 tests; the boot and `kiln run` cases skip themselves without their environment).

- [ ] **Step 15: Commit**

```bash
git add -A crates docs/format.md
git commit -m 'feat(kiln-image): the pinned artifact table, fetch by pinned digest, the init-layer cache and GC roots'
```

### Task 2: Boot layers: `convert` adds the pinned kernel and init layers; `kiln run` verifies an image's kernel and boots the pinned kiln-init

**Files:**
- Modify: `README.md`, `crates/kiln-image/src/convert.rs`, `crates/kiln-image/src/lib.rs`, `crates/kiln-image/tests/artifacts.rs`, `crates/kiln-run/Cargo.toml`, `crates/kiln-run/src/linux/run.rs`, `crates/kiln-run/src/prepare.rs`, `crates/kiln-run/tests/prepare.rs`, `crates/kiln/src/main.rs`, `crates/kiln/tests/cli.rs`, `crates/kiln/tests/run.rs`
- `Cargo.lock` (cargo updates it)

**Interfaces:**
- Consumes: Task 1's `artifacts` (`Pin`, `find`, `kernel_pin`, `init_pin`, `ensure`, `init_layer_blob`, `RegistryFetcher`, `Fetcher`, `INIT_VERSION`, `KERNEL_PROFILE`, `KERNEL_VERSION`, `CUSTOM_PROFILE`); M3b-1's `prepare`, `Run::start`, `RunInfo`.
- Produces:
  - `kiln_image::{Boot { kernel: Option<Pin>, init: Option<Pin> }, BootLayers::{Provisional (default), Pinned, Explicit(Boot)}}` with `BootLayers::for_arch(arch) -> Option<Boot>`; `ConvertOptions::boot`; `Converted::boot: Option<Boot>`.
  - `kiln_run::prepare`: `trait Pins { kernel(profile, version, arch), init(arch) -> Option<Pin> }`, `CompiledPins`, `Table<'a>(pub &'a [Pin])`, `InitLayer::{Stored(PathBuf), Built(Vec<u8>)}`; `prepare(&Store, &RunOptions, arch, &dyn Pins, &dyn Fetcher) -> Result<Prepared>` (`Prepared::init: InitLayer` replaces `init_layer: Vec<u8>`); `CUSTOM_PROFILE` re-exported from `artifacts`.
  - `kiln convert` adds the pinned boot layers (`BootLayers::Pinned`), prints a note for what is missing, and its `--json` summary gains `boot`; `kiln inspect` prints a `boot:` line.

The pins take effect (spec §6.5, §8.1, T4), still with an empty table.

- **convert:** each boot layer is added when its artifact is pinned for the image's architecture: the kernel layer first, then the init layer around the pinned kiln-init (P2; a kernel can be pinned before any kiln-init release). The config's `kernel` and `init` name what was added. Without either, the image is provisional, as M1 made it; `kiln convert` says what `kiln run` will need. The library default is `Provisional`; the CLI asks for `Pinned` (P1), so tests and other library callers never fetch from a real registry.
- **run:** `prepare` takes the table as `&dyn Pins` (P3). An image's kernel layer boots only when its digest is the one pinned for the `(profile, version, arch)` its config names; a kernel the table does not pin, or a layer with another digest, is refused with what to do; a `custom` profile keeps M3b-1's `--allow-custom-kernel` rule. A provisional image is refused, and once a kernel is pinned for the architecture the message says to convert it again. The init layer is the pinned kiln-init's, fetched by digest and built once (`cache/init-layers`), unless `--init` names one; it boots from its blob in the store. The image's own init layer still never boots.
- M3b-1's tests keep their meaning with an empty test table (`Table(&[])`), so they never depend on what the compiled table holds.

- [ ] **Step 1: Modify `crates/kiln-image/tests/artifacts.rs`**

convert with explicit pins: both boot layers first and named in the config, each fetched once; a kernel pinned alone; provisional images (the default, and `Pinned` while the table is empty).

At the end of `crates/kiln-image/tests/artifacts.rs`, append:
```rust

mod convert {
    use super::*;
    use kiln_image::types::{KILN_INIT, KILN_KERNEL, KILN_LAYER};
    use kiln_image::{Boot, BootLayers, ConvertOptions, LocalRequest, convert_local, load};
    use kiln_oci::Platform;
    use kiln_oci::testlayout::{LayoutBuilder, TestLayer};

    fn layout(dir: &std::path::Path) -> std::path::PathBuf {
        let path = dir.join("layout");
        let mut b = LayoutBuilder::new(&path);
        let tar = kiln_erofs::testtar::TarBuilder::new()
            .file("hello", b"hi", &kiln_erofs::testtar::Opts::default())
            .finish();
        let desc = b.image(
            &Platform::parse("linux/arm64").unwrap(),
            &[TestLayer::tar(tar)],
            Default::default(),
        );
        b.add(desc, Some("app"));
        b.finish();
        path
    }

    fn convert(store: &Store, path: &std::path::Path, boot: BootLayers) -> kiln_image::Output {
        let platforms = [Platform::parse("linux/arm64").unwrap()];
        let req = LocalRequest {
            source_ref: None,
            platforms: &platforms,
            tag: Some("t"),
        };
        let opts = ConvertOptions {
            boot,
            ..Default::default()
        };
        convert_local(store, path, &req, &opts).unwrap()
    }

    fn layer_types(store: &Store, out: &kiln_image::Output) -> Vec<String> {
        let m = &load(store, &out.digest).unwrap().entries[0].1;
        m.manifest.layers.iter().map(|l| l.media_type.clone()).collect()
    }

    #[test]
    fn pinned_boot_layers_come_first_and_name_their_versions() {
        let reg = TestRegistry::start(Config::default());
        let (kernel, init) = (b"a kernel Image".to_vec(), b"\x7fELF kiln-init".to_vec());
        reg.put_blob(&kernel);
        reg.put_blob(&init);
        let mut kpin = pin(&reg, &kernel);
        (kpin.kind, kpin.name, kpin.version) = (ArtifactKind::Kernel, "base", "6.18.54");
        let ipin = pin(&reg, &init);
        let src = tempfile::tempdir().unwrap();
        let path = layout(src.path());
        let home = tempfile::tempdir().unwrap();
        let store = Store::open(home.path()).unwrap();
        let boot = Boot {
            kernel: Some(kpin.clone()),
            init: Some(ipin.clone()),
        };
        let out = convert(&store, &path, BootLayers::Explicit(boot.clone()));
        assert_eq!(out.images[0].boot.as_ref(), Some(&boot));
        // Each fetched once, by digest, into the store.
        assert_eq!(reg.blob_requests("GET"), 2);
        assert_eq!(layer_types(&store, &out), [KILN_KERNEL, KILN_INIT, KILN_LAYER]);
        let m = &load(&store, &out.digest).unwrap().entries[0].1;
        assert_eq!(m.manifest.layers[0].digest, kpin.digest().unwrap());
        let init_layer = init_layer_blob(&store, &ipin.digest().unwrap()).unwrap();
        assert_eq!(m.manifest.layers[1].digest, init_layer);
        let k = m.config.kernel.as_ref().unwrap();
        assert_eq!((k.profile.as_str(), k.version.as_str()), ("base", "6.18.54"));
        assert_eq!(m.config.init.as_ref().unwrap().version, "0.0.0-test");
    }

    /// A kernel is pinned before any kiln-init is released: the image gets the
    /// kernel layer alone (and `kiln run` then needs only `--init`).
    #[test]
    fn each_boot_layer_is_added_when_it_is_pinned() {
        let reg = TestRegistry::start(Config::default());
        let kernel = b"a kernel Image".to_vec();
        reg.put_blob(&kernel);
        let mut kpin = pin(&reg, &kernel);
        (kpin.kind, kpin.name, kpin.version) = (ArtifactKind::Kernel, "base", "6.18.54");
        let src = tempfile::tempdir().unwrap();
        let path = layout(src.path());
        let home = tempfile::tempdir().unwrap();
        let store = Store::open(home.path()).unwrap();
        let boot = Boot {
            kernel: Some(kpin),
            init: None,
        };
        let out = convert(&store, &path, BootLayers::Explicit(boot));
        assert_eq!(layer_types(&store, &out), [KILN_KERNEL, KILN_LAYER]);
        let m = &load(&store, &out.digest).unwrap().entries[0].1;
        assert!(m.config.kernel.is_some() && m.config.init.is_none());
    }

    /// Provisional: no boot layers, as before M3. It is the library's default, and
    /// what `Pinned` gives while the compiled table pins nothing.
    #[test]
    fn provisional_images_have_no_boot_layers() {
        let src = tempfile::tempdir().unwrap();
        let path = layout(src.path());
        let home = tempfile::tempdir().unwrap();
        let store = Store::open(home.path()).unwrap();
        for boot in [BootLayers::default(), BootLayers::Explicit(Boot::default())] {
            let out = convert(&store, &path, boot.clone());
            assert!(out.images[0].boot.is_none(), "{boot:?}");
            assert_eq!(layer_types(&store, &out), [KILN_LAYER], "{boot:?}");
            let m = &load(&store, &out.digest).unwrap().entries[0].1;
            assert!(m.config.kernel.is_none() && m.config.init.is_none());
        }
        assert_eq!(BootLayers::default(), BootLayers::Provisional);
        if kiln_image::artifacts::PINS.is_empty() {
            assert_eq!(BootLayers::Pinned.for_arch("arm64"), None);
        }
    }
}
```

- [ ] **Step 2: Modify `crates/kiln-run/tests/prepare.rs`**

`prepare` with test tables: the pinned kernel boots and the pinned kiln-init is fetched once, then served from the store; a kernel layer that is not the pinned one; a provisional image after kernels are pinned; an unreachable pinned kiln-init. M3b-1's cases run with an empty table.

In `crates/kiln-run/tests/prepare.rs`, replace:
```rust
//! This kiln pins no kernel or kiln-init yet, so every kernel and init is the user's.

use std::path::PathBuf;

use kiln_erofs::testtar::{Opts, TarBuilder};
```
with:
```rust
//! The compiled table pins nothing yet, so each test passes its own: none, or pins
//! for blobs it puts in the store or serves from the in-process registry.

use std::cell::Cell;
use std::path::PathBuf;

use kiln_erofs::testtar::{Opts, TarBuilder};
use kiln_image::artifacts::{ArtifactKind, Fetcher, INIT_VERSION, Pin, RegistryFetcher};
```

In `crates/kiln-run/tests/prepare.rs`, replace:
```rust
use kiln_run::RunOptions;
use kiln_run::prepare::prepare;
use kiln_store::Store;
```
with:
```rust
use kiln_registry::DockerConfig;
use kiln_registry::testregistry::{Config, TestRegistry};
use kiln_run::RunOptions;
use kiln_run::prepare::{InitLayer, Pins, Prepared, Table};
use kiln_store::{Digest, Store};
```

In `crates/kiln-run/tests/prepare.rs`, replace:
```rust
fn err(r: kiln_run::Result<kiln_run::prepare::Prepared>) -> String {
    r.map(|_| ()).unwrap_err().to_string()
```
with:
```rust
fn err(r: kiln_run::Result<Prepared>) -> String {
    r.map(|_| ()).unwrap_err().to_string()
}

/// No pins, and a fetcher that must never be asked.
fn prepare(store: &Store, o: &RunOptions, arch: &str) -> kiln_run::Result<Prepared> {
    kiln_run::prepare::prepare(store, o, arch, &Table(&[]), &Offline::default())
}

/// Fails the test's fetch if asked: the store must have answered.
#[derive(Default)]
struct Offline(Cell<usize>);

impl Fetcher for Offline {
    fn fetch(&self, _: &Pin, _: &Store) -> kiln_image::Result<()> {
        self.0.set(self.0.get() + 1);
        Err(kiln_image::ImageError::BadOption("offline".into()))
    }
}

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// The kernel `with_boot_layers` puts in images, pinned as `base` 6.18.54.
fn kernel_pin(bytes: &[u8]) -> Pin {
    Pin {
        kind: ArtifactKind::Kernel,
        name: "base",
        version: "6.18.54",
        arch: ARCH,
        repository: "127.0.0.1:1/unused",
        digest: leak(Digest::of(bytes).to_string()),
        size: bytes.len() as u64,
    }
}

/// This build's kiln-init, served by `reg`.
fn init_pin(reg: &TestRegistry, bytes: &[u8]) -> Pin {
    Pin {
        kind: ArtifactKind::Init,
        name: "kiln-init",
        version: INIT_VERSION,
        arch: ARCH,
        repository: leak(format!("{}/kiln/kiln-init", reg.host())),
        digest: leak(Digest::of(bytes).to_string()),
        size: bytes.len() as u64,
    }
```

In `crates/kiln-run/tests/prepare.rs`, replace:
```rust
    assert!(!p.init_layer.is_empty());
```
with:
```rust
    assert!(matches!(&p.init, InitLayer::Built(b) if !b.is_empty()));
```

In `crates/kiln-run/tests/prepare.rs`, replace:
```rust
/// T4 without pins: an image's kernel that names a pinned profile cannot be
/// verified and is refused; `--kernel` replaces it.
#[test]
fn an_image_kernel_is_refused_until_kiln_pins_kernels() {
```
with:
```rust
/// T4: an image's kernel that names a profile this kiln does not pin cannot be
/// verified and is refused; `--kernel` replaces it.
#[test]
fn an_image_kernel_kiln_does_not_pin_is_refused() {
```

In `crates/kiln-run/tests/prepare.rs`, replace:
```rust
        e.contains("kernel base 6.18.54 for arm64") && e.contains("cannot verify") && e.contains("--kernel PATH"),
```
with:
```rust
        e.contains("does not pin the kernel base 6.18.54 for arm64") && e.contains("--kernel PATH"),
```

In `crates/kiln-run/tests/prepare.rs`, replace:
```rust
        p.init_layer,
        kiln_image::init_layer(b"\x7fELF my own init", &s.store.tmp_dir()).unwrap()
```
with:
```rust
        p.init,
        InitLayer::Built(kiln_image::init_layer(b"\x7fELF my own init", &s.store.tmp_dir()).unwrap())
```

In `crates/kiln-run/tests/prepare.rs`, replace:
```rust
    s.edited("ok", |_, l| assert!(l.iter().all(|d| d.media_type == KILN_LAYER)));
}
```
with:
```rust
    s.edited("ok", |_, l| assert!(l.iter().all(|d| d.media_type == KILN_LAYER)));
}

/// T4 with pins: the image's kernel boots when it is the pinned one; the init
/// layer is the pinned kiln-init's, fetched once by digest and then built once
/// (`cache/init-layers`); later runs need no registry. Neither is "custom".
#[test]
fn the_pinned_kernel_boots_and_the_pinned_init_is_fetched_once() {
    let s = setup();
    s.with_boot_layers("booted", "base", b"\x7fELF the image's init");
    let reg = TestRegistry::start(Config::default());
    let init = b"\x7fELF the pinned kiln-init".repeat(10);
    reg.put_blob(&init);
    let table = [kernel_pin(b"an image's kernel"), init_pin(&reg, &init)];
    let o = RunOptions::new("booted");
    let fetcher = RegistryFetcher {
        config: DockerConfig::anonymous(),
    };
    let p = kiln_run::prepare::prepare(&s.store, &o, ARCH, &Table(&table), &fetcher).unwrap();
    assert_eq!(std::fs::read(&p.kernel).unwrap(), b"an image's kernel");
    assert_eq!((p.custom_kernel.as_ref(), p.custom_init.as_ref()), (None, None));
    let InitLayer::Stored(path) = &p.init else {
        panic!("{:?}", p.init)
    };
    assert_eq!(
        std::fs::read(path).unwrap(),
        kiln_image::init_layer(&init, &s.store.tmp_dir()).unwrap()
    );
    assert_eq!(reg.blob_requests("GET"), 1);
    // The image's own init layer differs: replaced, with one warning and nothing else.
    assert_eq!(p.warnings.len(), 1, "{:?}", p.warnings);
    assert!(p.warnings[0].contains("is not the one booting"), "{:?}", p.warnings);
    // Cached: the second run asks no registry.
    let offline = Offline::default();
    let again = kiln_run::prepare::prepare(&s.store, &o, ARCH, &Table(&table), &offline).unwrap();
    assert_eq!(again.init, p.init);
    assert_eq!(offline.0.get(), 0);
}

/// A kernel layer whose digest is not the pinned one is refused (T4), whatever
/// its config claims; `--kernel` with `--allow-custom-kernel` still boots another.
#[test]
fn a_kernel_layer_that_is_not_the_pinned_one_is_refused() {
    let s = setup();
    s.with_boot_layers("booted", "base", b"\x7fELF the image's init");
    let table = [kernel_pin(b"the real kernel")];
    let mut o = RunOptions::new("booted");
    o.init = Some(s.files.join("init"));
    o.allow_custom_init = true;
    let none = Offline::default();
    let e = err(kiln_run::prepare::prepare(&s.store, &o, ARCH, &Table(&table), &none));
    assert!(
        e.contains("is not the pinned kernel base 6.18.54 for arm64") && e.contains("convert the image again"),
        "{e}"
    );
    s.custom(&mut o);
    kiln_run::prepare::prepare(&s.store, &o, ARCH, &Table(&table), &none).unwrap();
}

/// A provisional image converted before the table pinned a kernel: convert it
/// again (or name a kernel). A pinned kiln-init that cannot be fetched is named.
#[test]
fn a_provisional_image_after_kernels_are_pinned_and_an_unreachable_init() {
    let s = setup();
    let reg = TestRegistry::start(Config::default());
    let table = [kernel_pin(b"a kernel"), init_pin(&reg, b"\x7fELF never served")];
    let none = Offline::default();
    let mut o = RunOptions::new("app");
    let e = err(kiln_run::prepare::prepare(&s.store, &o, ARCH, &Table(&table), &none));
    assert!(
        e.contains("converted before this kiln pinned a kernel for arm64") && e.contains("convert it again"),
        "{e}"
    );
    o.kernel = Some(s.files.join("kernel"));
    o.allow_custom_kernel = true;
    let e = err(kiln_run::prepare::prepare(&s.store, &o, ARCH, &Table(&table), &none));
    assert!(
        e.contains(&format!(
            "fetching the pinned kiln-init kiln-init {INIT_VERSION} for arm64"
        )),
        "{e}"
    );
    assert_eq!(none.0.get(), 1);
}

/// The compiled table and a test table answer the same questions.
#[test]
fn pins_are_looked_up_by_profile_version_and_arch() {
    let reg = TestRegistry::start(Config::default());
    let table = [kernel_pin(b"k"), init_pin(&reg, b"i")];
    let pins: &dyn Pins = &Table(&table);
    assert!(pins.kernel("base", "6.18.54", ARCH).is_some());
    assert!(pins.kernel("base", "6.18.54", "amd64").is_none());
    assert!(pins.kernel("custom", "6.18.54", ARCH).is_none());
    assert!(pins.init(ARCH).is_some() && pins.init("amd64").is_none());
}
```

- [ ] **Step 3: Run the tests to see them fail**

Run:
```bash
cargo test -q -p kiln-image --test artifacts; cargo test -q -p kiln-run --test prepare
```
Expected: FAIL to compile: `kiln_image::Boot` and `BootLayers`, and `kiln_run::prepare::{InitLayer, Pins, Table}`, do not exist yet.

- [ ] **Step 4: Modify `crates/kiln-image/src/convert.rs`**

The boot layers.

In `crates/kiln-image/src/convert.rs`, replace:
```rust

use crate::ctx::{ctx_hash, parents_entry, parse_parents};
```
with:
```rust

use crate::artifacts::{self, Pin, RegistryFetcher};
use crate::ctx::{ctx_hash, parents_entry, parse_parents};
```

In `crates/kiln-image/src/convert.rs`, replace:
```rust
    pub jobs: usize,
}
```
with:
```rust
    pub jobs: usize,
    /// Which kernel and init layers to add (spec §6.5).
    pub boot: BootLayers,
}

/// The kernel and `kiln-init` an image boots, as pins (spec §8.1). Each is added
/// when it is pinned: an image may carry the kernel layer before a `kiln-init`
/// release exists (`kiln run` then needs only `--init`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Boot {
    pub kernel: Option<Pin>,
    pub init: Option<Pin>,
}

/// Which boot layers `convert` adds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum BootLayers {
    /// None: a provisional image. The default, so that library callers and tests
    /// never fetch boot artifacts unless they ask; `kiln convert` asks for `Pinned`.
    #[default]
    Provisional,
    /// The compiled-in pins (`kiln_image::artifacts::PINS`) for the image's
    /// architecture: the `base` kernel and this build's `kiln-init`, each when pinned.
    Pinned,
    /// These artifacts (tests, and later explicit choices).
    Explicit(Boot),
}

impl BootLayers {
    /// The pins to add for `arch`; `None` when there are none (a provisional image).
    pub fn for_arch(&self, arch: &str) -> Option<Boot> {
        let boot = match self {
            BootLayers::Provisional => return None,
            BootLayers::Pinned => Boot {
                kernel: artifacts::kernel_pin(artifacts::KERNEL_PROFILE, artifacts::KERNEL_VERSION, arch).cloned(),
                init: artifacts::init_pin(arch).cloned(),
            },
            BootLayers::Explicit(b) => b.clone(),
        };
        (boot.kernel.is_some() || boot.init.is_some()).then_some(boot)
    }
}
```

In `crates/kiln-image/src/convert.rs`, replace:
```rust
            jobs: std::thread::available_parallelism().map_or(4, |n| n.get()),
        }
```
with:
```rust
            jobs: std::thread::available_parallelism().map_or(4, |n| n.get()),
            boot: BootLayers::default(),
        }
```

In `crates/kiln-image/src/convert.rs`, replace:
```rust
    pub squashed: usize,
}
```
with:
```rust
    pub squashed: usize,
    /// The kernel and init layers the image carries; `None` for a provisional image.
    pub boot: Option<Boot>,
}
```

In `crates/kiln-image/src/convert.rs`, replace:
```rust

    let config = KilnConfig {
```
with:
```rust

    // The kernel, then the init layer (spec §5.1), each fetched by its pinned digest.
    let boot = opts.boot.for_arch(&platform.architecture);
    let (kernel_pin, init_pin) = boot
        .as_ref()
        .map_or((None, None), |b| (b.kernel.as_ref(), b.init.as_ref()));
    let mut boot_layers = Vec::new();
    let fetcher = RegistryFetcher::from_env();
    if let Some(pin) = kernel_pin {
        let kernel = artifacts::ensure(store, pin, &fetcher)?;
        boot_layers.push(Descriptor::new(KILN_KERNEL, kernel.clone(), store.blob_size(&kernel)?));
    }
    if let Some(pin) = init_pin {
        let init_bin = artifacts::ensure(store, pin, &fetcher)?;
        let init = artifacts::init_layer_blob(store, &init_bin)?;
        boot_layers.push(Descriptor::new(KILN_INIT, init.clone(), store.blob_size(&init)?));
    }
    let config = KilnConfig {
```

In `crates/kiln-image/src/convert.rs`, replace:
```rust
        kernel: None,
        init: None,
```
with:
```rust
        kernel: kernel_pin.map(|p| KernelRef {
            profile: p.name.to_string(),
            version: p.version.to_string(),
        }),
        init: init_pin.map(|p| InitRef {
            version: p.version.to_string(),
        }),
```

In `crates/kiln-image/src/convert.rs`, replace:
```rust
        layers: reports.iter().map(layer_descriptor).collect(),
```
with:
```rust
        layers: boot_layers
            .into_iter()
            .chain(reports.iter().map(layer_descriptor))
            .collect(),
```

In `crates/kiln-image/src/convert.rs`, replace:
```rust
        squashed,
    })
```
with:
```rust
        squashed,
        boot,
    })
```

- [ ] **Step 5: Modify `crates/kiln-image/src/lib.rs`**

Export `Boot` and `BootLayers`.

In `crates/kiln-image/src/lib.rs`, replace:
```rust
pub use convert::{ConvertOptions, Converted, LayerReport, LayerSource, StoredLayers, convert_image};
```
with:
```rust
pub use convert::{Boot, BootLayers, ConvertOptions, Converted, LayerReport, LayerSource, StoredLayers, convert_image};
```

- [ ] **Step 6: Modify `crates/kiln-run/Cargo.toml`**

The tests use the in-process registry.

In `crates/kiln-run/Cargo.toml`, replace:
```toml
kiln-oci = { path = "../kiln-oci" }
tempfile = "3.27.0"
```
with:
```toml
kiln-oci = { path = "../kiln-oci" }
kiln-registry = { path = "../kiln-registry" }
tempfile = "3.27.0"
```

- [ ] **Step 7: Modify `crates/kiln-run/src/prepare.rs`**

T4 with a table.

In `crates/kiln-run/src/prepare.rs`, replace:
```rust
//!
//! The kernels and kiln-init that §8.1's table pins have not been released yet, so
//! this kiln pins none, and every kernel and init that boots is the user's choice:
//! - the kernel is `--kernel PATH`, or an image's `custom`-profile kernel layer, each
//!   only with `--allow-custom-kernel`. An image's kernel layer that names another
//!   profile cannot be verified against a pin, so it is refused;
//! - the init layer is always built from `--init PATH` (with `--allow-custom-init`).
//!   An image's own init layer never boots: it is replaced, with a warning when it
//!   differs.

use std::path::{Path, PathBuf};

```
with:
```rust
//! - The kernel is the image's kernel layer, only when its digest is the one the
//!   table pins for the `(profile, version, arch)` its config names; or `--kernel
//!   PATH`, or an image's `custom`-profile kernel layer, each only with
//!   `--allow-custom-kernel`.
//! - The init layer is always the pinned `kiln-init`'s, fetched by digest and built
//!   once (`cache/init-layers`), or built from `--init PATH` (with
//!   `--allow-custom-init`). An image's own init layer never boots: it is replaced,
//!   with a warning when it differs.
//!
//! The table ([`kiln_image::artifacts::PINS`]) is empty until vmkit's kernel and
//! kiln's kiln-init releases exist (ruling 7); until then both flags are needed.

use std::path::{Path, PathBuf};

pub use kiln_image::artifacts::CUSTOM_PROFILE;
use kiln_image::artifacts::{self, ArtifactKind, Fetcher, Pin};
```

In `crates/kiln-run/src/prepare.rs`, replace:
```rust
/// The kernel profile of images that boot a kernel kiln does not pin (spec §8.1).
pub const CUSTOM_PROFILE: &str = "custom";
/// The largest `--init` binary read (a static kiln-init is a few MiB).
const MAX_INIT_BINARY: u64 = 64 << 20;
```
with:
```rust
/// The largest `--init` binary read (a static kiln-init is a few MiB).
const MAX_INIT_BINARY: u64 = 64 << 20;

/// The trusted artifacts: the compiled-in table, or another one in tests.
pub trait Pins {
    fn kernel(&self, profile: &str, version: &str, arch: &str) -> Option<Pin>;
    fn init(&self, arch: &str) -> Option<Pin>;
}

/// [`kiln_image::artifacts::PINS`].
#[derive(Debug, Clone, Copy, Default)]
pub struct CompiledPins;

impl Pins for CompiledPins {
    fn kernel(&self, profile: &str, version: &str, arch: &str) -> Option<Pin> {
        artifacts::kernel_pin(profile, version, arch).cloned()
    }

    fn init(&self, arch: &str) -> Option<Pin> {
        artifacts::init_pin(arch).cloned()
    }
}

/// Another table of pins (tests).
#[derive(Debug, Clone, Copy)]
pub struct Table<'a>(pub &'a [Pin]);

impl Pins for Table<'_> {
    fn kernel(&self, profile: &str, version: &str, arch: &str) -> Option<Pin> {
        artifacts::find(self.0, ArtifactKind::Kernel, profile, version, arch).cloned()
    }

    fn init(&self, arch: &str) -> Option<Pin> {
        artifacts::find(self.0, ArtifactKind::Init, "kiln-init", artifacts::INIT_VERSION, arch).cloned()
    }
}

/// The init layer that boots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InitLayer {
    /// The pinned kiln-init's layer: a blob in the store.
    Stored(PathBuf),
    /// Built from `--init` for this run.
    Built(Vec<u8>),
}
```

In `crates/kiln-run/src/prepare.rs`, replace:
```rust
    /// The init layer (an erofs image) built for this run.
    pub init_layer: Vec<u8>,
```
with:
```rust
    /// The init layer (an erofs image).
    pub init: InitLayer,
```

In `crates/kiln-run/src/prepare.rs`, replace:
```rust
/// Resolves `opts.image` for `arch` and decides the kernel and init (spec §8.1).
pub fn prepare(store: &Store, opts: &RunOptions, arch: &str) -> Result<Prepared> {
```
with:
```rust
/// Resolves `opts.image` for `arch` and decides the kernel and init (spec §8.1)
/// against `pins`; a pinned kiln-init missing from the store comes from `fetcher`.
pub fn prepare(
    store: &Store,
    opts: &RunOptions,
    arch: &str,
    pins: &dyn Pins,
    fetcher: &dyn Fetcher,
) -> Result<Prepared> {
```

In `crates/kiln-run/src/prepare.rs`, replace:
```rust
    let kernel = if let Some(path) = &opts.kernel {
```
with:
```rust
    let (kernel, custom_kernel) = if let Some(path) = &opts.kernel {
```

In `crates/kiln-run/src/prepare.rs`, replace:
```rust
        path
```
with:
```rust
        (path.clone(), Some(path))
```

In `crates/kiln-run/src/prepare.rs`, replace:
```rust
        if k.profile != CUSTOM_PROFILE {
            return Err(Error::refused(format!(
                "{shown}'s kernel layer is the kernel {} {} for {arch}, which this kiln cannot verify: it pins \
                 no kernels yet; pass --kernel PATH --allow-custom-kernel",
                clean_line(&k.profile),
                clean_line(&k.version)
            )));
        }
        if !opts.allow_custom_kernel {
            return Err(Error::refused(format!(
                "{shown} boots a custom kernel; pass --allow-custom-kernel to accept it"
            )));
```
with:
```rust
        let (profile, version) = (clean_line(&k.profile), clean_line(&k.version));
        let custom = k.profile == CUSTOM_PROFILE;
        if custom {
            if !opts.allow_custom_kernel {
                return Err(Error::refused(format!(
                    "{shown} boots a custom kernel; pass --allow-custom-kernel to accept it"
                )));
            }
            warnings.push(format!("booting {shown}'s custom kernel (--allow-custom-kernel)"));
        } else {
            match pins.kernel(&k.profile, &k.version, arch) {
                Some(pin) if pin.digest()? == *layer => {}
                Some(_) => {
                    return Err(Error::refused(format!(
                        "{shown}'s kernel layer {layer} is not the pinned kernel {profile} {version} for {arch}; \
                         kiln refuses it (convert the image again, or pass --kernel PATH --allow-custom-kernel)"
                    )));
                }
                None => {
                    return Err(Error::refused(format!(
                        "this kiln does not pin the kernel {profile} {version} for {arch} that {shown} names; \
                         pass --kernel PATH --allow-custom-kernel to boot another"
                    )));
                }
            }
```

In `crates/kiln-run/src/prepare.rs`, replace:
```rust
        warnings.push(format!("booting {shown}'s custom kernel (--allow-custom-kernel)"));
        store.blob_path(layer)
```
with:
```rust
        let path = store.blob_path(layer);
        (path.clone(), custom.then_some(path))
    } else if pins
        .kernel(artifacts::KERNEL_PROFILE, artifacts::KERNEL_VERSION, arch)
        .is_some()
    {
        return Err(Error::refused(format!(
            "{shown} has no kernel layer: it was converted before this kiln pinned a kernel for {arch}; \
             convert it again, or pass --kernel PATH --allow-custom-kernel"
        )));
```

In `crates/kiln-run/src/prepare.rs`, replace:
```rust
    let Some(init_path) = &opts.init else {
```
with:
```rust
    let (init, init_digest, custom_init) = if let Some(path) = &opts.init {
        let bin = read_capped(path)?;
        let built = kiln_image::init_layer(&bin, &store.tmp_dir())?;
        warnings.push(format!(
            "booting the custom kiln-init {} (--allow-custom-init)",
            path.display()
        ));
        let d = Digest::of(&built);
        (InitLayer::Built(built), d, Some(path.clone()))
    } else if let Some(pin) = pins.init(arch) {
        let bin = artifacts::ensure(store, &pin, fetcher)?;
        let d = artifacts::init_layer_blob(store, &bin)?;
        (InitLayer::Stored(store.blob_path(&d)), d, None)
    } else {
```

In `crates/kiln-run/src/prepare.rs`, replace:
```rust
    let bin = read_capped(init_path)?;
    let built = kiln_image::init_layer(&bin, &store.tmp_dir())?;
    warnings.push(format!(
        "booting the custom kiln-init {} (--allow-custom-init)",
        init_path.display()
    ));
    let built_digest = Digest::of(&built);
    if let Some(theirs) = &init_layer
        && *theirs != built_digest
    {
        warnings.push(format!(
            "{shown}'s init layer {theirs} is not the one booting; it is replaced by {built_digest}"
```
with:
```rust
    if let Some(theirs) = &init_layer
        && *theirs != init_digest
    {
        warnings.push(format!(
            "{shown}'s init layer {theirs} is not the one booting; it is replaced by {init_digest}"
```

In `crates/kiln-run/src/prepare.rs`, replace:
```rust
        custom_kernel: Some(kernel.clone()),
        kernel,
        init_layer: built,
        custom_init: Some(init_path.clone()),
```
with:
```rust
        kernel,
        custom_kernel,
        init,
        custom_init,
```

- [ ] **Step 8: Modify `crates/kiln-run/src/linux/run.rs`**

The compiled table and the registry fetcher; the pinned init layer boots from the store.

In `crates/kiln-run/src/linux/run.rs`, replace:
```rust

use kiln_proto::{ExitMethod, GUEST_CID};
```
with:
```rust

use kiln_image::artifacts::RegistryFetcher;
use kiln_proto::{ExitMethod, GUEST_CID};
```

In `crates/kiln-run/src/linux/run.rs`, replace:
```rust
use crate::prepare;
```
with:
```rust
use crate::prepare::{self, InitLayer};
```

In `crates/kiln-run/src/linux/run.rs`, replace:
```rust
        let prepared = prepare::prepare(store, opts, prepare::host_arch())?;
```
with:
```rust
        let prepared = prepare::prepare(
            store,
            opts,
            prepare::host_arch(),
            &prepare::CompiledPins,
            &RegistryFetcher::from_env(),
        )?;
```

In `crates/kiln-run/src/linux/run.rs`, replace:
```rust
        let init_layer = run_dir.path.join("init.erofs");
        std::fs::write(&init_layer, &prepared.init_layer)?;
```
with:
```rust
        // The pinned kiln-init's layer is a blob in the store, opened by the VMM while
        // the lock is held; one built from --init goes in the run directory.
        let init_layer = match &prepared.init {
            InitLayer::Stored(path) => path.clone(),
            InitLayer::Built(bytes) => {
                let path = run_dir.path.join("init.erofs");
                std::fs::write(&path, bytes)?;
                path
            }
        };
```

- [ ] **Step 9: Modify `crates/kiln/src/main.rs`**

`convert` asks for the pinned boot layers and says what is missing; `inspect`'s boot line.

In `crates/kiln/src/main.rs`, replace:
```rust
    ConvertOptions, LocalRequest, Output, RegistryRequest, convert_local, convert_registry, import_image, load,
    pull_image, push_image, resolve_name,
```
with:
```rust
    BootLayers, ConvertOptions, LocalRequest, Output, RegistryRequest, convert_local, convert_registry, import_image,
    load, pull_image, push_image, resolve_name,
```

In `crates/kiln/src/main.rs`, replace:
```rust
            max_layers: self.max_layers,
            ..Default::default()
```
with:
```rust
            max_layers: self.max_layers,
            // The pinned kernel and kiln-init (spec §6.5), fetched by digest.
            boot: BootLayers::Pinned,
            ..Default::default()
```

In `crates/kiln/src/main.rs`, replace:
```rust

fn convert_summary(out: &Output, tag: &str) -> serde_json::Value {
```
with:
```rust

/// What `kiln run` will need for an image `convert` made without some boot layer
/// (the table pins no kernel or kiln-init for its architecture yet).
fn boot_note(c: &kiln_image::Converted) -> Option<String> {
    let b = c.boot.clone().unwrap_or_default();
    let missing: Vec<(&str, &str)> = [
        ("kernel", "--kernel", b.kernel.is_none()),
        ("kiln-init", "--init", b.init.is_none()),
    ]
    .into_iter()
    .filter(|m| m.2)
    .map(|m| (m.0, m.1))
    .collect();
    if missing.is_empty() {
        return None;
    }
    let names: Vec<&str> = missing.iter().map(|m| m.0).collect();
    let flags: Vec<&str> = missing.iter().map(|m| m.1).collect();
    Some(format!(
        "this kiln pins no {} for {} yet, so the image has no {} layer{}; `kiln run` needs {} for it",
        names.join(" or "),
        clean_line(&c.platform.architecture),
        names.join(" or "),
        if missing.len() > 1 { "s" } else { "" },
        flags.join(" and ")
    ))
}

/// `inspect`'s boot line: the kernel and kiln-init an image names (spec §8.1).
fn boot_line(config: &kiln_image::types::KilnConfig) -> String {
    let kernel = config
        .kernel
        .as_ref()
        .map(|k| format!("kernel {} {}", clean_line(&k.profile), clean_line(&k.version)));
    let init = config
        .init
        .as_ref()
        .map(|i| format!("kiln-init {}", clean_line(&i.version)));
    match (kernel, init) {
        (Some(k), Some(i)) => format!("{k}, {i}"),
        (Some(k), None) => format!("{k}, no init layer (run it with --init)"),
        (None, Some(i)) => format!("{i}, no kernel layer (run it with --kernel)"),
        (None, None) => "none (a provisional image: run it with --kernel and --init)".to_string(),
    }
}

fn convert_summary(out: &Output, tag: &str) -> serde_json::Value {
```

In `crates/kiln/src/main.rs`, replace:
```rust
            "squashed": c.squashed,
            "layers": c.layers.iter().map(|l| json!({
```
with:
```rust
            "squashed": c.squashed,
            "boot": c.boot.as_ref().map(|b| json!({
                "kernel": b.kernel.as_ref().map(|k| format!("{}@{}", k.name, k.version)),
                "init": b.init.as_ref().map(|i| i.version),
            })),
            "layers": c.layers.iter().map(|l| json!({
```

In `crates/kiln/src/main.rs`, replace:
```rust
            };
            if *json {
```
with:
```rust
            };
            for c in &out.images {
                if let Some(note) = boot_note(c) {
                    eprintln!("note: {note}");
                }
            }
            if *json {
```

In `crates/kiln/src/main.rs`, replace:
```rust
                );
                println!("  layers:");
```
with:
```rust
                );
                println!("  boot:       {}", boot_line(&m.config));
                println!("  layers:");
```

- [ ] **Step 10: Modify `crates/kiln/tests/cli.rs`**

The provisional note and the boot line.

In `crates/kiln/tests/cli.rs`, replace:
```rust
        .stdout(predicate::str::contains("app:latest → sha256:"));
```
with:
```rust
        .stdout(predicate::str::contains("app:latest → sha256:"))
        // The table pins nothing yet: a provisional image, and what `kiln run` needs for it.
        .stderr(predicate::str::contains(
            "note: this kiln pins no kernel or kiln-init for arm64 yet, so the image has no kernel or \
             kiln-init layers; `kiln run` needs --kernel and --init for it",
        ));
```

In `crates/kiln/tests/cli.rs`, replace:
```rust
                .and(predicate::str::contains("unverified provenance")),
```
with:
```rust
                .and(predicate::str::contains("unverified provenance"))
                .and(predicate::str::contains(
                    "boot:       none (a provisional image: run it with --kernel and --init)",
                )),
```

- [ ] **Step 11: Modify `crates/kiln/tests/run.rs`**

End to end: an image `convert` gave boot layers (pins for the test kernel and, as its kiln-init, busybox) is refused for its kernel, then runs with the custom flags and has its init layer replaced.

In `crates/kiln/tests/run.rs`, replace:
```rust
/// An image with boot layers (spec §8.1, §11.5): a `base` kernel is refused while
/// kiln pins no kernel, a `custom` one boots with --allow-custom-kernel, and the
```
with:
```rust
/// An image with boot layers (spec §8.1, §11.5): a `base` kernel this kiln does
/// not pin is refused, a `custom` one boots with --allow-custom-kernel, and the
```

In `crates/kiln/tests/run.rs`, replace:
```rust
        text(&o.stderr).contains("kernel base 6.18.54") && text(&o.stderr).contains("cannot verify"),
```
with:
```rust
        text(&o.stderr).contains("does not pin the kernel base 6.18.54"),
```

In `crates/kiln/tests/run.rs`, replace:
```rust
    let o = run(&["--allow-custom-kernel", "custom"]);
    assert_eq!((code(&o), text(&o.stdout)), (0, "ran\n".into()), "{}", text(&o.stderr));
```
with:
```rust
    let o = run(&["--allow-custom-kernel", "custom"]);
    assert_eq!((code(&o), text(&o.stdout)), (0, "ran\n".into()), "{}", text(&o.stderr));
    assert!(
        text(&o.stderr).contains("init layer") && text(&o.stderr).contains("replaced"),
        "{}",
        text(&o.stderr)
    );
}

/// The boot layers `convert` adds (spec §6.5), from pins for the test kernel and,
/// as the image's "kiln-init", busybox: the image runs with the custom flags, and
/// its init layer is replaced.
#[test]
fn converted_boot_layers_are_replaced_or_refused_at_run() {
    let Some(f) = fixture() else { return };
    use kiln_image::artifacts::{ArtifactKind, Pin};
    use kiln_image::{Boot, BootLayers, ConvertOptions, LocalRequest, convert_local};
    let store = kiln_store::Store::open(&f.store).unwrap();
    let leak = |s: String| -> &'static str { Box::leak(s.into_boxed_str()) };
    let put = |path: &Path, kind, name, version| {
        let bytes = std::fs::read(path).unwrap();
        let d = store.put_bytes(&bytes).unwrap();
        Pin {
            kind,
            name,
            version,
            arch: leak(Platform::host().architecture),
            repository: "127.0.0.1:1/unused",
            digest: leak(d.to_string()),
            size: bytes.len() as u64,
        }
    };
    let boot = Boot {
        kernel: Some(put(&f.env.kernel, ArtifactKind::Kernel, "base", "6.18.54")),
        init: Some(put(&f.env.busybox, ArtifactKind::Init, "kiln-init", "0.0.1")),
    };
    let layout = f._dirs[0].path().join("busybox");
    let platforms = [Platform::host()];
    let req = LocalRequest {
        source_ref: None,
        platforms: &platforms,
        tag: Some("booted"),
    };
    let opts = ConvertOptions {
        boot: BootLayers::Explicit(boot),
        ..Default::default()
    };
    convert_local(&store, &layout, &req, &opts).unwrap();
    let o = f
        .kiln()
        .args(["run", "--vmm", &f.env.vmm, "--init"])
        .arg(&f.env.init)
        .args(["--allow-custom-init", "booted", "--", "true"])
        .finish();
    assert_eq!(code(&o), 125);
    assert!(
        text(&o.stderr).contains("does not pin the kernel base 6.18.54"),
        "{}",
        text(&o.stderr)
    );
    let o = f.output(&["booted", "--", "echo", "ran"]);
    assert_eq!((code(&o), text(&o.stdout)), (0, "ran\n".into()), "{}", text(&o.stderr));
```

- [ ] **Step 12: Modify `README.md`**

What boots.

In `README.md`, replace:
```markdown
- The kernel and kiln-init that boot are pinned by digest in kiln once they are released; until then `--kernel PATH --allow-custom-kernel --init PATH --allow-custom-init` name them (vmkit builds the kernel; `kiln-init` is built as below). Both are warned about and recorded in the run's `run.json`. An image's own init layer never boots.
```
with:
```markdown
- What boots: kiln compiles in a table of the kernels (vmkit's releases) and kiln-init binaries (kiln's) it trusts, by digest. `kiln convert` adds a kernel layer and an init layer for each one the table pins for the image's architecture, fetched once by digest (`kiln inspect` shows them on its `boot:` line; without them the image is provisional, and convert says what `kiln run` will need). `kiln run` boots an image's kernel layer only when it is the pinned one for the profile and version its config names, and always replaces the image's init layer with the pinned kiln-init's; neither needs the network once fetched, and `kiln gc` keeps them. The table is empty until those releases exist: until then `--kernel PATH --allow-custom-kernel --init PATH --allow-custom-init` name them (vmkit builds the kernel; `kiln-init` is built as below). Custom ones are warned about and recorded in the run's `run.json`. An image's own init layer never boots.
```

- [ ] **Step 13: Run the crate's tests**

Run:
```bash
cargo test -q -p kiln-run
```
Expected: all pass (36 tests on macOS, 118 on Linux), among them `tests/prepare.rs`' 11 cases.

- [ ] **Step 14: Format, lint and run the whole workspace**

Run:
```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test --all -q
```
Expected: no clippy warnings; every test passes (411 tests).

- [ ] **Step 15: Lint and test on Linux**

In the Lima VM (`limactl shell vmkit`, then `cd` to the repository and `export CARGO_TARGET_DIR=$HOME/target-m3b2`):

Run (Linux):
```bash
cargo clippy --all-targets -- -D warnings && cargo test --all -q
```
Expected: no clippy warnings; every test passes (511 tests; the boot and `kiln run` cases skip themselves without their environment).

- [ ] **Step 16: Run the boot-layer cases end to end**

In the Lima VM, in an `sg kvm` shell, with Firecracker and Cloud Hypervisor on `PATH`:

Run (Linux, KVM):
```bash
kernel=/Users/alfonso/Github/Personal/playground/vmkit/out/vmlinux-6.18.54-aarch64
KILN_TEST_NET=1 KILN_TEST_VMM=firecracker scripts/boot-tests.sh $kernel boot_layers -- --test-threads=4
KILN_TEST_NET=1 KILN_TEST_VMM=firecracker scripts/boot-tests.sh $kernel a_provisional_image -- --test-threads=4
```
Expected: `image_boot_layers_are_verified_and_init_replaced`, `converted_boot_layers_are_replaced_or_refused_at_run` and `a_provisional_image_needs_the_custom_kernel_flags` pass.

- [ ] **Step 17: Commit**

```bash
git add -A crates README.md Cargo.lock
git commit -m 'feat: convert adds the pinned kernel and init layers; kiln run verifies the kernel layer against the table and boots the pinned kiln-init'
```

### Task 3: `kiln-run`: `--persist` keeps the scratch disk across runs, guarded by its sidecar and a lock

**Files:**
- Create: `crates/kiln-run/src/persist.rs`
- Modify: `README.md`, `crates/kiln-run/src/lib.rs`, `crates/kiln-run/src/linux/run.rs`, `crates/kiln-run/src/options.rs`, `crates/kiln/src/run_cmd.rs`, `crates/kiln/tests/cli.rs`, `crates/kiln/tests/run.rs`

**Interfaces:**
- Consumes: M3b-1's `RunOptions`, `Run::start`, `ScratchDir`, `RunInfo::scratch_dir`, `SessionOptions::end_timeout`; `kiln_image::scratch::create_scratch`; rustix `fs` (`flock`, `O_NOFOLLOW`).
- Produces:
  - `RunOptions { persist: Option<PathBuf>, persist_reset, persist_force, .. }`; `check()` refuses reset or force without `--persist`, and both together.
  - `kiln_run::persist`: `Sidecar { version, manifest, layers, size }` (`<PATH>.kiln.json`, camelCase), `SIDECAR_VERSION` (1), `sidecar_path`, `Plan::{Create, Recreate, Reuse} { size }`, `Existing { size, sidecar }`, `inspect(path) -> Result<Option<Existing>>`, `decide(path, existing, manifest, layers, disk, reset, force) -> Result<(Plan, Vec<String>)>` (lines start `warning: ` or `note: `), `apply(path, &Plan, manifest, layers) -> Result<File>` (the locked disk).
  - `Report::kept: Option<(PathBuf, Option<PathBuf>)>` (no scratch directory with `--persist`).
  - `kiln run --persist PATH [--persist-reset | --persist-force]`.

`--persist PATH` (spec §9.4): the scratch disk lives at `PATH` with a sidecar `PATH.kiln.json`, the only thing kiln trusts about it (D4).

- A missing disk is created from the template (`--disk`, default 4 GiB). An existing one is reused when its sidecar lists the same app layers in the same order (overlayfs forbids changing the lower layers under an upper directory); when only the manifest changed (an env or cmd edit), it is reused and the sidecar records the new manifest, with a note (ruling 2). Other layers are refused unless `--persist-reset` (recreate) or `--persist-force` (reuse, with a warning); the two exclude each other. A disk without a readable sidecar is refused unless reset or forced.
- Never truncated: a smaller `--disk` is refused, a larger one grows the file (the guest grows the filesystem at boot). Symlinks are refused (`O_NOFOLLOW`).
- The disk is locked (`flock`, non-blocking) for the whole run, so two runs never share it; the lock is taken before a reset removes the old disk, and a disk that appears while kiln creates one is not overwritten (P5).
- The persisted disk replaces the run's scratch directory: none is made, `run.json` records none, and no stale-run sweep can reach the disk. The guest gets 60 s instead of 10 s to end the VM after its final message (D-3), since it syncs a disk that outlives the run.

- [ ] **Step 1: Create `crates/kiln-run/src/persist.rs`**

The sidecar, the decision (pure, unit-tested) and applying it. Its tests cover every decision, ruling 2, growing, locking, a reset refused while the disk is in use, creating over a file that appeared, symlinks, and an unreadable sidecar.

`crates/kiln-run/src/persist.rs`:
```rust
//! `--persist PATH` (spec §9.4): a scratch disk kept across runs, with a host-side
//! sidecar `<PATH>.kiln.json` naming the image it belongs to. kiln never parses the
//! guest-written ext4: the sidecar is all it trusts, and it writes the sidecar itself.
//!
//! - A missing disk is created from the template (`--disk`, default 4 GiB).
//! - An existing disk is reused only with the same app layers, in the same order:
//!   overlayfs (with `xino`) forbids changing lower layers under an upper directory.
//!   Only the layers matter, so an image whose manifest changed but whose layers did
//!   not (another env or cmd) reuses it, and the sidecar records the new manifest
//!   (ruling 2). `--persist-reset` recreates it; `--persist-force` reuses it with
//!   other layers anyway, with a warning. The two exclude each other.
//! - A disk is never truncated: a `--disk` smaller than it is refused, a larger
//!   one grows the file (the guest grows the filesystem at boot).
//! - A disk without a readable sidecar is refused unless reset or forced.
//! - The disk is locked (`flock`) for the run, so two runs never share it. It lives
//!   where the user put it, outside the store's scratch directories, so no cleanup
//!   of stale runs ever touches it.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use kiln_store::Digest;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::options::DEFAULT_DISK;

/// The sidecar's contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Sidecar {
    pub version: u32,
    /// The platform manifest the disk was last used with.
    pub manifest: Digest,
    /// Its app layers, lowest first.
    pub layers: Vec<Digest>,
    pub size: u64,
}

pub const SIDECAR_VERSION: u32 = 1;

/// The sidecar's path for a disk.
pub fn sidecar_path(disk: &Path) -> PathBuf {
    let mut s = disk.as_os_str().to_owned();
    s.push(".kiln.json");
    PathBuf::from(s)
}

/// What to do with the disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Create it with this size.
    Create { size: u64 },
    /// (Re)create it, removing what is there.
    Recreate { size: u64 },
    /// Use it, growing the file to `size` when that is larger.
    Reuse { size: u64 },
}

/// What is at the path now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Existing {
    pub size: u64,
    /// `None`: no sidecar, or one kiln cannot read.
    pub sidecar: Option<Sidecar>,
}

/// Decides, without touching anything. Returns the plan and the lines to print
/// (each starting `warning: ` or `note: `).
pub fn decide(
    path: &Path,
    existing: Option<&Existing>,
    manifest: &Digest,
    layers: &[Digest],
    disk: Option<u64>,
    reset: bool,
    force: bool,
) -> Result<(Plan, Vec<String>)> {
    let shown = path.display();
    let Some(e) = existing else {
        return Ok((
            Plan::Create {
                size: disk.unwrap_or(DEFAULT_DISK),
            },
            Vec::new(),
        ));
    };
    if reset {
        return Ok((
            Plan::Recreate {
                size: disk.unwrap_or(DEFAULT_DISK),
            },
            vec![format!("warning: --persist-reset: recreating {shown}")],
        ));
    }
    let mut warnings = Vec::new();
    match &e.sidecar {
        None if force => warnings.push(format!(
            "warning: --persist-force: {shown} has no readable {}; using it anyway",
            sidecar_path(path).display()
        )),
        None => {
            return Err(Error::refused(format!(
                "{shown} exists but has no readable kiln sidecar ({}); pass --persist-reset to \
                 recreate it, or --persist-force to use it anyway",
                sidecar_path(path).display()
            )));
        }
        Some(s) if s.layers == layers => {
            if s.manifest != *manifest {
                warnings.push(format!(
                    "note: {shown} was last used with the image {}, which has the same layers; reusing it",
                    s.manifest
                ));
            }
        }
        Some(s) => {
            if !force {
                return Err(Error::refused(format!(
                    "{shown} was used with another image ({}) whose layers differ; overlayfs cannot keep its \
                     files over other layers. Pass --persist-reset to recreate it, or --persist-force to reuse \
                     it anyway",
                    s.manifest
                )));
            }
            warnings.push(format!(
                "warning: --persist-force: {shown} was used with other layers; files from them may misbehave"
            ));
        }
    }
    let size = match disk {
        Some(d) if d < e.size => {
            return Err(Error::refused(format!(
                "--disk {d} is smaller than {shown} ({} bytes); persisted disks are never shrunk",
                e.size
            )));
        }
        Some(d) => d,
        None => e.size,
    };
    Ok((Plan::Reuse { size }, warnings))
}

/// Reads what is at `path`: a regular file (never followed through a symlink), and
/// its sidecar if kiln can parse it.
pub fn inspect(path: &Path) -> Result<Option<Existing>> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if !meta.is_file() {
        return Err(Error::refused(format!(
            "--persist {} is not a regular file",
            path.display()
        )));
    }
    let sidecar = std::fs::read(sidecar_path(path))
        .ok()
        .and_then(|b| serde_json::from_slice::<Sidecar>(&b).ok())
        .filter(|s| s.version == SIDECAR_VERSION);
    Ok(Some(Existing {
        size: meta.len(),
        sidecar,
    }))
}

/// Opens the disk for the run (creating, recreating or growing it per `plan`),
/// takes an exclusive lock on it so two runs never share it, and writes the sidecar.
/// A disk in use by another run is refused before anything changes.
pub fn apply(path: &Path, plan: &Plan, manifest: &Digest, layers: &[Digest]) -> Result<File> {
    let size = match *plan {
        Plan::Recreate { size } => {
            // Held while the old disk is replaced: never under a run using it.
            let _old = locked(path)?;
            std::fs::remove_file(path)?;
            create(path, size)?;
            size
        }
        Plan::Create { size } => {
            create(path, size)?;
            size
        }
        Plan::Reuse { size } => size,
    };
    let file = locked(path)?;
    if file.metadata()?.len() < size {
        file.set_len(size)?;
    }
    let sidecar = Sidecar {
        version: SIDECAR_VERSION,
        manifest: manifest.clone(),
        layers: layers.to_vec(),
        size: file.metadata()?.len(),
    };
    write_sidecar(path, &sidecar)?;
    Ok(file)
}

/// The disk, opened without following a symlink and locked for this run.
fn locked(path: &Path) -> Result<File> {
    let file = open_nofollow(path)?;
    if !try_lock(&file)? {
        return Err(Error::refused(format!(
            "{} is in use by another kiln run",
            path.display()
        )));
    }
    Ok(file)
}

/// Creates the disk from the template; never over a file that appeared meanwhile.
fn create(path: &Path, size: u64) -> Result<()> {
    match kiln_image::scratch::create_scratch(path, size) {
        Err(kiln_image::ImageError::Io(e)) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(Error::refused(format!(
                "{} appeared while kiln was creating it (another kiln run?)",
                path.display()
            )))
        }
        r => Ok(r?),
    }
}

fn open_nofollow(path: &Path) -> Result<File> {
    let mut o = OpenOptions::new();
    o.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    }
    Ok(o.open(path)?)
}

#[cfg(unix)]
fn try_lock(file: &File) -> Result<bool> {
    use rustix::fs::{FlockOperation, flock};
    match flock(file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(true),
        Err(rustix::io::Errno::WOULDBLOCK) => Ok(false),
        Err(e) => Err(std::io::Error::from(e).into()),
    }
}

#[cfg(not(unix))]
fn try_lock(_: &File) -> Result<bool> {
    Ok(true)
}

/// Writes the sidecar atomically (temp file, then rename).
fn write_sidecar(disk: &Path, s: &Sidecar) -> Result<()> {
    let path = sidecar_path(disk);
    let mut tmp = path.clone().into_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let _ = std::fs::remove_file(&tmp);
    let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
    let bytes = serde_json::to_vec_pretty(s).map_err(std::io::Error::other)?;
    f.write_all(&bytes)?;
    f.sync_all()?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Digest {
        Digest::of(s.as_bytes())
    }

    fn existing(manifest: &str, layers: &[&str], size: u64) -> Existing {
        Existing {
            size,
            sidecar: Some(Sidecar {
                version: 1,
                manifest: d(manifest),
                layers: layers.iter().map(|l| d(l)).collect(),
                size,
            }),
        }
    }

    #[test]
    fn decisions() {
        let p = Path::new("/disk.img");
        let (m, l) = (d("m"), vec![d("a"), d("b")]);
        let g = 1u64 << 30;
        assert_eq!(
            decide(p, None, &m, &l, None, false, false).unwrap().0,
            Plan::Create { size: DEFAULT_DISK }
        );
        let same = existing("m", &["a", "b"], g);
        assert_eq!(
            decide(p, Some(&same), &m, &l, None, false, false).unwrap(),
            (Plan::Reuse { size: g }, vec![])
        );
        assert_eq!(
            decide(p, Some(&same), &m, &l, Some(2 * g), false, false).unwrap().0,
            Plan::Reuse { size: 2 * g }
        );
        let err = decide(p, Some(&same), &m, &l, Some(g / 2), false, false).unwrap_err();
        assert!(err.to_string().contains("never shrunk"), "{err}");
        // A reset recreates, at any size.
        assert_eq!(
            decide(p, Some(&same), &m, &l, Some(g / 2), true, false).unwrap().0,
            Plan::Recreate { size: g / 2 }
        );

        let other = existing("m2", &["a", "c"], g);
        let err = decide(p, Some(&other), &m, &l, None, false, false).unwrap_err();
        assert!(
            err.to_string().contains("layers differ") && err.to_string().contains("--persist-reset"),
            "{err}"
        );
        let (plan, warnings) = decide(p, Some(&other), &m, &l, None, false, true).unwrap();
        assert_eq!(plan, Plan::Reuse { size: g });
        assert!(
            warnings.len() == 1 && warnings[0].starts_with("warning: --persist-force"),
            "{warnings:?}"
        );
        // The same layers in another order are other layers.
        let reordered = existing("m", &["b", "a"], g);
        assert!(decide(p, Some(&reordered), &m, &l, None, false, false).is_err());
        // Ruling 2: the same layers under another manifest (env or cmd changed) are
        // reused, with one note.
        let same_layers = existing("m2", &["a", "b"], g);
        let (plan, notes) = decide(p, Some(&same_layers), &m, &l, None, false, false).unwrap();
        assert_eq!(plan, Plan::Reuse { size: g });
        assert!(
            notes.len() == 1 && notes[0].starts_with("note: ") && notes[0].contains("same layers"),
            "{notes:?}"
        );

        let bare = Existing { size: g, sidecar: None };
        assert!(decide(p, Some(&bare), &m, &l, None, false, false).is_err());
        assert!(decide(p, Some(&bare), &m, &l, None, false, true).is_ok());
    }

    #[test]
    fn apply_creates_grows_locks_and_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk.img");
        let (m, l) = (d("m"), vec![d("a")]);
        assert_eq!(inspect(&path).unwrap(), None);
        let f = apply(&path, &Plan::Create { size: 64 << 20 }, &m, &l).unwrap();
        let e = inspect(&path).unwrap().unwrap();
        assert_eq!(e.size, 64 << 20);
        assert_eq!(e.sidecar.as_ref().unwrap().layers, l);
        // Locked while in use.
        let err = apply(&path, &Plan::Reuse { size: 64 << 20 }, &m, &l).unwrap_err();
        assert!(err.to_string().contains("in use"), "{err}");
        drop(f);
        // Grows, never shrinks.
        drop(apply(&path, &Plan::Reuse { size: 128 << 20 }, &m, &l).unwrap());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 128 << 20);
        assert_eq!(inspect(&path).unwrap().unwrap().sidecar.unwrap().size, 128 << 20);
        drop(apply(&path, &Plan::Reuse { size: 64 << 20 }, &m, &l).unwrap());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 128 << 20);
        // A symlink is refused, never followed.
        #[cfg(unix)]
        {
            let link = dir.path().join("link.img");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            assert!(inspect(&link).is_err());
        }
        // Ruling 2: reused under another manifest, the sidecar records the new one.
        let m2 = d("m2");
        drop(apply(&path, &Plan::Reuse { size: 128 << 20 }, &m2, &l).unwrap());
        assert_eq!(inspect(&path).unwrap().unwrap().sidecar.unwrap().manifest, m2);
        // A disk in use is never recreated under its run.
        let held = apply(&path, &Plan::Reuse { size: 128 << 20 }, &m, &l).unwrap();
        let err = apply(&path, &Plan::Recreate { size: 64 << 20 }, &m, &l).unwrap_err();
        assert!(err.to_string().contains("in use"), "{err}");
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 128 << 20);
        drop(held);
        drop(apply(&path, &Plan::Recreate { size: 64 << 20 }, &m, &l).unwrap());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 64 << 20);
        // Created only where nothing is.
        let err = apply(&path, &Plan::Create { size: 64 << 20 }, &m, &l).unwrap_err();
        assert!(err.to_string().contains("appeared"), "{err}");
        // A sidecar kiln cannot read counts as missing.
        std::fs::write(sidecar_path(&path), b"{}").unwrap();
        assert_eq!(inspect(&path).unwrap().unwrap().sidecar, None);
    }
}
```

- [ ] **Step 2: Run the module's tests**

Run:
```bash
cargo test -q -p kiln-run --lib persist
```
Expected: FAIL to compile until `lib.rs` declares the module (next step); then `decisions` and `apply_creates_grows_locks_and_records` pass.

- [ ] **Step 3: Modify `crates/kiln-run/src/lib.rs`**

Export the module.

In `crates/kiln-run/src/lib.rs`, replace:
```rust
pub mod options;
pub mod prepare;
```
with:
```rust
pub mod options;
pub mod persist;
pub mod prepare;
```

- [ ] **Step 4: Modify `crates/kiln-run/src/options.rs`**

The flags and their combinations.

In `crates/kiln-run/src/options.rs`, replace:
```rust
    /// `--disk`: the scratch disk's size; `None` is the default, 4 GiB.
    pub disk: Option<u64>,
```
with:
```rust
    /// `--disk`: the scratch disk's size; `None` is the default, 4 GiB, or a
    /// persisted disk's own size.
    pub disk: Option<u64>,
    /// `--persist PATH`: a scratch disk kept across runs (spec §9.4).
    pub persist: Option<PathBuf>,
    /// `--persist-reset`: recreate the persisted disk.
    pub persist_reset: bool,
    /// `--persist-force`: reuse it although its sidecar names other layers.
    pub persist_force: bool,
```

In `crates/kiln-run/src/options.rs`, replace:
```rust
            disk: None,
            env: Vec::new(),
```
with:
```rust
            disk: None,
            persist: None,
            persist_reset: false,
            persist_force: false,
            env: Vec::new(),
```

In `crates/kiln-run/src/options.rs`, replace:
```rust
                "--init boots a kiln-init kiln has not pinned; add --allow-custom-init to accept that",
            ));
        }
        if let Some(d) = self.disk {
```
with:
```rust
                "--init boots a kiln-init kiln has not pinned; add --allow-custom-init to accept that",
            ));
        }
        if (self.persist_reset || self.persist_force) && self.persist.is_none() {
            return Err(Error::refused("--persist-reset and --persist-force need --persist"));
        }
        if self.persist_reset && self.persist_force {
            return Err(Error::refused("--persist-reset and --persist-force exclude each other"));
        }
        if let Some(d) = self.disk {
```

In `crates/kiln-run/src/options.rs`, replace:
```rust
        o.disk = None;
        o.memory_mib = 32;
```
with:
```rust
        o.disk = None;
        o.persist_reset = true;
        assert!(o.check().unwrap_err().to_string().contains("need --persist"));
        o.persist = Some("/disk.img".into());
        o.check().unwrap();
        o.persist_force = true;
        assert!(o.check().unwrap_err().to_string().contains("exclude each other"));
        o.persist_reset = false;
        o.check().unwrap();
        o.memory_mib = 32;
```

- [ ] **Step 5: Modify `crates/kiln-run/src/linux/run.rs`**

The persisted disk instead of the scratch directory; its lock held until the run ends; the longer end bound.

Replace the contents of `crates/kiln-run/src/linux/run.rs` with:
```rust
//! One `kiln run`, start to end (spec §9): resolve, check, prepare the run
//! directory and disks, boot through vmkit, run the session, report.

use std::path::{Path, PathBuf};
use std::time::Duration;

use kiln_image::artifacts::RegistryFetcher;
use kiln_proto::{ExitMethod, GUEST_CID};
use kiln_store::Store;
use vmkit::{Backend, Disk, GuestExit, NetSpec, VmSpec, Vmm, VsockSpec};

use super::console::ConsoleRelay;
use super::session::{Handle, Outcome, Session, SessionOptions, Streams};
use crate::config::{self, Runtime};
use crate::console::{TAIL_LINES, tail};
use crate::error::{Error, Result};
use crate::options::{DEFAULT_DISK, RunOptions, VmmKind};
use crate::persist;
use crate::prepare::{self, InitLayer};
use crate::rundir::{self, Identity, RUN_INFO_VERSION, RunDir, RunInfo, ScratchDir, ScratchInfo};

/// The kernel command line (spec §8.3); vmkit appends the console and its backend's parameters.
pub const CMDLINE: [&str; 7] = [
    "root=/dev/vda",
    "ro",
    "rootfstype=erofs",
    "init=/kiln-init",
    "panic=-1",
    "quiet",
    "loglevel=3",
];

/// How long the guest has to end the VM after its final message (it syncs and
/// remounts the scratch disk first, D-3); longer with `--persist`, whose disk
/// outlives the run.
const END_TIMEOUT: Duration = Duration::from_secs(10);
const END_TIMEOUT_PERSIST: Duration = Duration::from_secs(60);

pub fn backend(kind: VmmKind) -> Backend {
    match kind {
        VmmKind::Firecracker => Backend::Firecracker,
        VmmKind::CloudHypervisor => Backend::CloudHypervisor,
    }
}

pub fn exit_method(vmm: &dyn Vmm) -> ExitMethod {
    match vmm.capabilities().guest_exit {
        GuestExit::Reboot => ExitMethod::Reboot,
        GuestExit::Poweroff => ExitMethod::Poweroff,
    }
}

/// The VM: init layer, scratch disk, app layers, and a network when `net` is set (spec §9.1).
#[allow(clippy::too_many_arguments)]
pub fn vm_spec(
    kernel: &Path,
    init_layer: &Path,
    scratch: &Path,
    layers: &[PathBuf],
    cpus: u8,
    memory_mib: u32,
    net: Option<NetSpec>,
    console_log: &Path,
    run_dir: &Path,
) -> VmSpec {
    let mut disks = vec![
        Disk {
            path: init_layer.to_path_buf(),
            read_only: true,
        },
        Disk {
            path: scratch.to_path_buf(),
            read_only: false,
        },
    ];
    disks.extend(layers.iter().map(|p| Disk {
        path: p.clone(),
        read_only: true,
    }));
    VmSpec {
        kernel: kernel.to_path_buf(),
        initramfs: None,
        cmdline: CMDLINE.map(String::from).to_vec(),
        disks,
        vcpus: cpus,
        memory_mib,
        vsock: Some(VsockSpec { guest_cid: GUEST_CID }),
        net,
        console_log: console_log.to_path_buf(),
        run_dir: run_dir.to_path_buf(),
    }
}

/// The VMM's identity, from vmkit's PID file, read while a pidfd pins the process.
pub fn vmm_identity(run_dir: &Path) -> Option<Identity> {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    use rustix::process::{Pid, PidfdFlags, pidfd_open};
    let pid: u32 = std::fs::read_to_string(vmkit::sandbox::pid_file(run_dir))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let fd = pidfd_open(Pid::from_raw(i32::try_from(pid).ok()?)?, PidfdFlags::empty()).ok()?;
    let id = rundir::identity(pid)?;
    // Readable means exited: then the start time may belong to another process.
    let zero = Timespec { tv_sec: 0, tv_nsec: 0 };
    let mut fds = [PollFd::new(&fd, PollFlags::IN)];
    (poll(&mut fds, Some(&zero)).ok()? == 0).then_some(id)
}

/// A run in progress.
pub struct Run {
    session: Session,
    console: ConsoleRelay,
    /// Dropped before `run_dir`, whose run.json records it; none with `--persist`.
    scratch_dir: Option<ScratchDir>,
    run_dir: RunDir,
    /// The `--persist` disk, locked until the run ends.
    _persist: Option<std::fs::File>,
    /// `KILN_KEEP_RUN_DIR=1` at the start.
    keep: bool,
    /// For the message when the VM ends without the guest's final message.
    vmm: VmmKind,
    memory_mib: u32,
    /// The VMM runs in a cgroup with a memory limit.
    limited: bool,
}

/// How a run ended, for the CLI.
#[derive(Debug)]
pub struct Report {
    pub outcome: Outcome,
    /// The sanitised end of the console when the run failed (T8).
    pub console_tail: Vec<String>,
    /// The run directory and the scratch directory (none with `--persist`), when
    /// kept (`KILN_KEEP_RUN_DIR=1`).
    pub kept: Option<(PathBuf, Option<PathBuf>)>,
    /// The VMM's own logs, in the run directory.
    pub vmm_logs: Vec<PathBuf>,
    /// Guest memory, and whether the VMM's memory was limited to it plus 256 MiB.
    pub memory_mib: u32,
    pub limited: bool,
}

impl Report {
    /// What to say when the VM ended without the guest's final message (no
    /// `Exited` or `InitFailed`): vmkit's end reason cannot tell why (a VMM killed
    /// for its memory use looks `Exited`), so the likely causes are named, with
    /// where to look.
    pub fn no_final_message(&self) -> String {
        let names = |full: bool| {
            self.vmm_logs
                .iter()
                .map(|p| {
                    if full {
                        p.display().to_string()
                    } else {
                        p.file_name().unwrap_or_default().to_string_lossy().into_owned()
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        let memory = if self.limited {
            format!(
                "the VMM ran out of memory (it may use --memory plus 256 MiB, {} MiB here; the kernel log shows an \
                 out-of-memory kill)",
                u64::from(self.memory_mib) + 256
            )
        } else {
            "the host ran out of memory (the kernel log shows an out-of-memory kill)".to_string()
        };
        let logs = if self.kept.is_some() {
            format!("the VMM's log is in {}", names(true))
        } else {
            format!(
                "KILN_KEEP_RUN_DIR=1 keeps the run directory, with the VMM's log ({}) and console.log",
                names(false)
            )
        };
        format!(
            "the VM ended without reporting the command's exit (the VMM: {:?}). Likely causes: the VMM failed, or {memory}; \
             {logs}",
            self.outcome.end.reason
        )
    }
}

/// The VMM's own log files in a run directory (vmkit writes them there).
pub fn vmm_logs(kind: VmmKind, run_dir: &Path) -> Vec<PathBuf> {
    match kind {
        VmmKind::Firecracker => vec![run_dir.join("sock/firecracker.log"), run_dir.join("firecracker.stderr")],
        VmmKind::CloudHypervisor => vec![run_dir.join("cloud-hypervisor.log")],
    }
}

impl Run {
    /// Resolves, checks and boots. `notice` gets one-line warnings and notes, before
    /// boot. When `interrupted` turns true during setup (the user's SIGINT or
    /// SIGTERM), setup stops with [`Error::Interrupted`], removing what it made.
    pub fn start(
        store: &Store,
        opts: &RunOptions,
        streams: Streams,
        notice: &mut dyn FnMut(&str),
        interrupted: &dyn Fn() -> bool,
    ) -> Result<Self> {
        let check = || if interrupted() { Err(Error::Interrupted) } else { Ok(()) };
        opts.check()?;
        // Held until the VMM has every blob open (spec §5.2).
        let lock = store.lock_shared()?;
        let prepared = prepare::prepare(
            store,
            opts,
            prepare::host_arch(),
            &prepare::CompiledPins,
            &RegistryFetcher::from_env(),
        )?;
        check()?;
        for w in &prepared.warnings {
            notice(&format!("warning: {w}"));
        }
        let vmm = backend(opts.vmm).discover()?;
        let caps = vmm.capabilities();
        config::check_devices(prepared.layers.len(), false, caps.available_devices(), vmm.name())?;
        let limited = vmkit::cgroups_available();
        if !limited {
            notice("warning: no systemd user session for a cgroup: the VM's memory and CPU use are not limited");
        }

        let base = rundir::base_dir()?;
        let (boot_id, hostname) = (rundir::boot_id(), rundir::hostname());
        let cleanup = rundir::clean_stale(&base, &boot_id, &hostname, rundir::alive);
        if let Some(note) = cleanup.notice(&base) {
            notice(&note);
        }
        // On the store's filesystem, not the run directory's tmpfs (spec §5.3, rev 2.9).
        // Those whose run directories are gone (a reboot or a logout emptied them) are
        // swept by their own identity.
        let scratch_base = rundir::scratch_base(store.root())?;
        let swept = rundir::clean_stale_scratch(
            &scratch_base,
            &boot_id,
            &hostname,
            rundir::alive,
            std::time::SystemTime::now(),
        );
        if let Some(note) = swept.scratch_notice(&scratch_base) {
            notice(&note);
        }
        let mut run_dir = RunDir::create(&base)?;
        let keep = std::env::var_os("KILN_KEEP_RUN_DIR").is_some_and(|v| v == "1");
        if keep {
            run_dir.keep();
        }
        // A persisted disk replaces the run's scratch directory (spec §9.4).
        let scratch_path = opts.persist.is_none().then(|| scratch_base.join(&run_dir.id));
        let mut info = RunInfo {
            version: RUN_INFO_VERSION,
            id: run_dir.id.clone(),
            boot_id,
            hostname,
            kiln: rundir::identity(std::process::id()).ok_or_else(|| Error::refused("cannot read /proc/self/stat"))?,
            vmm: None,
            vmm_kind: opts.vmm.name().into(),
            image: opts.image.clone(),
            custom_kernel: prepared.custom_kernel.as_ref().map(|p| p.display().to_string()),
            custom_init: prepared.custom_init.as_ref().map(|p| p.display().to_string()),
            keep,
            scratch_dir: scratch_path.as_ref().map(|p| p.display().to_string()),
        };
        // Recorded before it exists, so a stale cleanup always knows of it.
        run_dir.write_info(&info)?;
        let mut scratch_dir = match &scratch_path {
            Some(_) => Some(ScratchDir::create(&scratch_base, &ScratchInfo::from(&info))?),
            None => None,
        };
        if keep && let Some(d) = &mut scratch_dir {
            d.keep();
        }

        // The pinned kiln-init's layer is a blob in the store, opened by the VMM while
        // the lock is held; one built from --init goes in the run directory.
        let init_layer = match &prepared.init {
            InitLayer::Stored(path) => path.clone(),
            InitLayer::Built(bytes) => {
                let path = run_dir.path.join("init.erofs");
                std::fs::write(&path, bytes)?;
                path
            }
        };
        let (scratch, scratch_bytes, persist_file) = match (&opts.persist, &scratch_dir) {
            (Some(path), _) => {
                let existing = persist::inspect(path)?;
                let (plan, lines) = persist::decide(
                    path,
                    existing.as_ref(),
                    &prepared.manifest,
                    &prepared.layers,
                    opts.disk,
                    opts.persist_reset,
                    opts.persist_force,
                )?;
                for line in lines {
                    notice(&line);
                }
                let file = persist::apply(path, &plan, &prepared.manifest, &prepared.layers)?;
                let size = file.metadata()?.len();
                (std::fs::canonicalize(path)?, size, Some(file))
            }
            (None, Some(dir)) => {
                let path = dir.path.join("scratch.img");
                let size = opts.disk.unwrap_or(DEFAULT_DISK);
                kiln_image::scratch::create_scratch(&path, size)?;
                (path, size, None)
            }
            (None, None) => return Err(Error::refused("no scratch directory for the run")),
        };
        let window = if opts.tty { super::tty::window_size() } else { None };
        let config = config::build(
            &prepared.process,
            opts,
            &Runtime {
                hostname: config::hostname(&run_dir.id),
                layers: prepared.layers.len() as u32,
                scratch_bytes,
                exit_method: exit_method(vmm.as_ref()),
                window,
                host_env: config::host_env(std::env::vars_os()),
            },
        )?;
        check()?;
        let console = ConsoleRelay::start(&run_dir.path)?;
        let spec = vm_spec(
            &prepared.kernel,
            &init_layer,
            &scratch,
            &prepared.layer_paths,
            opts.cpus,
            opts.memory_mib,
            None,
            &console.fifo,
            &run_dir.path,
        );
        let vm = vmm.create(&spec)?;
        info.vmm = vmm_identity(&run_dir.path);
        if info.vmm.is_none() {
            notice(
                "warning: could not read the VMM's identity for run.json; if kiln is killed, its run directory \
                 may be cleaned up while the VMM is still exiting",
            );
        }
        run_dir.write_info(&info)?;
        if let Some(d) = &scratch_dir {
            d.write_info(&ScratchInfo::from(&info))?;
        }
        drop(lock);
        // Dropping the VM kills it; the run's directories go with `run_dir` and `scratch_dir`.
        check()?;
        let session = Session::start(
            vm,
            config,
            streams,
            SessionOptions {
                boot_timeout: opts.boot_timeout,
                stop_timeout: opts.stop_timeout,
                end_timeout: if opts.persist.is_some() {
                    END_TIMEOUT_PERSIST
                } else {
                    END_TIMEOUT
                },
                escape_keys: opts.tty && opts.interactive,
                ..SessionOptions::default()
            },
        )?;
        Ok(Run {
            session,
            console,
            scratch_dir,
            run_dir,
            _persist: persist_file,
            keep,
            vmm: opts.vmm,
            memory_mib: opts.memory_mib,
            limited,
        })
    }

    pub fn handle(&self) -> Handle {
        self.session.handle()
    }

    pub fn run_dir(&self) -> &Path {
        &self.run_dir.path
    }

    /// Runs to the end; the run's directories are removed unless kept.
    pub fn wait(self) -> Report {
        let Run {
            session,
            console,
            scratch_dir,
            run_dir,
            _persist,
            keep,
            vmm,
            memory_mib,
            limited,
        } = self;
        let outcome = session.finish();
        let log = console.log.clone();
        console.finish(Duration::from_secs(2));
        let failed = outcome.violation.is_some()
            || outcome.init_failed.is_some()
            || (outcome.exited.is_none() && !outcome.killed);
        let console_tail = if failed { tail(&log, TAIL_LINES) } else { Vec::new() };
        let kept = keep.then(|| (run_dir.path.clone(), scratch_dir.as_ref().map(|d| d.path.clone())));
        let vmm_logs = vmm_logs(vmm, &run_dir.path);
        // The scratch directory first: while run.json lasts, it records it. The
        // persisted disk's lock goes once the VMM has ended.
        drop(scratch_dir);
        drop(run_dir);
        drop(_persist);
        Report {
            outcome,
            console_tail,
            kept,
            vmm_logs,
            memory_mib,
            limited,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vmkit::{EndReason, VmEnd};

    fn report(kept: bool, limited: bool) -> Report {
        let dir = Path::new("/run/user/1/kiln/abc");
        Report {
            outcome: Outcome {
                exit_code: 125,
                exited: None,
                init_failed: None,
                stages: vec![3, 4, 5, 6, 7],
                hello: true,
                running: true,
                violation: None,
                killed: false,
                end: VmEnd {
                    reason: EndReason::Exited,
                    code: Some(0),
                    signal: None,
                },
                warnings: Vec::new(),
                hello_after: None,
                running_after: None,
                ended_after: None,
                drained_after: Duration::ZERO,
            },
            console_tail: Vec::new(),
            kept: kept.then(|| (dir.to_path_buf(), Some(PathBuf::from("/s/scratch/abc")))),
            vmm_logs: vmm_logs(VmmKind::Firecracker, dir),
            memory_mib: 512,
            limited,
        }
    }

    /// The VM ended without the guest's final message: the likely causes, and
    /// where to look (the run directory goes with the run unless kept).
    #[test]
    fn a_vm_ending_without_a_final_message_names_the_likely_causes() {
        assert_eq!(
            report(false, true).no_final_message(),
            "the VM ended without reporting the command's exit (the VMM: Exited). Likely causes: the VMM failed, \
             or the VMM ran out of memory (it may use --memory plus 256 MiB, 768 MiB here; the kernel log shows an \
             out-of-memory kill); KILN_KEEP_RUN_DIR=1 keeps the run directory, with the VMM's log (firecracker.log, \
             firecracker.stderr) and console.log"
        );
        let kept = report(true, false).no_final_message();
        assert!(kept.contains("the host ran out of memory"), "{kept}");
        assert!(
            kept.ends_with(
                "the VMM's log is in /run/user/1/kiln/abc/sock/firecracker.log, \
                 /run/user/1/kiln/abc/firecracker.stderr"
            ),
            "{kept}"
        );
    }
}
```

- [ ] **Step 6: Modify `crates/kiln/src/run_cmd.rs`**

The flags; a kept run without a scratch directory.

In `crates/kiln/src/run_cmd.rs`, replace:
```rust
    /// The scratch disk's size (default 4G; a multiple of 4096, at least 64M).
    #[arg(long, value_name = "SIZE", value_parser = parse_size)]
    disk: Option<u64>,
```
with:
```rust
    /// The scratch disk's size (default 4G, or a persisted disk's size; a multiple
    /// of 4096, at least 64M).
    #[arg(long, value_name = "SIZE", value_parser = parse_size)]
    disk: Option<u64>,
    /// Keep the scratch disk in this file across runs (with a PATH.kiln.json sidecar).
    #[arg(long, value_name = "PATH")]
    persist: Option<PathBuf>,
    /// Recreate the --persist disk.
    #[arg(long)]
    persist_reset: bool,
    /// Reuse the --persist disk although it was used with other layers.
    #[arg(long)]
    persist_force: bool,
```

In `crates/kiln/src/run_cmd.rs`, replace:
```rust
            disk: self.disk,
            env: self.env.clone(),
```
with:
```rust
            disk: self.disk,
            persist: self.persist.clone(),
            persist_reset: self.persist_reset,
            persist_force: self.persist_force,
            env: self.env.clone(),
```

In `crates/kiln/src/run_cmd.rs`, replace:
```rust
        eprintln!("kiln: scratch directory kept: {}", scratch.display());
```
with:
```rust
        if let Some(scratch) = scratch {
            eprintln!("kiln: scratch directory kept: {}", scratch.display());
        }
```

- [ ] **Step 7: Modify `crates/kiln/tests/cli.rs`**

Refusals before anything runs.

In `crates/kiln/tests/cli.rs`, replace:
```rust
        .stderr(predicate::str::contains("multiple of 4096"));
}
```
with:
```rust
        .stderr(predicate::str::contains("multiple of 4096"));
    kiln(home.path())
        .args(["run", "--persist-reset", "img"])
        .assert()
        .code(125)
        .stderr(predicate::str::contains("need --persist"));
    kiln(home.path())
        .args([
            "run",
            "--persist",
            "/d.img",
            "--persist-reset",
            "--persist-force",
            "img",
        ])
        .assert()
        .code(125)
        .stderr(predicate::str::contains("exclude each other"));
}
```

- [ ] **Step 8: Modify `crates/kiln/tests/run.rs`**

End to end, on one disk: created at 128M with its sidecar and no scratch directory, reused, a smaller `--disk` refused, another image with the same layers reused with a note, other layers refused then forced, a second run refused while one holds the disk, and a reset to 64M from the template.

In `crates/kiln/tests/run.rs`, replace:
```rust

/// The scratch disk lives on the store's filesystem, in `$KILN_HOME/scratch/<id>/`
```
with:
```rust

/// Converts, as `tag`, a busybox image whose `marker` holds `content`, with `config`.
fn add_image(f: &Fixture, tag: &str, content: &str, config: ContainerConfig) {
    let path = f._dirs[0].path().join(tag);
    let mut b = LayoutBuilder::new(&path);
    let d = b.image(
        &Platform::host(),
        &[TestLayer::tar(busybox_layer(&f.env.busybox, content))],
        config,
    );
    b.add(d, None).finish();
    let out = f.kiln().args(["convert", "--tag", tag]).arg(&path).finish();
    assert!(out.status.success(), "{}", text(&out.stderr));
}

/// `--persist` (spec §9.4): the disk and its sidecar outlive the run; it is never
/// shrunk; another image with the same layers reuses it with a note (ruling 2),
/// one with other layers only with --persist-force; --persist-reset starts over;
/// two runs never share it; and no cleanup of the run's directories touches it.
#[test]
fn persisted_disks_are_reused_and_guarded() {
    let Some(f) = fixture() else { return };
    let env = ContainerConfig {
        env: Some(vec!["CHANGED=1".into()]),
        ..ContainerConfig::default()
    };
    add_image(&f, "same-layers", "one", env);
    add_image(&f, "other", "two", ContainerConfig::default());
    let disk = f._dirs[0].path().join("data.img");
    let d = disk.to_str().unwrap();
    let o = f.output(&[
        "--persist",
        d,
        "--disk",
        "128M",
        "busybox",
        "--",
        "sh",
        "-c",
        "echo kept > /keep",
    ]);
    assert_eq!(code(&o), 0, "{}", text(&o.stderr));
    assert_eq!(std::fs::metadata(&disk).unwrap().len(), 128 << 20);
    let sidecar: serde_json::Value =
        serde_json::from_slice(&std::fs::read(f._dirs[0].path().join("data.img.kiln.json")).unwrap()).unwrap();
    assert_eq!(sidecar["size"], 128 << 20);
    // No scratch directory was made for the run, and none is left.
    let scratch: Vec<_> = std::fs::read_dir(f.store.join("scratch")).unwrap().flatten().collect();
    assert!(scratch.is_empty(), "{scratch:?}");
    let o = f.output(&["--persist", d, "busybox", "--", "cat", "/keep"]);
    assert_eq!((code(&o), text(&o.stdout)), (0, "kept\n".into()), "{}", text(&o.stderr));
    // Never shrunk.
    let o = f.output(&["--persist", d, "--disk", "64M", "busybox", "--", "true"]);
    assert_eq!(code(&o), 125);
    assert!(text(&o.stderr).contains("never shrunk"), "{}", text(&o.stderr));
    // The same layers under another manifest: reused, with a note.
    let o = f.output(&[
        "--persist",
        d,
        "same-layers",
        "--",
        "sh",
        "-c",
        "cat /keep; echo $CHANGED",
    ]);
    assert_eq!(
        (code(&o), text(&o.stdout)),
        (0, "kept\n1\n".into()),
        "{}",
        text(&o.stderr)
    );
    assert!(
        text(&o.stderr).contains("kiln: note:") && text(&o.stderr).contains("same layers"),
        "{}",
        text(&o.stderr)
    );
    // Other layers: refused, unless forced or reset.
    let o = f.output(&["--persist", d, "other", "--", "true"]);
    assert_eq!(code(&o), 125);
    assert!(text(&o.stderr).contains("--persist-reset"), "{}", text(&o.stderr));
    let o = f.output(&[
        "--persist",
        d,
        "--persist-force",
        "other",
        "--",
        "cat",
        "/keep",
        "/marker",
    ]);
    assert_eq!(
        (code(&o), text(&o.stdout)),
        (0, "kept\ntwo".into()),
        "{}",
        text(&o.stderr)
    );
    assert!(text(&o.stderr).contains("--persist-force"), "{}", text(&o.stderr));
    // Two runs never share it.
    let mut first = f
        .run(&[
            "--persist",
            d,
            "--persist-force",
            "busybox",
            "--",
            "sh",
            "-c",
            "echo up; sleep 60",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .start();
    let out = Captured::spawn(first.stdout.take().unwrap());
    out.wait_for("up\n", BOOT);
    let o = f.output(&["--persist", d, "busybox", "--", "true"]);
    assert_eq!(code(&o), 125);
    assert!(
        text(&o.stderr).contains("in use by another kiln run"),
        "{}",
        text(&o.stderr)
    );
    signal(&first, rustix::process::Signal::TERM);
    assert_eq!(wait(&mut first, Duration::from_secs(30)), 143);
    // A reset starts from the template, at the size asked for.
    let o = f.output(&[
        "--persist",
        d,
        "--persist-reset",
        "--disk",
        "64M",
        "busybox",
        "--",
        "cat",
        "/keep",
    ]);
    assert_eq!(code(&o), 1, "a reset disk is empty: {}", text(&o.stderr));
    assert_eq!(std::fs::metadata(&disk).unwrap().len(), 64 << 20);
}

/// The scratch disk lives on the store's filesystem, in `$KILN_HOME/scratch/<id>/`
```

- [ ] **Step 9: Modify `README.md`**

`--persist`.

In `README.md`, replace:
```markdown
- Other flags: `--vmm firecracker|cloud-hypervisor` (default Firecracker), `--cpus N`, `--memory MiB`, `--disk SIZE` (the scratch disk, default 4G), `-e KEY[=VALUE]`. kiln's flags go before IMAGE: as with `docker run`, everything after IMAGE is the command, flags included (`kiln run img -i` runs `-i`), and a `--` right after IMAGE is optional. `--persist`, `--net` and `-p` arrive with milestone M3b-2; until then they are not kiln flags.
```
with:
```markdown
- Other flags: `--vmm firecracker|cloud-hypervisor` (default Firecracker), `--cpus N`, `--memory MiB`, `--disk SIZE` (the scratch disk, default 4G), `-e KEY[=VALUE]`. kiln's flags go before IMAGE: as with `docker run`, everything after IMAGE is the command, flags included (`kiln run img -i` runs `-i`), and a `--` right after IMAGE is optional. `--net` and `-p` are not kiln flags yet.
- `--persist PATH` keeps the scratch disk (everything the guest wrote over the image) in the file PATH across runs, with a sidecar `PATH.kiln.json` naming the image and its layers. A disk is reused only with the same app layers in the same order, since overlayfs cannot keep files over other layers; another manifest with the same layers (a changed env or cmd) reuses it, with a note. `--persist-reset` recreates it and `--persist-force` reuses it anyway, with a warning. It is never shrunk (a smaller `--disk` is refused; a larger one grows it), two runs never share it, and kiln never removes it.
```

- [ ] **Step 10: Run the crate's tests**

Run:
```bash
cargo test -q -p kiln-run
```
Expected: all pass (38 tests on macOS, 120 on Linux).

- [ ] **Step 11: Format, lint and run the whole workspace**

Run:
```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test --all -q
```
Expected: no clippy warnings; every test passes (413 tests).

- [ ] **Step 12: Lint and test on Linux**

In the Lima VM (`limactl shell vmkit`, then `cd` to the repository and `export CARGO_TARGET_DIR=$HOME/target-m3b2`):

Run (Linux):
```bash
cargo clippy --all-targets -- -D warnings && cargo test --all -q
```
Expected: no clippy warnings; every test passes (514 tests; the boot and `kiln run` cases skip themselves without their environment).

- [ ] **Step 13: Run the `--persist` case end to end**

In the Lima VM, in an `sg kvm` shell, with Firecracker and Cloud Hypervisor on `PATH`:

Run (Linux, KVM):
```bash
kernel=/Users/alfonso/Github/Personal/playground/vmkit/out/vmlinux-6.18.54-aarch64
KILN_TEST_NET=1 KILN_TEST_VMM=firecracker scripts/boot-tests.sh $kernel persisted_disks -- --test-threads=4
```
Expected: `persisted_disks_are_reused_and_guarded` passes (seven runs, about a minute in the Lima VM).

- [ ] **Step 14: Commit**

```bash
git add -A crates README.md
git commit -m 'feat(kiln-run): --persist keeps the scratch disk across runs, guarded by its sidecar and a lock'
```

### Task 4: Networking: `--net`, `--egress` and `-p` (published on 127.0.0.1 by default); a resolver that refuses at once without a network

**Files:**
- Modify: `README.md`, `crates/kiln-init/src/etc.rs`, `crates/kiln-run/Cargo.toml`, `crates/kiln-run/src/config.rs`, `crates/kiln-run/src/linux/mod.rs`, `crates/kiln-run/src/linux/run.rs`, `crates/kiln-run/src/options.rs`, `crates/kiln-run/tests/boot.rs`, `crates/kiln/src/run_cmd.rs`, `crates/kiln/tests/cli.rs`, `crates/kiln/tests/run.rs`, `docs/format.md`
- `Cargo.lock` (cargo updates it)

**Interfaces:**
- Consumes: vmkit at the forward-bind-address change (Global Constraints): `NetSpec { egress, allow, forwards }`, `PortForward { protocol, address, host, guest }`, `net::{Cidr, Egress, Protocol, GUEST, GATEWAY, PREFIX, outbound_address}`, `NetSpec::check_allow(&[Ipv4Addr]) -> Result<(), String>`, `Cidr::contains`; M3b-1's `config::check_devices(layers, net, ..)` and `vm_spec(.., net, ..)`; `kiln_proto::Network`.
- Produces:
  - `kiln_run::options`: `Egress::{Restricted (default), DenyAll, Open}`, `Protocol::{Tcp, Udp}`, `PortSpec { address, host, guest, protocol }` (`FromStr`: `[ADDRESS:]HOST:GUEST[/tcp|udp]`), `DEFAULT_PUBLISH_ADDRESS` (`127.0.0.1`), `Cidr { addr, prefix }` (`FromStr`), `NetOptions { egress, allow, publish }` with `add_egress(value, &mut policy_set)`; `RunOptions::net: Option<NetOptions>`; `check()` refuses a host port published twice per protocol.
  - `kiln_run::config::{GUEST, GATEWAY, PREFIX}`; `Config.network` set with `--net`.
  - `kiln_run::net_spec(&NetOptions) -> vmkit::NetSpec` (Linux).
  - `kiln run --net [--egress POLICY]... [-p [ADDRESS:]HOST:GUEST[/tcp|udp]]...`.
  - kiln-init: `/etc/resolv.conf` without DNS servers ends with `nameserver 127.0.0.1`.

Networking for `kiln run` (spec §9.3), shaped by a real user's field report.

**Repin vmkit first.** Before this task the controller substitutes the vmkit revision placeholder in this task's `Cargo.toml` step (Global Constraints). The pinned change adds `PortForward::address` (pasta binds each forward to its own address), `NetSpec::check_allow` and `net::outbound_address`; it is based on vmkit PR #6, whose generalised `sandbox` keeps the `pid_file` and `cgroups_available` kiln uses (P6).

- **`-p [ADDRESS:]HOST:GUEST[/tcp|udp]`**, repeatable, needs `--net`. The address defaults to `127.0.0.1` (Docker's is `0.0.0.0`): an untrusted image's ports stay on this machine unless the user says otherwise, and Lima carries loopback listeners to the Mac's `localhost` (Task 5). A host port is published once per protocol whatever its addresses: the VM's namespace sees only the port.
- **The listen port belongs to the image** (potter sets it); kiln does not guess. The app must listen on `0.0.0.0` or the guest's address; a forward to a guest listening on `127.0.0.1` cannot work, and its connections end at once (pinned end to end; the README says so).
- **`--egress`** `restricted` (default) | `deny-all` | `open` (warned) | `allow=CIDR` (repeatable). pasta gives the VM's namespace the host's outbound address (the one on its default route), so the guest can never reach it (finding 4.1): an exception that is only that address is refused before boot, by kiln (P7, with vmkit's own check) and again by vmkit; a wider exception that includes it is warned about, since its other addresses work. The host's other addresses (vmkit's network suite reaches its fixtures on `lo`) can be reached through an exception.
- **Addressing:** `Config.network` is vmkit's fixed `172.30.0.2/30`, gateway and DNS `172.30.0.1` (D5).
- **A resolver without a network:** kiln-init wrote only a comment into `/etc/resolv.conf`, so tools that start a resolver refused to run. It now adds `nameserver 127.0.0.1`, as Docker's `--network none` lists one: lookups fail at once (P8: also when a network names no DNS server). `docs/format.md` is normative for this.

- [ ] **Step 1: Modify `crates/kiln/tests/cli.rs`**

Refusals: `-p` or `--egress` without `--net`, a bad `-p` or `--egress`, two policies, a port published twice, and (Linux) an exception for the host's outbound address.

In `crates/kiln/tests/cli.rs`, replace:
```rust
        .stderr(predicate::str::contains("exclude each other"));
}
```
with:
```rust
        .stderr(predicate::str::contains("exclude each other"));
    for (args, needle) in [
        (&["run", "-p", "8080:80", "img"][..], "need --net"),
        (&["run", "--egress", "deny-all", "img"], "need --net"),
        (&["run", "--net", "--egress", "everything", "img"], "unknown --egress"),
        (
            &["run", "--net", "--egress", "open", "--egress", "deny-all", "img"],
            "two policies",
        ),
        (
            &["run", "--net", "--egress", "allow=10.0.0.0/40", "img"],
            "not an IPv4 address or CIDR",
        ),
        (
            &["run", "--net", "-p", "8080:80", "-p", "0.0.0.0:8080:81", "img"],
            "published twice",
        ),
    ] {
        kiln(home.path())
            .args(args)
            .assert()
            .code(125)
            .stderr(predicate::str::contains(needle));
    }
}
```

In `crates/kiln/tests/cli.rs`, replace:
```rust
        (&["run", "-e"], "-e"),
    ] {
```
with:
```rust
        (&["run", "-e"], "-e"),
        (&["run", "--net", "-p", "80", "img"], "[ADDRESS:]HOST:GUEST"),
    ] {
```

In `crates/kiln/tests/cli.rs`, replace:
```rust
    kiln(home.path()).args(["run", "missing"]).assert().code(125);
    // -t -i puts kiln's terminal in raw mode: without one on stdin, a clear error.
```
with:
```rust
    kiln(home.path()).args(["run", "missing"]).assert().code(125);
    // An egress exception for the host's outbound address is refused before boot
    // (pasta gives the VM's namespace that address).
    let outbound = {
        let s = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
        s.connect("192.0.2.1:9").unwrap();
        s.local_addr().unwrap().ip()
    };
    kiln(home.path())
        .args(["run", "--net", "--egress", &format!("allow={outbound}"), "app"])
        .assert()
        .code(125)
        .stderr(predicate::str::contains(format!(
            "is the host's own address {outbound}"
        )));
    // -t -i puts kiln's terminal in raw mode: without one on stdin, a clear error.
```

- [ ] **Step 2: Run the tests to see them fail**

Run:
```bash
cargo test -q -p kiln --test cli run_
```
Expected: FAIL: `kiln run` has no `--net`, `--egress` or `-p` yet (the refusals name unexpected arguments instead).

- [ ] **Step 3: Modify `crates/kiln-run/Cargo.toml`**

The vmkit pin; its revision is the controller's substitution (Global Constraints). `Cargo.lock` follows on the first build.

In `crates/kiln-run/Cargo.toml`, replace:
```toml
# Pinned to the commit where the VMM leads its own process group (vmkit PR #5).
[target.'cfg(target_os = "linux")'.dependencies]
rustix = { version = "1.1.5", features = ["event", "net", "pipe", "stdio", "termios"] }
signal-hook = { version = "0.4.4", default-features = false, features = ["iterator"] }
vmkit = { git = "https://github.com/AlfonsoCampodonico/vmkit", rev = "4aa091361798dd6fa4e99d6494efb235bd727d20" }
```
with:
```toml
# Pinned to the commit where port forwards name their host address and `allow`
# entries covering the host are refused (vmkit's forward-bind-address).
[target.'cfg(target_os = "linux")'.dependencies]
rustix = { version = "1.1.5", features = ["event", "net", "pipe", "stdio", "termios"] }
signal-hook = { version = "0.4.4", default-features = false, features = ["iterator"] }
vmkit = { git = "https://github.com/AlfonsoCampodonico/vmkit", rev = "<VMKIT_REV>" }
```

- [ ] **Step 4: Modify `crates/kiln-run/src/options.rs`**

The flags' syntax; parsed and checked on every platform.

In `crates/kiln-run/src/options.rs`, replace:
```rust
//! What the user asked for (`kiln run`'s flags, spec §9.7), and parsing sizes.

```
with:
```rust
//! What the user asked for (`kiln run`'s flags, spec §9.7), and parsing the values
//! that have their own syntax: sizes, `--egress` and `-p`.

use std::net::Ipv4Addr;
```

In `crates/kiln-run/src/options.rs`, replace:
```rust

/// Everything `kiln run` was asked to do.
```
with:
```rust

/// `--egress` (spec §9.3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Egress {
    /// Everything but private, link-local (cloud metadata), CGNAT, loopback,
    /// multicast and reserved ranges, and the host's own addresses.
    #[default]
    Restricted,
    /// Nothing but DNS and the `allow=` exceptions.
    DenyAll,
    /// Everything (kiln warns).
    Open,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Tcp,
    Udp,
}

/// `-p [ADDRESS:]HOST:GUEST[/tcp|udp]`: the guest's port `guest` on the host's
/// `address:host`. The address defaults to `127.0.0.1`, so a published port stays
/// on this host unless an address (`0.0.0.0` for all) is given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortSpec {
    pub address: Ipv4Addr,
    pub host: u16,
    pub guest: u16,
    pub protocol: Protocol,
}

/// The host address `-p` binds when none is given.
pub const DEFAULT_PUBLISH_ADDRESS: Ipv4Addr = Ipv4Addr::LOCALHOST;

impl FromStr for PortSpec {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, String> {
        let bad = || format!("{s:?} is not [ADDRESS:]HOST:GUEST[/tcp|udp] with an IPv4 address and ports 1-65535");
        let (rest, protocol) = match s.split_once('/') {
            Some((p, "tcp")) => (p, Protocol::Tcp),
            Some((p, "udp")) => (p, Protocol::Udp),
            Some(_) => return Err(bad()),
            None => (s, Protocol::Tcp),
        };
        let parts: Vec<&str> = rest.split(':').collect();
        let (address, h, g) = match parts.as_slice() {
            [h, g] => (DEFAULT_PUBLISH_ADDRESS, *h, *g),
            [a, h, g] => (a.parse().map_err(|_| bad())?, *h, *g),
            _ => return Err(bad()),
        };
        let port = |p: &str| p.parse::<u16>().ok().filter(|&n| n != 0).ok_or_else(bad);
        Ok(PortSpec {
            address,
            host: port(h)?,
            guest: port(g)?,
            protocol,
        })
    }
}

/// An IPv4 network for `--egress allow=`; a bare address is a /32.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    pub addr: Ipv4Addr,
    pub prefix: u8,
}

impl FromStr for Cidr {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, String> {
        let bad = || format!("{s:?} is not an IPv4 address or CIDR");
        let (a, p) = s.split_once('/').unwrap_or((s, "32"));
        let prefix = p.parse::<u8>().ok().filter(|&p| p <= 32).ok_or_else(bad)?;
        Ok(Cidr {
            addr: a.parse().map_err(|_| bad())?,
            prefix,
        })
    }
}

/// `--net` and its options.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetOptions {
    pub egress: Egress,
    pub allow: Vec<Cidr>,
    pub publish: Vec<PortSpec>,
}

impl NetOptions {
    /// Applies one `--egress` value: `restricted`, `deny-all`, `open` or `allow=CIDR`
    /// (repeatable; two different policies are an error).
    pub fn add_egress(&mut self, value: &str, policy_set: &mut bool) -> std::result::Result<(), String> {
        if let Some(cidr) = value.strip_prefix("allow=") {
            self.allow.push(cidr.parse()?);
            return Ok(());
        }
        let policy = match value {
            "restricted" => Egress::Restricted,
            "deny-all" => Egress::DenyAll,
            "open" => Egress::Open,
            _ => {
                return Err(format!(
                    "unknown --egress {value:?} (restricted, deny-all, open or allow=CIDR)"
                ));
            }
        };
        if *policy_set && self.egress != policy {
            return Err("--egress names two policies".into());
        }
        *policy_set = true;
        self.egress = policy;
        Ok(())
    }
}

/// Everything `kiln run` was asked to do.
```

In `crates/kiln-run/src/options.rs`, replace:
```rust
    pub persist_force: bool,
    /// `--env K=V` or `--env K` (taken from kiln's environment, dropped when unset).
```
with:
```rust
    pub persist_force: bool,
    /// `--net`, with its options.
    pub net: Option<NetOptions>,
    /// `--env K=V` or `--env K` (taken from kiln's environment, dropped when unset).
```

In `crates/kiln-run/src/options.rs`, replace:
```rust
            persist_force: false,
            env: Vec::new(),
```
with:
```rust
            persist_force: false,
            net: None,
            env: Vec::new(),
```

In `crates/kiln-run/src/options.rs`, replace:
```rust
            return Err(Error::invalid("--cpus/--memory", "at least 1 vCPU and 64 MiB"));
        }
```
with:
```rust
            return Err(Error::invalid("--cpus/--memory", "at least 1 vCPU and 64 MiB"));
        }
        if let Some(net) = &self.net {
            // The VM's namespace sees only the port, whatever address it was published on.
            for (i, p) in net.publish.iter().enumerate() {
                if net.publish[..i]
                    .iter()
                    .any(|q| q.protocol == p.protocol && q.host == p.host)
                {
                    return Err(Error::invalid("-p", format!("host port {} is published twice", p.host)));
                }
            }
        }
```

In `crates/kiln-run/src/options.rs`, replace:
```rust
    #[test]
    fn custom_artifacts_need_their_flags() {
```
with:
```rust
    #[test]
    fn published_ports() {
        let p: PortSpec = "8080:80".parse().unwrap();
        assert_eq!(
            p,
            PortSpec {
                address: Ipv4Addr::LOCALHOST,
                host: 8080,
                guest: 80,
                protocol: Protocol::Tcp
            }
        );
        let p: PortSpec = "0.0.0.0:5353:53/udp".parse().unwrap();
        assert_eq!(
            (p.address, p.host, p.guest, p.protocol),
            (Ipv4Addr::UNSPECIFIED, 5353, 53, Protocol::Udp)
        );
        assert_eq!(
            "192.168.5.15:1:2/tcp".parse::<PortSpec>().unwrap().address,
            Ipv4Addr::new(192, 168, 5, 15)
        );
        for bad in [
            "80",
            "0:80",
            "80:0",
            "a:80",
            "80:80/sctp",
            "::1:80:80",
            "1.2.3:80:80",
            "x:1:2:3",
            "70000:80",
        ] {
            let e = bad.parse::<PortSpec>().unwrap_err();
            assert!(e.contains("[ADDRESS:]HOST:GUEST"), "{bad}: {e}");
        }
        let mut o = RunOptions::new("img");
        let mut net = NetOptions {
            publish: vec!["8080:80".parse().unwrap(), "0.0.0.0:8080:81".parse().unwrap()],
            ..NetOptions::default()
        };
        o.net = Some(net.clone());
        assert!(o.check().unwrap_err().to_string().contains("published twice"));
        net.publish[1].protocol = Protocol::Udp;
        o.net = Some(net);
        o.check().unwrap();
    }

    #[test]
    fn egress_values() {
        let mut n = NetOptions::default();
        let mut set = false;
        n.add_egress("allow=10.1.0.0/16", &mut set).unwrap();
        n.add_egress("allow=1.1.1.1", &mut set).unwrap();
        n.add_egress("deny-all", &mut set).unwrap();
        n.add_egress("deny-all", &mut set).unwrap();
        assert_eq!(n.egress, Egress::DenyAll);
        assert_eq!(
            n.allow[1],
            Cidr {
                addr: Ipv4Addr::new(1, 1, 1, 1),
                prefix: 32
            }
        );
        assert!(n.add_egress("open", &mut set).unwrap_err().contains("two policies"));
        assert!(n.add_egress("allow=10.0.0.0/33", &mut set).is_err());
        assert!(
            n.add_egress("everything", &mut set)
                .unwrap_err()
                .contains("unknown --egress")
        );
    }

    #[test]
    fn custom_artifacts_need_their_flags() {
```

- [ ] **Step 5: Modify `crates/kiln-run/src/config.rs`**

The guest's addressing with `--net`.

In `crates/kiln-run/src/config.rs`, replace:
```rust
use kiln_image::types::Process as ImageProcess;
use kiln_proto::{Config, ExitMethod, HostMessage, MAX_FRAME, Process, ProtoError, Scratch, WindowSize};

use crate::error::{Error, Result};
use crate::options::RunOptions;
```
with:
```rust
use std::net::Ipv4Addr;

use kiln_image::types::Process as ImageProcess;
use kiln_proto::{Config, ExitMethod, HostMessage, MAX_FRAME, Network, Process, ProtoError, Scratch, WindowSize};

use crate::error::{Error, Result};
use crate::options::RunOptions;

/// The guest's address on vmkit's tap network (spec §9.3; vmkit's `net::GUEST`,
/// which a Linux test compares).
pub const GUEST: Ipv4Addr = Ipv4Addr::new(172, 30, 0, 2);
/// The guest's gateway and DNS server (vmkit's `net::GATEWAY`).
pub const GATEWAY: Ipv4Addr = Ipv4Addr::new(172, 30, 0, 1);
/// vmkit's `net::PREFIX`.
pub const PREFIX: u8 = 30;
```

In `crates/kiln-run/src/config.rs`, replace:
```rust
        network: None,
```
with:
```rust
        // `--net`: vmkit's fixed addressing, with DNS through the gateway.
        network: opts.net.as_ref().map(|_| Network {
            address: GUEST,
            prefix_len: PREFIX,
            gateway: GATEWAY,
            dns: vec![GATEWAY],
        }),
```

In `crates/kiln-run/src/config.rs`, replace:
```rust
        o.interactive = true;
        o.stop_timeout = 3;
```
with:
```rust
        o.interactive = true;
        o.net = Some(Default::default());
        o.stop_timeout = 3;
```

In `crates/kiln-run/src/config.rs`, replace:
```rust
        assert_eq!(c.tty, Some(WindowSize { rows: 24, cols: 80 }));
        assert_eq!(c.shutdown_grace_secs, 3);
```
with:
```rust
        assert_eq!(c.tty, Some(WindowSize { rows: 24, cols: 80 }));
        assert_eq!(
            c.network,
            Some(Network {
                address: GUEST,
                prefix_len: 30,
                gateway: GATEWAY,
                dns: vec![GATEWAY]
            })
        );
        assert_eq!(c.shutdown_grace_secs, 3);
```

- [ ] **Step 6: Modify `crates/kiln-run/src/linux/run.rs`**

`net_spec`; the `allow=` check and the `open` warning before anything is prepared; the NIC in the device budget and the VM. Two tests: kiln's addressing is vmkit's, and the options map one to one.

In `crates/kiln-run/src/linux/run.rs`, replace:
```rust
use crate::options::{DEFAULT_DISK, RunOptions, VmmKind};
```
with:
```rust
use crate::options::{DEFAULT_DISK, Egress, NetOptions, Protocol, RunOptions, VmmKind};
```

In `crates/kiln-run/src/linux/run.rs`, replace:
```rust
        GuestExit::Poweroff => ExitMethod::Poweroff,
    }
```
with:
```rust
        GuestExit::Poweroff => ExitMethod::Poweroff,
    }
}

/// `--net`'s options as vmkit's (spec §9.3).
pub fn net_spec(n: &NetOptions) -> NetSpec {
    use vmkit::net::{Cidr, Egress as E, PortForward, Protocol as P};
    NetSpec {
        egress: match n.egress {
            Egress::Restricted => E::Restricted,
            Egress::DenyAll => E::DenyAll,
            Egress::Open => E::Open,
        },
        allow: n.allow.iter().filter_map(|c| Cidr::new(c.addr, c.prefix)).collect(),
        forwards: n
            .publish
            .iter()
            .map(|p| PortForward {
                protocol: match p.protocol {
                    Protocol::Tcp => P::Tcp,
                    Protocol::Udp => P::Udp,
                },
                address: p.address,
                host: p.host,
                guest: p.guest,
            })
            .collect(),
    }
```

In `crates/kiln-run/src/linux/run.rs`, replace:
```rust
        opts.check()?;
        // Held until the VMM has every blob open (spec §5.2).
```
with:
```rust
        opts.check()?;
        let net = opts.net.as_ref().map(net_spec);
        if let Some(n) = &net {
            // pasta gives the VM's namespace the host's outbound address, so the guest
            // can never reach it (finding 4.1): an exception for it alone is refused,
            // a wider one that includes it is warned about.
            let outbound = vmkit::net::outbound_address();
            n.check_allow(outbound.as_slice())
                .map_err(|e| Error::refused(format!("--egress: {e}")))?;
            if n.egress == vmkit::net::Egress::Open {
                notice("warning: --egress open: the guest can reach private networks, the host and cloud metadata");
            } else if let Some(a) = outbound {
                for c in n.allow.iter().filter(|c| c.contains(a)) {
                    notice(&format!(
                        "warning: --egress allow={c} includes the host's own address {a}, which the guest can never \
                         reach"
                    ));
                }
            }
        }
        // Held until the VMM has every blob open (spec §5.2).
```

In `crates/kiln-run/src/linux/run.rs`, replace:
```rust
        config::check_devices(prepared.layers.len(), false, caps.available_devices(), vmm.name())?;
```
with:
```rust
        config::check_devices(
            prepared.layers.len(),
            net.is_some(),
            caps.available_devices(),
            vmm.name(),
        )?;
```

In `crates/kiln-run/src/linux/run.rs`, replace:
```rust
            None,
```
with:
```rust
            net,
```

In `crates/kiln-run/src/linux/run.rs`, replace:
```rust

    /// The VM ended without the guest's final message: the likely causes, and
```
with:
```rust

    /// kiln's copy of vmkit's fixed addressing (the platform-independent config
    /// builds the guest's `Config` from it) matches vmkit's.
    #[test]
    fn the_guest_addressing_is_vmkits() {
        assert_eq!(config::GUEST, vmkit::net::GUEST);
        assert_eq!(config::GATEWAY, vmkit::net::GATEWAY);
        assert_eq!(config::PREFIX, vmkit::net::PREFIX);
    }

    #[test]
    fn net_options_map_onto_vmkit() {
        use std::net::Ipv4Addr;
        let mut n = NetOptions::default();
        let mut set = false;
        n.add_egress("deny-all", &mut set).unwrap();
        n.add_egress("allow=10.1.2.3/16", &mut set).unwrap();
        n.add_egress("allow=192.0.2.7", &mut set).unwrap();
        n.publish = vec!["8080:80".parse().unwrap(), "0.0.0.0:5353:53/udp".parse().unwrap()];
        let spec = net_spec(&n);
        assert_eq!(spec.egress, vmkit::net::Egress::DenyAll);
        assert_eq!(spec.allow[0].to_string(), "10.1.0.0/16");
        let f = &spec.forwards;
        assert_eq!(
            (f[0].address, f[0].host, f[0].guest, f[0].protocol),
            (Ipv4Addr::LOCALHOST, 8080, 80, vmkit::net::Protocol::Tcp)
        );
        assert_eq!(
            (f[1].address, f[1].protocol),
            (Ipv4Addr::UNSPECIFIED, vmkit::net::Protocol::Udp)
        );
        // An exception that is only the host's outbound address is refused before boot;
        // one that includes it still reaches its other addresses.
        assert!(spec.check_allow(&[Ipv4Addr::new(192, 0, 2, 7)]).is_err());
        assert!(spec.check_allow(&[Ipv4Addr::new(10, 1, 9, 9)]).is_ok());
    }

    /// The VM ended without the guest's final message: the likely causes, and
```

- [ ] **Step 7: Modify `crates/kiln-run/src/linux/mod.rs`**

Export `net_spec`.

In `crates/kiln-run/src/linux/mod.rs`, replace:
```rust
pub use run::{CMDLINE, Report, Run, backend, exit_method, vm_spec, vmm_identity};
```
with:
```rust
pub use run::{CMDLINE, Report, Run, backend, exit_method, net_spec, vm_spec, vmm_identity};
```

- [ ] **Step 8: Modify `crates/kiln/src/run_cmd.rs`**

The flags.

In `crates/kiln/src/run_cmd.rs`, replace:
```rust
use kiln_run::options::{DEFAULT_CPUS, DEFAULT_MEMORY_MIB, DEFAULT_STOP_TIMEOUT, parse_size};
```
with:
```rust
use kiln_run::options::{DEFAULT_CPUS, DEFAULT_MEMORY_MIB, DEFAULT_STOP_TIMEOUT, NetOptions, PortSpec, parse_size};
```

In `crates/kiln/src/run_cmd.rs`, replace:
```rust
    persist_force: bool,
    /// KEY=VALUE, or KEY to pass kiln's own value (repeatable).
```
with:
```rust
    persist_force: bool,
    /// Give the guest a network (eth0, egress through the host).
    #[arg(long)]
    net: bool,
    /// With --net: restricted (the default), deny-all, open, or allow=CIDR (repeatable).
    #[arg(long, value_name = "POLICY")]
    egress: Vec<String>,
    /// With --net: publish a guest port on the host, [ADDRESS:]HOST:GUEST[/tcp|udp]
    /// (repeatable). The address defaults to 127.0.0.1; 0.0.0.0 publishes it on
    /// every address.
    #[arg(short = 'p', long = "publish", value_name = "[ADDRESS:]HOST:GUEST", value_parser = |s: &str| s.parse::<PortSpec>())]
    publish: Vec<PortSpec>,
    /// KEY=VALUE, or KEY to pass kiln's own value (repeatable).
```

In `crates/kiln/src/run_cmd.rs`, replace:
```rust
    pub fn options(&self) -> Result<RunOptions> {
        let o = RunOptions {
```
with:
```rust
    pub fn options(&self) -> Result<RunOptions> {
        let net = if self.net {
            let mut n = NetOptions {
                publish: self.publish.clone(),
                ..NetOptions::default()
            };
            let mut policy_set = false;
            for e in &self.egress {
                n.add_egress(e, &mut policy_set).map_err(anyhow::Error::msg)?;
            }
            Some(n)
        } else {
            if !self.egress.is_empty() || !self.publish.is_empty() {
                anyhow::bail!("--egress and -p need --net");
            }
            None
        };
        let o = RunOptions {
```

In `crates/kiln/src/run_cmd.rs`, replace:
```rust
            persist_force: self.persist_force,
            env: self.env.clone(),
```
with:
```rust
            persist_force: self.persist_force,
            net,
            env: self.env.clone(),
```

- [ ] **Step 9: Modify `crates/kiln-init/src/etc.rs`**

`nameserver 127.0.0.1` without DNS servers.

In `crates/kiln-init/src/etc.rs`, replace:
```rust
pub fn resolv_conf(network: Option<&Network>) -> String {
    match network {
        Some(n) if !n.dns.is_empty() => n.dns.iter().map(|d| format!("nameserver {d}\n")).collect(),
        Some(_) => "# kiln: no DNS servers configured\n".to_string(),
        None => "# kiln: this VM has no network\n".to_string(),
```
with:
```rust
/// The guest's resolvers. Without any, `nameserver 127.0.0.1` (as Docker's
/// `--network none` still lists one): lookups fail at once with "connection
/// refused" instead of tools refusing to start without a resolver.
pub fn resolv_conf(network: Option<&Network>) -> String {
    match network {
        Some(n) if !n.dns.is_empty() => n.dns.iter().map(|d| format!("nameserver {d}\n")).collect(),
        Some(_) => "# kiln: no DNS servers configured\nnameserver 127.0.0.1\n".to_string(),
        None => "# kiln: this VM has no network\nnameserver 127.0.0.1\n".to_string(),
```

In `crates/kiln-init/src/etc.rs`, replace:
```rust
        assert!(resolv_conf(None).starts_with('#'));
        assert!(resolv_conf(None).contains("no network"));
        let no_dns = Network { dns: vec![], ..net };
        assert_eq!(resolv_conf(Some(&no_dns)), "# kiln: no DNS servers configured\n");
```
with:
```rust
        assert_eq!(
            resolv_conf(None),
            "# kiln: this VM has no network\nnameserver 127.0.0.1\n"
        );
        let no_dns = Network { dns: vec![], ..net };
        assert_eq!(
            resolv_conf(Some(&no_dns)),
            "# kiln: no DNS servers configured\nnameserver 127.0.0.1\n"
        );
```

- [ ] **Step 10: Modify `docs/format.md`**

The normative guest description.

In `docs/format.md`, replace:
```markdown
`/etc/hosts` has Docker's layout: `127.0.0.1 localhost`, the IPv6 loopback and multicast names, then the guest's address (or `127.0.1.1` without a network) with the hostname. `/etc/resolv.conf` lists `Config.network.dns` as `nameserver` lines; without a network it holds only the comment `# kiln: this VM has no network`, and with a network but no DNS servers only `# kiln: no DNS servers configured`.
```
with:
```markdown
`/etc/hosts` has Docker's layout: `127.0.0.1 localhost`, the IPv6 loopback and multicast names, then the guest's address (or `127.0.1.1` without a network) with the hostname. `/etc/resolv.conf` lists `Config.network.dns` as `nameserver` lines. Without a network it holds the comment `# kiln: this VM has no network`, and with a network but no DNS servers the comment `# kiln: no DNS servers configured`; either is followed by `nameserver 127.0.0.1`, as Docker's `--network none` still lists a resolver, so that lookups fail at once (nothing answers there) instead of tools that need a resolver refusing to start.
```

- [ ] **Step 11: Modify `crates/kiln-run/tests/boot.rs`**

The boot case's expected `/etc/resolv.conf`; the networking case names its forward's address.

In `crates/kiln-run/tests/boot.rs`, replace:
```rust
        out.contains("127.0.1.1\tkiln-guest\n# kiln: this VM has no network\n"),
```
with:
```rust
        out.contains("127.0.1.1\tkiln-guest\n# kiln: this VM has no network\nnameserver 127.0.0.1\n"),
```

In `crates/kiln-run/tests/boot.rs`, replace:
```rust
            protocol: Protocol::Tcp,
            host: port,
```
with:
```rust
            protocol: Protocol::Tcp,
            address: std::net::Ipv4Addr::LOCALHOST,
            host: port,
```

- [ ] **Step 12: Modify `crates/kiln/tests/run.rs`**

End to end: one VM with three published ports (the default loopback address, refused on the host's other address; `0.0.0.0`, reached on the host's address; a guest listener on `127.0.0.1` only, whose connection ends at once); the egress policy against the host and `1.1.1.1:80`; the `open` warning; the resolver without a network. The busybox image gains `httpd` and `nslookup` (P9).

In `crates/kiln/tests/run.rs`, replace:
```rust
const APPLETS: [&str; 17] = [
    "cat", "dd", "echo", "env", "grep", "hostname", "id", "kill", "ls", "nc", "sh", "sleep", "stty", "test", "true",
    "tty", "wc",
```
with:
```rust
const APPLETS: [&str; 19] = [
    "cat", "dd", "echo", "env", "grep", "hostname", "httpd", "id", "kill", "ls", "nc", "nslookup", "sh", "sleep",
    "stty", "test", "true", "tty", "wc",
```

In `crates/kiln/tests/run.rs`, replace:
```rust

/// The scratch disk lives on the store's filesystem, in `$KILN_HOME/scratch/<id>/`
```
with:
```rust

fn net_enabled() -> bool {
    let on = std::env::var_os("KILN_TEST_NET").is_some_and(|v| v == "1");
    assert!(
        on || std::env::var_os("KILN_REQUIRE_KVM_TESTS").is_none_or(|v| v != "1"),
        "KILN_REQUIRE_KVM_TESTS=1 but KILN_TEST_NET is not (needs pasta and nft)"
    );
    on
}

/// The host's primary address (no packet is sent).
fn host_ip() -> std::net::Ipv4Addr {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
    s.connect("1.1.1.1:80").unwrap();
    match s.local_addr().unwrap().ip() {
        std::net::IpAddr::V4(a) => a,
        other => panic!("{other}"),
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("0.0.0.0:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// What `addr` sends before it closes, once it sends anything (retried while the
/// guest starts).
fn read_from(addr: (std::net::Ipv4Addr, u16)) -> String {
    let until = Instant::now() + BOOT;
    loop {
        let mut got = String::new();
        if let Ok(mut conn) = std::net::TcpStream::connect(addr) {
            conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let _ = conn.read_to_string(&mut got);
            if !got.is_empty() {
                return got;
            }
        }
        assert!(Instant::now() < until, "{addr:?} never answered");
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// `-p` (spec §9.3, rev 2.10): published on 127.0.0.1 unless an address is given,
/// so the host's other addresses refuse it; `0.0.0.0` publishes it on all of them.
/// A guest listening on its own loopback only cannot be reached: the connection
/// ends at once (the image must listen on 0.0.0.0), it does not hang.
#[test]
fn published_ports_bind_to_loopback_unless_told_otherwise() {
    let Some(f) = fixture() else { return };
    if !net_enabled() {
        return;
    }
    let (loopback, everywhere, guest_loopback) = (free_port(), free_port(), free_port());
    let script = "httpd -f -p 127.0.0.1:8080 -h / & \
                  (while :; do echo hello-from-guest | nc -l -p 8081; done) & \
                  (while :; do echo hello-everywhere | nc -l -p 8082; done) & \
                  sleep 1; echo up; sleep 120";
    let (p0, p1, p2) = (
        format!("{guest_loopback}:8080"),
        format!("{loopback}:8081"),
        format!("0.0.0.0:{everywhere}:8082"),
    );
    let mut child = f
        .run(&[
            "--net", "-p", &p0, "-p", &p1, "-p", &p2, "busybox", "--", "sh", "-c", script,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .start();
    let out = Captured::spawn(child.stdout.take().unwrap());
    out.wait_for("up\n", BOOT);
    let (lo, host) = (std::net::Ipv4Addr::LOCALHOST, host_ip());
    assert_eq!(read_from((lo, loopback)), "hello-from-guest\n");
    let refused = std::net::TcpStream::connect((host, loopback)).map(|_| ()).unwrap_err();
    assert_eq!(
        refused.kind(),
        std::io::ErrorKind::ConnectionRefused,
        "{host}: {refused}"
    );
    assert_eq!(read_from((host, everywhere)), "hello-everywhere\n");
    // The guest's 127.0.0.1-only listener: closed or reset quickly, never a hang.
    let started = Instant::now();
    let ended = match std::net::TcpStream::connect((lo, guest_loopback)) {
        Ok(mut conn) => {
            conn.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
            let mut buf = Vec::new();
            match conn.read_to_end(&mut buf) {
                Ok(_) => buf.is_empty(),
                Err(e) => matches!(
                    e.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                ),
            }
        }
        Err(e) => e.kind() == std::io::ErrorKind::ConnectionRefused,
    };
    assert!(
        ended,
        "the connection to a loopback-only guest listener did not end cleanly"
    );
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    signal(&child, rustix::process::Signal::TERM);
    assert_eq!(wait(&mut child, Duration::from_secs(30)), 143, "{}", out.text());
}

/// T5 and `--egress` (spec §9.3): the host's own address is denied, a public
/// address is reachable by default, not under deny-all, and again with an
/// allow= exception (1.1.1.1:80, so this needs the internet). Without `--net`,
/// lookups fail at once instead of tools lacking a resolver (rev 2.10).
#[test]
fn egress_policy_and_the_resolver_without_a_network() {
    let Some(f) = fixture() else { return };
    if !net_enabled() {
        return;
    }
    let server = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    let hport = server.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for c in server.incoming().flatten() {
            let _ = (&c).write_all(b"host\n");
        }
    });
    let probe = |addr: String| format!("nc -w 3 {addr} </dev/null >/dev/null 2>&1 && echo reached || echo blocked");
    let o = f.output(&[
        "--net",
        "busybox",
        "--",
        "sh",
        "-c",
        &probe(format!("{} {hport}", host_ip())),
    ]);
    assert_eq!(text(&o.stdout), "blocked\n", "{}", text(&o.stderr));
    let public = probe("1.1.1.1 80".into());
    for (egress, want) in [
        (&[][..], "reached\n"),
        (&["--egress", "deny-all"][..], "blocked\n"),
        (&["--egress", "deny-all", "--egress", "allow=1.1.1.1"][..], "reached\n"),
    ] {
        let mut args = vec!["--net"];
        args.extend_from_slice(egress);
        args.extend(["busybox", "--", "sh", "-c", &public]);
        let o = f.output(&args);
        assert_eq!(text(&o.stdout), want, "{egress:?}: {}", text(&o.stderr));
    }
    let o = f.output(&["--net", "--egress", "open", "busybox", "--", "true"]);
    assert!(
        text(&o.stderr).contains("warning: --egress open"),
        "{}",
        text(&o.stderr)
    );
    // No network: a resolver that refuses at once.
    let o = f.output(&[
        "busybox",
        "--",
        "sh",
        "-c",
        "grep nameserver /etc/resolv.conf; nslookup example.com >/dev/null 2>&1; echo rc=$?",
    ]);
    assert_eq!(
        (code(&o), text(&o.stdout)),
        (0, "nameserver 127.0.0.1\nrc=1\n".into()),
        "{}",
        text(&o.stderr)
    );
}

/// The scratch disk lives on the store's filesystem, in `$KILN_HOME/scratch/<id>/`
```

- [ ] **Step 13: Modify `README.md`**

`--net`, `--egress`, `-p` and the listen-port contract.

In `README.md`, replace:
```markdown
- Other flags: `--vmm firecracker|cloud-hypervisor` (default Firecracker), `--cpus N`, `--memory MiB`, `--disk SIZE` (the scratch disk, default 4G), `-e KEY[=VALUE]`. kiln's flags go before IMAGE: as with `docker run`, everything after IMAGE is the command, flags included (`kiln run img -i` runs `-i`), and a `--` right after IMAGE is optional. `--net` and `-p` are not kiln flags yet.
```
with:
```markdown
- Other flags: `--vmm firecracker|cloud-hypervisor` (default Firecracker), `--cpus N`, `--memory MiB`, `--disk SIZE` (the scratch disk, default 4G), `-e KEY[=VALUE]`. kiln's flags go before IMAGE: as with `docker run`, everything after IMAGE is the command, flags included (`kiln run img -i` runs `-i`), and a `--` right after IMAGE is optional.
- `--net` gives the guest `eth0` (`172.30.0.2/30`, gateway and DNS `172.30.0.1`) in the VM's own network namespace, with egress through the host by `pasta`. `--egress` sets what it may reach: `restricted` (the default: no private, link-local or cloud metadata, CGNAT, loopback or multicast ranges, and none of the host's own addresses), `deny-all` (DNS only), `open` (everything, with a warning), and `allow=CIDR` exceptions (repeatable). pasta gives the VM's namespace the host's outbound address (the one on its default route), so the guest can never reach that address: an exception for it alone is refused, and a wider one that includes it is warned about. The host's other addresses can be reached through an exception. Without `--net` the guest has only loopback, and its `/etc/resolv.conf` names `127.0.0.1`, so lookups fail at once.
- `-p [ADDRESS:]HOST:GUEST[/tcp|udp]` (repeatable, with `--net`) publishes a guest port on the host. The address defaults to `127.0.0.1`, unlike Docker's `0.0.0.0`: a published port stays on this machine unless you give `-p 0.0.0.0:8080:8080` (or another of the host's addresses). The image decides which port its app listens on, and the app must listen on `0.0.0.0` (or the guest's address) at the guest port: a forward to an app listening on `127.0.0.1` cannot work, and its connections end at once.
```

- [ ] **Step 14: Run the crate's tests**

Run:
```bash
cargo test -q -p kiln-run && cargo test -q -p kiln-init -- etc
```
Expected: all pass (40 tests on macOS, 124 on Linux) for kiln-run; kiln-init's `etc` test passes.

- [ ] **Step 15: Format, lint and run the whole workspace**

Run:
```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test --all -q
```
Expected: no clippy warnings; every test passes (415 tests).

- [ ] **Step 16: Lint and test on Linux**

In the Lima VM (`limactl shell vmkit`, then `cd` to the repository and `export CARGO_TARGET_DIR=$HOME/target-m3b2`):

Run (Linux):
```bash
cargo clippy --all-targets -- -D warnings && cargo test --all -q
```
Expected: no clippy warnings; every test passes (520 tests; the boot and `kiln run` cases skip themselves without their environment).

- [ ] **Step 17: Run the networking cases end to end and in the boot suite**

In the Lima VM, in an `sg kvm` shell, with Firecracker and Cloud Hypervisor on `PATH`:

Run (Linux, KVM):
```bash
kernel=/Users/alfonso/Github/Personal/playground/vmkit/out/vmlinux-6.18.54-aarch64
KILN_TEST_NET=1 KILN_TEST_VMM=firecracker scripts/boot-tests.sh $kernel published_ports -- --test-threads=4
KILN_TEST_NET=1 KILN_TEST_VMM=firecracker scripts/boot-tests.sh $kernel egress_policy -- --test-threads=4
KILN_TEST_NET=1 KILN_TEST_VMM=firecracker scripts/boot-tests.sh $kernel firecracker::networking_from_config -- --test-threads=4
KILN_TEST_NET=1 KILN_TEST_VMM=firecracker scripts/boot-tests.sh $kernel firecracker::hostname_hosts -- --test-threads=4
```
Expected: `published_ports_bind_to_loopback_unless_told_otherwise`, `egress_policy_and_the_resolver_without_a_network` (it needs the internet), and the boot cases `networking_from_config` and `hostname_hosts_and_resolv_conf_replace_symlinks` pass.

- [ ] **Step 18: Commit**

```bash
git add -A crates docs/format.md README.md Cargo.lock
git commit -m 'feat(kiln-run): --net, --egress and -p, published on 127.0.0.1 by default; a resolver that refuses at once without a network'
```

### Task 5: The Lima VM: `lima/kiln.yaml`, `KILN_IMPORT_FROM`, and the docs for `kiln run` on macOS

**Files:**
- Create: `lima/kiln.yaml`
- Modify: `README.md`, `crates/kiln-run/Cargo.toml`, `crates/kiln-run/src/lib.rs`, `crates/kiln-run/src/linux/run.rs`, `crates/kiln-run/src/prepare.rs`, `crates/kiln-run/tests/prepare.rs`, `crates/kiln/tests/cli.rs`, `crates/kiln/tests/run.rs`, `docs/format.md`

**Interfaces:**
- Consumes: `kiln_image::import_image`, `Store::open_read_only`, `kiln_registry::Reference`; Tasks 2–4.
- Produces:
  - `kiln_run::prepare::import_from(&Store, from: &Path, name) -> Result<Option<String>>` (a note when blobs were copied); `Run::start` calls it when `KILN_IMPORT_FROM` is set, before the store lock.
  - `kiln_run::LIMA_INSTRUCTIONS` points at `lima/kiln.yaml`.
  - `lima/kiln.yaml`.

macOS development (spec §10).

- **`lima/kiln.yaml`:** `vmType: vz`, nested virtualization, Firecracker 1.17 and Cloud Hypervisor 53 by checksum (as vmkit's `install-vmms.sh`), `passt`, `nftables`, `uidmap`, the user in `kvm`, the AppArmor profile for `/usr/local/bin/vmkit-sandbox`, the Mac's store mounted read-only at `/mnt/kiln-store`, and `KILN_IMPORT_FROM` set (environment and `/etc/profile.d`). `limactl validate` passes. Until releases exist, `kiln` and `vmkit-sandbox` are built in the VM and installed in `/usr/local/bin`.
- **`KILN_IMPORT_FROM`** (ruling 4): when the source store has the name (as given, or as its normalised reference), the image is imported (missing blobs only, each verified) and tagged again on every run, so the Mac's newest convert is what runs. It happens before the store lock; a failure is a warning (D6).
- **Published ports reach the Mac:** a `-p` port on the VM's `127.0.0.1` (the default) is carried to the Mac's `localhost` by Lima's default port forwarding of loopback listeners; verified by hand (Validation), so the template needs no `portForwards` entry.
- **Docs:** the README's Lima section, the format's layer and config descriptions (the boot layers as Task 2 made them; `config.init` informational, finding 4.3), and the instructions `kiln run` prints on macOS.

- [ ] **Step 1: Modify `crates/kiln-run/tests/prepare.rs`**

An image imported from another store, then nothing new, then the source's newer convert winning; a name the source lacks; a source that is no store.

At the end of `crates/kiln-run/tests/prepare.rs`, append:
```rust

/// `KILN_IMPORT_FROM` (spec §10, ruling 4): an image the source store has is
/// imported (only missing blobs) and tagged again on every run, so the source's
/// newest convert wins; names the source lacks are left to this store.
#[test]
fn images_are_imported_and_retagged_from_another_store() {
    let mac = setup();
    let home = tempfile::tempdir().unwrap();
    let vm = Store::open(home.path()).unwrap();
    let note = kiln_run::prepare::import_from(&vm, mac.store.root(), "app")
        .unwrap()
        .unwrap();
    assert!(
        note.starts_with("imported app from ") && note.ends_with(" blobs)"),
        "{note}"
    );
    let first = vm.get_ref("app").unwrap().unwrap();
    assert_eq!(Some(first.clone()), mac.store.get_ref("app").unwrap());
    // Nothing new: no note, the same tag.
    assert_eq!(
        kiln_run::prepare::import_from(&vm, mac.store.root(), "app").unwrap(),
        None
    );
    // A newer convert on the source wins over the older tag here.
    mac.with_boot_layers("app", "custom", b"\x7fELF init");
    let note = kiln_run::prepare::import_from(&vm, mac.store.root(), "app").unwrap();
    assert!(note.is_some());
    let second = vm.get_ref("app").unwrap().unwrap();
    assert_ne!(second, first);
    assert_eq!(Some(second), mac.store.get_ref("app").unwrap());
    // A name the source lacks, and a source that is no store.
    assert_eq!(
        kiln_run::prepare::import_from(&vm, mac.store.root(), "other").unwrap(),
        None
    );
    assert!(kiln_run::prepare::import_from(&vm, &home.path().join("nowhere"), "app").is_err());
}
```

- [ ] **Step 2: Run the tests to see them fail**

Run:
```bash
cargo test -q -p kiln-run --test prepare
```
Expected: FAIL to compile: `kiln_run::prepare::import_from` does not exist yet.

- [ ] **Step 3: Modify `crates/kiln-run/Cargo.toml`**

`kiln-registry` becomes a normal dependency (normalised references).

In `crates/kiln-run/Cargo.toml`, replace:
```toml
kiln-proto = { path = "../kiln-proto" }
kiln-store = { path = "../kiln-store" }
```
with:
```toml
kiln-proto = { path = "../kiln-proto" }
kiln-registry = { path = "../kiln-registry" }
kiln-store = { path = "../kiln-store" }
```

In `crates/kiln-run/Cargo.toml`, replace:
```toml
kiln-oci = { path = "../kiln-oci" }
kiln-registry = { path = "../kiln-registry" }
tempfile = "3.27.0"
```
with:
```toml
kiln-oci = { path = "../kiln-oci" }
tempfile = "3.27.0"
```

- [ ] **Step 4: Modify `crates/kiln-run/src/prepare.rs`**

`import_from`.

In `crates/kiln-run/src/prepare.rs`, replace:
```rust
use kiln_proto::sanitize::clean_line;
use kiln_store::{Digest, Store};
```
with:
```rust
use kiln_proto::sanitize::clean_line;
use kiln_registry::Reference;
use kiln_store::{Digest, Store};
```

In `crates/kiln-run/src/prepare.rs`, replace:
```rust
        other => other,
    }
}

```
with:
```rust
        other => other,
    }
}

/// With `KILN_IMPORT_FROM` (the Lima VM's read-only mount of the macOS store, spec
/// §10): when that store has `name` (as given, or as its normalised reference),
/// imports it, copying only the blobs this store lacks, and tags it here: every
/// time, so the newest convert on macOS wins over an older tag in the VM (ruling 4).
/// Returns a note for the user when blobs were copied.
pub fn import_from(store: &Store, from: &Path, name: &str) -> Result<Option<String>> {
    let src = Store::open_read_only(from)?;
    let refs = src.refs()?;
    let normalised = Reference::parse(name).ok().map(|r| r.to_string());
    let Some(src_name) = refs
        .keys()
        .find(|k| *k == name)
        .or_else(|| refs.keys().find(|k| Some(k.as_str()) == normalised.as_deref()))
    else {
        return Ok(None);
    };
    let r = kiln_image::import_image(store, &src, src_name, src_name)?;
    Ok((r.blobs_copied > 0).then(|| {
        format!(
            "imported {} from {} ({} blobs)",
            clean_line(src_name),
            from.display(),
            r.blobs_copied
        )
    }))
}

```

- [ ] **Step 5: Modify `crates/kiln-run/src/linux/run.rs`**

Import before the lock.

In `crates/kiln-run/src/linux/run.rs`, replace:
```rust
        }
        // Held until the VMM has every blob open (spec §5.2).
```
with:
```rust
        }
        // The Lima VM's dev loop (spec §10): the macOS store's latest convert of the image.
        if let Some(from) = std::env::var_os("KILN_IMPORT_FROM").filter(|v| !v.is_empty()) {
            match prepare::import_from(store, Path::new(&from), &opts.image) {
                Ok(Some(note)) => notice(&note),
                Ok(None) => {}
                Err(e) => notice(&format!("warning: importing from KILN_IMPORT_FROM: {e}")),
            }
        }
        check()?;
        // Held until the VMM has every blob open (spec §5.2).
```

- [ ] **Step 6: Modify `crates/kiln-run/src/lib.rs`**

The macOS instructions.

In `crates/kiln-run/src/lib.rs`, replace:
```rust
virtualization (vmType: vz, nestedVirtualization: true; an M3 or later Mac with
macOS 15 or later) where Firecracker or Cloud Hypervisor and vmkit-sandbox are
installed as vmkit's README describes. Copy images into the VM's store with
`kiln import --from-store <this store> <image>`.";
```
with:
```rust
virtualization (an M3 or later Mac with macOS 15 or later): kiln's lima/kiln.yaml
sets one up with Firecracker, Cloud Hypervisor and pasta, and mounts this Mac's
store read-only (limactl start --name kiln lima/kiln.yaml; install kiln and
vmkit-sandbox in it as the template says). Then run images there with
limactl shell kiln kiln run <image>: images converted on the Mac are imported
from its store when they run (KILN_IMPORT_FROM), and ports published with -p
on 127.0.0.1 reach the Mac's localhost through Lima.";
```

- [ ] **Step 7: Modify `crates/kiln/tests/cli.rs`**

The instructions name the template.

In `crates/kiln/tests/cli.rs`, replace:
```rust
    kiln(home.path())
        .args(["run", "img"])
        .assert()
        .code(125)
        .stderr(predicate::str::contains("Linux with KVM").and(predicate::str::contains("Lima VM")));
```
with:
```rust
    kiln(home.path()).args(["run", "img"]).assert().code(125).stderr(
        predicate::str::contains("Linux with KVM")
            .and(predicate::str::contains("Lima VM"))
            .and(predicate::str::contains("lima/kiln.yaml")),
    );
```

- [ ] **Step 8: Modify `crates/kiln/tests/run.rs`**

End to end: a run in an empty store imports the image first.

In `crates/kiln/tests/run.rs`, replace:
```rust

fn net_enabled() -> bool {
```
with:
```rust

/// `KILN_IMPORT_FROM` (spec §10, the Lima template's dev loop): a run in an empty
/// store imports the image from the other store first.
#[test]
fn images_are_imported_from_kiln_import_from() {
    let Some(f) = fixture() else { return };
    let other = tempfile::tempdir().unwrap();
    let mut c = Command::new(env!("CARGO_BIN_EXE_kiln"));
    c.arg("--store").arg(other.path()).env("KILN_IMPORT_FROM", &f.store);
    c.args(["run", "--vmm", &f.env.vmm, "--kernel"])
        .arg(&f.env.kernel)
        .arg("--allow-custom-kernel")
        .arg("--init")
        .arg(&f.env.init)
        .args(["--allow-custom-init", "busybox", "--", "cat", "/marker"]);
    let o = c.finish();
    assert_eq!((code(&o), text(&o.stdout)), (0, "one".into()), "{}", text(&o.stderr));
    assert!(
        text(&o.stderr).contains("kiln: imported busybox from"),
        "{}",
        text(&o.stderr)
    );
}

fn net_enabled() -> bool {
```

- [ ] **Step 9: Create the Lima template**

The template.

`lima/kiln.yaml`:
```yaml
# kiln on macOS (spec §10): a Linux VM with nested KVM that runs `kiln run`.
# Needs an Apple M3 or later and macOS 15 or later.
#   limactl start --name kiln lima/kiln.yaml
#   limactl shell kiln kiln run <image> [args...]
# Images converted on macOS stay usable: the macOS store is mounted read-only at
# /mnt/kiln-store, and KILN_IMPORT_FROM makes `kiln run` import what it lacks
# (only missing blobs are copied). Create ~/.local/share/kiln on macOS first
# (`kiln convert` does). The VM keeps its own store and run directories:
# Unix sockets and flock are not reliable on shared mounts.
# kiln and vmkit-sandbox go to /usr/local/bin (root-owned: the AppArmor profile
# below lets that path create user namespaces); until releases exist, build them
# in the VM and install them there.
# Ports `kiln run --net -p HOST:GUEST` publishes on the VM's 127.0.0.1 (the
# default) reach the Mac's localhost: Lima forwards the VM's loopback listeners.
base: template:ubuntu-26.04
vmType: vz
nestedVirtualization: true
cpus: 4
memory: 8GiB
disk: 50GiB
mounts:
  - location: "~/.local/share/kiln"
    mountPoint: /mnt/kiln-store
    writable: false
env:
  KILN_IMPORT_FROM: /mnt/kiln-store
provision:
  - mode: system
    script: |
      #!/bin/bash
      set -eux
      export DEBIAN_FRONTEND=noninteractive
      apt-get update
      apt-get install -y curl passt nftables iproute2 uidmap
      usermod -aG kvm "{{.User}}"
      # Firecracker 1.17.0 and Cloud Hypervisor 53.0, the versions vmkit is tested with.
      arch=$(uname -m)
      case $arch in
        aarch64)
          fc_sha=e351ebe4f7a16b5873bbd51005d2e6767103cff4d5ebc829df2d3f95a93e2256
          ch_asset=cloud-hypervisor-static-aarch64
          ch_sha=f192b510eea1c710cbc439d716bb0573c223fc463dbe3e6523788a2b7ef62850 ;;
        x86_64)
          fc_sha=06094a1108ae9e82aa4c23a775aa92758f53f1175d422270d9d6162cb9ade558
          ch_asset=cloud-hypervisor-static
          ch_sha=448af3d4e59b22c2987f7df94c213ad40fb53a10d437e42b5ee6c4fce7c29ecc ;;
      esac
      work=$(mktemp -d)
      curl -sfL --retry 3 -o "$work/fc.tgz" \
        "https://github.com/firecracker-microvm/firecracker/releases/download/v1.17.0/firecracker-v1.17.0-$arch.tgz"
      echo "$fc_sha  $work/fc.tgz" | sha256sum -c -
      tar -xzf "$work/fc.tgz" -C "$work"
      install -m 0755 "$work/release-v1.17.0-$arch/firecracker-v1.17.0-$arch" /usr/local/bin/firecracker
      curl -sfL --retry 3 -o "$work/ch" \
        "https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/v53.0/$ch_asset"
      echo "$ch_sha  $work/ch" | sha256sum -c -
      install -m 0755 "$work/ch" /usr/local/bin/cloud-hypervisor
      rm -rf "$work"
      # vmkit's sandbox helper may create user namespaces (Ubuntu restricts them).
      cat > /etc/apparmor.d/vmkit-sandbox <<'PROFILE'
      abi <abi/4.0>,
      include <tunables/global>
      profile vmkit-sandbox /usr/local/bin/vmkit-sandbox flags=(unconfined) {
        userns,
      }
      PROFILE
      apparmor_parser -r /etc/apparmor.d/vmkit-sandbox
      echo 'export KILN_IMPORT_FROM=/mnt/kiln-store' > /etc/profile.d/kiln.sh
```

- [ ] **Step 10: Validate the template (macOS)**

Run:
```bash
limactl validate lima/kiln.yaml
```
Expected: `` `lima/kiln.yaml`: OK ``.

- [ ] **Step 11: Modify `README.md`**

The Lima section; the status line.

In `README.md`, replace:
```markdown
Status: milestone M3b (runtime and CLI). `kiln convert` works on registry images and local inputs, `kiln pull`/`kiln push` move kiln images through registries, and `kiln run` boots an image under Firecracker or Cloud Hypervisor on Linux with KVM. No kernel or kiln-init release is pinned yet, so `kiln run` needs `--kernel` and `--init` (below).
```
with:
```markdown
Status: milestone M3b. `kiln convert` works on registry images and local inputs, `kiln pull`/`kiln push` move kiln images through registries, and `kiln run` boots an image under Firecracker or Cloud Hypervisor on Linux with KVM (on macOS, in a Lima VM), with a persistent disk and networking. No kernel or kiln-init release is pinned yet, so `kiln run` needs `--kernel` and `--init` (below).
```

In `README.md`, replace:
```markdown
- On macOS, `kiln run` explains how to use a Lima VM.
```
with:
````markdown
- On macOS, `kiln run` explains how to use a Lima VM (below).

### On macOS: the Lima VM

`lima/kiln.yaml` is a Lima VM with nested KVM (an Apple M3 or later, macOS 15 or later) with Firecracker, Cloud Hypervisor, pasta and nftables installed, and the Mac's store (`~/.local/share/kiln`, which `kiln convert` creates) mounted read-only at `/mnt/kiln-store`. Install `kiln` and `vmkit-sandbox` in the VM's `/usr/local/bin` (until releases exist, build them there); the template's AppArmor profile lets that `vmkit-sandbox` create user namespaces.

```bash
limactl start --name kiln lima/kiln.yaml
kiln convert php:8.4-cli                                         # on the Mac
limactl shell kiln kiln run $K php:8.4-cli -- php -v             # in the VM
limactl shell kiln kiln run $K --net -p 8080:8080 app:1          # then, on the Mac: curl localhost:8080
```

- The VM keeps its own store (Unix sockets and `flock` are not reliable on shared mounts). `KILN_IMPORT_FROM=/mnt/kiln-store` is set there, so `kiln run` first imports the image from the Mac's store when that store has it, copying only missing blobs and tagging it again every time: the latest convert on the Mac is what runs.
- A port published on the VM's `127.0.0.1` (the `-p` default) reaches the Mac's `localhost`: Lima forwards the VM's loopback listeners to the Mac's `127.0.0.1` (IPv4; if something on the Mac already listens on that port over IPv6, `localhost` may reach it instead). Nothing else on the Mac's network sees it.
````

In `README.md`, replace:
```markdown
`scripts/boot-tests.sh` builds `kiln-init` and the hostile test guest for the host's musl target and runs the suite; without its environment the tests are skipped. `KILN_TEST_NET=1` adds the networking case (pasta and nft), `KILN_TEST_KEEP=1` keeps each run directory with its console log (the `kiln run` cases' through `KILN_KEEP_RUN_DIR=1`; CI uploads their logs when the job fails), and `KILN_TEST_VCPUS` sets the guests' vCPUs (default 1). Cargo arguments go after the kernel, for example `firecracker::` to run one VMM's boot cases; `KILN_TEST_VMM` (default `firecracker`) picks the VMM of the `kiln run` cases.
```
with:
```markdown
`scripts/boot-tests.sh` builds `kiln-init` and the hostile test guest for the host's musl target and runs the suite; without its environment the tests are skipped. `KILN_TEST_NET=1` adds the networking cases (pasta and nft; the `kiln run` egress case also needs the internet), `KILN_TEST_KEEP=1` keeps each run directory with its console log (the `kiln run` cases' through `KILN_KEEP_RUN_DIR=1`; CI uploads their logs when the job fails), and `KILN_TEST_VCPUS` sets the guests' vCPUs (default 1). Cargo arguments go after the kernel, for example `firecracker::` to run one VMM's boot cases; `KILN_TEST_VMM` (default `firecracker`) picks the VMM of the `kiln run` cases.
```

- [ ] **Step 12: Modify `docs/format.md`**

The boot layers and the config's `kernel` and `init`.

In `docs/format.md`, replace:
```markdown
  1. Kernel, `application/vnd.kiln.kernel.v1` (from M3; absent in schema-1 images built before M3).
  2. Init layer, `application/vnd.kiln.init.v1.erofs` (from M3; absent likewise).
```
with:
```markdown
  1. Kernel, `application/vnd.kiln.kernel.v1`: the kernel image itself, the blob kiln pins for the `kernel` the config names. Absent in provisional images, converted while kiln pins no kernel for the architecture (and in images built before M3).
  2. Init layer, `application/vnd.kiln.init.v1.erofs`: the init layer around the `kiln-init` kiln pins (see Init layer below). Absent likewise. `kiln run` never boots it: it boots the init layer of the `kiln-init` it pins itself (or of `--init`), and warns when the image's differs.
```

In `docs/format.md`, replace:
```markdown
- `kernel` (`{ "profile", "version" }`) and `init` (`{ "version" }`): set from M3, omitted before.
```
with:
```markdown
- `kernel` (`{ "profile", "version" }`) and `init` (`{ "version" }`): set when the image has that boot layer, omitted otherwise. `kiln run` verifies the kernel layer against the digest kiln pins for `kernel` and the image's architecture (a `custom` profile needs `--allow-custom-kernel`); `init` is informational, since the init layer is always replaced.
```

- [ ] **Step 13: Run the crate's tests**

Run:
```bash
cargo test -q -p kiln-run
```
Expected: all pass (41 tests on macOS, 125 on Linux).

- [ ] **Step 14: Format, lint and run the whole workspace**

Run:
```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test --all -q
```
Expected: no clippy warnings; every test passes (416 tests).

- [ ] **Step 15: Lint and test on Linux**

In the Lima VM (`limactl shell vmkit`, then `cd` to the repository and `export CARGO_TARGET_DIR=$HOME/target-m3b2`):

Run (Linux):
```bash
cargo clippy --all-targets -- -D warnings && cargo test --all -q
```
Expected: no clippy warnings; every test passes (522 tests; the boot and `kiln run` cases skip themselves without their environment).

- [ ] **Step 16: Build the guests for both architectures, as the `musl` job does**

Run (Linux):
```bash
for t in aarch64-unknown-linux-musl x86_64-unknown-linux-musl; do
  cargo clippy --release --target $t -p kiln-init -p kiln-testguest -- -D warnings
  CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld cargo build --release --target $t -p kiln-init -p kiln-testguest
  file "$CARGO_TARGET_DIR/$t/release/kiln-init" "$CARGO_TARGET_DIR/$t/release/hostile"
done
```
Expected: no clippy warnings; aarch64 `statically linked`, x86_64 `static-pie linked`.

- [ ] **Step 17: Run the boot suite and `kiln run` end to end on Firecracker**

In the Lima VM, in an `sg kvm` shell, with Firecracker and Cloud Hypervisor on `PATH`:

Run (Linux, KVM):
```bash
kernel=/Users/alfonso/Github/Personal/playground/vmkit/out/vmlinux-6.18.54-aarch64
KILN_TEST_NET=1 scripts/boot-tests.sh $kernel firecracker:: -- --test-threads=4
KILN_TEST_NET=1 KILN_TEST_VMM=firecracker scripts/boot-tests.sh $kernel -- --skip firecracker:: --skip cloud_hypervisor:: --test-threads=4
```
Expected: the boot suite 23 passed (the 24 Cloud Hypervisor cases filtered out); `kiln run` end to end 16 passed (`the_device_budget_is_checked_before_boot` refuses before boot on any VMM). About 45 s and 65 s in the Lima VM.

- [ ] **Step 18: Run them on Cloud Hypervisor**

Run (Linux, KVM):
```bash
KILN_TEST_NET=1 scripts/boot-tests.sh $kernel cloud_hypervisor:: -- --test-threads=4
KILN_TEST_NET=1 KILN_TEST_VMM=cloud-hypervisor scripts/boot-tests.sh $kernel -- --skip firecracker:: --skip cloud_hypervisor:: --test-threads=4
```
Expected: Cloud Hypervisor 53 under nested virtualization stalls guests, so it is judged by pass rate: rerun what fails serially (`scripts/boot-tests.sh $kernel <full test name> -- --exact --test-threads=1`, with `KILN_TEST_VMM=cloud-hypervisor` for the `kiln run` cases) and accept failures that are stalls (a boot timeout, a VM that does not end after its final message, a test's time limit), never a wrong result. Validation got 14/23 boot cases and 6/16 `kiln run` cases in the full run (Global Constraints, Validation).

- [ ] **Step 19: Check the Lima hop to the Mac**

In the Lima VM (KVM shell), publish a port on the default address and keep it open:

Run (Linux, KVM):
```bash
export VMKIT_SANDBOX=$CARGO_TARGET_DIR/debug/vmkit-sandbox
cargo build -q -p kiln && kiln=$CARGO_TARGET_DIR/debug/kiln
$kiln --store ~/kiln-fwd convert --platform linux/arm64 --tag busybox docker.io/library/busybox:1.37
$kiln --store ~/kiln-fwd run --kernel $kernel --allow-custom-kernel \
  --init "$CARGO_TARGET_DIR/$(uname -m)-unknown-linux-musl/release/kiln-init" --allow-custom-init \
  --net -p 28473:8080 busybox -- sh -c 'while :; do echo hello-mac | nc -l -p 8080; done'
```
Then, on the Mac (`nc` answers without HTTP headers, hence `--http0.9`; pick a port nothing on the Mac listens on, IPv6 included, or `localhost` may reach that instead):

Run:
```bash
curl -sS --http0.9 --max-time 10 localhost:28473
```
Expected: `hello-mac`: the VM's `127.0.0.1:28473` is on the Mac's `127.0.0.1:28473` (`lsof -iTCP:28473` shows `limactl`), while inside the VM `192.168.5.15:28473` (its own non-loopback address) refuses the connection.

- [ ] **Step 20: Commit**

```bash
git add -A crates lima README.md docs/format.md
git commit -m 'feat: the Lima template, KILN_IMPORT_FROM imports from the macOS store, and the docs for kiln run on macOS'
```

---

## Spec coverage (M3b-2)

| Spec item | Task |
|---|---|
| §8.1 the compiled table `(kind, profile/version, arch) → digest` (empty until the releases; ruling 7), artifacts fetched by pinned digest and verified, store first | 1 |
| §5.2 `cache/init-layers` (the init layer cached by kiln-init digest and format version, §6.5) and GC roots for pinned artifacts (ruling 3, finding 4.2) | 1 |
| §6.5 convert adds the kernel and init layers it has pins for (rev 2.10: each independently), config `kernel`/`init`; the provisional-image note and `inspect`'s boot line | 2 |
| §8.1 at run: an image's kernel layer must equal the pinned digest for its config's `(profile, version, arch)`; `custom` needs `--allow-custom-kernel`; the init layer is always the pinned kiln-init's (or `--init`'s), with a warning when the image's differs | 2 |
| §11.5 kernel and init swap: a kernel layer that differs from the pin is refused, a mismatched init layer replaced | 2 |
| §9.4 `--persist PATH`: sidecar (manifest, ordered layers, size), refusal of other layers unless `--persist-reset`/`--persist-force` (exclusive), ruling 2 for the same layers, never truncated, a smaller `--disk` refused; the disk locked for the run and outside the scratch sweep | 3 |
| §11.4 the `--persist` mismatch refusal and the `--disk` shrink refusal | 3 |
| §9.3 `--net`: guest addressing in `Config.network` (vmkit's `172.30.0.2/30`), `--egress restricted|deny-all|open|allow=CIDR`, `-p [ADDRESS:]HOST:GUEST[/proto]` with `127.0.0.1` by default (rev 2.10); the `allow=` refusal for the host's outbound address (finding 4.1); the NIC in the device budget (§9.1) | 4 (+ vmkit) |
| §9.6 stage 5 and `docs/format.md`: `nameserver 127.0.0.1` without a network (rev 2.10) | 4 |
| §11.5 hostile guest on the network: the host's address unreachable under `restricted`, `deny-all` leaving only DNS, `allow=` exceptions | 4 (+ vmkit's network suite) |
| §10 `lima/kiln.yaml`, the read-only macOS store, `KILN_IMPORT_FROM` (ruling 4); published ports on the Mac's `localhost` | 5 |
| §9.7 on macOS: instructions naming the Lima template | 5 |
| §5.1 and `docs/format.md`: the boot layers and the config's `kernel`/`init` as `kiln run` uses them (finding 4.3) | 5 |

**Left to M3c:** the kiln-init release that fills its pins (and the release pipeline), `kiln build`, the full end-to-end matrix (§11.4), the docs (§15). The kernel entries of the table wait only for vmkit's `kernels-6.18.54` release (one commit in `crates/kiln-image/src/artifacts.rs`).
**Left to vmkit (ruling 10):** `Vm::identity()` and a waitable end of the VMM.
