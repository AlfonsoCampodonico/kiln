# kiln — Design Spec

**Date:** 2026-09-30 (rev 2.10: 2026-10-07)
**Status:** Draft rev 2.10, awaiting review
**Scope:** `kiln` (OCI image / Dockerfile → bootable microVM image builder) and the v1 scope of `vmkit` (shared VMM crate)
**Related:**
- `../../../../ROADMAP.md`: the three-repo roadmap (`vmkit`, `kiln`, `potter`).
- `2026-10-05-kiln-shim-design.md`: `kiln-shim` (codename slip), the containerd shim (§12, M4).
- The potter spec, `potter/docs/superpowers/specs/2026-10-04-potter-design.md` (rev 1.3): potter's needs from kiln are its §4, §6.1.1 and §9.3.

### Revision history

- **rev 1 (2026-09-30):** initial design.
- **rev 2 (2026-10-01):** incorporates the adversarial review of rev 1 (five reviewers, ~60 raw findings, 24 after dedup; none refuted). Decisions taken with the user:
  - **A.** A vsock control channel between `kiln-init` and the host is part of v1. It replaces the config disk and the status sector.
  - **B.** Implicit parent directories inherit attributes from lower layers, as containerd does.
  - **C.** Hardening is in v1: unprivileged sandboxed VMMs, default-deny egress, verified kernels and init.
  - **D.** The work is split into three milestones, each with its own implementation plan (§12).
- **rev 2.1 (2026-10-01):** §6.3, §7.3 and §7.4 corrected to match containerd's overlay snapshotter, as found by the oracle while validating the M1a plan:
  - symlink modes are always 0777;
  - a hardlink header's metadata applies to its inode;
  - a whiteout naming an entry already in its layer is an error;
  - implicit directories described later keep their inherited xattrs.

  §7.4 also records three deliberate divergences, and §12 splits M1 into M1a and M1b.
- **rev 2.2 (2026-10-02):** §5.2 and §6.2 layer cache keys include the verified `diff_id`, found by the M1b-1 task review: a key on the compressed digest alone let a warm store skip the `diff_id` check. §7.6 states that cached layers do not count toward the per-image limit.
- **rev 2.3 (2026-10-02):** §4.1 aligned with vmkit M2a: version checks at discovery, Cloud Hypervisor serial on stdout, Landlock rules in vm.create.
- **rev 2.4 (2026-10-02):** §4.1, §9.2 and §9.3 aligned with the validated M2b plan:
  - the sandbox is a helper binary with an AppArmor profile;
  - the VMM's directory is `<run_dir>/sock`;
  - cgroups come from a systemd user scope;
  - DNS and port forwards are DNAT'd, and pasta handles loopback resolver stubs;
  - `deny-all` honours `allow`;
  - pasta's version floor is documented rather than checked.

- **rev 2.5 (2026-10-03):** §4.2 aligned with the validated M1b-2 plan:
  - T2's exception applies only to registries without a public address, and only to their own address classes;
  - tags are resolved HEAD-first, so a warm convert costs no Docker Hub pull;
  - `pull` accepts at most 8 platform manifests;
  - token realms and upload locations get the same destination check as redirects;
  - credentials go only to the registry's own origin and its token realm;
  - proxies are not used;
  - plain HTTP is allowed only for loopback registries;
  - `pull` fetches every platform of a kiln image;
  - registry inputs are tagged and looked up by their normalised reference.
- **rev 2.6 (2026-10-03):** §9.4, §9.5, §9.6 and §14 aligned with the validated M3a plan:
  - vsock endpoints follow vmkit's `<vsock socket>_<port>`;
  - the supervision order kills and reaps the other processes before draining stdio;
  - stop signals are limited to 1–31;
  - the ext4 template lives in the crate that embeds it;
  - Cloud Hypervisor 53 cannot grow the scratch disk online beyond about 8 GiB.
- **rev 2.7 (2026-10-04):** `kiln build` no longer requires BuildKit (user decision):
  - pluggable backends: BuildKit, Podman/Buildah, and a native backend for Dockerfiles without `RUN`;
  - `RUN` executed in a kiln microVM is planned post-v1 (§6, non-goals).
- **rev 2.8 (2026-10-05):** project consolidation driven by potter (user decisions):
  - rev 2.7's native RUN-free Dockerfile backend is cancelled. `kiln build --builder auto|potter|buildkit|podman|buildah` dispatches to an external builder, and `auto` tries potter first. kiln runs the `potter` binary; potter is the only Dockerfile implementation kiln ships with, `RUN` in a microVM included (§1, §2, §6).
  - Three repositories only: `vmkit`, `kiln` and `potter`, depending `potter` → `kiln` → `vmkit`. The agent sandbox (sagger), snapshot distribution (sherd) and the containerd shim (slip, `kiln-shim`) live in kiln; lazy restore (raku) lives in vmkit (§1, §4).
  - kiln-proto is extended, never replaced. The planned extensions (services, build mode, exec, guest stats, OOM reports, file updates) and the versioning rule for them are in §9.8.
  - Milestones (§12): M3 is split into M3a (done), M3b and M3c, with new M3d (`services`), M3e (build mode) and M4 (`kiln-shim`). sagger and sherd follow, each with its own spec.
  - The kiln-shim draft spec moved here from the potter repo.
- **rev 2.9 (2026-10-06):** §5.2, §5.3, §8.1, §9.5 and §9.7 aligned with the M3b-1 plan (`kiln run`, runtime and CLI; validated prototype `m3b-proto` and the controller's rulings). M3b is planned in two parts: M3b-1, the runtime and CLI, and M3b-2, pinned artifacts, `--persist`, `--net` and the Lima template (§12).
  - `--init <path>` boots a static kiln-init of the user's, only with `--allow-custom-init`, as `--kernel` needs `--allow-custom-kernel`; both are warned about and recorded in `run.json`. Until kiln pins released kernels and kiln-init, every run names both; an image's kernel layer is used only with `profile: custom` and `--allow-custom-kernel` (§8.1).
  - A run the user kills (a second SIGINT, `Ctrl-]` `k`, or a stop request before the guest's `Hello`) exits 137, as `docker kill`, unless the guest's final message came first: a kill after `Exited` keeps the app's code, one after `InitFailed` keeps init's 127, 126 or 125. Kills `kiln` decides (protocol violations, the boot timeout, the stop and end bounds below) stay 125 (§9.5).
  - After `Exited`, `InitFailed` or the control connection closing, the guest has 10 s to end the VM, then it is killed; after `Exited` the app's exit code is kept, with a warning. After `Shutdown`, `kiln` kills the VM itself once the grace period plus 5 s has passed (125, with a warning); the guest's final message ends that bound, and only the 10 s one applies from then. A stop request before `Hello` kills at once. A killed VMM has 5 s to exit; then `kiln` stops waiting for it, with a warning (§9.7).
  - After the VMM exits, stdout and stderr (or the terminal) are drained to EOF however long `kiln`'s own stdout takes them (a pager, a stopped terminal), as a pipeline would; only reading is bounded (2 s without a byte while nothing is being written). After `kiln` gave up on a VMM, or when the user asks to stop (SIGINT, SIGTERM, `Ctrl-]`) after the VM's end, the output gets a fixed 2 s, so a sink that never takes it cannot hold `kiln`. A stream cut off with bytes pending is warned about. Output whose connection the VMM made just before it exited is still accepted (§9.7).
  - When the VM ends without `Exited` or `InitFailed`, `kiln` names the likely causes (the VMM failed, or ran out of memory) and the VMM's log, or that `KILN_KEEP_RUN_DIR=1` keeps it (§9.7).
  - SIGHUP, SIGQUIT, SIGUSR1 and SIGUSR2 are forwarded to the main process as `Signal`, as Docker's sig-proxy does. A second SIGTERM re-sends `Shutdown`, whose deadline can only shrink; only a second SIGINT kills. SIGWINCH is forwarded with `-t` only (§9.7).
  - Signal handling starts before setup. SIGINT, SIGTERM or SIGHUP during setup stops it, removes what it made and exits 137, as a user's kill before `Hello` does; SIGQUIT, SIGUSR1 and SIGUSR2 then are dropped with a note, never delivered once the app runs (§9.7).
  - With `-t -i`, terminal input to a guest that stops reading it is dropped after 5 s, so `Ctrl-]` keys always act (§9.7).
  - `-t` without `-i` gives the guest a terminal without raw mode or stdin relay; `-t -i` needs a terminal on stdin. Both as Docker (§9.7).
  - Errors before the guest runs exit 125, and so do usage errors of `kiln run`, as `docker run`'s do. As with `docker run`, everything after IMAGE is the command, flags included; a `--` right after IMAGE is optional. A `Config` over the protocol's frame is refused before boot. Defaults: `--vmm firecracker`, `--cpus 1`, `--memory 512`; the guest's hostname is the run id's first 12 hex digits; `-e`/`--env KEY[=VALUE]` as Docker's (§9.7).
  - `console.log` drops its oldest half when it would pass 1 MiB; the VMM writes the console to a FIFO that `kiln` copies (§5.3).
  - Stale run directories from other boots or hosts are left alone and reported in a note; directories kept for debugging (`KILN_KEEP_RUN_DIR=1`) are never cleaned (§5.3).
  - The scratch disk moves out of the run directory, which is usually a tmpfs (`$XDG_RUNTIME_DIR`): its pages would hold the guest's disk in host memory, charged to the VMM's cgroup (`--memory` plus 256 MiB), so a guest writing a few hundred MB had its VMM killed. It lives in a per-run directory on the store's filesystem, `$KILN_HOME/scratch/<run-id>/`, recorded in `run.json` and removed with the run or by the stale cleanup; it carries its own identity (`scratch.json`), so it is cleaned up even after a reboot or logout emptied the run directories; GC never touches it (§5.2, §5.3).
- **rev 2.10 (2026-10-07):** §5.1, §5.2, §6.5, §8.1, §9.3, §9.4, §9.5, §9.6, §9.7, §10 and §12 aligned with the M3b-2 plan (`docs/superpowers/plans/2026-10-07-m3b2-artifacts-persist-net.md`: pinned artifacts, `--persist`, `--net` and the Lima VM), the controller's rulings 2, 3, 4, 7 and 10, and a field report from a real user of `kiln run`.
  - `-p [ADDRESS:]HOST:GUEST[/tcp|udp]`: the host address defaults to `127.0.0.1` (Docker's is `0.0.0.0`), so an untrusted image's ports stay on the host unless `-p 0.0.0.0:…` (or another of the host's addresses) says otherwise; vmkit's `PortForward` gains the address. A host port is published once per protocol whatever its addresses. The image owns its listen port: the app must listen on `0.0.0.0` or the guest's address, and a forward to a guest listening on `127.0.0.1` ends at once (§9.3).
  - Without a network, or with one that names no DNS server, `/etc/resolv.conf` adds `nameserver 127.0.0.1` after its comment, as Docker's `--network none` lists a resolver: lookups fail at once instead of tools refusing to start (§9.6, `docs/format.md`).
  - pasta copies the host's outbound address (the one on its default route) into the VM's namespace, so the guest can never reach it: an `--egress allow=` entry that is only that address is refused before boot, by `kiln` and by vmkit, and a wider one that includes it is warned about. The host's other addresses can be reached through `allow=` (finding 4.1, §9.3).
  - `--persist` reuses a disk whose sidecar lists the same app layers in the same order even when the manifest differs (another env or cmd); the sidecar records the new manifest and a note is printed (ruling 2). `--persist-reset` and `--persist-force` exclude each other; a disk without a readable sidecar needs one of them; the disk is locked for the run; it replaces the run's scratch directory and no cleanup touches it; the guest gets 60 s instead of 10 s to end the VM after its final message (§9.4, §9.7).
  - `kiln gc` keeps every blob the compiled pin table names, and the init layers built from pinned kiln-init binaries with their `cache/init-layers` entries, so offline runs survive it (ruling 3, finding 4.2, §5.2).
  - A pin names the artifact's blob (the kernel image, the static kiln-init), its repository and size: fetching is one digest-verified blob GET, from the store when it is there. The table ships empty until vmkit's kernel and kiln's kiln-init releases exist (ruling 7). `convert` adds each boot layer whose artifact is pinned for the architecture, independently (a kernel can be pinned before any kiln-init release); without either it makes a provisional image and says what `kiln run` needs, and `inspect` prints a `boot:` line. `config.init` is informational, since `kiln run` always replaces the init layer (finding 4.3) (§5.1, §6.5, §8.1).
  - The device budget's remedy is `kiln convert --max-layers N` (finding 4.4, §9.7); the config's grace field is `shutdownGraceSecs` (finding 4.5, §9.5).
  - §10: `KILN_IMPORT_FROM` imports (missing blobs only) and retags on every run when the source store has the name, so the newest convert on macOS wins (ruling 4); ports published on the Lima VM's `127.0.0.1` reach the Mac's `localhost` through Lima's own forwarding.
---

## 1. Goals

`kiln` turns an OCI image, or a Dockerfile via an external builder (potter, BuildKit, Podman or Buildah), into a bootable microVM image.
- Images run on Firecracker and Cloud Hypervisor, on aarch64 and x86_64.
- Caching is per layer, fast enough that an unchanged rebuild is near-instant.

It is meant to be a serious open-source tool: a stable format, real docs and a polished CLI. It is built to exercise the internals (a native erofs writer and reader, a custom init, a control protocol) rather than wrap existing tools. It treats images and guests as untrusted (§3), because it is also the base of the rest of the project family:
- **potter** (its own repo) builds images from source and runs build steps in kiln microVMs.
- **`kiln-shim`** (codename slip, in this repo) runs kiln images as a Kubernetes RuntimeClass.
- **The agent sandbox** (codename sagger) and **snapshot distribution** (codename sherd) are planned in this repo, with lazy restore (codename raku) in `vmkit`.

Codenames name internal pieces only. The products are `kiln`, `vmkit` and `potter`.

### Success criteria

1. `kiln convert php:8.4-cli` produces an image that boots and runs `php -v` on both VMMs and both architectures.
2. `kiln build -f Dockerfile .` produces the same kind of image through any available backend: potter, BuildKit, Podman or Buildah (§6).
3. The filesystem a guest sees matches what containerd produces for the same image, as checked by the oracle test (§11.2), across the fixture set.
4. Performance targets, measured by `kiln bench` (§11.6). All are measured with `kiln convert` from a local OCI layout:
   - Warm, unchanged image: **< 200 ms**.
   - One changed top layer (~50 MB uncompressed): **< 2 s**.
   - Cold `php:8.4-cli`: **< 5 s**.
5. Builds are deterministic: for the same `kiln` version, identical inputs produce byte-identical blobs and manifests on any host.
6. Where each command runs:
   - `convert`, `pull`, `push`, `inspect`, `ls`, `gc`, `import` run natively on macOS: no root, no Linux VM.
   - `build` needs one of potter, BuildKit, Podman or Buildah, and runs wherever that backend does (§6). kiln has no Dockerfile engine of its own.
   - `run` needs Linux, KVM and membership in the `kvm` group. It **never needs root**.
7. Untrusted images cannot poison the cache, reach host-internal network endpoints from the build host, or swap the kernel or init. Untrusted guests cannot reach link-local, private or host addresses by default, or other VMs. A VMM compromise lands in an unprivileged sandbox (§3).
8. `docs/format.md` documents the image format, the erofs profile and the control protocol well enough for a third party to produce or consume `kiln` images.

### Non-goals (v1)

- A Dockerfile engine in kiln. `kiln build` delegates every Dockerfile to a backend (§6). potter is the project family's only Dockerfile implementation, including `RUN` in a kiln microVM.
- Per-workload kernel tuning or local kernel builds.
- erofs compression (LZ4 is the planned follow-up).
- Auto-delegating `kiln run` to a Lima VM on macOS.
- Snapshot/restore *implementation* (its trait shape is fixed here), forking, and lazy restore. Snapshot, restore and fork belong to sagger, and lazy restore to raku, in later milestones with their own specs (§12).
- DNS-name-based egress policy (CIDR policy only in v1).
- IPv6 inside guests.
- Windows hosts, GPUs, non-Linux guests.

---

## 2. Decisions (with rationale)

| Decision | Choice | Why |
|---|---|---|
| Language | Rust, all components | Matches Firecracker, Cloud Hypervisor and rust-vmm. No GC suits future UFFD work. One toolchain across all three projects. |
| VMMs | Firecracker **and** Cloud Hypervisor from day one, behind `vmkit` | Real portability. The abstraction is designed against two concrete backends rather than retrofitted. |
| Architectures | aarch64 and x86_64 from day one | aarch64 is native on the dev machine; x86_64 is the common production target. |
| Input | OCI images are the core input; Dockerfiles go through an external backend: potter first, then BuildKit, then Podman or Buildah (§6) | Effort goes into conversion, not into a BuildKit clone. potter owns the project family's only Dockerfile implementation, so kiln keeps none. |
| Rootfs format | One read-only erofs image per OCI layer, overlayfs in the guest, writable ext4 scratch | True per-layer caching, cross-image dedupe, and a compact format that mounts quickly. |
| erofs | Native Rust writer **and reader**, streaming tar to erofs without extraction | Runs on macOS, where APFS can't faithfully hold Linux ownership, device nodes or case-distinct names. The reader is needed for squash (§6.4) and parent inheritance (§6.3). |
| Implicit parents | Inherit attributes from the merged lower layers (decision B) | Matches containerd. Otherwise overlayfs shows invented directory permissions (e.g. `/tmp` losing 1777). |
| Registry client | Own minimal client, `kiln-registry` | `oci-client` verifies digests only at end of stream, follows descriptor `urls` unauthenticated (an SSRF vector), and has no Docker credential-helper support. Owning the client gives full control over verification, redirects and auth. |
| Guest↔host control | virtio-vsock control protocol (decision A) | One mechanism covers config delivery, readiness, graceful shutdown, signals, stdio with EOF, TTY mode and exit status, on both VMMs. potter, `kiln-shim` and sagger extend the same channel (§9.8). |
| VMM privileges | VMMs run **unprivileged**, inside a per-VM sandbox built by `vmkit` from user, mount, PID and net namespaces plus seccomp or Landlock (decision C) | Firecracker requires the jailer "or equal or more restrictive" constraints. The jailer itself needs root. An unprivileged sandbox meets the bar without any root component. |
| Networking | A tap device inside the VM's own net namespace, a default-deny nftables policy in that namespace, and `pasta` for unprivileged egress | No host-global `ip_forward` and no host firewall edits. Every VM is isolated in its own namespace. The guest cannot alter the policy. It works on hosts running Docker or systemd-resolved. |
| Kernel | Prebuilt, versioned profiles, published from the **`vmkit` repo** and fetched by pinned digest | Kernel builds take minutes. The profiles are VMM-facing and shared by everything that boots through `vmkit`. |
| `kiln-init` delivery | Released as a versioned artifact fetched by pinned digest (same mechanism as kernels) | Avoids `include_bytes!` breaking `cargo install`. Run-time verification compares against the pin. |
| Running | `kiln run` on top of `vmkit` | Images must boot to be testable. VMM drivers are written once. |
| License | Apache-2.0 | Matches the ecosystem. |

---

## 3. Threat model

### Assets

The host, other VMs on the host, credentials reachable from the host (cloud instance metadata, Docker credentials, registry tokens), the `kiln` store's integrity, and the user's terminal.

### Adversaries

- **Hostile registry or image content:** manifests, configs, layer tars, annotations, descriptor URLs, and pulled `kiln` images including their kernel and init layers.
- **Hostile guest:** arbitrary code running as root inside the VM. It controls everything it sends over vsock, the network, the serial console and its writable disks.
- **Local users on the host other than the invoking user.**

### Trusted

The `kiln` and `vmkit` binaries; the pinned digest tables compiled into `kiln`; the invoking user; the VMM binaries `vmkit` selects (§4.1).

### Out of scope

- Attackers already running as the invoking user. They can ptrace `kiln` anyway.
- Hardware side channels.
- Host DoS by CPU, when cgroup delegation is unavailable (§9.2 degrades with a warning).
- Integrity of exit codes, status, stats and OOM reports from the guest. These are guest-asserted, and `kiln`, potter and `kiln-shim` must treat them as such: they never drive a security or quota decision.

### Required properties (each tested in §11.5)

| # | Property | Where enforced |
|---|---|---|
| T1 | A blob enters the store only after its full content hashes to the expected digest. Layer conversions are cached only for verified sources. Annotations never populate caches. | §6.1, §6.2 |
| T2 | Image content cannot direct host network requests outside the registry: descriptor `urls` are ignored, and redirects to private or link-local destinations are refused. | §4.2 `kiln-registry` |
| T3 | Untrusted input cannot exhaust host disk, memory or CPU without bound during conversion. | §7.6 |
| T4 | The kernel and init that boot are the pinned ones unless the user explicitly opts out. Images cannot influence the kernel cmdline. | §8.1, §8.3 |
| T5 | Default guest egress cannot reach link-local (including cloud metadata), private, loopback-mapped or host addresses, or other VMs. Guests cannot spoof addresses. | §9.3 |
| T6 | VMMs run unprivileged in a sandbox that exposes only the files and devices that VM needs. No `kiln` component runs as root. | §9.2 |
| T7 | Run state lives in a 0700 per-user runtime directory and is never trusted across boots or hosts. | §5.3 |
| T8 | Every string that comes from the guest or the image and is printed by `kiln` (diagnostics, `inspect`, `ls`, console excerpts) is sanitised. App stdio is passed through unmodified, like `docker run`, and this is documented. | §13 |
| T9 | Guest control messages are size-bounded and schema-validated. Protocol violations end the VM. | §9.5 |

---

## 4. Repositories and components

The project family has three repositories, and only three: `vmkit`, `kiln` and `potter`. Dependencies point one way, `potter` → `kiln` → `vmkit`; nothing depends upward. This spec covers `kiln` and `vmkit`'s v1 scope. Beyond crate dependencies, coupling is through documented formats: the image format and kiln-proto (`docs/format.md`).

| Repo | Owns |
|---|---|
| `vmkit` | VMMs behind the `Vmm` trait, the sandbox, networking, kernel profiles (§4.1); later, lazy restore (raku) |
| `kiln` | The image format, conversion, the store and registry client, `kiln-init` and kiln-proto, `kiln run`, `kiln build` dispatch (§4.2); `kiln-shim` (slip, M4); later, the agent sandbox's snapshot, restore and fork (sagger) and snapshot distribution in OCI registries (sherd) |
| `potter` | Source → kiln image: detectors, the plan IR, the runtime catalog, the build executors, the Dockerfile frontend, the remote cache. It depends on kiln crates and `vmkit` pinned by git rev |

### 4.1 `vmkit` (separate repo)

VMM-neutral VM lifecycle, sandboxing, networking and kernel profiles.

- **`Vmm` trait.**
  - `create(VmSpec) -> Vm` and `restore(SnapshotBundle, RestoreSpec) -> Vm`. Restore is a constructor because both VMMs require a fresh, unconfigured process for it.
  - On `Vm`: `start`, `pause`, `resume`, `kill`, `wait -> VmEnd`, `snapshot(dest)`, `capabilities()`.
  - Graceful shutdown is **not** a VMM operation. It goes through the guest control channel (§9.5).
  - `snapshot` and `restore` are part of the trait so its shape is fixed now. Their implementation and contract tests ship with sagger.
- **`VmSpec`.**
  - Kernel path and cmdline. The cmdline is assembled only by `kiln`/`vmkit` (§8.3).
  - Ordered block devices (path, read-only flag), vCPUs, memory MiB.
  - Optional network (§9.3), vsock (always present for `kiln`), and a console log path.
  - Sandbox options (§9.2).
- **Drivers: `firecracker` and `cloud-hypervisor`.** Each spawns the VMM inside the sandbox and drives its REST API over a Unix socket. Backend-specific details the drivers hide:
  - **Console:** `ttyS0` on Firecracker on both arches and on Cloud Hypervisor x86_64 (guest serial in `tty` mode on the VMM's stdout, which `vmkit` appends to the console log because a `file=` serial is truncated when the guest resets; `--console off`); `ttyAMA0` on Cloud Hypervisor aarch64.
  - **Exit method** passed to the guest: `reboot` on Firecracker (x86_64 exits only on reboot with `reboot=k`; aarch64 exits on both), `poweroff` on Cloud Hypervisor (guest power-off exits the VMM; guest reset rebuilds the VM).
  - **Backstop:** on Cloud Hypervisor, `vmkit` subscribes to `--event-monitor` and kills the VMM on any reboot event. Combined with the one-shot config rule (§9.5), a guest reset can never run the workload twice.
  - **Firecracker defaults:** a custom `boot_args` replaces Firecracker's default cmdline, so the driver re-adds every Firecracker parameter it needs, e.g. `reboot=k` and the i8042 options on x86_64.
- **`Capabilities`.**
  - `max_virtio_devices`: Firecracker x86_64 17, Firecracker aarch64 92, Cloud Hypervisor 31 per PCI segment.
  - `supports_diff_snapshot`, `supports_balloon`, `supports_drive_remap`.
  - Callers budget devices against `max_virtio_devices` and never match on backend type.
- **`net` module:** the per-VM net namespace, tap, nftables policy and `pasta` attachment (§9.3).
- **`sandbox` module:** user, mount, PID and net namespaces; the fixed in-sandbox file layout; cgroups; rlimits; seccomp and Landlock flags (§9.2).
- **Binary discovery:** `$VMKIT_FIRECRACKER`, `$VMKIT_CLOUD_HYPERVISOR` and `$VMKIT_PASTA`, else `PATH`; the sandbox helper from `$VMKIT_SANDBOX`, else next to the running program, else `PATH`. VMM minimum versions are pinned and checked when a backend is discovered. pasta's floor (2024-02-20) is documented, not checked: distribution builds print no usable version. No component runs with elevated privileges, so environment-selected binaries confer nothing beyond what the user already has.
- **`kernels/`:** per-arch config fragments, a pinned LTS version, and the CI build, boot-test and publish pipeline (§8.2).
- **Planned growth** (vmkit's own milestones; listed here because kiln's consumers need them):
  - a generalised `sandbox` that runs any process, with nested user namespaces, and `net` without a tap for a bare net namespace (potter spec §4, for potter P2);
  - `Capabilities.supports_memory_hotplug` (virtio-mem on Cloud Hypervisor) and vCPU hotplug (shim spec §4.3, for M4's S4);
  - lazy restore below the VMM (raku), with its own spec.

### 4.2 `kiln` (this repo, Cargo workspace)

| Crate | Responsibility | Depends on |
|---|---|---|
| `kiln-store` | Content-addressed blob store, layer caches, refs index, store lock, GC, artifact fetch by pinned digest. | — |
| `kiln-registry` | Minimal OCI distribution client: bearer and basic token auth, `~/.docker/config.json` including `credsStore`/`credHelpers` (exec `docker-credential-*`), HEAD/GET of manifests and indexes, verified blob GET, monolithic blob PUT, manifest PUT. Enforces T2:<ul><li>Descriptor `urls` are never fetched.</li><li>Redirects are HTTPS-only (plain HTTP only from a loopback registry to loopback) and at most 5.</li><li>Redirects never go to loopback, link-local, private, CGNAT, multicast or reserved destinations, unless the configured registry host resolves only to non-public addresses and the destination is in one of their classes (so a loopback registry may redirect to loopback, never to link-local, and a registry with any public address may hand out only public destinations). The check runs after DNS resolution, and connections are pinned to the checked addresses.</li><li>Token realms and upload `Location`s get the same check.</li><li>Credentials are sent only to the registry's own origin and to the token realm its challenge names (the token protocol requires it), never to a redirect target such as a CDN.</li><li>Proxies are not used, since a proxy would resolve names outside the check.</li><li>Plain HTTP is used only for loopback registries.</li></ul>Query strings, userinfo and tokens are redacted from errors. | `reqwest`, `kiln-store` |
| `kiln-oci` | Resolve references and platforms; read OCI image layouts and `docker save` archives, hashing every file (filenames and `manifest.json` are never trusted). | `kiln-registry`, `kiln-store` |
| `kiln-erofs` | erofs **writer** (tar stream → erofs) and **reader** (lookup, readdir, attributes, file data). Implements the erofs profile in §7 and the layer semantics in §7.4. Pure `Read`/`Write + Seek`. | — |
| `kiln-init` | Static musl PID 1 for the guest, including the guest side of the control protocol. Gains `services` (M3d), build mode (M3e), and exec, stats, OOM reports and file updates (M4) (§9.8). | — (guest only) |
| `kiln-proto` | Control-protocol message types and framing, shared by `kiln-init` and every host side: `kiln run`, potter and `kiln-shim` (§9.5, §9.8). | `serde` |
| `kiln-image` | `kiln` image manifest and config types; assembly; run-time verification of kernel and init (§8.1). | `kiln-oci`, `kiln-erofs`, `kiln-store` |
| `kiln` (bin) | CLI: `convert`, `build` (backend dispatch, §6), `pull`, `push`, `import`, `run`, `inspect`, `ls`, `gc`, `bench`. | all of the above, `vmkit` |
| `kiln-shim` (bin, M4) | containerd shim v2 `io.containerd.kiln.v2`, one microVM per pod, behind the RuntimeClass `kiln`. Specified in `2026-10-05-kiln-shim-design.md`. | `kiln-proto`, `kiln-image`, `vmkit` (exact set designed in M4) |

potter depends on `kiln-oci`, `kiln-erofs`, `kiln-image`, `kiln-store`, `kiln-registry` and `kiln-proto` (potter spec §4). kiln never depends on potter's crates: that would be a cycle, so `kiln build` runs the `potter` binary instead (§6).

Other repo contents:
- `assets/make-ext4-template.sh`, the reproducible generation script for `crates/kiln-image/assets/ext4-template.img.zst` (§9.4).
- `lima/kiln.yaml` (§10).
- `tools/oracle/`: a Go test helper wrapping containerd's `archive.Apply` (§11.2).
- `xtask/`.
- `docs/format.md`, `docs/architecture.md`, `docs/security.md`.

---

## 5. Image format and local store

### 5.1 Image format

A `kiln` image is an OCI 1.1 artifact.
- **Multi-arch:** an OCI image index with one entry per platform, each pointing to a `kiln` manifest.
- **Manifest:** an OCI image manifest with `artifactType: application/vnd.kiln.image.v1`.
- **Config blob:** `application/vnd.kiln.image.config.v1+json`, containing:
  - `schemaVersion` (starts at 1)
  - `architecture` (`arm64` | `amd64`)
  - `process`: `entrypoint`, `cmd`, `env`, `workingDir`, `user`, `stopSignal` (all from the source OCI config)
  - `kernel`: `{ profile, version }`, present with the kernel layer (`kiln run` verifies that layer against the pin for it, §8.1)
  - `init`: `{ version }`, present with the init layer; informational, since `kiln run` always replaces that layer
  - `source`: `{ manifestDigest, reference? }`. `reference` is present only for registry inputs, normalised (e.g. `docker.io/library/php:8.4-cli`). Local inputs omit it, keeping manifests deterministic.
  - `erofsFormatVersion`
  - There is **no** kernel cmdline field (T4). There is **no** `kiln` version field, so patch releases that produce identical output produce identical digests.
- **Layers, in boot order:**
  1. Kernel: `application/vnd.kiln.kernel.v1`.
  2. Init layer: `application/vnd.kiln.init.v1.erofs`.
  3. App layers, lowest first: `application/vnd.kiln.layer.v1.erofs`.
- **Annotations:**
  - `dev.kiln.source.digests` on each app layer (the OCI layer digests it was built from).
  - `dev.kiln.inherits` on layers converted with inherited parents (§6.3).
  - Annotations are **informational only**: never used for caching or verification (T1).

`docs/format.md` is the normative version of §5.1, §7 and §9.5. Breaking changes bump `schemaVersion`; readers reject unknown major versions.

**containerd alignment (M4, shim spec §3.1):** containerd 2.3's erofs snapshotter and differ must accept kiln's app layers as native EROFS layers. M4 either aligns the app-layer media type with containerd's or registers kiln's as an alias, and tests that the §7 profile (4096-byte blocks, no compression, xattrs) mounts through the snapshotter. Which of the two is designed in M4. Changing kiln's own media type is a breaking change under the rule above.

### 5.2 Store (`$KILN_HOME`, default `~/.local/share/kiln`)

```
$KILN_HOME/
  lock                                        # flock: shared for convert/build/pull/import/run-setup, exclusive for gc
  refs.json                                   # tag → digest index (single file, atomic replace; avoids case-insensitive FS collisions)
  blobs/sha256/<hex>
  cache/layers/<src-digest>@<diff-id-hex>@<fmt>            # "erofs <digest>"  or  "parents <json list of implicit paths>"
  cache/layers-ctx/<src-digest>@<diff-id-hex>@<fmt>@<ctx>  # erofs digest for a layer with inherited parents (§6.3)
  cache/squash/<sha256(ordered erofs digests)>@<fmt>
  cache/warnings/<layer cache key>             # JSON list of the layer's conversion warnings, removed with its cache/layers entry
  cache/init-layers/<kiln-init digest>@<fmt>   # the init layer built around that kiln-init binary (§6.5)
  tmp/                                        # staging, same filesystem as blobs/
  scratch/<run-id>/                           # a running VM's scratch disk (§5.3), 0700; never touched by GC
```

- **Atomic writes:** write to `tmp/`, `fsync`, then `rename`. A cache entry is written only after its blob is committed and verified.
- **Cache hits** `stat` the target blob. A missing blob counts as a miss.
- **GC** takes the exclusive lock, marks from `refs.json` and from the pinned artifacts (every blob the compiled table of §8.1 names, and the init layers built from its kiln-init binaries), deletes dangling or unreferenced cache entries **before** blobs, then deletes blobs. The pinned roots keep runs that need no network working after GC.
- **`kiln run`** holds the shared lock only during setup. Once the VMM sandbox holds open file descriptors and bind mounts, a GC that deletes a blob cannot affect the running VM on Linux.

### 5.3 Run state

- Each run's state lives in `$XDG_RUNTIME_DIR/kiln/<run-id>/`, mode 0700. If `XDG_RUNTIME_DIR` is unset, the fallback is `/tmp/kiln-<uid>/`, which is created 0700 and owner-verified with `O_NOFOLLOW`. That directory is always on a local filesystem.
- Its contents:
  - `run.json`: VMM pidfd-derived identity (pid plus start time), `boot_id`, hostname, VMM kind, and the run's scratch directory.
  - The VMM API socket and the vsock UDS endpoints.
  - `console.log`: a 1 MiB ring.
- The scratch disk, unless persisted, is not in that directory, which is usually a tmpfs whose pages would hold the guest's disk in host memory (and count against the VMM's memory limit). It lives in the run's scratch directory, `$KILN_HOME/scratch/<run-id>/` on the store's filesystem, created 0700 under an owner-verified `scratch/` and recorded in `run.json` before it is made. It is removed with the run directory, never by GC. It also holds its own identity, `scratch.json` (`boot_id`, hostname, `kiln`'s and the VMM's identities), written atomically when it is made, because a reboot or logout can empty the run directory's tmpfs first.
- Cleanup only considers entries whose `boot_id` and hostname match the current host and whose process identity is dead; it removes the scratch directory `run.json` records (only a path of the form `…/scratch/<run-id>`) with the run directory. Entries from other boots or hosts are reported but never touched (T7). `$KILN_HOME/scratch/` is swept too, by each directory's `scratch.json`: a real directory of this user's is removed when it names this hostname and either another boot or dead identities (unless kept); other hosts' directories and those without a readable identity are reported and left alone, and the latter are skipped silently for their first minute (a run may be creating one).

---

## 6. Build pipeline

`kiln convert <ref|path>` runs §6.1–§6.5.

`kiln build -f Dockerfile [--platform …] [--builder auto|potter|buildkit|podman|buildah] <context>` hands the Dockerfile to a backend and turns the result into a kiln image. kiln has no Dockerfile engine of its own; it dispatches.
- **Backend selection** (`--builder auto`, the default; the first available wins):
  1. **potter:** kiln runs the `potter` binary (`potter build -f …`). potter is the only Dockerfile implementation kiln ships with, and it executes `RUN` in a kiln microVM (potter spec §6.3, §9.3). It is a process boundary, never a crate dependency: potter depends on kiln's crates, so the reverse would be a cycle (§4.2).
  2. **BuildKit:** `buildctl` if `BUILDKIT_HOST` is set, else `docker buildx build --output type=oci,tar=false,dest=<tmp>`. The OCI layout is then converted (§6.1–§6.5).
  3. **Podman or Buildah:** `podman build` or `buildah build`, exported as an OCI layout and converted. Rootless and daemonless.
- **No backend:** `kiln build` fails with the list of backends to install.
- **potter's contract** (arguments kiln passes, the output form, and how `--platform` maps onto potter, which builds only for the host's architecture) is designed in M3c. potter writes its images into `$KILN_HOME` (potter spec §4) and can also emit an OCI layout (`--output oci-layout=DIR`, potter spec §11).
- **Docker driver check:** for BuildKit through buildx, `kiln` checks the selected buildx driver first. The `docker` driver without the containerd image store can't export OCI. In that case `kiln` fails with the exact command to create a `docker-container` builder; it never creates one implicitly.
- **Cross-platform builds** need binfmt/QEMU on the BuildKit host. This is documented, and `kiln` detects the failure from the `exec format error` signature.

### 6.1 Resolve and fetch (verified)

1. **Resolve** the reference to an OCI index and then a per-platform OCI manifest. A missing platform produces an error listing the available ones.
2. **Validate descriptors before any fetch:**
   - Reject foreign or non-distributable media types and Docker schema v1.
   - Ignore descriptor `urls`.
3. **Fetch verified.** For each blob:
   - Stream it while hashing the compressed bytes.
   - Read the source **to EOF**, even after the tar end-of-archive marker.
   - Check the size and digest.
   - For layers, also hash the decompressed stream and check it against the config's `rootfs.diff_ids[i]`.
   - The erofs output and cache entry are committed **only if both match**; otherwise everything is discarded (T1).
4. **Local inputs** follow the same rule: every blob file is hashed and the digest is checked against the filename and the descriptor. `docker save` archives get their digests computed from content.

### 6.2 Convert layers

- **Cache key:** compressed layer digest, the verified `diff_id`, and `erofsFormatVersion`.
  - Same content with different compression gives a different key but the same erofs output. That costs a miss, never a wrong result.
- **`erofs <digest>` entry:** the layer is independent of the layers below it. Done.
- **`parents [paths]` entry:** the layer needs inherited attributes. Resolve them per §6.3 and look up `cache/layers-ctx/`.
- **On a miss:** stream the layer through the decompressor (gzip or zstd, chosen by media type), then the tar reader, then the `kiln-erofs` writer.
  - Layers convert in parallel.
  - A layer that has implicit parents streams its data immediately but defers metadata finalisation until the lower layers' erofs blobs exist (§6.3).
- **Pulled `kiln` images:** their erofs layers are stored by verified digest but **never** enter `cache/layers/`. Only conversions kiln performed itself from verified OCI sources do.

### 6.3 Implicit parent directories (decision B)

A layer tar may contain `a/b/file` with no `a/` or `a/b/` header. overlayfs shows the *upper* directory's metadata for merged directories, so invented attributes would mask the real ones.

1. While writing, the writer records every **implicit directory**: a directory created only because a descendant, whiteout or opaque marker needed it.
   - The layer root (`/`) is special. When implicit, it always gets the fixed defaults below and does not create a dependency. (Most layer tars omit `./`.)
2. If a layer has no implicit directories other than root, its output depends only on its tar. The cache entry is `erofs <digest>`.
3. Otherwise:
   - The cache entry is `parents <sorted implicit paths>`.
   - To finalise, resolve each implicit path in the **merged view of the lower layers** (layers `0..i-1`, applying their whiteouts and opaque markers) using the `kiln-erofs` reader.
   - If the path resolves to a directory, inherit its mode, uid, gid, mtime and xattrs. Otherwise, or if it is absent, use the defaults.
   - The context hash `ctx` is SHA-256 of the canonical encoding of the resolved `(path, attrs)` list.
   - The erofs output is cached in `cache/layers-ctx/<src>@<diff-id>@<fmt>@<ctx>`, and the layer is annotated with `dev.kiln.inherits`.
4. **Defaults** for invented directories: mode `0755`, uid and gid `0`, mtime equal to the layer's base time (§7.3), no xattrs.
5. **Described later:** a directory that was created implicitly and then described by its own header later in the same layer keeps that header's mode, uid, gid and mtime. The inherited xattrs lie under its own, as in containerd, so it is reported with the implicit directories too.

### 6.4 Squash

The VM's device budget is `max_virtio_devices` minus the fixed devices: the init disk, the scratch disk and vsock, plus net and balloon when present.

- **Default `--max-layers`: 10.** On Firecracker x86_64 that leaves headroom: 2 disks + 10 layers + vsock + net + balloon = 15 of 17.
- **When app layers exceed the limit:** the bottom `N − max + 1` layers are merged **from their erofs blobs** (reader in, writer out), so squash never needs the source tars or any network access.
- **Whiteouts:** the squashed layer is always the bottom of the stack, so all whiteout markers and opaque xattrs are applied and then **removed** from its output.
- **Data:** data extents orphaned by later replacements are not copied.
- **Cache key:** SHA-256 of the ordered erofs digests plus `erofsFormatVersion`.
- **At run time:** if an image's app layers exceed the device budget for the chosen backend, `kiln run` fails with a message telling the user to re-convert with a lower `--max-layers`. There is no squashing at run time.

### 6.5 Init, kernel, commit

1. **Init layer.**
   - The `kiln-init` binary for the arch is fetched by pinned digest (§8.1).
   - It is wrapped in a tiny erofs containing `/kiln-init` and the empty directories `/proc`, `/sys`, `/dev` and `/kiln`.
   - Cache key: `kiln-init` digest plus `erofsFormatVersion`.
2. **Kernel:** fetch `profile@version` for the arch by pinned digest.
   - Each of the two layers is added when the table pins its artifact for the arch, independently; without either the image is provisional (app layers only), `convert` says what `kiln run` will need, and `inspect` shows a `boot:` line. The library's convert makes provisional images unless asked for the pinned layers; `kiln convert` asks.
3. **Commit:** write the config, manifest and multi-arch index, then update `refs.json` under the lock.

### 6.6 Determinism rules

For a given `kiln` version:
- erofs output depends only on the verified tar content, the format version, and (for inheriting layers) the resolved parent attributes.
- The erofs determinism rules in §7.3 apply.
- JSON is serialised with sorted keys and no insignificant whitespace.
- Golden tests enforce this (§11.1). CI fails if golden digests change without an `erofsFormatVersion` bump.

---

## 7. `kiln-erofs` profile

The profile is the exact subset of erofs that `kiln` writes. It is normative in `docs/format.md`.

### 7.1 Layout

- Block size 4096, uncompressed, no chunk-based files.
- **Single pass:**
  - Regular-file data larger than the inline threshold is written to block-aligned extents as the tar streams.
  - Small-file tails are spilled to a temporary file, not RAM.
  - The metadata area (inodes, directories, shared xattrs) is written after the data.
  - The superblock at offset 1024 is written last.
- `meta_blkaddr` is the first metadata block. The root directory is the first inode in the metadata area. nid 0 is never used.
  - This keeps the root nid within the 16-bit superblock field without the 48-bit feature.
  - The writer asserts this.
- The image is zero-padded to a multiple of 4096. The superblock block count equals `size / 4096`. (Firecracker ignores a trailing partial sector.)
- Directory entries are sorted in strict byte order, as erofs's binary search requires.

### 7.2 Inodes

A compact inode is used only when **all** of these hold. Otherwise the inode is extended.
- uid and gid ≤ 65535.
- `nlink` ≤ 65535.
- size < 2³².
- mtime equals the superblock base time, including nanoseconds.

### 7.3 Determinism rules

- The superblock UUID is all zeros, the volume name is empty, and there is no superblock checksum feature.
- Base time is the minimum `(sec, nsec)` mtime of the layer's explicit entries (directory, file, symlink, device, FIFO, whiteout, hardlink; opaque markers excluded), stored as `epoch` and `fixed_nsec`. An empty layer uses base time 0.
- Inode numbering follows a breadth-first traversal in sorted name order.
- Inodes, including hardlinked ones, are numbered at their first occurrence in the breadth-first walk (as in `docs/format.md`).
- Inline xattrs are sorted by `(name index, name bytes)`.
  - An xattr goes into the shared table when it occurs on ≥ 2 inodes.
  - The shared table is ordered by first use in inode-numbering order.
  - No xattr name filter.
- All padding and gaps are zero bytes.

### 7.4 Layer semantics (applying one tar)

These rules match containerd's `archive.Apply`, which is the test oracle (§11.2).

- **Directory over directory:** merge. The new header's attributes replace the old ones; children are kept; an opaque marker already set is kept.
- **Non-directory over anything, or directory over non-directory:** replace, dropping any subtree.
- **Symlinks:** permission bits are always `0777`, whatever the header says, as Linux reports them.
- **Hardlinks:** a hardlink binds to the inode that exists at that path **when the link entry is read**. A later replacement of the target path does not affect earlier links. `nlink` is computed at the end.
  - The link header's mode (unless the target is a symlink), uid, gid, mtime and xattrs are applied to the shared inode, as containerd does.
  - Links to directories, to a missing path, to self, or to a path inside the entry the link replaces are typed errors.
- **Whiteouts (OCI → overlayfs):**
  - `.wh.<name>` becomes a character device 0:0 named `<name>`.
  - `.wh..wh..opq` becomes `trusted.overlay.opaque=y` on the containing directory.
  - Collisions are decided by the **translated** name. A whiteout for a name that already exists in the same layer is an error: OCI says a whiteout cannot hide its own layer, and containerd rejects it. A real entry after a whiteout replaces it.
  - An opaque marker hides only lower layers, never entries from the same layer, regardless of order.
  - A root-level opaque marker has no effect on the merged view, as in overlayfs, and a directory is opaque only when `trusted.overlay.opaque` is exactly `y`.
  - `trusted.overlay.*` xattrs supplied by the tar are dropped with a warning; only whiteout entries produce overlay markers.
- **Entry types:**
  - Supported: regular files, directories, symlinks, hardlinks, char and block devices, FIFOs.
  - Sockets are skipped with a warning.
  - Sparse entries (PAX 1.0 `GNU.sparse.*` and GNU type `S`) are a typed error in v1.
- **Headers:**
  - PAX extended headers and GNU long names and links are supported.
  - `SCHILY.xattr.*` records map to erofs xattrs for the `user.`, `trusted.`, `security.` and `system.posix_acl_*` namespaces.
  - Other namespaces (e.g. `com.apple.*`) are dropped with a warning.
- **Paths:** a path that escapes the root after normalisation is a typed error.
- **Known divergences from containerd:** each is documented in `docs/format.md`, and none occurs in layers image builders produce.
  1. containerd's final pass that sets directory times fails, or re-times the replacement, when a directory or one of its parents is later replaced by a non-directory in the same layer.
  2. containerd resolves implicit parents per lower layer, not through the overlay view.
  3. containerd follows lower-layer symlinks while resolving parents.

### 7.5 Reader

A supported internal API: open, lookup, readdir, getattr, read, and xattrs for profile-conformant images. It is used for squash, parent inheritance and tests. It rejects images that fall outside the profile.

### 7.6 Resource limits (T3)

All limits are configurable. Exceeding one is a typed error naming the limit.

| Limit | Default |
|---|---|
| Uncompressed bytes per layer / per image | 16 GiB / 64 GiB |
| Expansion ratio (uncompressed / compressed) | 200 |
| Entries per layer | 2,000,000 |
| PAX record, long name or link, single xattr value | 1 MiB |
| Path length / depth | 4096 bytes / 256 components |

Traversals are iterative, never recursive.

The per-image limit counts the bytes one conversion decompresses. Layers served from the layer cache are not decompressed and do not count, so the limit bounds the work a conversion does (T3), not the image's total size.

---

## 8. Kernel, init and cmdline

### 8.1 Pinned artifacts and run-time verification (T4)

- `kiln` compiles in a table: `(kind ∈ {kernel, init}, profile/version, arch) → digest`.
  - A pin names the artifact's blob itself (the kernel image, the static kiln-init binary), the registry repository holding it and its size. Fetching is one blob GET by digest, verified; no tag or manifest is resolved. A blob already in the store is used without contacting the registry.
  - The table is empty until the first vmkit kernel and kiln kiln-init releases exist; until then every run names `--kernel` and `--init`.
  - Kernels come from `vmkit` releases. `kiln-init` comes from `kiln` releases.
  - Both are also published as OCI artifacts.
- **At build:** artifacts are fetched and verified against the table.
- **At run:**
  - The image's kernel layer digest must equal the pinned digest for the `(profile, version, arch)` in its config. An image without a kernel layer needs `--kernel`.
  - `--kernel <path>` and `profile: custom` images are refused unless `--allow-custom-kernel` is given; `--init <path>` (a static kiln-init) is refused unless `--allow-custom-init` is given.
  - The image's init layer is **replaced** with the init layer for the pinned `kiln-init` of the running `kiln`. A warning is printed when the image's init layer differs.
  - `inspect` labels `source` as unverified provenance.

### 8.2 Kernel profiles (in `vmkit`)

- The version is pinned to the current LTS at implementation time.
- **Config:** upstream Firecracker CI guest config for the arch, plus `vmkit` fragments:
  - `EROFS_FS`, `OVERLAY_FS`, `VIRTIO_VSOCKETS`, `SERIAL_AMBA_PL011` and its console (for Cloud Hypervisor aarch64)
  - ext4 with online resize, `DEVPTS`, `POSIX_MQUEUE`, `CGROUPS` (v2), netfilter-free guest
  - PVH on x86_64
- **One binary per arch** boots on both VMMs: an ELF `vmlinux` with PVH on x86_64, an `Image` on aarch64.
- v1 ships one profile: `base`.
- CI builds per arch, boot-tests on both VMMs with a test guest, and publishes GitHub release assets and `ghcr.io/<project-org>/vmkit-kernels/<profile>:<version>-<arch>`.
  - `<project-org>` is the GitHub org hosting the repos, fixed at the first release.

### 8.3 Kernel cmdline (T4)

The cmdline is assembled only by `kiln` and `vmkit`. Images contribute nothing.

```
root=/dev/vda ro rootfstype=erofs init=/kiln-init panic=-1 quiet loglevel=3 console=<vmkit> <vmkit backend params>
```

`panic=-1` reboots immediately on a panic, which ends the VM on every backend (§4.1).

---

## 9. Boot and runtime

### 9.1 Devices (fixed order)

| Device | Content | Mode |
|---|---|---|
| `vda` | Init erofs | ro |
| `vdb` | Scratch ext4 (overlay upper and work dirs) | rw |
| `vdc…` | App layers, lowest first | ro |
| vsock | Control protocol (§9.5), guest CID 3 | — |
| net (optional) | `--net` (§9.3) | — |

### 9.2 Sandbox (`vmkit::sandbox`, T6)

Each VMM runs as the invoking user, started by the `vmkit-sandbox` helper. Ubuntu 23.10 and later let only AppArmor-profiled binaries create user namespaces, so vmkit ships an installer for the helper's profile (the bubblewrap model). Inside:
- **A user namespace** that identity-maps the invoking uid and gid.
- **A mount namespace** whose root is a minimal tmpfs containing only:
  - `/dev/kvm`, `/dev/null`, `/dev/urandom`, and `/dev/net/tun` when there's a network
  - the VMM binary, bind-mounted read-only
  - `/vm/kernel`, `/vm/disk/<n>` and `/vm/sock/` (`<run_dir>/sock` on the host; the rest of the run dir stays out of the VMM's reach)
- **A PID namespace.**
- **A net namespace:** empty, or with networking per §9.3.

Further constraints:
- **File handling:** `vmkit` opens every file with `O_NOFOLLOW` and attaches it by file descriptor (`open_tree`/`move_mount`), so path swaps between check and use are impossible.
- **In-sandbox paths are fixed** (`/vm/disk/<n>`), so snapshots can be restored from any host-side location (sagger).
- **Seccomp and Landlock:** Firecracker's built-in seccomp stays at its default. Cloud Hypervisor runs with `--seccomp true --landlock` and Landlock rules limited to `/vm` and `/dev/net/tun`, which are set in its `vm.create` configuration. The sandbox has no `/proc` or `/sys`.
- **Hardening:** `no_new_privs`, all inherited file descriptors closed except those needed, rlimits on file descriptors and processes.
- **cgroups:** if the systemd user session can create a scope (`systemd-run --user --scope`), the VMM goes into one with memory, CPU and tasks limits from `--memory` and `--cpus`. Otherwise `kiln run` warns once (`vmkit::cgroups_available()`).
- **API socket:** lives in the 0700 run dir (§5.3). The driver keeps it open for sagger, but only the invoking user can reach it.

### 9.3 Networking (`--net`, T5)

Inside the VM's own net namespace:
- A tap device `tap0` with address `172.30.0.1/30`. The guest is `172.30.0.2/30`, gateway `.1`.
  - Every VM has its own namespace, so addresses never collide and VMs can't route to each other.
- `ip_forward=1` is set **in that namespace only**.
- An nftables table in that namespace:
  - **forward:** accept `iif tap0 ip saddr 172.30.0.2` to the egress interface, subject to the egress policy; drop everything else, including IPv6 and spoofed sources.
  - **input:** drop every new connection from the guest; DNS to `.1:53` never reaches input, because it is DNAT'd to pasta's DNS forward address. ARP is unaffected, since it isn't IP traffic.
  - masquerade on egress.
- `pasta` attaches to the namespace and provides unprivileged egress through host sockets:
  - `--no-map-gw`: host loopback services are unreachable.
  - DNS from the guest to `.1:53` is DNAT'd to pasta's `--dns-forward` address. pasta queries the host's resolver from the host namespace, so loopback stubs such as `127.0.0.53` work.
- **Egress policy** (`--egress`):
  - **Default `restricted`:** deny `169.254.0.0/16`, `100.64.0.0/10`, RFC 1918, `0.0.0.0/8`, `127.0.0.0/8`, multicast, and the host's own addresses; allow the rest.
  - `--egress allow=<cidr>`: adds exceptions (repeatable). pasta copies the host's outbound address (the one on its default route) into the namespace, so the guest can never reach it: an exception that is only that address is refused before boot, and a wider one that includes it is warned about. The host's other addresses can be reached through an exception.
  - `--egress deny-all`: blocks everything except DNS and `allow=` exceptions.
  - `--egress open`: removes the default denies, with a warning.
  - The guest cannot modify the policy; it lives outside the guest.
- **Port forwarding:** `-p [ADDRESS:]HOST:GUEST[/tcp|udp]` (repeatable). The host address `pasta` listens on defaults to `127.0.0.1`, so a published port stays on the host unless an address is given (`0.0.0.0` for all); a host port is published once per protocol whatever its addresses, since the namespace sees only the port. The image owns its listen port: the app must listen on `0.0.0.0` or the guest's address, and a forward to a guest listening on `127.0.0.1` ends at once. Forwarding is `pasta` port forwarding plus DNAT to `.2`, on both of pasta's paths: through its interface (prerouting), and spliced from host loopback as a local connection to the namespace's own address (nat output, with `route_localnet` on `tap0`).
- **Guest addressing** is delivered in the config message (§9.5). The guest configures `eth0` over netlink.

### 9.4 Scratch disk and `--persist`

- **Template.** `crates/kiln-image/assets/ext4-template.img.zst` is created by the committed script `assets/make-ext4-template.sh` with pinned e2fsprogs:
  - `mke2fs -t ext4 -b 4096 -I 256 -i 65536 -O meta_bg,^resize_inode -U <fixed> -E hash_seed=<fixed>`, 64 MiB.
  - The template is committed and embedded in `kiln` (a few KB compressed), so no `mkfs` is needed anywhere.
- **Per run:** the template is decompressed to a sparse file in the run dir and extended to `--disk` (default 4 GiB).
- **In the guest** (§9.6 stage 3): mount with `noinit_itable`, **then** grow online with `EXT4_IOC_RESIZE_FS` on an fd for the mount point.
- **`--persist <path>`:**
  - The disk lives at `<path>`, with a host-side sidecar `<path>.kiln.json` recording the image manifest digest, the ordered layer digests and the size.
  - **Different layers:** reuse with a different ordered set of app layers is refused unless `--persist-reset` (recreate) or `--persist-force` (accept the risk, with a warning) is given; the two exclude each other. overlayfs forbids offline lower-layer changes when `xino` is on. The same layers under another manifest (a changed env or cmd) are reused: the sidecar records the new manifest, with a note. A disk without a readable sidecar is refused unless reset or forced.
  - **Size:** the disk is never truncated; a `--disk` smaller than the current size is refused; a larger one grows it.
  - **One run at a time:** the disk is locked (`flock`) for the run. It replaces the run's scratch directory (§5.3), so no cleanup touches it; symlinks are refused.
  - The host never parses the guest-written ext4.

### 9.5 Control protocol (`kiln-proto`, normative in `docs/format.md`, T9)

**Transport:** hybrid vsock. The guest connects to host CID 2. The VMM maps guest port `P` to the host Unix socket `<vsock socket>_P`, where `<vsock socket>` is the path vmkit reports for the VM (`Vm::vsock_socket()`); kiln listens on each port's socket before boot.

| Port | Stream | Direction |
|---|---|---|
| 1024 | control (framed) | both |
| 1025 | stdin (raw bytes, EOF = half-close) | host → guest |
| 1026 | stdout (raw) | guest → host |
| 1027 | stderr (raw) | guest → host |
| 1028 | tty (raw, replaces 1025–1027 in `-t` mode) | both |

**Framing:** `u32` little-endian length (≤ 64 KiB), then a `u8` message type, then a JSON payload. Unknown types, oversize frames or invalid payloads end the VM.

**Messages:**

| Direction | Message | Meaning |
|---|---|---|
| guest → host | `Hello { protocol }` | First message. The host replies with `Config` **once per run**. A second `Hello` (e.g. after a guest reset) makes the host kill the VM. |
| host → guest | `Config { … }` | Process overrides, `stopSignal`, `tty`, `interactive`, `hostname`, network, `layers`, `scratch`, `exitMethod`, `shutdownGraceSecs`. |
| guest → host | `Stage { n }` | Progress, for diagnostics. |
| guest → host | `Running` | Workload started. Ends the boot timeout. |
| guest → host | `Exited { signaled, code }` | Main process ended. Sent after its stdout and stderr hit EOF. |
| guest → host | `InitFailed { stage, errno?, message }` | Init failure (sanitised on display). |
| host → guest | `Shutdown { graceSecs }` | Send `stopSignal` to the main process, then SIGKILL everything after the grace period. |
| host → guest | `Signal { sig }` | Forward a signal to the main process. |
| host → guest | `WindowSize { rows, cols }` | `-t` mode only. |

This is protocol version 1. Later versions add to it under the rules in §9.8.

**Host-side exit codes:**
- The app's exit code, or `128 + signal`.
- **125:** `kiln` or infrastructure error, including boot timeout, the VM ending without `Exited`, and any init failure other than the two below (for example an unknown user).
- **137:** the user killed the VM (a second SIGINT, `Ctrl-]` `k`) before the app exited, as `docker kill`.
- **126:** the entrypoint was found but could not be executed (`EACCES` or `EISDIR` from exec).
- **127:** the entrypoint was not found (`ENOENT` from exec).

The numbers match Docker. These codes are guest-asserted (§3).

### 9.6 `kiln-init` sequence

Each stage reports `Stage { n }`. A failure reports `InitFailed` when the control channel is up, prints `kiln-init: <stage>: <error>` to the console, and exits via `exitMethod`, or `reboot()` if no config has arrived. A panic hook does the same.

1. **Early:** mount `/proc`, `/sys` and `/dev` (devtmpfs) in the init root. Call `reboot(LINUX_REBOOT_CMD_CAD_OFF)` so Ctrl-Alt-Del becomes SIGINT to init instead of an instant reset.
2. **Control:** connect vsock port 1024, send `Hello`, receive and validate `Config`.
3. **Storage:**
   - Mount a tmpfs at `/kiln` and create the mount points.
   - Mount the app layers read-only at `/kiln/layers/<n>`.
   - Mount the scratch disk at `/kiln/rw` with `noinit_itable`, grow it with `EXT4_IOC_RESIZE_FS`, and create `upper/` and `work/`.
   - Mount the overlay at `/kiln/root` with `lowerdir=<top…bottom>,upperdir,workdir,xino=on,redirect_dir=off,index=off,metacopy=off`.
4. **Root:**
   - In the overlay, `mkdir -p` any missing `/proc`, `/sys`, `/dev` and `/etc` (written to the upper layer).
   - Mount fresh `proc`, `sysfs`, `devtmpfs`, `devpts` (`newinstance,ptmxmode=0666`), `/dev/shm` (tmpfs, 1777), `/dev/mqueue` and `/sys/fs/cgroup` (cgroup2).
   - Create the `/dev/fd`, `/dev/stdin`, `/dev/stdout`, `/dev/stderr` and `/dev/ptmx` symlinks.
   - **No** tmpfs on `/run` or `/tmp`, matching Docker.
   - `pivot_root(".", ".")`, then detach the old root.
5. **Identity and network:**
   - Write `/etc/hostname`, `/etc/hosts` and `/etc/resolv.conf` as regular files, replacing any symlink at those paths (unlink, then `O_CREAT|O_NOFOLLOW`). Without a network (or DNS servers), `resolv.conf` names `127.0.0.1`, as Docker's `--network none` does, so lookups fail at once.
   - Bring up `lo`. Configure `eth0` over netlink when there's a network.
6. **Process:**
   - **User:** resolve `user` against the image's `/etc/passwd` and `/etc/group`. Numeric `uid[:gid]` works without entries.
   - **Groups:** call `setgroups` with the user's supplementary groups.
   - **Environment:** the image env plus overrides. Default `PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin` if unset; `HOME` from passwd, else `/`; `HOSTNAME`.
   - **Stdio:** pipes relayed to ports 1025–1027, or in `-t` mode a pty from devpts whose slave becomes the main child's controlling tty (`setsid` + `TIOCSCTTY`), relayed to port 1028.
   - `chdir`, `exec`, then send `Running`.
7. **Supervise:**
   - Reap all children.
   - Handle `Shutdown`, `Signal` and `WindowSize`. SIGINT (from Ctrl-Alt-Del) is treated as `Shutdown`.
   - When the main process exits: SIGKILL and reap every remaining process, drain stdout and stderr to EOF, send `Exited`, `syncfs`, remount the scratch disk read-only on a best-effort basis (EBUSY ignored), and exit via `exitMethod`. Killing the rest before draining matches Docker (a container ends with its PID 1) and keeps a background process that holds stdout from blocking the drain forever.
   - Stop signals and forwarded signals are numbers 1–31; images whose `STOPSIGNAL` is a real-time signal are refused with an error.

### 9.7 `kiln run`

```
kiln run <ref> [--vmm firecracker|cloud-hypervisor] [--cpus N] [--memory MiB]
               [-i] [-t] [--disk SIZE] [--persist PATH [--persist-reset|--persist-force]]
               [--net [--egress POLICY]... [-p [ADDRESS:]HOST:GUEST[/proto]]...]
               [--env K=V]... [--stop-timeout SECS] [--boot-timeout SECS]
               [--kernel PATH] [--allow-custom-kernel] [--init PATH] [--allow-custom-init] [-- CMD ARGS...]
```

- **stdio:** without `-i`, guest stdin gets EOF immediately. Pipes are binary-safe, and EOF propagates, so `echo hi | kiln run -i img -- cat` exits.
- **`-t`:** `kiln` puts the terminal in raw mode itself and always restores it, including on panic and SIGTERM. Ctrl-C reaches the guest app through its controlling tty. The escape sequence `Ctrl-]` then `q` sends `Shutdown`; `Ctrl-]` then `k` kills the VM.
- **Without `-t`:**
  - First SIGINT or SIGTERM: send `Shutdown { stop-timeout }` (default 10 s).
  - Second SIGINT: kill the VMM.
- **The VMM never touches the terminal.** Its stdin is `/dev/null` and the serial console goes to `console.log`. A sanitised tail of `console.log` is printed when the run fails.
- **`--boot-timeout`** (default 30 s) bounds the time from VMM start to `Running`.
- **Device budget:** checked before boot; the remedy offered is `kiln convert --max-layers N` (§6.4).
- **On macOS:** `run` exits with instructions for the Lima template (§10).

### 9.8 Control protocol growth

kiln-proto is extended, never replaced. Every host that drives a kiln guest (`kiln run`, potter's VM executor, `kiln-shim`, later sagger) speaks kiln-proto to `kiln-init`. None of them adds a second protocol or a second PID 1.

**Versioning** (ruling, rev 2.8; it keeps version 1's `Hello` handling in `docs/format.md`):
1. Each milestone below that changes the protocol bumps the version once: 2, 3, … in the order they land. A version only adds things: message types (new type numbers), optional `Config` fields and new values. It never changes or removes what an earlier version defines.
2. The guest's `Hello { protocol }` carries the highest version it implements. A host supports every version from 1 up to its own. A version higher than the host's still ends the VM (125), as in version 1.
3. The host sends only messages, fields and values that the guest's version defines. If the run needs something the guest's version lacks (for example `services` or build mode), the host ends the VM instead of sending `Config` and reports 125 with an error naming the version required.
4. The guest keeps version 1's strict decoding. An unknown type, field or value is a protocol error (`InitFailed`, then the VM ends), so an older `kiln-init` refuses an addition instead of ignoring it.

kiln-init is pinned to the kiln release that boots it (§8.1), and potter and `kiln-shim` use that pinned kiln-init, so these rules fire only on mixed installs. Each addition stays inside T9: messages are size-bounded and schema-validated, and every value the guest sends is guest-asserted (§3).

**Planned extensions.** Each is specified here only as far as its source spec specifies it. Anything marked "designed in" is decided in that milestone's plan and then made normative in `docs/format.md`. Wire names follow `docs/format.md`'s conventions (camelCase keys); the source specs' spellings are notation.

| Extension | Milestone | Defined in |
|---|---|---|
| `services` | M3d | potter spec §4, §6.1.1 |
| Build mode | M3e | potter spec §9.3 |
| Exec, `Stats`, `OomKill`, `FileUpdate` | M4 (shim S1) | shim spec §4.1, §4.2, §4.5 |

**`services` (M3d):**
- `Config.services: [Service{name, process, ready, stop_signal}]`, where `ready` is `unix:<path> | tcp:<port> | none`.
- Services start in order, each waiting for the previous one's readiness. The main process starts last.
- If any of them exits, the rest are stopped in reverse order with their `stop_signal`, within the shutdown grace period. The VM's exit status is that of the first process to exit.
- Signals the host delivers go to the main process. Every service shares the main process's stdout and stderr.
- Users: potter's PHP web images run php-fpm as a service (`ready = unix:/run/php/fpm.sock`) with nginx as the main process (potter spec §6.1.1); `kiln-shim` runs sidecars this way (shim spec §4.4).
- Designed in M3d: the shape of a service's `process`, readiness timeouts, how `Exited` reports a service that exited first, and whether `kiln run` exposes services.

**Build mode (M3e):**
- `Config.mode = Run | Build`. In build mode, `Config` carries the layer and mount table and limits.
- Messages: the guest sends `Ready`. For each step the host sends `RunStep{id, argv|shell, env, workdir, user, lowers_added, mounts, secrets, timeout_ms}`, where `lowers_added` names the pending inputs-layer disks to stack before the step runs. The guest streams the step's stdout and stderr, then sends `Exited{code|signal}`. On exit 0 it seals the step and sends `StepSealed{id, offset, len, sha256, entries}`.
- Secrets arrive inline. kiln-init writes them to a tmpfs that is never part of the upper.
- kiln-init build mode:
  - runs the step;
  - seals it: kills the step's process group and any stragglers, then unmounts the root overlay;
  - converts the upper into an OCI tar diff (char 0/0 whiteouts become `.wh.<name>`, and `trusted.overlay.opaque` becomes `.wh..wh..opq`);
  - appends the diff to the raw output disk at the next 4 KiB-aligned offset;
  - remounts with the sealed upper as the topmost lower and a fresh upper. Mounts use per-layer `lowerdir+` or mount(2), never one long `lowerdir` string.
- The host trusts none of `StepSealed`'s values. potter reads the range itself, checks its bounds, hashes it, and ingests it through kiln's untrusted tar path (potter spec §9.3, P4).
- Designed in M3e: the session order (§9.5's `Hello`-first rule stands; see the note below), the build-mode device order (output disk, cache-mount disks) against §9.1, how a step's stdout and stderr are carried, how a step's `Exited` is told apart from the main process's, and the size bound on `RunStep`.

**Exec, guest stats and file updates (M4):**
- **Exec:**
  - Host → guest `ExecStart{exec_id, argv, env, cwd, user, tty, window}`. Guest → host `ExecStarted{exec_id, pid}`, then `ExecExited{exec_id, code|signal}`.
  - Stdio, `Signal` and `WindowSize` gain a `proc` field, `main | exec:<id>`. The main process stays `main`, so its messages are unchanged on the wire.
  - At most 32 concurrent execs per VM (configurable), each with stdio bounded like the main process's.
  - Exec ids are host-chosen and validated. The guest can report only on ids the host created.
  - An exec runs in the guest's root with the image env merged with `env`. `user` follows kiln-init's id rules. Execs are killed when the main process exits.
- **Stats:** guest → host `Stats{mem_total, mem_available, mem_rss_main, cpu_usage_ns, pressure{cpu,mem,io}}`, every 10 s by default and on request, read from `/proc/meminfo`, `/proc/stat` and PSI.
- **OOM:** guest → host `OomKill{pid, proc, comm}` when the guest kernel OOM-kills a process. kiln-init watches `/proc/vmstat` `oom_kill` and the kernel log. Stats and OOM reports are used for reporting only (§3).
- **Files:** projected volumes are delivered as files in `Config` at start. Updates arrive as `FileUpdate{path, content, mode}`, and kiln-init reproduces kubelet's atomic symlink swap. Their size is bounded.
- Designed in M4:
  - how exec stdio is carried (version 1 relays stdio as raw vsock streams on ports 1025–1028, not as messages);
  - the stats request message;
  - the `Config` field for initial files, and its bound against the 64 KiB frame;
  - the `Config.network` additions the shim needs for the pod's IP, routes and DNS (shim spec §3.2);
  - the dm-verity root hashes the shim passes to kiln-init (shim spec §3.1, S4).

**Note on potter spec §9.3:** it has the host send `Config{mode: Build}` and the guest reply with `Hello`, then `Ready`. Under §9.5 the guest's `Hello` always comes first and the host answers it with `Config`. M3e keeps that order unless the user decides otherwise.

---

## 10. macOS development (Lima)

- `lima/kiln.yaml`: `vmType: vz`, `nestedVirtualization: true` (needs an M3 or later and macOS 15 or later), Firecracker, Cloud Hypervisor and `pasta` installed, and the user in the `kvm` group.
- **The macOS store is mounted read-only** into the VM. The Linux side uses its own local `$KILN_HOME` and its own `$XDG_RUNTIME_DIR`. Unix sockets and flock aren't reliable on Lima shared mounts.
- **`kiln import --from-store <path> <ref>`** copies an image's blobs into the local store by digest, verifying each one.
- The template sets `KILN_IMPORT_FROM` to the read-only mount, so `kiln run <ref>` imports the image from it whenever that store has the name: only missing blobs are copied, and the image is tagged again every time, so the newest convert on macOS is what runs.
- Ports published on the VM's `127.0.0.1` (the `-p` default) reach the Mac's `localhost` through Lima's forwarding of loopback listeners; the template needs no `portForwards` entry.

---

## 11. Testing

### 11.1 `kiln-erofs` unit and property tests

- **Unit tests** for every rule in §7.4 and every limit in §7.6.
- **Property tests (`proptest`):** random multi-layer stacks with nested dirs, implicit parents, hardlinks (including relink after replace), xattrs, long paths, whiteouts and opaques in all orders, duplicate paths, and empty layers. Each stack is written, then read back with the reader and compared against a model implementation of §7.4.
- **Golden determinism tests** run on macOS and Linux. CI fails if golden digests change without a format-version bump.
- **On Linux CI:** `fsck.erofs` on every generated image, plus a kernel loop-mount test.

### 11.2 Oracle tests (Linux CI)

1. For each fixture image, flatten the layers with containerd's `archive.Apply` (via `tools/oracle`) into directory A.
2. Mount `kiln`'s erofs layers with overlayfs, using kiln's exact mount options, as B.
3. Compare A and B: names, types, modes, uid and gid, mtimes, xattrs, content hashes, symlink targets, device numbers and hardlink groups.

Random stacks from §11.1 go through the oracle too.

### 11.3 `vmkit` contract suite

One parametrised suite runs against both backends, using a test guest (the kernel profile plus a busybox initramfs). It covers:
- boot
- the configured exit method ends the VMM
- a guest reset ends the VM (Cloud Hypervisor backstop)
- `panic=-1` ends the VM
- pause and resume
- kill and cleanup
- sandbox contents (the guest-side VMM view contains only the expected files)
- device-budget enforcement against `max_virtio_devices`

### 11.4 End-to-end matrix

{Firecracker, Cloud Hypervisor} × {aarch64, x86_64} × these fixtures:
- `alpine`, `debian`, `php:8.4-cli`, a distroless image, `FROM scratch` with a static binary
- `nginx` (`/dev/stderr` logs, `STOPSIGNAL SIGQUIT`), `mysql:8.4` and `postgres` (files in `/var/run`)
- An image with a symlinked `/etc/resolv.conf`
- `composer` (HOME), a non-root `USER` with supplementary groups
- An image with implicit parents (built with `crane append`)
- More than 10 layers (squash)
- Whiteout and opaque cases, an empty layer
- A Dockerfile build through each backend (§6): potter, BuildKit, and Podman or Buildah
- Each run checks: exit codes, stdio EOF and binary safety, `-t` mode, graceful shutdown within `--stop-timeout`, the `--persist` mismatch refusal, and the `--disk` shrink refusal.

### 11.5 Security tests

- **Hostile registry fixture**, which must be rejected with no cache entry:
  - wrong content for a digest
  - trailing junk after the tar end
  - a `diff_id` mismatch
  - descriptor `urls`
  - a redirect to `169.254.169.254` or RFC 1918
  - a decompression bomb
  - a million-entry layer
  - oversized PAX records
  - hardlink cycles
- **Hostile guest fixture:**
  - Reaching `169.254.169.254`, the gateway's host services, RFC 1918 addresses and other VMs must fail.
  - Spoofed source addresses must be dropped.
  - A second `Hello`, oversized frames and invalid JSON must each end the VM.
  - Terminal control sequences in `InitFailed` must be sanitised.
- **Kernel and init swap:** an image whose kernel layer differs from the pin must be refused; a mismatched init layer must be replaced.

### 11.6 Performance

`kiln bench` measures the §1 targets from a local OCI layout and emits JSON. CI records results per commit. Regressions are reported, not failing, until the targets are first met.

### 11.7 CI hosts and registries

- **x86_64:** hosted Linux runners with `/dev/kvm` (a udev rule grants access).
- **aarch64:** hosted arm64 runners have **no** `/dev/kvm`. aarch64 VMM tests run on a self-hosted bare-metal ARM runner. Until one is provisioned, aarch64 end-to-end tests are a manual pre-release gate on the Lima template.
- **Registry tests:** a local `zot` and `distribution` registry seeded from cached fixtures, so per-commit CI never touches Docker Hub's rate limits. A nightly authenticated job pulls real Docker Hub images. Push is tested against `zot` and `distribution` per commit and against GHCR in release CI. Docker Hub and ECR are best-effort, with their status documented.

---

## 12. Milestones (one implementation plan each)

**Order:** M1 and M2 are independent. M3a depends on both, and M3b on M3a. M3c and M3d each depend on M3b. M3e depends on M3d. M4's phases depend as listed below. sagger and sherd come after, and each needs its own spec first.

| Milestone | Repo | Contents | Needs KVM | Status |
|---|---|---|---|---|
| **M1: convert** (planned as **M1a**, `kiln-erofs`, and **M1b**, store, registry, OCI inputs, pipeline and CLI) | `kiln` | `kiln-store`, `kiln-registry`, `kiln-oci`, `kiln-erofs` (writer, reader, profile, semantics, limits), §6.1–6.4 and §6.6, the oracle tests, the hostile-registry tests, and the CLI `convert`/`pull`/`push`/`import`/`inspect`/`ls`/`gc`/`bench`. Output is the app-layer set plus a provisional manifest without kernel and init layers. | No | Done |
| **M2: vmkit** | `vmkit` | `Vmm` trait (snapshot/restore shapes only), Firecracker and Cloud Hypervisor drivers, `sandbox`, `net` (tap, nftables, `pasta`), `Capabilities`, the kernel profiles pipeline, the contract suite, and the hostile-guest network tests (with the busybox guest). | Yes | Done |
| **M3a: guest** | `kiln` | `kiln-proto` (protocol version 1), `kiln-init`, the ext4 template, the init layer (§6.5), the guest side of §9, the boot suite on both VMMs, and the control protocol in `docs/format.md`. | Yes | Done (PR #5) |
| **M3b: run** (planned as **M3b-1**, the runtime and CLI, and **M3b-2**, pinned artifacts, `--persist`, `--net` and Lima) | `kiln` | `kiln run` (§9.7): the host runtime, its CLI flags, host signal handling and console tails; kernel and init layers in images with their pins and run-time verification (§6.5, §8.1); the Lima template (§10). | Yes | Planned (M3b-1: `docs/superpowers/plans/2026-10-06-m3b1-run.md`; M3b-2: `docs/superpowers/plans/2026-10-07-m3b2-artifacts-persist-net.md`) |
| **M3d: services** | `kiln` | kiln-proto and `kiln-init` `services` (§9.8), as a protocol version bump. Needed by potter P2 (PHP web images). | Yes | Planned |
| **M3c: build** | `kiln` | `kiln build` backend dispatch: potter, BuildKit, Podman or Buildah (§6). The end-to-end matrix (§11.4), the kernel and init swap tests (§11.5), the release pipeline and the docs (§15). | Yes | Planned |
| **M3e: build mode** | `kiln` | kiln-proto `Config.mode = Build`, the build-mode messages and `kiln-init` build mode (§9.8), as a protocol version bump. Lands with potter P4 (the VM executor). | Yes | Planned |
| **M4: `kiln-shim`** (slip) | `kiln` | The containerd shim, in the phases of the shim spec (§8 there): **S0** a KVM node-group spike, nested C8i against metal, with kiln's bench and end-to-end matrix (after M3b); **S1** kiln-proto exec, `Stats`, `OomKill` and `FileUpdate` (§9.8; after M3a); **S2** media type alignment with containerd 2.3's erofs snapshotter, plus a test (§5.1; after M1); **S3** `kiln-shim`: lifecycle, networking, logs, exec, stats, projected volumes (after S1, S2 and M3b); **S4** resize, verity, init containers (after S3; uses the vmkit hotplug capabilities in §4.1). The shim spec's S5, a platform canary, is a rollout step, not kiln work. | Yes | Planned |
| **sagger** (agent sandbox) | `kiln`, `vmkit` | Snapshot, restore and fork on the `Vmm` trait shapes fixed in M2. | Yes | Needs a spec |
| **sherd** and **raku** | `kiln` (sherd), `vmkit` (raku) | Snapshot artifacts in OCI registries with chunk dedup (sherd); lazy restore below the VMM (raku). | Yes | Needs a spec |

potter's own milestones (P1–P7) are in the potter spec §15. Its P2 needs M3d and vmkit's generalised `sandbox` and tap-less `net` (§4.1); its P4 needs M3b and M3e; its P5 provides the Dockerfile frontend that M3c's potter backend runs.

---

## 13. Error handling

- **Errors:** libraries use typed `thiserror` errors. The CLI uses `miette` diagnostics with remediation hints. Registry errors are redacted (§4.2).
- **Untrusted text (T8):** every string from an image or the guest that `kiln` prints is sanitised: lossy UTF-8; C0 and C1 control characters removed except `\n` and `\t`; ESC removed; length capped. App stdio is passed through unmodified, as with `docker run`. `docs/security.md` documents the terminal-escape risk of running untrusted images with `-t`.
- **Store:** store mutations are atomic and serialised against GC (§5.2).
- **Guest failures:** reported by init stage via `InitFailed`, with a sanitised console tail.

---

## 14. Verification items for planning

Each item has a defined fallback, so none blocks the design.

| Item | Milestone | Fallback if false |
|---|---|---|
| Firecracker and Cloud Hypervisor run inside an unprivileged user, mount, PID and net namespace sandbox with `/dev/kvm` access through `kvm` group membership. | M2 | Run without the user namespace but keep the mount namespace (via a userns-less `unshare` where permitted), seccomp, Landlock and the per-run dir. Document the weaker profile. |
| A tap device and nftables rules can be created inside a net namespace owned by a user namespace, and `pasta` can attach to it. | M2 | A small root-owned `vmkit-netd` with a fixed root-owned config, in a later milestone. Until then, `--net` requires `CAP_NET_ADMIN` and prints a warning. |
| Cloud Hypervisor's vsock uses the same `<uds>_<port>` hybrid scheme as Firecracker for guest-initiated connections. | M2 | A per-driver port-to-path mapping inside `vmkit`. |
| Cloud Hypervisor exits on guest power-off on both arches (ACPI S5 on x86_64, PSCI SYSTEM_OFF on aarch64). Source-level reading suggests yes. | M2 | Treat a guest halt detected by the event monitor as the end of the VM. |
| Cloud Hypervisor aarch64 works under Lima nested virtualization on Apple Silicon (GIC/ITS). | M2 | Local aarch64 development is Firecracker-only. Cloud Hypervisor aarch64 is tested on the bare-metal runner. |
| The ext4 template with the §9.4 parameters grows online to ≥ 64 GiB. | M3a | Several template sizes embedded, picking the nearest. **Result (M3a):** true on Firecracker. On Cloud Hypervisor 53 under nested virtualization, growth past about 8 GiB hangs: CH advertises WRITE_ZEROES without a sparse backend. The default 4 GiB disk works on both; re-check on newer CH and bare metal. |
| Crate names `kiln-*` and `vmkit` are free on crates.io. | M1 | Rename before first publish. |

---

## 15. Release and docs

- **Binaries:** macOS arm64 and x86_64; Linux arm64 and x86_64 (static musl). From M4, `kiln-shim` for Linux arm64 and x86_64.
- **Artifacts:** `kiln-init` binaries and their pinned digests are published from the `kiln` release pipeline; kernels from `vmkit`'s.
- **Docs:**
  - `README.md`: quickstart, building on the Mac, running in Lima.
  - `docs/format.md`: normative image format, erofs profile and control protocol, with every protocol version (§9.8).
  - `docs/architecture.md`.
  - `docs/security.md`: the threat model, sandbox and egress defaults.
- **License:** Apache-2.0.
