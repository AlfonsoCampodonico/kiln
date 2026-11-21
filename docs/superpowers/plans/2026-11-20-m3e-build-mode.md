# M3e: kiln build mode (control protocol version 3) Implementation Plan

**Goal:** a kiln microVM can run build steps for potter's VM executor (potter P4, spec §9.3): the host boots a build VM over read-only erofs layers, sends one `RunStep` at a time, streams each step's stdout and stderr back, and the guest seals each successful step's changes as an OCI tar diff onto a raw output disk that potter reads, verifies and stacks as the next layer (spec §9.8 build mode, milestone M3e). It ships as control protocol version 3, so a `kiln-init` that predates M3e refuses a build VM instead of misreading it.

**Architecture:**
- **Protocol version 3** (Task 1): `kiln_proto::Config` gains `mode` (`run` default, `build`), `disks` and `sourceDateEpoch`; new messages `RunStep` (17), `Finish` (18), `StepExited` (19), `StepSealed` (20) after version 1's exec and file types 10–16. A run-mode `Config` is byte for byte version 2's. `PROTOCOL_VERSION` becomes 3, and the host refuses a build VM to an older guest with M3d's `version_refusal` (the version-2 negotiation pattern: no `Config`, VM killed, error naming the version).
- **The diff writer** (Task 2): `kiln_init::diff`, a deterministic ustar/PAX writer (whiteouts, opaque markers, hard links, long names, large ids, `SCHILY.xattr.*`), with the xattr allowlist and the `SOURCE_DATE_EPOCH` clamp as pure functions, read back by kiln-erofs in a test.
- **The guest** (Task 3): `kiln-init` build mode. Stages 3–6 mount the base layers and scratch without an overlay; each step gets a fresh `nodev` overlay with a minimal read-only `/dev` (no block devices), runs in its own mount namespace pivoted into the overlay with Docker's default bounding set less `CAP_MKNOD`, as `uid:gid`, with secrets bind-mounted from a tmpfs; after exit 0 the upper is sealed onto the output disk and becomes the next lower.
- **The host model** (Task 4): `kiln_run::build` (all platforms): `BuildSpec`, `RunStep`, `Secret`, `Stream`, `Exit`, `StepOutcome`, the device budget, the `Config`/`RunStep` mapping, the lowers cap with its own error (`Error::TooManyLowers`), and `BuildProtocol`, the guest's message order.
- **The host VM** (Task 5): `kiln_run::BuildVm` (Linux): boot, bounded deadlines on every wait, per-step stdout/stderr connections, seals passed through unchecked but for shape and id.
- **Boot tests** (Task 6): two VMMs, a two-step build stacked and converted by kiln into an image that runs, plus the rulings' tests (no block devices, `mknod` in `/dev` and `/tmp`, `SOURCE_DATE_EPOCH`, secrets 0400, a version 2 guest, a hostile seal, a timeout).
- **Per-step egress** (Task 7, last code task): `RunStep.network` through vmkit's new runtime API `Vm::set_egress` (ruling 5), pinning kiln to vmkit's `feature/runtime-egress` merge rev.
- **Docs** (Task 8): `docs/format.md` (normative) and spec rev 2.16.

**Tech Stack:**
- Rust 2024, in the existing workspace. `kiln-init` gains `sha2` (already in the workspace's lock through kiln-erofs) and dev-depends on `kiln-erofs` (workspace). `kiln-run` dev-depends on `sha2`. No other new dependency.
- Tools (Tasks 3, 5–7 on Linux): as M3d: Firecracker 1.17, Cloud Hypervisor 53, vmkit's 6.18.54 kernel, a static busybox (Ubuntu's `busybox-static`, with `mknod`, `mount`, `stat`, `nc`), vmkit's `vmkit-sandbox` with its AppArmor profile. The mixed-version case needs a static `kiln-init` built from `m3d-services` (version 2), named by `KILN_TEST_INIT_V2`; the egress case needs `KILN_TEST_NET=1` (pasta, nft, the internet).

**Spec:** `docs/superpowers/specs/2026-09-30-kiln-design.md` (rev 2.15; rev 2.16, written in Task 8, records this plan's rulings). This plan implements milestone M3e (§12):
- §9.8 build mode: `Config.mode`, the layer and mount table, `RunStep`, `StepSealed`, secrets on a tmpfs, the seal (whiteouts, opaque directories, 4 KiB alignment, the sealed upper as the next lower), and what M3e was to design (session order, device order, step stdio, the step's exit message, the `RunStep` size bound);
- §9.8 versioning rules 1–4 for version 3;
- potter spec §9.3 (VmExecutor) and §8.4 (executor trust ordering: kiln's part is that a `vm`-class layer comes only from a VM whose steps cannot reach the output disk or another layer, ruling 1).

**Inputs** (binding): `.scratch-notes/m3e-design-brief.md` (rulings 1–7 of the brief), `.scratch-notes/m3e-rulings.md` (rulings 1–8 on the prototype and potter's binding answers), `.scratch-notes/m3e-findings.txt` (the API already promised to potter), `.scratch-notes/m3e-potter-requirements.txt` (potter's P4a contract, revised), and the controller's security ruling of 2026-11-20 (below, G-S).

It builds on `m3d-services` (4ab0388: kiln `main` b96a9a6 plus M3d, PR #18). M3e is stacked on that branch and retargeted to `main` once #18 merges; nothing in it depends on #18's review changes beyond what is on 4ab0388.

## Global Constraints

- **Repository:**
  - `/Users/alfonso/Github/Personal/playground/kiln` (GitHub `AlfonsoCampodonico/kiln`, private).
  - Work on a new branch `m3e-build-mode` created from `m3d-services` (4ab0388), in a new worktree; do not touch the `m3d-services` worktree. Retarget the PR to `main` once #18 merges (rebase onto `main`; the tasks touch nothing #18 changes after 4ab0388).
  - Do not push; the controller asks the user.
- **One PR:** Task 1 sets `PROTOCOL_VERSION` to 3, so a `kiln-init` built at Tasks 1 or 2 announces version 3 but treats a build-mode `Config` as a failure until Task 3. Merge Tasks 1–8 together.
- **Commits:** plain messages with no attribution lines (no `Co-Authored-By`, no session or tool footers).
- **Crates:** every crate root but `kiln-init`'s keeps `#![forbid(unsafe_code)]`. `kiln-init` gains two `unsafe` blocks, each with a SAFETY comment: `unshare_unsafe(NEWNS)` in the step's pre-exec hook, and the step's `pre_exec` itself. No `expect()`, `unwrap()` or panicking index on the host's build path; tests may. `kiln-run`'s Linux code stays `#[cfg(target_os = "linux")]`; `kiln_run::build`'s model compiles everywhere, so `cargo test --workspace` works on macOS.
- **Compatibility:** a run-mode `Config` serialises exactly as version 2's (`mode`, `disks`, `sourceDateEpoch` omitted), so a version 1 or 2 `kiln-init` keeps working for runs; `Config::required_version()` is 3 only in build mode. kiln-shim always sends run mode (Task 1 sets `mode: Run` in its `Config`), and a build-mode guest treats `ExecStart`, `ExecSignal`, `ExecWindowSize` and `FileUpdate` as protocol violations (Task 3). Build mode has no services: `Config::validate` refuses build mode with a process, a terminal, stdin, services, `readyTimeoutMs`, volumes or a pod disk, and `BuildSpec` has no services field.
- **Message numbering (R-A):** build mode's types are 17–20, after version 1's exec and file types 10–16 (the prototype used 10–13, which collide with them on this base).
- **G-S, step isolation (ruling 1 plus the security review of 2026-11-20):** a step, root or not, must never open the output disk or a layer disk.
  - (a) The step's capability bounding set is Docker's default set **less `CAP_MKNOD`** (there is no device cgroup in the guest to stop a node from being opened). The step therefore also has no `CAP_SYS_ADMIN` (no mounts), no `CAP_SYS_PTRACE` (no `/proc/1/root`), no `CAP_SYS_RAWIO`.
  - (b) The step's `/dev` is a tmpfs populated with `null`, `zero`, `full`, `random`, `urandom`, `tty`, the `ptmx`/`fd`/`stdin`/`stdout`/`stderr` links and the `pts`/`shm` directories, then made **read-only**. rustix 1.1.5 has no `mount_setattr`, so the superblock (private to that step) is reconfigured `ro` through `fspick` + `fsconfig_set_flag("ro")` + `fsconfig_reconfigure`, which is equivalent here; `devpts` and the `shm` tmpfs are mounts of their own over it.
  - (c) The step's overlay root is mounted `nodev`; the cache disks, `/dev/shm`, the secrets/`/etc` tmpfs are `nodev`; `/sys` is a fresh read-only sysfs. Checked: no base image needs device nodes outside `/dev` (the step's `/dev` is kiln's tmpfs, never the image's), and Docker-built images carry none elsewhere; a layer that does have one keeps it in its files but cannot open it in a build step (documented in `docs/format.md`).
  - (d) The step runs in its own mount namespace, `pivot_root(".", ".")` into the overlay with init's root detached (not chrooted, as the prototype was).
  - Pinned by `steps_cannot_reach_the_output_disk` (Task 6): a root step lists `/dev`, tries `mount -t devtmpfs`, `mknod b <major:minor of vdc>` in `/dev` and in `/tmp` and opening each, `/proc/1/root/dev/vdc` and writing `/sys`; every attempt fails.
  - Potter still reads and hashes each diff before the next `RunStep` (defence in depth, ruling 1).
- **Environment:**
  - macOS for every task's checks.
  - Linux checks (every task) in the Lima VM `vmkit` (Ubuntu 26.04 aarch64, nested KVM, Rust 1.99): `limactl shell vmkit`, `cd` to the repository, `export CARGO_TARGET_DIR=$HOME/target-m3e` (the AppArmor profile covers `/home/*/target-*/debug/vmkit-sandbox`). KVM runs need the `kvm` group (`sg kvm -c '…'`), Firecracker and Cloud Hypervisor on `PATH` (`$HOME/bin`), `busybox-static`, and the kernel `/Users/alfonso/Github/Personal/playground/vmkit/out/vmlinux-6.18.54-aarch64`.
  - The boot suite: `sg kvm -c 'KILN_TEST_NET=1 scripts/boot-tests.sh /Users/alfonso/Github/Personal/playground/vmkit/out/vmlinux-6.18.54-aarch64'` (Task 6 adds `--test build` to it). For the mixed-version case also export `KILN_TEST_INIT_V2=<static kiln-init built from m3d-services>` (build it with `cargo build --release --target aarch64-unknown-linux-musl -p kiln-init` in a worktree of `m3d-services`, its own `CARGO_TARGET_DIR`).
  - Cloud Hypervisor 53 under nested virtualization stalls guests (spec §14): judged by pass rate with serial reruns, Firecracker is the reference. Re-check Cloud Hypervisor (and "egress on", ruling 6) on x86_64 CI before merging.
- **Formatting:** `rustfmt.toml` sets `max_width = 120`. The plan's code is `cargo fmt`-clean. CI runs fmt, clippy (`-D warnings`) and the tests on ubuntu-24.04 and macos-15, and the boot job runs `scripts/boot-tests.sh` with `KILN_TEST_NET=1` on x86_64 KVM.
- **Validation** (this plan was generated from the task commits of a port of the prototype onto `m3d-services`: branch `m3e-port` in the plan writer's scratch clone `/Users/alfonso/Github/Personal/playground/.scratch-m3e-plan/kiln`; each task's code below is that commit's diff, so applying Tasks 1–8 in order reproduces it exactly, except the vmkit pin in Task 7, which the port points at the local `feature/runtime-egress` head 7a680a4 through a `file://` git URL):
  - Test counts are deltas from a fresh baseline on `m3d-services` (4ab0388), not absolute numbers.
  - macOS (Apple Silicon), after each task, fmt clean and the whole workspace's tests passing: 631 on the baseline, then 638, 644, 645, 652, 652, 652, 652, 652 after Tasks 1–8.
  - Linux (Lima `vmkit`): clippy `--workspace --all-targets -D warnings` clean after every task; the workspace's tests 770 on the baseline and 805 after Task 8 (the boot and `kiln run` cases skip themselves without their environment).
  - Firecracker 1.17 (nested, `KILN_TEST_NET=1`, `KILN_TEST_INIT_V2` set): on the final tree `tests/build.rs` 7/7 (egress through `Vm::set_egress`, the new helper), `tests/boot.rs` 30/30, `kiln`'s `tests/run.rs` 17/18: `egress_policy_and_the_resolver_without_a_network` fails its `--egress deny-all` `nslookup` probe identically on the baseline (`m3d-services`, old vmkit pin), so it is environmental (the Lima VM's resolver), not M3e. The fail-first runs of Task 6's new isolation case against the prototype's guest were not done; do them when executing (expected: `/dev` lists block devices and `mknod` in `/tmp` opens).
  - Cloud Hypervisor 53 (serial, one pass, final tree): `tests/build.rs` 5/7; `egress_follows_each_step` and `source_date_epoch_clamps_the_diff` failed (both need two VMs or the network; not rerun). Judge by Task 6's rule; re-check on x86_64 CI.
  - Cloud Hypervisor 53: not re-run for this plan (the nested stall; the prototype's pass rates are in the findings). Task 6 says how to judge it.
  - `kiln-init` and `hostile` build static for aarch64 (`statically linked`).

## Review Focus

Inputs and conditions no other test targets that will bite a real user, most likely first. Each has a pinned test in the task named.

1. **A hostile `RUN` as root trying to rewrite an earlier, cache-keyed diff or a base layer** (`mknod` a block node for `vdc`, mount `devtmpfs`, `/proc/1/root/dev/vdc`, writing sysfs). Expected: every attempt fails; the step can only change its own upper. Pinned in Task 6 (`steps_cannot_reach_the_output_disk`).
2. **potter's cache hit rate across hosts**: the same step on two hosts must give the same diff bytes when potter sets `source_date_epoch`, including `/root` copied up for a `/root/.cache` cache mount. Pinned in Task 2 (`source_date_epoch_clamps_later_times_only`) and Task 6 (`source_date_epoch_clamps_the_diff`).
3. **`apt-get install` of a package that `setcap`s a binary** (e.g. `iputils-ping`): the capability must survive into the layer; SELinux labels and overlay attributes must not. Pinned in Task 2 (`only_user_attributes_and_file_capabilities_are_kept`, and the `security.capability` round trip through kiln-erofs).
4. **A long monorepo chain** stacking many inputs layers in one VM: at 231 lowers the step is refused with `Error::TooManyLowers` before anything is sent, and the VM is still usable. Pinned in Task 4 (`a_step_past_the_lowers_cap_is_refused_with_its_own_error`) and Task 3 (`the_most_build_lowers_fit_in_a_page`).
5. **A hung `RUN` (a network fetch that never ends)**: `run_step` returns `Exit::TimedOut` within `timeout + STEP_GRACE + KILL_WAIT` and the VM is gone. Pinned in Task 6 (`a_step_past_its_timeout_is_killed`).
6. **A secret leaking into a layer**: never in any diff (raw byte scan), mode 0400 owned by the step's uid:gid. Pinned in Task 6 (`build_steps_seal_stack_and_convert`).
7. **Mixed installs**: potter pointing at a `kiln-init` from before M3e. Expected: the build VM fails to start, naming version 3. Pinned in Task 1 (`version_refusal` unit tests) and Task 6 (`a_version_2_guest_is_refused_build_mode`).

## Design decisions

Controller rulings (`.scratch-notes/m3e-rulings.md`) and where they land:
- **R1.** Steps never see block devices: a minimal `/dev` (null, zero, full, random, urandom, tty, ptmx/pts, shm, fd/stdin/stdout/stderr). Extended by **G-S** above (no `CAP_MKNOD`, read-only `/dev`, `nodev` everywhere, own mount namespace). Task 3; test in Task 6.
- **R2.** About 230 lowers per VM: `kiln_proto::MAX_BUILD_LOWERS = 230`, checked by the host (`Error::TooManyLowers`, distinct variant per potter, VM still usable) and by the guest (violation). The guest names sealed uppers by their lower index, so a long run of failed steps cannot lengthen the mount data. Tasks 1, 3, 4.
- **R3.** `BuildSpec.source_date_epoch: Option<u64>` → `Config.sourceDateEpoch`; the seal clamps every entry's mtime (copied-up parent directories included, per potter); unset keeps times. Tasks 1–4; test both ways in Task 6.
- **R4.** Secrets mode 0400, owned by the step's uid:gid. Task 3; test in Task 6.
- **R5.** Per-step egress through vmkit's runtime API `Vm::set_egress`, pinned to vmkit's `feature/runtime-egress` merge rev. Task 7 (last code task). The prototype's `nsenter` path is not shipped.
- **R6.** Re-check "egress on" on Firecracker (done in the port's runs, through `Vm::set_egress`) and on x86_64 CI (before merging).
- **R7.** Accepted deviations, documented in `docs/format.md` (Task 8) and for potter: per-step stdout/stderr connections on ports 1026/1027; `Exit::TimedOut` kills the VM; directories copied up to create mount points stay in the diff, as BuildKit's do.
- **R8.** The plan is written from `m3e-proto` rebased onto the merged base; T3's storage code is rebased onto M3b-2's (`storage::mount_all` keeps M3b-2's `finish`, and gains `mount_base(config, first_disk)`).
- Potter's binding answers: the lowers cap is a distinct error (R2); the clamp covers copied-up parents (R3); seals keep `security.capability` and `user.*` (as PAX `SCHILY.xattr.*`) and drop `trusted.overlay.*` (Task 2's allowlist also drops `user.overlay.*`, other `trusted.*`, other `security.*` and `system.*`); no step cancellation (the bounded deadline suffices); `Vm::set_egress` preferred.

Decisions made while porting and planning (not in the prototype):
- **P1. Message types 17–20** (R-A above).
- **P2. The xattr rule is an allowlist** (`diff::keep_xattr`): `user.*` except `user.overlay.*` (potter's namespace executor mounts its lowers with `userxattr`, where `user.overlay.opaque` would be read as an overlay marker), and `security.capability`. The prototype kept everything but `trusted.overlay.*`.
- **P3. Steps are pivoted, not chrooted** (G-S d), with the bounding set cut in the same pre-exec hook, before `setresuid`.
- **P4. `/sys` is a fresh read-only sysfs** in each step (Docker mounts it read-only for unprivileged containers); `/proc` stays a clone of init's (with no `CAP_SYS_PTRACE`, `/proc/1/root` and `/proc/1/fd` are closed to the step), but `/proc/sys`, `/proc/sysrq-trigger`, `/proc/irq` and `/proc/bus` are non-recursive read-only clones, and `/proc/kcore`, `/proc/keys`, `/proc/timer_list` and `/proc/sched_debug` are masked (`/dev/null` over a file, an empty read-only tmpfs over a directory): a uid-0 step could otherwise write `core_pattern` or `modprobe`, which the kernel runs as root in init's mount namespace, where the output disk is visible.
- **P5. The output disk is opened at stage 4** (init's own descriptor), and stage 4 no longer creates `/dev` links in init's devtmpfs: they live in each step's `/dev`.
- **P6. A build VM's network is IPv4 only** (potter spec §9.5): `NetSpec::default()` (vmkit's restricted egress, `ipv6: false`) and `Config.network.ipv6: None`, from `kiln_run::config::{GUEST, GATEWAY, PREFIX}` (the prototype duplicated them).
- **P7. The mixed-version boot case skips itself without `KILN_TEST_INIT_V2`**, as M3d's does without `KILN_TEST_INIT_V1`; `scripts/boot-tests.sh` does not build an old `kiln-init`.

From the prototype (findings), kept: the public API promised to potter (findings "Public API"), `FIXED_DEVICES = 4` + NIC with `network`, `max_devices(vmm, network)` returning the disk budget (potter must not subtract the fixed devices again), `Secret` redacting `Debug`, `Stream` + bytes sink, seal layout (packed, 4 KiB-aligned, `len` including zero padding, `sha256` over exactly the range, `entries` without PAX headers, fsync before `StepSealed`), host checks on `StepSealed` (shape and id only), `kill(-1)` for stragglers, mount-point stubs, `/etc` files only over a regular file or nothing, opaque markers only where a lower has the path, `RunStep` in one 64 KiB frame (~44 KiB of secrets).

**API delta for potter** against `m3e-findings.txt` (send with the merge rev):
- `BuildSpec` gains `source_date_epoch: Option<u64>`.
- `kiln_run::Error::TooManyLowers { step, lowers, max }`; `kiln_proto::MAX_BUILD_LOWERS = 230`; `BuildSpec::lowers(&stacked, sealed)` counts the way kiln does (root + stacked + sealed steps). After this error the VM is still usable; every other `run_step` error still means the VM is gone.
- `BuildSpec::proto_step` takes `sealed: usize` (internal to kiln; potter does not call it).
- Seals keep only `user.*` (not `user.overlay.*`) and `security.capability` xattrs.
- Steps lack `CAP_MKNOD` (Docker grants it): a `RUN mknod` fails with EPERM. Steps lack `CAP_SYS_ADMIN` as under Docker.

**Known gaps:**
- **Egress (Task 7)** depends on vmkit's `feature/runtime-egress` (signatures from `.scratch-notes/vmkit-set-egress-report.md`, validated against its unpushed head 7a680a4); kiln pins that branch's merge rev on vmkit `main`, a placeholder in Task 7 until it exists. Tasks 1–6 and 8 do not wait for it.
- **Lowers cap boot test:** the cap is unit-tested on both sides; no boot test runs 230 steps.
- **xattrs in a boot test:** busybox has no `setfattr`, so the allowlist and the PAX round trip are tested at unit level (Task 2), not through a real overlay.
- **kiln-shim:** never boots build mode (always `mode: Run`); a shim-driven build is out of scope.
- Cloud Hypervisor 53 under nested virtualization stalls guests (spec §14); the prototype's CH pass rates were low for the same reason (findings).
- `Exit::TimedOut` kills the VM even when the guest answered the SIGKILL in time (R7).

## File Structure

```
crates/kiln-proto/src/{build,config,message,lib}.rs   Mode, BuildDisk, Command, RunStep, StepExited, StepSealed, Secret,    Task 1
                                                      base64, MAX_BUILD_LOWERS, Config.mode/disks/sourceDateEpoch, v3
crates/kiln-proto/tests/props.rs, crates/kiln-run/src/{config,protocol}.rs, linux/session.rs, tests/{session,support}  Task 1
crates/kiln-shim/src/linux/worker.rs                  mode: Run                                                          Task 1
crates/kiln-init/src/linux/supervise.rs               build messages outside build mode are a violation                    Task 1
crates/kiln-init/src/{diff,lib}.rs, tests/diff.rs     the tar diff writer, keep_xattr, clamp_mtime                           Task 2
crates/kiln-init/src/linux/{build,seal}.rs            build mode, sealing                                                    Task 3
crates/kiln-init/src/linux/{storage,identity,process,mod}.rs, src/overlay.rs   mount_base, network(), step_mount_data       Task 3
crates/kiln-run/src/{build,error,lib}.rs              the host model, Error::TooManyLowers                                    Task 4
crates/kiln-run/src/linux/{build,mod,session}.rs      BuildVm                                                                Task 5 (+ 7)
crates/kiln-run/tests/{build.rs,support/mod.rs}, crates/kiln-testguest/src/main.rs, scripts/boot-tests.sh            Task 6 (+ 7)
Cargo.toml (vmkit pin)                                                                                                       Task 7
docs/format.md, docs/superpowers/specs/2026-09-30-kiln-design.md                                                             Task 8
```

Each step gives a new file's whole content, or a unified diff against the previous task's tree (apply with `git apply` or by hand; context lines are exact). Cargo updates `Cargo.lock` on the first build.

---

### Task 1: `kiln-proto`: protocol version 3 build mode: `Config.mode`, `disks` and `sourceDateEpoch`, `RunStep`, `Finish`, `StepExited`, `StepSealed`

**Files:**
- Create: `crates/kiln-proto/src/build.rs`
- Modify: `crates/kiln-proto/src/{config,lib,message}.rs`, `crates/kiln-proto/tests/props.rs`, `crates/kiln-init/src/linux/supervise.rs`, `crates/kiln-run/src/{config,protocol}.rs`, `crates/kiln-run/src/linux/session.rs`, `crates/kiln-run/tests/{session.rs,support/mod.rs}`, `crates/kiln-shim/src/linux/worker.rs`

**Interfaces:**
- Consumes: M3d's `Config::required_version`, `kiln_run::protocol::{Protocol, version_refusal}`, `PROTOCOL_VERSION` (2), version 1's exec/file messages (types 10–16).
- Produces:
  - `kiln_proto::{Mode::{Run, Build}, BuildDisk { kind, read_only }, DiskKind::{Erofs, Ext4}, Command::{Argv, Shell}, StepMount { disk, target, read_only }, Secret { target, data }, RunStep, StepExited { id, code, signal }, StepSealed { id, offset, len, sha256, entries }, base64}`; `MAX_STEP_MOUNTS` (64), `MAX_STEP_SECRETS` (64), `DIFF_ALIGN` (4096), `MAX_BUILD_LOWERS` (230).
  - `Config { mode, disks, source_date_epoch, .. }` (all three omitted in run mode); `Config::OUTPUT_DISK` (2), `Config::build_disk_index(i)`, `Config::build_disk(disk) -> Option<BuildDisk>`; `required_version()` 3 in build mode.
  - `HostMessage::{RunStep(Box<RunStep>), Finish}` (types 17, 18), `GuestMessage::{StepExited, StepSealed}` (19, 20). `PROTOCOL_VERSION = 3`.
  - `version_refusal(guest, &config)` names build mode ("build mode needs guest protocol version 3, and the guest's kiln-init speaks version 2: …").

The wire format of version 3 (§9.8 rule 1): only additions. A run-mode `Config` serialises exactly as version 2's. Decoding stays strict.
- `Config::validate`: run mode refuses `disks` and `sourceDateEpoch`; build mode refuses a process, a terminal, stdin, services, volumes and a pod disk (kiln-shim's fields and M3d's services are run mode's), and caps `layers + disks.len()` at `MAX_LAYERS` (128).
- `RunStep::validate`: a non-empty argv (or script), no NUL anywhere, `KEY=value` env, an absolute workdir, uid/gid not `u32::MAX`, `lowersAdded` unique, at most 64 mounts and 64 secrets, targets absolute, not `/`, without `.`, `..` or empty components, unique, `timeoutMs` ≥ 1. `Secret.data` is strict padded base64 on the wire and its `Debug` prints only the length. `StepExited` has exactly one of `code` (0–255) and `signal` (1–64). `StepSealed.sha256` is 64 lowercase hex digits; its other values are guest-asserted and not checked.
- The host side (§9.8 rules 2–3, M3d's pattern): the host accepts `Hello` 1 to 3 (4 is now the unsupported example, in `protocol.rs`'s unit tests and `tests/session.rs`'s `bad_frames_are_violations`); `version_refusal` refuses build mode to a guest below 3 before `Config`; run-mode sessions treat `StepExited`/`StepSealed` as violations ("outside build mode"); `kiln-init`'s run-mode supervisor treats `RunStep`/`Finish` as violations. kiln-shim's `Config` sets `mode: Run` explicitly.
- The type numbers are 17–20 (P1): types 10–16 are version 1's exec and file messages on this base.

- [ ] **Step 1: Modify `crates/kiln-init/src/linux/supervise.rs`**

Apply (unified diff against the previous task's tree):

```diff
--- a/crates/kiln-init/src/linux/supervise.rs
+++ b/crates/kiln-init/src/linux/supervise.rs
@@ -181,6 +181,9 @@ pub fn run(config: &Config, started: Started, events: &Receiver<Event>, volumes:
             Some(Event::Host(HostMessage::Config(_))) => {
                 return Err(Failure::msg("protocol violation: a second Config"));
             }
+            Some(Event::Host(HostMessage::RunStep(_) | HostMessage::Finish)) => {
+                return Err(Failure::msg("protocol violation: a build message outside build mode"));
+            }
             Some(Event::HostError(e)) => return Err(Failure::msg(format!("protocol violation: {e}"))),
             Some(Event::HostClosed) => return Err(Failure::msg("the host closed the control connection")),
         }
```

- [ ] **Step 2: Create `crates/kiln-proto/src/build.rs`**

```rust
//! Build mode (protocol version 3, spec §9.8): the messages a build VM's host and
//! guest exchange after `Config { mode: build }`.
//!
//! The host sends one [`RunStep`] at a time; the guest runs it, streams its stdout
//! and stderr on fresh connections to ports 1026 and 1027 (EOF ends the step's
//! output), and answers [`StepExited`]. After exit 0 it seals the step's changes
//! into an OCI tar diff on the output disk and answers [`StepSealed`]. `Signal`
//! goes to the running step's process group. [`Finish`](crate::HostMessage::Finish)
//! ends the VM.
//!
//! Secrets travel inline, base64 in the JSON: a `RunStep` frame is at most
//! [`MAX_FRAME`](crate::MAX_FRAME) bytes, so a step's secrets are at most about
//! 44 KiB together (`RunStep::validate` and the host refuse more).

use serde::{Deserialize, Serialize};

use crate::error::{ProtoError, Result};

/// The most disks a step may mount, and the most secrets it may have.
pub const MAX_STEP_MOUNTS: usize = 64;
pub const MAX_STEP_SECRETS: usize = 64;
/// The output disk's alignment: every diff starts at a multiple of it.
pub const DIFF_ALIGN: u64 = 4096;
/// The most lower directories a build VM's step overlay may have: the base
/// layers, the layers stacked so far and one sealed upper per successful step.
/// The guest names them all in one mount(2) data page; the host refuses a
/// `RunStep` that would need more (the caller squashes or boots a new VM).
pub const MAX_BUILD_LOWERS: usize = 230;

/// What `Config` asks the guest to do.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Run the image's process (versions 1 and 2).
    #[default]
    Run,
    /// Run build steps and seal each one's changes (version 3).
    Build,
}

impl Mode {
    pub fn is_run(&self) -> bool {
        *self == Mode::Run
    }
}

/// A build VM's disk after its base layers (`Config.disks`), in disk order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BuildDisk {
    pub kind: DiskKind,
    /// Attached read-only: a step may mount it only read-only.
    pub read_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiskKind {
    /// A layer a step stacks (`RunStep.lowersAdded`).
    Erofs,
    /// A cache disk a step mounts (`RunStep.mounts`).
    Ext4,
}

/// What a step runs: an argv, or a script for `/bin/sh -c`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum Command {
    Argv(Vec<String>),
    Shell(String),
}

impl Command {
    /// The command line kiln-init executes.
    pub fn argv(&self) -> Vec<&str> {
        match self {
            Command::Argv(a) => a.iter().map(String::as_str).collect(),
            Command::Shell(s) => vec!["/bin/sh", "-c", s],
        }
    }
}

/// A cache disk mounted for one step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StepMount {
    /// The disk's index (0 is the init layer, `vda`); an `ext4` entry of `Config.disks`.
    pub disk: u32,
    pub target: String,
    pub read_only: bool,
}

/// A file the step sees at `target`, on a tmpfs: never in the upper or the output.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Secret {
    pub target: String,
    #[serde(with = "base64")]
    pub data: Vec<u8>,
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Secret {{ target: {:?}, data: <{} bytes> }}",
            self.target,
            self.data.len()
        )
    }
}

/// Runs one build step (host to guest).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunStep {
    pub id: u32,
    pub command: Command,
    /// `KEY=value` entries; kiln-init adds `PATH`, `HOSTNAME` and `HOME` when unset.
    #[serde(default)]
    pub env: Vec<String>,
    /// Absolute; created (as root) when missing, then entered.
    pub workdir: String,
    pub uid: u32,
    pub gid: u32,
    /// Disk indexes of `erofs` entries of `Config.disks` to stack on the root
    /// before the step runs, bottom first; each at most once per VM.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lowers_added: Vec<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<StepMount>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<Secret>,
    /// Whether the step may reach the network; the host enforces it (informational for the guest).
    pub egress: bool,
    /// The host's deadline for the step and its seal (informational for the guest).
    pub timeout_ms: u32,
}

/// The step's command ended (guest to host), after its output reached EOF.
/// Exactly one of `code` (0–255) and `signal` (1–64).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepExited {
    pub id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
}

/// After `StepExited { code: 0 }`: the step's diff is on the output disk at
/// `[offset, offset + len)`, flushed, with `entries` tar entries (PAX headers not
/// counted). Guest-asserted: the host passes it through, and its reader checks it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepSealed {
    pub id: u32,
    pub offset: u64,
    pub len: u64,
    /// Lowercase hex.
    pub sha256: String,
    pub entries: u64,
}

fn invalid(name: &'static str, reason: impl Into<String>) -> ProtoError {
    ProtoError::invalid(name, reason)
}

/// An absolute path that is not `/` and holds no NUL.
fn check_target(what: &str, t: &str) -> std::result::Result<(), String> {
    if !t.starts_with('/') || t.contains('\0') || t.split('/').all(str::is_empty) {
        return Err(format!("{what} {t:?} is not an absolute path below /"));
    }
    Ok(())
}

impl RunStep {
    pub fn validate(&self) -> Result<()> {
        let v = |r: String| invalid("RunStep", r);
        match &self.command {
            Command::Argv(a) if a.first().is_none_or(String::is_empty) => {
                return Err(v("an empty argv".into()));
            }
            Command::Shell(s) if s.is_empty() => return Err(v("an empty shell script".into())),
            _ => {}
        }
        if self.command.argv().iter().any(|a| a.contains('\0')) {
            return Err(v("an argument contains a NUL byte".into()));
        }
        for e in &self.env {
            if e.contains('\0')
                || e.split_once('=')
                    .is_none_or(|(k, _)| k.is_empty() || k.chars().any(char::is_control))
            {
                return Err(v(format!("environment entry {e:?} is not KEY=value")));
            }
        }
        if !self.workdir.starts_with('/') || self.workdir.contains('\0') {
            return Err(v(format!("workdir {:?} is not absolute", self.workdir)));
        }
        if self.uid == u32::MAX || self.gid == u32::MAX {
            return Err(v("uid or gid 4294967295".into()));
        }
        let mut seen = std::collections::BTreeSet::new();
        if self.lowers_added.len() > crate::MAX_LAYERS as usize || !self.lowers_added.iter().all(|d| seen.insert(*d)) {
            return Err(v("lowersAdded repeats a disk or has too many".into()));
        }
        if self.mounts.len() > MAX_STEP_MOUNTS || self.secrets.len() > MAX_STEP_SECRETS {
            return Err(v(format!(
                "{} mounts and {} secrets (at most {MAX_STEP_MOUNTS} each)",
                self.mounts.len(),
                self.secrets.len()
            )));
        }
        let mut targets = std::collections::BTreeSet::new();
        let targets_of = self
            .mounts
            .iter()
            .map(|m| ("mount target", &m.target))
            .chain(self.secrets.iter().map(|s| ("secret target", &s.target)));
        for (what, t) in targets_of {
            check_target(what, t).map_err(v)?;
            if !targets.insert(t.trim_end_matches('/')) {
                return Err(v(format!("two mounts or secrets at {t:?}")));
            }
        }
        if self.timeout_ms == 0 {
            return Err(v("timeoutMs 0".into()));
        }
        Ok(())
    }
}

impl StepExited {
    pub fn validate(&self) -> Result<()> {
        match (self.code, self.signal) {
            (Some(c), None) if (0..=255).contains(&c) => Ok(()),
            (None, Some(s)) if (1..=64).contains(&s) => Ok(()),
            _ => Err(invalid(
                "StepExited",
                format!("code {:?} and signal {:?}", self.code, self.signal),
            )),
        }
    }
}

impl StepSealed {
    pub fn validate(&self) -> Result<()> {
        let hex = self.sha256.len() == 64
            && self
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !hex {
            return Err(invalid("StepSealed", "sha256 is not 64 lowercase hex digits"));
        }
        Ok(())
    }
}

/// Standard base64 with padding, decoded strictly (no whitespace, canonical padding).
pub mod base64 {
    use serde::{Deserialize, Deserializer, Serializer};

    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn encode(data: &[u8]) -> String {
        let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
        for chunk in data.chunks(3) {
            let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(ALPHABET[(n >> (18 - 6 * i)) as usize & 63] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    pub fn decode(text: &str) -> Option<Vec<u8>> {
        let s = text.as_bytes();
        if !s.len().is_multiple_of(4) {
            return None;
        }
        let value = |c: u8| ALPHABET.iter().position(|&a| a == c).map(|p| p as u32);
        let mut out = Vec::with_capacity(s.len() / 4 * 3);
        for (i, q) in s.chunks(4).enumerate() {
            let last = i == s.len() / 4 - 1;
            let pad = q.iter().rev().take_while(|&&c| c == b'=').count();
            if pad > 2 || (pad > 0 && !last) {
                return None;
            }
            let mut n = 0u32;
            for &c in &q[..4 - pad] {
                n = (n << 6) | value(c)?;
            }
            n <<= 6 * pad as u32;
            let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
            // Canonical: the bits padding drops must be zero.
            if bytes[3 - pad..].iter().any(|&b| b != 0) {
                return None;
            }
            out.extend_from_slice(&bytes[..3 - pad]);
        }
        Some(out)
    }

    pub fn serialize<S: Serializer>(data: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&encode(data))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(d)?;
        decode(&text).ok_or_else(|| serde::de::Error::custom("invalid base64"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn step() -> RunStep {
        RunStep {
            id: 1,
            command: Command::Shell("echo hi > /out".into()),
            env: vec!["A=1".into()],
            workdir: "/app".into(),
            uid: 0,
            gid: 0,
            lowers_added: vec![5],
            mounts: vec![StepMount {
                disk: 6,
                target: "/root/.cache".into(),
                read_only: false,
            }],
            secrets: vec![Secret {
                target: "/run/secrets/npmrc".into(),
                data: b"token=\x00\xff".to_vec(),
            }],
            egress: false,
            timeout_ms: 60_000,
        }
    }

    #[test]
    fn base64_round_trips_and_decodes_strictly() {
        for (raw, text) in [
            (&b""[..], ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
            (b"\x00\xff\xfe", "AP/+"),
        ] {
            assert_eq!(base64::encode(raw), text);
            assert_eq!(base64::decode(text).as_deref(), Some(raw));
        }
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(base64::decode(&base64::encode(&all)).unwrap(), all);
        for bad in [
            "Zg=", "Zg===", "Z===", "Zh==", "Zm9=v", "Zg==Zg==", " Zg==", "Zm9v\n", "Zm-v",
        ] {
            assert_eq!(base64::decode(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_step_serialises_in_camel_case_and_round_trips() {
        let s = step();
        s.validate().unwrap();
        let json = serde_json::to_string(&s).unwrap();
        for key in [
            r#""command":{"shell":"echo hi > /out"}"#,
            r#""lowersAdded":[5]"#,
            r#""mounts":[{"disk":6,"target":"/root/.cache","readOnly":false}]"#,
            r#""secrets":[{"target":"/run/secrets/npmrc","data":"dG9rZW49AP8="}]"#,
            r#""timeoutMs":60000"#,
        ] {
            assert!(json.contains(key), "{key} missing in {json}");
        }
        assert_eq!(serde_json::from_str::<RunStep>(&json).unwrap(), s);
        let argv = RunStep {
            command: Command::Argv(vec!["make".into(), "-j4".into()]),
            ..step()
        };
        assert!(
            serde_json::to_string(&argv)
                .unwrap()
                .contains(r#""command":{"argv":["make","-j4"]}"#)
        );
        assert_eq!(argv.command.argv(), ["make", "-j4"]);
        assert_eq!(step().command.argv(), ["/bin/sh", "-c", "echo hi > /out"]);
        assert!(!format!("{:?}", step()).contains("token"), "secrets are not printed");
    }

    #[test]
    fn invalid_steps_are_refused() {
        type Mutation = Box<dyn Fn(&mut RunStep)>;
        let cases: Vec<(&str, Mutation)> = vec![
            ("empty argv", Box::new(|s| s.command = Command::Argv(vec![]))),
            (
                "empty argv0",
                Box::new(|s| s.command = Command::Argv(vec![String::new()])),
            ),
            ("empty script", Box::new(|s| s.command = Command::Shell(String::new()))),
            ("NUL", Box::new(|s| s.command = Command::Shell("a\0".into()))),
            ("env", Box::new(|s| s.env = vec!["X".into()])),
            ("workdir", Box::new(|s| s.workdir = "app".into())),
            ("uid", Box::new(|s| s.uid = u32::MAX)),
            ("lowers twice", Box::new(|s| s.lowers_added = vec![5, 5])),
            ("root target", Box::new(|s| s.mounts[0].target = "/".into())),
            ("relative target", Box::new(|s| s.secrets[0].target = "run/x".into())),
            (
                "same target",
                Box::new(|s| s.secrets[0].target = "/root/.cache/".into()),
            ),
            ("timeout", Box::new(|s| s.timeout_ms = 0)),
        ];
        for (name, mutate) in cases {
            let mut s = step();
            mutate(&mut s);
            assert!(s.validate().is_err(), "{name} was accepted");
        }
    }

    #[test]
    fn step_exits_and_seals_are_range_checked() {
        let e = |code, signal| StepExited { id: 1, code, signal }.validate();
        assert!(e(Some(0), None).is_ok() && e(None, Some(9)).is_ok());
        assert!(e(None, None).is_err() && e(Some(0), Some(9)).is_err());
        assert!(e(Some(256), None).is_err() && e(None, Some(0)).is_err());
        let sealed = |sha: &str| {
            StepSealed {
                id: 1,
                offset: 0,
                len: 1024,
                sha256: sha.into(),
                entries: 1,
            }
            .validate()
        };
        assert!(sealed(&"ab".repeat(32)).is_ok());
        assert!(sealed(&"AB".repeat(32)).is_err());
        assert!(sealed("ab").is_err());
    }
}
```

- [ ] **Step 3: Modify `crates/kiln-proto/src/config.rs`**

Apply (unified diff against the previous task's tree):

```diff
--- a/crates/kiln-proto/src/config.rs
+++ b/crates/kiln-proto/src/config.rs
@@ -4,6 +4,7 @@ use std::net::{Ipv4Addr, Ipv6Addr};
 
 use serde::{Deserialize, Serialize};
 
+use crate::build::{BuildDisk, Mode};
 use crate::error::{ProtoError, Result};
 use crate::message::WindowSize;
 use crate::volume::{MAX_VOLUMES, Volume, VolumeKind};
@@ -68,6 +69,19 @@ pub struct Config {
     /// before the host's own boot timeout ends the VM.
     #[serde(default, skip_serializing_if = "Option::is_none")]
     pub ready_timeout_ms: Option<u32>,
+    /// Protocol version 3: `build` runs `RunStep`s instead of `process` (which
+    /// is then empty). Omitted when `run`.
+    #[serde(default, skip_serializing_if = "Mode::is_run")]
+    pub mode: Mode,
+    /// Protocol version 3, build mode only: the disks after the output disk
+    /// (`vdc`) and the `layers` base layers, in disk order.
+    #[serde(default, skip_serializing_if = "Vec::is_empty")]
+    pub disks: Vec<BuildDisk>,
+    /// Protocol version 3, build mode only: when set, a sealed diff's entries
+    /// have mtimes of at most this many seconds since the epoch (copied-up
+    /// parent directories included), for reproducible diffs.
+    #[serde(default, skip_serializing_if = "Option::is_none")]
+    pub source_date_epoch: Option<u64>,
 }
 
 /// A process kiln-init starts before the main process and stops after it
@@ -337,7 +351,32 @@ fn valid_hostname(h: &str) -> bool {
 impl Config {
     /// Checks every field; both sides call it (on encode and on decode).
     pub fn validate(&self) -> Result<()> {
-        self.process.validate("Config")?;
+        match self.mode {
+            Mode::Run if !self.disks.is_empty() || self.source_date_epoch.is_some() => {
+                return Err(invalid("disks or sourceDateEpoch outside build mode"));
+            }
+            Mode::Run => self.process.validate("Config")?,
+            Mode::Build => {
+                if self.process != Process::default()
+                    || self.tty.is_some()
+                    || self.interactive
+                    || !self.services.is_empty()
+                    || !self.volumes.is_empty()
+                    || self.pod_disk.is_some()
+                {
+                    return Err(invalid(
+                        "build mode with a process, a terminal, stdin, services, volumes or a pod disk",
+                    ));
+                }
+                if self.layers as usize + self.disks.len() > MAX_LAYERS as usize {
+                    return Err(invalid(format!(
+                        "{} layers and {} disks (at most {MAX_LAYERS} together)",
+                        self.layers,
+                        self.disks.len()
+                    )));
+                }
+            }
+        }
         if crate::signal::name(self.stop_signal).is_none() {
             return Err(invalid(format!(
                 "stopSignal {} (signals 1-31 only; real-time signals are not supported)",
@@ -387,10 +426,32 @@ impl Config {
     }
 
     /// The lowest protocol version that defines everything this `Config` uses
-    /// (spec §9.8 rule 3): 2 with services, else 1. Everything before version 2
-    /// counts as version 1.
+    /// (spec §9.8 rule 3): 3 in build mode, 2 with services, else 1. Everything
+    /// before version 2 counts as version 1.
     pub fn required_version(&self) -> u32 {
-        if self.services.is_empty() { 1 } else { 2 }
+        if self.mode == Mode::Build {
+            3
+        } else if self.services.is_empty() {
+            1
+        } else {
+            2
+        }
+    }
+
+    /// The disk index of the build mode output disk (`vdc`).
+    pub const OUTPUT_DISK: u32 = 2;
+
+    /// The disk index of `Config.disks[i]`, in build mode.
+    pub fn build_disk_index(&self, i: usize) -> u32 {
+        Self::OUTPUT_DISK + 1 + self.layers + i as u32
+    }
+
+    /// The `Config.disks` entry at disk index `disk`, in build mode.
+    pub fn build_disk(&self, disk: u32) -> Option<BuildDisk> {
+        let first = Self::OUTPUT_DISK + 1 + self.layers;
+        disk.checked_sub(first)
+            .and_then(|i| self.disks.get(i as usize))
+            .copied()
     }
 }
 
@@ -527,9 +588,86 @@ mod tests {
             pod_disk: None,
             services: Vec::new(),
             ready_timeout_ms: None,
+            mode: Mode::Run,
+            disks: Vec::new(),
+            source_date_epoch: None,
         }
     }
 
+    #[test]
+    fn build_mode_needs_version_3_and_no_process() {
+        use crate::build::DiskKind;
+        let mut c = Config {
+            process: Process::default(),
+            mode: Mode::Build,
+            disks: vec![
+                BuildDisk {
+                    kind: DiskKind::Erofs,
+                    read_only: true,
+                },
+                BuildDisk {
+                    kind: DiskKind::Ext4,
+                    read_only: false,
+                },
+            ],
+            ..config()
+        };
+        c.validate().unwrap();
+        assert_eq!(c.required_version(), 3);
+        let json = serde_json::to_string(&c).unwrap();
+        assert!(
+            json.contains(r#""mode":"build","disks":[{"kind":"erofs","readOnly":true},"#),
+            "{json}"
+        );
+        assert_eq!(serde_json::from_str::<Config>(&json).unwrap(), c);
+        assert_eq!(c.build_disk_index(0), 6);
+        assert_eq!(c.build_disk(7).unwrap().kind, DiskKind::Ext4);
+        assert_eq!(c.build_disk(5), None);
+        assert_eq!(c.build_disk(8), None);
+        assert!(!serde_json::to_string(&config()).unwrap().contains("mode"));
+        c.process = config().process;
+        assert!(c.validate().is_err());
+        let mut r = config();
+        r.disks = vec![BuildDisk {
+            kind: DiskKind::Ext4,
+            read_only: true,
+        }];
+        assert!(r.validate().is_err());
+        let r = Config {
+            source_date_epoch: Some(0),
+            ..config()
+        };
+        assert!(r.validate().is_err(), "sourceDateEpoch is build mode's");
+    }
+
+    /// kiln-shim's pod fields and services are run mode's: a build VM has none.
+    #[test]
+    fn build_mode_refuses_services_volumes_and_a_pod_disk() {
+        let build = Config {
+            process: Process::default(),
+            mode: Mode::Build,
+            source_date_epoch: Some(1_700_000_000),
+            ..config()
+        };
+        build.validate().unwrap();
+        let json = serde_json::to_string(&build).unwrap();
+        assert!(
+            json.ends_with(r#","mode":"build","sourceDateEpoch":1700000000}"#),
+            "{json}"
+        );
+        let pod = Config {
+            pod_disk: Some(Scratch { size_bytes: 1 << 30 }),
+            ..build.clone()
+        };
+        assert!(pod.validate().is_err());
+        let services = Config {
+            services: vec![fpm()],
+            ready_timeout_ms: Some(1000),
+            ..build
+        };
+        assert!(services.validate().is_err());
+    }
+
     fn fpm() -> Service {
         Service {
             name: "php-fpm".into(),
```

- [ ] **Step 4: Modify `crates/kiln-proto/src/lib.rs`**

Apply (unified diff against the previous task's tree):

```diff
--- a/crates/kiln-proto/src/lib.rs
+++ b/crates/kiln-proto/src/lib.rs
@@ -10,6 +10,7 @@
 //! The other ports carry raw stdio bytes, where EOF is a half-close.
 #![forbid(unsafe_code)]
 
+mod build;
 mod config;
 mod error;
 mod exec;
@@ -20,6 +21,10 @@ pub mod sanitize;
 pub mod signal;
 mod volume;
 
+pub use build::{
+    BuildDisk, Command, DIFF_ALIGN, DiskKind, MAX_BUILD_LOWERS, MAX_STEP_MOUNTS, MAX_STEP_SECRETS, Mode, RunStep,
+    Secret, StepExited, StepMount, StepSealed, base64,
+};
 pub use config::{
     Config, ExitMethod, Ipv6Network, MAX_LAYERS, MAX_SERVICE_NAME, MAX_SERVICES, MAX_SOCKET_PATH, MIN_SCRATCH_BYTES,
     Network, Process, Ready, Scratch, Service, valid_service_name, validate_services,
@@ -41,8 +46,10 @@ pub use volume::{
 
 /// The highest protocol version this crate implements: what a guest announces in
 /// [`Hello`], and the highest a host accepts (spec §9.8). Version 2 adds
-/// `Config.services`, `Config.readyTimeoutMs` and `Exited.service`.
-pub const PROTOCOL_VERSION: u32 = 2;
+/// `Config.services`, `Config.readyTimeoutMs` and `Exited.service`; version 3
+/// adds build mode (`Config.mode`, `Config.disks`, `RunStep`, `Finish`,
+/// `StepExited`, `StepSealed`).
+pub const PROTOCOL_VERSION: u32 = 3;
 /// The host's vsock CID; the guest connects to it.
 pub const HOST_CID: u32 = 2;
 /// The guest's vsock CID (spec §9.1).
```

- [ ] **Step 5: Modify `crates/kiln-proto/src/message.rs`**

Apply (unified diff against the previous task's tree):

```diff
--- a/crates/kiln-proto/src/message.rs
+++ b/crates/kiln-proto/src/message.rs
@@ -4,6 +4,7 @@
 use serde::de::DeserializeOwned;
 use serde::{Deserialize, Serialize};
 
+use crate::build::{RunStep, StepExited, StepSealed};
 use crate::config::Config;
 use crate::error::{ProtoError, Result};
 use crate::exec::{ExecExited, ExecFailed, ExecSignal, ExecStart, ExecStarted, ExecWindowSize, check_code, check_id};
@@ -135,6 +136,10 @@ pub enum GuestMessage {
     ExecStarted(ExecStarted),
     ExecExited(ExecExited),
     ExecFailed(ExecFailed),
+    /// Version 3: the build step's command ended.
+    StepExited(StepExited),
+    /// Version 3: the build step's diff is on the output disk.
+    StepSealed(StepSealed),
 }
 
 /// Messages from the host.
@@ -148,6 +153,10 @@ pub enum HostMessage {
     ExecSignal(ExecSignal),
     ExecWindowSize(ExecWindowSize),
     FileUpdate(FileUpdate),
+    /// Version 3: run a build step.
+    RunStep(Box<RunStep>),
+    /// Version 3: the build is over; sync and end the VM.
+    Finish,
 }
 
 const HELLO: u8 = 1;
@@ -166,6 +175,10 @@ const FILE_UPDATE: u8 = 13;
 const EXEC_STARTED: u8 = 14;
 const EXEC_EXITED: u8 = 15;
 const EXEC_FAILED: u8 = 16;
+const RUN_STEP: u8 = 17;
+const FINISH: u8 = 18;
+const STEP_EXITED: u8 = 19;
+const STEP_SEALED: u8 = 20;
 
 fn type_name(ty: u8) -> Option<&'static str> {
     Some(match ty {
@@ -185,6 +198,10 @@ fn type_name(ty: u8) -> Option<&'static str> {
         EXEC_STARTED => "ExecStarted",
         EXEC_EXITED => "ExecExited",
         EXEC_FAILED => "ExecFailed",
+        RUN_STEP => "RunStep",
+        FINISH => "Finish",
+        STEP_EXITED => "StepExited",
+        STEP_SEALED => "StepSealed",
         _ => return None,
     })
 }
@@ -272,6 +289,8 @@ impl Message for GuestMessage {
             GuestMessage::ExecStarted(_) => EXEC_STARTED,
             GuestMessage::ExecExited(_) => EXEC_EXITED,
             GuestMessage::ExecFailed(_) => EXEC_FAILED,
+            GuestMessage::StepExited(_) => STEP_EXITED,
+            GuestMessage::StepSealed(_) => STEP_SEALED,
         }
     }
 
@@ -304,6 +323,8 @@ impl Message for GuestMessage {
                 check_errno("InitFailed", f.errno)?;
                 check_text("InitFailed", &f.message)
             }
+            GuestMessage::StepExited(e) => e.validate(),
+            GuestMessage::StepSealed(s) => s.validate(),
         }
     }
 
@@ -317,6 +338,8 @@ impl Message for GuestMessage {
             GuestMessage::ExecStarted(e) => json(e),
             GuestMessage::ExecExited(e) => json(e),
             GuestMessage::ExecFailed(f) => json(f),
+            GuestMessage::StepExited(e) => json(e),
+            GuestMessage::StepSealed(s) => json(s),
         }
     }
 
@@ -333,6 +356,8 @@ impl Message for GuestMessage {
             EXEC_STARTED => GuestMessage::ExecStarted(parse(ty, payload)?),
             EXEC_EXITED => GuestMessage::ExecExited(parse(ty, payload)?),
             EXEC_FAILED => GuestMessage::ExecFailed(parse(ty, payload)?),
+            STEP_EXITED => GuestMessage::StepExited(parse(ty, payload)?),
+            STEP_SEALED => GuestMessage::StepSealed(parse(ty, payload)?),
             _ => return Err(wrong_direction(ty)),
         };
         msg.validate()?;
@@ -351,6 +376,8 @@ impl Message for HostMessage {
             HostMessage::ExecSignal(_) => EXEC_SIGNAL,
             HostMessage::ExecWindowSize(_) => EXEC_WINDOW_SIZE,
             HostMessage::FileUpdate(_) => FILE_UPDATE,
+            HostMessage::RunStep(_) => RUN_STEP,
+            HostMessage::Finish => FINISH,
         }
     }
 
@@ -360,7 +387,6 @@ impl Message for HostMessage {
             HostMessage::Signal(s) if crate::signal::name(s.sig).is_none() => {
                 Err(ProtoError::invalid("Signal", format!("signal {}", s.sig)))
             }
-            HostMessage::Signal(_) | HostMessage::Shutdown(_) | HostMessage::WindowSize(_) => Ok(()),
             HostMessage::ExecStart(e) => e.validate(),
             HostMessage::ExecSignal(s) => {
                 check_id("ExecSignal", s.id)?;
@@ -371,6 +397,10 @@ impl Message for HostMessage {
             }
             HostMessage::ExecWindowSize(w) => check_id("ExecWindowSize", w.id),
             HostMessage::FileUpdate(u) => u.validate(),
+            HostMessage::RunStep(s) => s.validate(),
+            HostMessage::Signal(_) | HostMessage::Shutdown(_) | HostMessage::WindowSize(_) | HostMessage::Finish => {
+                Ok(())
+            }
         }
     }
 
@@ -384,6 +414,8 @@ impl Message for HostMessage {
             HostMessage::ExecSignal(s) => json(s),
             HostMessage::ExecWindowSize(w) => json(w),
             HostMessage::FileUpdate(u) => json(u),
+            HostMessage::RunStep(s) => json(s),
+            HostMessage::Finish => json(&Empty {}),
         }
     }
 
@@ -397,6 +429,11 @@ impl Message for HostMessage {
             EXEC_SIGNAL => HostMessage::ExecSignal(parse(ty, payload)?),
             EXEC_WINDOW_SIZE => HostMessage::ExecWindowSize(parse(ty, payload)?),
             FILE_UPDATE => HostMessage::FileUpdate(parse(ty, payload)?),
+            RUN_STEP => HostMessage::RunStep(Box::new(parse(ty, payload)?)),
+            FINISH => {
+                parse::<Empty>(ty, payload)?;
+                HostMessage::Finish
+            }
             _ => return Err(wrong_direction(ty)),
         };
         msg.validate()?;
@@ -503,6 +540,29 @@ mod tests {
         );
     }
 
+    #[test]
+    fn build_messages_have_their_own_types_and_directions() {
+        let sealed = GuestMessage::StepSealed(StepSealed {
+            id: 2,
+            offset: 4096,
+            len: 2048,
+            sha256: "0".repeat(64),
+            entries: 3,
+        });
+        assert_eq!(sealed.message_type(), 20);
+        assert_eq!(GuestMessage::decode(20, &sealed.payload()).unwrap(), sealed);
+        let exited = GuestMessage::StepExited(StepExited {
+            id: 2,
+            code: None,
+            signal: Some(9),
+        });
+        assert_eq!(exited.payload(), br#"{"id":2,"signal":9}"#);
+        assert_eq!(HostMessage::decode(18, b"{}").unwrap(), HostMessage::Finish);
+        assert!(HostMessage::decode(19, br#"{"id":2,"code":0}"#).is_err());
+        assert!(GuestMessage::decode(17, b"{}").is_err());
+        assert!(GuestMessage::decode(19, br#"{"id":2}"#).is_err());
+    }
+
     #[test]
     fn init_failed_truncates_on_a_char_boundary() {
         let long = "é".repeat(MAX_MESSAGE_BYTES);
```

- [ ] **Step 6: Modify `crates/kiln-proto/tests/props.rs`**

Apply (unified diff against the previous task's tree):

```diff
--- a/crates/kiln-proto/tests/props.rs
+++ b/crates/kiln-proto/tests/props.rs
@@ -129,6 +129,9 @@ fn config() -> impl Strategy<Value = Config> {
                     volumes: Vec::new(),
                     pod_disk: None,
                     ready_timeout_ms: (!services.is_empty()).then_some(ready_ms),
+                    mode: kiln_proto::Mode::Run,
+                    disks: Vec::new(),
+                    source_date_epoch: None,
                     services,
                 }
             },
```

- [ ] **Step 7: Modify `crates/kiln-run/src/config.rs`**

Apply (unified diff against the previous task's tree):

```diff
--- a/crates/kiln-run/src/config.rs
+++ b/crates/kiln-run/src/config.rs
@@ -115,6 +115,9 @@ pub fn build(image: &ImageProcess, services: &[ImageService], opts: &RunOptions,
         // The session sets what remains of the boot timeout when it sends
         // `Config`; the largest value is what the size check must allow for.
         ready_timeout_ms: (!services.is_empty()).then_some(u32::MAX),
+        mode: kiln_proto::Mode::Run,
+        disks: Vec::new(),
+        source_date_epoch: None,
         services,
     };
     check_size(&config)?;
```

- [ ] **Step 8: Modify `crates/kiln-run/src/linux/session.rs`**

Apply (unified diff against the previous task's tree):

```diff
--- a/crates/kiln-run/src/linux/session.rs
+++ b/crates/kiln-run/src/linux/session.rs
@@ -789,6 +789,8 @@ impl Session {
                         self.expect_end(now, FINAL_MESSAGE);
                     }
                     GuestMessage::ExecStarted(_) | GuestMessage::ExecExited(_) | GuestMessage::ExecFailed(_) => {}
+                    // Refused by the protocol outside build mode.
+                    GuestMessage::StepExited(_) | GuestMessage::StepSealed(_) => {}
                 }
             }
             Event::Violation(why) => self.violate(why),
```

- [ ] **Step 9: Modify `crates/kiln-run/src/protocol.rs`**

Apply (unified diff against the previous task's tree):

```diff
--- a/crates/kiln-run/src/protocol.rs
+++ b/crates/kiln-run/src/protocol.rs
@@ -141,6 +141,9 @@ impl Protocol {
                 }
                 self.failed = Some(f.clone());
             }
+            GuestMessage::StepExited(_) | GuestMessage::StepSealed(_) => {
+                return v(format!("{} outside build mode", name(msg)));
+            }
         }
         Ok(Accepted::Recorded)
     }
@@ -191,8 +194,13 @@ impl Protocol {
 pub fn version_refusal(guest: u32, config: &Config) -> Option<String> {
     let needed = config.required_version();
     (guest < needed).then(|| {
+        let what = if needed >= 3 {
+            "build mode needs"
+        } else {
+            "the image's services need"
+        };
         format!(
-            "the image's services need guest protocol version {needed}, and the guest's kiln-init speaks \
+            "{what} guest protocol version {needed}, and the guest's kiln-init speaks \
              version {guest}: boot it with a kiln-init from this kiln release"
         )
     })
@@ -208,6 +216,8 @@ fn name(msg: &GuestMessage) -> &'static str {
         GuestMessage::ExecStarted(_) => "ExecStarted",
         GuestMessage::ExecExited(_) => "ExecExited",
         GuestMessage::ExecFailed(_) => "ExecFailed",
+        GuestMessage::StepExited(_) => "StepExited",
+        GuestMessage::StepSealed(_) => "StepSealed",
     }
 }
 
@@ -238,6 +248,9 @@ mod tests {
             volumes: Vec::new(),
             pod_disk: None,
             ready_timeout_ms: (!services.is_empty()).then_some(1000),
+            mode: kiln_proto::Mode::Run,
+            disks: Vec::new(),
+            source_date_epoch: None,
             services: services
                 .iter()
                 .map(|n| Service {
@@ -271,9 +284,9 @@ mod tests {
             assert_eq!(p.version(), v);
         }
         let mut p = Protocol::default();
-        let why = p.accept(&GuestMessage::Hello(Hello { protocol: 3 })).unwrap_err();
+        let why = p.accept(&GuestMessage::Hello(Hello { protocol: 4 })).unwrap_err();
         assert!(
-            why.0.contains("unsupported guest protocol 3 (this kiln speaks 1 to 2)"),
+            why.0.contains("unsupported guest protocol 4 (this kiln speaks 1 to 3)"),
             "{why}"
         );
         assert_eq!(version_refusal(1, &config(&[])), None);
@@ -449,8 +462,8 @@ mod tests {
             let (at, why) = run(msgs).unwrap_err();
             assert_eq!(at, msgs.len() - 1, "{msgs:?}: {why}");
         }
-        let (_, why) = run(&[GuestMessage::Hello(Hello { protocol: 3 })]).unwrap_err();
-        assert!(why.0.contains("unsupported guest protocol 3"), "{why}");
+        let (_, why) = run(&[GuestMessage::Hello(Hello { protocol: 4 })]).unwrap_err();
+        assert!(why.0.contains("unsupported guest protocol 4"), "{why}");
     }
 
     #[test]
```

- [ ] **Step 10: Modify `crates/kiln-run/tests/session.rs`**

Apply (unified diff against the previous task's tree):

```diff
--- a/crates/kiln-run/tests/session.rs
+++ b/crates/kiln-run/tests/session.rs
@@ -37,6 +37,9 @@ fn config() -> Config {
         pod_disk: None,
         services: Vec::new(),
         ready_timeout_ms: None,
+        mode: kiln_proto::Mode::Run,
+        disks: Vec::new(),
+        source_date_epoch: None,
     }
 }
 
@@ -856,14 +859,14 @@ fn bad_frames_are_violations() {
     }
     let vm = fake(|sock, killed| {
         let mut c = connect(&sock, port::CONTROL);
-        send(&mut c, GuestMessage::Hello(Hello { protocol: 3 }));
+        send(&mut c, GuestMessage::Hello(Hello { protocol: 4 }));
         wait_killed(&killed);
     });
     let (st, _) = streams(None);
     let o = Session::start(vm, config(), st, opts()).unwrap().finish_within(LIMIT);
     assert_eq!(
         o.violation.as_deref(),
-        Some("unsupported guest protocol 3 (this kiln speaks 1 to 2)")
+        Some("unsupported guest protocol 4 (this kiln speaks 1 to 3)")
     );
 }
```

- [ ] **Step 11: Modify `crates/kiln-run/tests/support/mod.rs`**

Apply (unified diff against the previous task's tree):

```diff
--- a/crates/kiln-run/tests/support/mod.rs
+++ b/crates/kiln-run/tests/support/mod.rs
@@ -314,6 +314,9 @@ impl Case {
             pod_disk: None,
             services: Vec::new(),
             ready_timeout_ms: None,
+            mode: kiln_proto::Mode::Run,
+            disks: Vec::new(),
+            source_date_epoch: None,
         }
     }
```

- [ ] **Step 12: Modify `crates/kiln-shim/src/linux/worker.rs`**

Apply (unified diff against the previous task's tree):

```diff
--- a/crates/kiln-shim/src/linux/worker.rs
+++ b/crates/kiln-shim/src/linux/worker.rs
@@ -267,6 +267,10 @@ fn prepare(plan: &WorkerPlan) -> Result<Prepared, Fail> {
         // kiln-shim does not support services (recorded known gap): a pod's Config is version 1.
         services: Vec::new(),
         ready_timeout_ms: None,
+        // A pod is always run mode: kiln-shim never boots a build VM (protocol version 3).
+        mode: kiln_proto::Mode::Run,
+        disks: Vec::new(),
+        source_date_epoch: None,
     };
     ctx(config.validate(), "the guest's configuration")?;
     ctx(kiln_run::config::check_size(&config), "the guest's configuration")?;
```

- [ ] **Step 13: Check (macOS)**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: clean; 7 more tests than the baseline pass (kiln-proto: `build.rs`'s `a_step_serialises_in_camel_case_and_round_trips`, `invalid_steps_are_refused`, `step_exits_and_seals_are_range_checked` and its base64 tests; `config.rs`'s `build_mode_needs_version_3_and_no_process` and `build_mode_refuses_services_volumes_and_a_pod_disk`; `message.rs`'s `build_messages_have_their_own_types_and_directions`). The fail-first check: `build_mode_refuses_services_volumes_and_a_pod_disk` fails if build mode's `validate` arm drops the `volumes`/`pod_disk` conditions.

- [ ] **Step 14: Check (Linux) and commit**

Run (Lima): `cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace` (kiln-shim's Linux `worker.rs` compiles only here).
Expected: clean, all pass.

```bash
git add -A && git commit -m "feat(kiln-proto): protocol version 3 build mode: Config.mode and disks, RunStep, Finish, StepExited, StepSealed"
```

---

### Task 2: `kiln-init`: the OCI tar diff writer, the xattr allowlist and the `SOURCE_DATE_EPOCH` clamp

**Files:**
- Create: `crates/kiln-init/src/diff.rs`, `crates/kiln-init/tests/diff.rs`
- Modify: `crates/kiln-init/Cargo.toml` (dev-dependency `kiln-erofs`), `crates/kiln-init/src/lib.rs`

**Interfaces:**
- Consumes: nothing new (pure, platform-independent).
- Produces: `kiln_init::diff::{TarWriter<W: Write>, Node { path, kind, mode, uid, gid, mtime, xattrs }, Kind::{Dir, File(u64), Symlink, HardLink, Char, Block, Fifo}, Written { len, entries }, join, whiteout, opaque, keep_xattr(&[u8]) -> bool, clamp_mtime(i64, Option<u64>) -> i64}`; `TarWriter::entry(&Node, Option<&mut dyn Read>)`, `TarWriter::finish(align) -> (W, Written)`.

The diff format potter ingests (§9.8, potter §9.3): ustar headers, PAX records for what ustar cannot hold (paths over 100 bytes, link targets, ids over 2097151, sizes and times over 11 octal digits, `SCHILY.xattr.*`), two end blocks, then zeros to `align`. `entries` counts logical entries, not PAX headers (potter assumption 5). Paths are relative, never absolute or with `..` (assumption 6).
- `keep_xattr` (potter's answer, P2): `user.*` except `user.overlay.*`, and `security.capability`; everything else dropped.
- `clamp_mtime` (R3): `min(mtime, epoch)` when an epoch is set, otherwise the time as is.
- The integration test writes a diff with whiteouts, an opaque marker, a hard link, 200- and 255-byte names, a 300-byte link target, ids over 2^21, `user.k` and a `security.capability` value, and reads it back through kiln-erofs (`LayerWriter::append_tar`, `Image`): hard links share an inode, the whiteout and opaque marker become overlay markers, both xattrs survive.

- [ ] **Step 1: Modify `crates/kiln-init/Cargo.toml`**

Apply (unified diff against the previous task's tree):

```diff
--- a/crates/kiln-init/Cargo.toml
+++ b/crates/kiln-init/Cargo.toml
@@ -17,4 +17,5 @@ rustix = { version = "1.1.5", features = ["fs", "mount", "net", "pipe", "process
 signal-hook = { version = "0.4.4", default-features = false, features = ["iterator"] }
 
 [dev-dependencies]
+kiln-erofs = { path = "../kiln-erofs" }
 tempfile = "3.27.0"
```

- [ ] **Step 2: Create `crates/kiln-init/src/diff.rs`**

```rust
//! The OCI tar diff a build step is sealed into (spec §9.8 build mode): ustar
//! headers, with PAX records for what ustar cannot hold (long paths and link
//! targets, large ids, sizes and times, extended attributes). The walker over the
//! overlay's upper directory (`linux::seal`) turns overlay whiteouts into
//! `.wh.<name>` and opaque directories into `.wh..wh..opq` before they get here.
//!
//! Paths are relative (no leading `/`, never `..`); directories end in `/`.
//! `entries` counts the logical entries, not the PAX headers. The archive ends
//! with two zero blocks, then zeros up to the alignment the output disk asks for,
//! so the range a reader is told about holds nothing after the tar's end.

use std::io::{self, Read, Write};

const BLOCK: usize = 512;

/// One entry's type and type-specific data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Dir,
    /// A regular file of this many bytes; its data follows the header.
    File(u64),
    Symlink(Vec<u8>),
    /// A hard link to an earlier entry of the same archive (its path).
    HardLink(Vec<u8>),
    Char(u32, u32),
    Block(u32, u32),
    Fifo,
}

/// One entry: everything but a file's data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// Relative, `/`-separated, without a trailing `/`.
    pub path: Vec<u8>,
    pub kind: Kind,
    /// Permission bits with setuid, setgid and sticky (`0o7777`).
    pub mode: u32,
    pub uid: u64,
    pub gid: u64,
    /// Seconds since the epoch.
    pub mtime: i64,
    /// Full names (`user.foo`, `security.capability`) and raw values.
    pub xattrs: Vec<(Vec<u8>, Vec<u8>)>,
}

/// Whether a sealed diff keeps the extended attribute `name`: `user.*` (but
/// `user.overlay.*`, which an overlay mounted with `userxattr`, as potter's
/// namespace executor mounts its lowers, would read as its own) and
/// `security.capability` (file capabilities, as `setcap` sets them). Everything
/// else is dropped: the overlay's `trusted.overlay.*`, other `trusted.*` and
/// `security.*` attributes (SELinux labels), and `system.*` (POSIX ACLs).
pub fn keep_xattr(name: &[u8]) -> bool {
    (name.starts_with(b"user.") && !name.starts_with(b"user.overlay.")) || name == b"security.capability"
}

/// The mtime a diff records: at most `epoch` when one is set (`SOURCE_DATE_EPOCH`).
pub fn clamp_mtime(mtime: i64, epoch: Option<u64>) -> i64 {
    match epoch {
        Some(e) => mtime.min(i64::try_from(e).unwrap_or(i64::MAX)),
        None => mtime,
    }
}

impl Node {
    /// An empty regular file (a whiteout or opaque marker).
    pub fn marker(path: Vec<u8>) -> Self {
        Node {
            path,
            kind: Kind::File(0),
            mode: 0o644,
            uid: 0,
            gid: 0,
            mtime: 0,
            xattrs: Vec::new(),
        }
    }
}

/// Writes a tar archive and counts its entries and bytes.
pub struct TarWriter<W: Write> {
    out: W,
    entries: u64,
    len: u64,
}

/// What a finished archive holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Written {
    pub entries: u64,
    /// Bytes written, alignment padding included.
    pub len: u64,
}

const MAX_7: u64 = 0o7777777;
const MAX_11: u64 = 0o77777777777;

/// A PAX record: `<len> <key>=<value>\n`, where `len` counts itself.
fn pax_record(key: &[u8], value: &[u8]) -> Vec<u8> {
    let body = key.len() + value.len() + 3;
    let mut n = body + 1;
    while (body + n.to_string().len()) != n {
        n = body + n.to_string().len();
    }
    let mut r = format!("{n} ").into_bytes();
    r.extend_from_slice(key);
    r.push(b'=');
    r.extend_from_slice(value);
    r.push(b'\n');
    r
}

fn octal(field: &mut [u8], v: u64) {
    let digits = field.len() - 1;
    let s = format!("{v:0digits$o}");
    field[..digits].copy_from_slice(&s.as_bytes()[s.len() - digits..]);
    field[digits] = 0;
}

impl<W: Write> TarWriter<W> {
    pub fn new(out: W) -> Self {
        Self {
            out,
            entries: 0,
            len: 0,
        }
    }

    fn put(&mut self, b: &[u8]) -> io::Result<()> {
        self.out.write_all(b)?;
        self.len += b.len() as u64;
        Ok(())
    }

    fn pad(&mut self, n: u64) -> io::Result<()> {
        let rem = (n % BLOCK as u64) as usize;
        if rem != 0 {
            self.put(&[0u8; BLOCK][..BLOCK - rem])?;
        }
        Ok(())
    }

    fn header(&mut self, name: &[u8], typeflag: u8, size: u64, node: &Node, link: &[u8]) -> io::Result<()> {
        let mut h = [0u8; BLOCK];
        let n = name.len().min(100);
        h[..n].copy_from_slice(&name[..n]);
        octal(&mut h[100..108], u64::from(node.mode & 0o7777));
        octal(&mut h[108..116], node.uid.min(MAX_7));
        octal(&mut h[116..124], node.gid.min(MAX_7));
        octal(&mut h[124..136], size.min(MAX_11));
        octal(&mut h[136..148], u64::try_from(node.mtime).unwrap_or(0).min(MAX_11));
        h[156] = typeflag;
        let l = link.len().min(100);
        h[157..157 + l].copy_from_slice(&link[..l]);
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        if let Kind::Char(ma, mi) | Kind::Block(ma, mi) = node.kind {
            octal(&mut h[329..337], u64::from(ma));
            octal(&mut h[337..345], u64::from(mi));
        }
        h[148..156].fill(b' ');
        let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
        let s = format!("{sum:06o}\0 ");
        h[148..156].copy_from_slice(s.as_bytes());
        self.put(&h)
    }

    /// Writes one entry; a regular file's data comes from `data` (exactly its size).
    pub fn entry(&mut self, node: &Node, data: Option<&mut dyn Read>) -> io::Result<()> {
        let mut name = node.path.clone();
        if node.kind == Kind::Dir {
            name.push(b'/');
        }
        let (typeflag, size, link): (u8, u64, &[u8]) = match &node.kind {
            Kind::Dir => (b'5', 0, b""),
            Kind::File(n) => (b'0', *n, b""),
            Kind::Symlink(t) => (b'2', 0, t),
            Kind::HardLink(t) => (b'1', 0, t),
            Kind::Char(..) => (b'3', 0, b""),
            Kind::Block(..) => (b'4', 0, b""),
            Kind::Fifo => (b'6', 0, b""),
        };
        let mut pax = Vec::new();
        if name.len() > 100 {
            pax.extend(pax_record(b"path", &name));
        }
        if link.len() > 100 {
            pax.extend(pax_record(b"linkpath", link));
        }
        for (key, v) in [(&b"uid"[..], node.uid), (b"gid", node.gid)] {
            if v > MAX_7 {
                pax.extend(pax_record(key, v.to_string().as_bytes()));
            }
        }
        if size > MAX_11 {
            pax.extend(pax_record(b"size", size.to_string().as_bytes()));
        }
        if node.mtime < 0 || node.mtime as u64 > MAX_11 {
            pax.extend(pax_record(b"mtime", node.mtime.to_string().as_bytes()));
        }
        for (k, v) in &node.xattrs {
            let mut key = b"SCHILY.xattr.".to_vec();
            key.extend_from_slice(k);
            pax.extend(pax_record(&key, v));
        }
        if !pax.is_empty() {
            let mut pname = b"PaxHeaders/".to_vec();
            pname.extend_from_slice(&name[name.len().saturating_sub(80)..]);
            let header = Node {
                mode: 0o644,
                uid: 0,
                gid: 0,
                mtime: 0,
                ..node.clone()
            };
            self.header(&pname, b'x', pax.len() as u64, &header, b"")?;
            self.put(&pax)?;
            self.pad(pax.len() as u64)?;
        }
        self.header(&name, typeflag, size, node, link)?;
        if let (Kind::File(n), Some(r)) = (&node.kind, data) {
            let copied = io::copy(&mut r.take(*n), &mut CountingWrite(self))?;
            if copied != *n {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!(
                        "{} changed size while it was sealed",
                        String::from_utf8_lossy(&node.path)
                    ),
                ));
            }
            self.pad(*n)?;
        } else if size != 0 {
            return Err(io::Error::other("a file entry without its data"));
        }
        self.entries += 1;
        Ok(())
    }

    /// Ends the archive (two zero blocks), then pads with zeros to a multiple of `align`.
    pub fn finish(mut self, align: u64) -> io::Result<(W, Written)> {
        self.put(&[0u8; 2 * BLOCK])?;
        let rem = self.len % align;
        if rem != 0 {
            let zeros = vec![0u8; (align - rem) as usize];
            self.put(&zeros)?;
        }
        self.out.flush()?;
        Ok((
            self.out,
            Written {
                entries: self.entries,
                len: self.len,
            },
        ))
    }
}

struct CountingWrite<'a, W: Write>(&'a mut TarWriter<W>);

impl<W: Write> Write for CountingWrite<'_, W> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        let n = self.0.out.write(b)?;
        self.0.len += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.out.flush()
    }
}

/// `<dir>/.wh.<name>`: the entry that deletes `name`.
pub fn whiteout(parent: &[u8], name: &[u8]) -> Vec<u8> {
    join(parent, &[b".wh.", name].concat())
}

/// `<dir>/.wh..wh..opq`: hides everything below `dir` in the layers beneath.
pub fn opaque(dir: &[u8]) -> Vec<u8> {
    join(dir, b".wh..wh..opq")
}

pub fn join(parent: &[u8], name: &[u8]) -> Vec<u8> {
    if parent.is_empty() {
        name.to_vec()
    } else {
        [parent, b"/", name].concat()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_user_attributes_and_file_capabilities_are_kept() {
        for keep in [&b"user.k"[..], b"user.mime_type", b"security.capability"] {
            assert!(keep_xattr(keep), "{}", String::from_utf8_lossy(keep));
        }
        for drop in [
            &b"trusted.overlay.opaque"[..],
            b"trusted.overlay.origin",
            b"trusted.x",
            b"user.overlay.opaque",
            b"security.selinux",
            b"security.ima",
            b"system.posix_acl_access",
            b"capability",
        ] {
            assert!(!keep_xattr(drop), "{}", String::from_utf8_lossy(drop));
        }
    }

    #[test]
    fn source_date_epoch_clamps_later_times_only() {
        assert_eq!(clamp_mtime(1_800_000_000, Some(1_700_000_000)), 1_700_000_000);
        assert_eq!(clamp_mtime(5, Some(1_700_000_000)), 5);
        assert_eq!(clamp_mtime(-1, Some(0)), -1);
        assert_eq!(clamp_mtime(1_800_000_000, None), 1_800_000_000);
        assert_eq!(clamp_mtime(7, Some(u64::MAX)), 7);
    }

    #[test]
    fn pax_records_count_their_own_length() {
        assert_eq!(pax_record(b"path", b"a"), b"9 path=a\n");
        let r = pax_record(b"path", &[b'x'; 95]);
        assert_eq!(r.len(), 105);
        assert!(r.starts_with(b"105 path="));
    }

    #[test]
    fn the_archive_is_block_aligned_and_padded_with_zeros() {
        let mut t = TarWriter::new(Vec::new());
        t.entry(&Node::marker(b"a".to_vec()), Some(&mut &b""[..])).unwrap();
        let mut data = &b"hello"[..];
        let file = Node {
            kind: Kind::File(5),
            ..Node::marker(b"b".to_vec())
        };
        t.entry(&file, Some(&mut data)).unwrap();
        let (bytes, w) = t.finish(4096).unwrap();
        assert_eq!(w, Written { entries: 2, len: 4096 });
        assert_eq!(bytes.len(), 4096);
        // a, b, b's data block, two end blocks; zeros after.
        assert!(bytes[5 * 512..].iter().all(|&b| b == 0));
        assert_eq!(&bytes[3 * 512..3 * 512 + 5], &[0; 5], "the end blocks follow the data");
        assert_eq!(&bytes[2 * 512..2 * 512 + 5], b"hello");
    }

    #[test]
    fn a_file_that_shrank_is_an_error() {
        let mut t = TarWriter::new(Vec::new());
        let file = Node {
            kind: Kind::File(10),
            ..Node::marker(b"f".to_vec())
        };
        assert!(t.entry(&file, Some(&mut &b"short"[..])).is_err());
    }
}
```

- [ ] **Step 3: Modify `crates/kiln-init/src/lib.rs`**

Apply (unified diff against the previous task's tree):

```diff
--- a/crates/kiln-init/src/lib.rs
+++ b/crates/kiln-init/src/lib.rs
@@ -8,6 +8,7 @@
 #![deny(unsafe_code)]
 
 pub mod atomic;
+pub mod diff;
 pub mod disks;
 pub mod env;
 pub mod error;
```

- [ ] **Step 4: Create `crates/kiln-init/tests/diff.rs`**

```rust
//! The sealed diff's format, read back by kiln's own converter (kiln-erofs), as
//! `kiln convert` and potter's ingest read it: whiteouts, opaque directories, hard
//! links, long names and link targets, large ids, extended attributes.

use std::collections::BTreeMap;
use std::io::Cursor;

use kiln_erofs::{Image, LayerWriter, Limits};
use kiln_init::diff::{Kind, Node, TarWriter, opaque, whiteout};

fn node(path: &str, kind: Kind) -> Node {
    Node {
        kind,
        mode: 0o755,
        ..Node::marker(path.as_bytes().to_vec())
    }
}

#[test]
fn a_diff_converts_with_its_whiteouts_links_and_long_names() {
    let long_dir = "d".repeat(200);
    let long_file = format!("{long_dir}/{}", "f".repeat(255));
    let long_target = format!("/{}", "t".repeat(300));
    let mut t = TarWriter::new(Vec::new());
    let mut entries = vec![
        (node("app", Kind::Dir), None),
        (node("app/a", Kind::File(5)), Some(b"hello".to_vec())),
        (node("app/hard", Kind::HardLink(b"app/a".to_vec())), None),
        (Node::marker(whiteout(b"app", b"gone")), Some(Vec::new())),
        (node("cache", Kind::Dir), None),
        (Node::marker(opaque(b"cache")), Some(Vec::new())),
        (node(&long_dir, Kind::Dir), None),
        (node(&long_file, Kind::File(3)), Some(b"abc".to_vec())),
        (node("link", Kind::Symlink(long_target.clone().into_bytes())), None),
    ];
    let mut big = node("big", Kind::File(0));
    big.uid = 5_000_000;
    big.gid = 3_000_000;
    big.xattrs = vec![
        (b"security.capability".to_vec(), CAP_NET_BIND_SERVICE.to_vec()),
        (b"user.k".to_vec(), b"v".to_vec()),
    ];
    entries.push((big, Some(Vec::new())));
    for (n, data) in &entries {
        let mut r: Option<&[u8]> = data.as_deref();
        t.entry(n, r.as_mut().map(|r| r as &mut dyn std::io::Read)).unwrap();
    }
    let (tar, written) = t.finish(4096).unwrap();
    assert_eq!(written.entries, entries.len() as u64);
    assert_eq!(written.len as usize, tar.len());
    assert_eq!(tar.len() % 4096, 0);

    let dir = tempfile::tempdir().unwrap();
    let mut w = LayerWriter::new(Cursor::new(Vec::new()), dir.path(), Limits::default()).unwrap();
    w.append_tar(tar.as_slice()).unwrap();
    let (out, _) = w.finish(&BTreeMap::new()).unwrap();
    let mut img = Image::open(Cursor::new(out.into_inner())).unwrap();
    let a = img.lookup(b"app/a").unwrap().unwrap();
    assert_eq!(img.read_data(a).unwrap(), b"hello");
    assert_eq!(
        img.lookup(b"app/hard").unwrap(),
        Some(a),
        "a hard link shares the inode"
    );
    let gone = img.lookup(b"app/gone").unwrap().unwrap();
    assert!(img.inode(gone).unwrap().is_whiteout());
    let cache = img.lookup(b"cache").unwrap().unwrap();
    let xattrs: Vec<Vec<u8>> = img.xattrs(cache).unwrap().into_keys().map(|k| k.full_name()).collect();
    assert!(xattrs.iter().any(|k| k.ends_with(b"overlay.opaque")), "{xattrs:?}");
    let f = img.lookup(long_file.as_bytes()).unwrap().unwrap();
    assert_eq!(img.read_data(f).unwrap(), b"abc");
    let l = img.lookup(b"link").unwrap().unwrap();
    assert_eq!(img.readlink(l).unwrap(), long_target.as_bytes());
    let b = img.lookup(b"big").unwrap().unwrap();
    let info = img.inode(b).unwrap();
    assert_eq!((info.uid, info.gid), (5_000_000, 3_000_000));
    let bx: std::collections::BTreeSet<(Vec<u8>, Vec<u8>)> = img
        .xattrs(b)
        .unwrap()
        .into_iter()
        .map(|(k, v)| (k.full_name(), v))
        .collect();
    assert_eq!(
        bx,
        [
            (b"security.capability".to_vec(), CAP_NET_BIND_SERVICE.to_vec()),
            (b"user.k".to_vec(), b"v".to_vec())
        ]
        .into(),
        "file capabilities (setcap) and user attributes survive"
    );
}

/// `setcap cap_net_bind_service=+ep`: a `vfs_cap_data` revision 2 (magic and
/// effective flag, then permitted and inheritable, low and high words).
const CAP_NET_BIND_SERVICE: [u8; 20] = [1, 0, 0, 2, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
```

- [ ] **Step 5: Check (macOS, Linux) and commit**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p kiln-init` (macOS, then Lima).
Expected: clean; 6 more tests than after Task 1 (5 unit tests in `diff.rs`, among them `only_user_attributes_and_file_capabilities_are_kept` and `source_date_epoch_clamps_later_times_only`, and `tests/diff.rs`).

```bash
git add -A && git commit -m "feat(kiln-init): the OCI tar diff writer for sealed build steps, read back by kiln-erofs"
```

---

### Task 3: `kiln-init`: build mode: a fresh overlay per step, an isolated step, secrets on a tmpfs, sealing onto the output disk

**Files:**
- Create: `crates/kiln-init/src/linux/build.rs`, `crates/kiln-init/src/linux/seal.rs`
- Modify: `crates/kiln-init/Cargo.toml` (`sha2`), `crates/kiln-init/src/linux/{mod,storage,identity,process}.rs`, `crates/kiln-init/src/overlay.rs`

**Interfaces:**
- Consumes: Task 1's build-mode `Config` and messages; Task 2's `diff`; M3b-2's `storage` (`mount_all`, `finish`, `grow`), `identity::configure`, `process::{watch_signals, connect, pipe, give}`, `relay::spawn`, `sys::{reap, reap_all, kill_all}`, `control::connect` (which on this base returns `(config, config_at, files)`).
- Produces:
  - `linux::build::run(&Config, Sender<Event>, &Receiver<Event>)`, entered from `boot()` right after stage 2 when `config.mode == Build` (volumes are refused in build mode, so `files` is empty and dropped).
  - `linux::seal::Output { open(path), seal(upper, lowers, epoch) -> Sealed { offset, written, sha256 } }`.
  - `storage::mount_base(config, first_disk) -> OwnedFd` (the `/kiln` tmpfs, base layers from disk `first_disk`, the grown scratch disk; no overlay) under `mount_all`, `storage::grow_to_device`, `storage::mkdir` (`pub(super)`); `identity::network(config)` (hostname, `lo`, `eth0`, without the `/etc` files); `overlay::step_mount_data(lowers, upper, work)`.

The guest side of §9.8 build mode, with rulings R1–R4, G-S and R8 (the port onto M3b-2's storage: `mount_all` = `mount_base(config, 2)` + the upper, work and overlay as before, so run mode is unchanged; `finish` is shared).
- **Boot**: stage 3 `mount_base(config, 3)` (base layers start after the output disk `vdc`) and `/kiln/rw/{u,w}`, `/kiln/disks`, `/kiln/root`; stage 4 opens the output disk (P5); stage 5 `identity::network` (the `/etc` files are bind-mounted per step instead, so they never reach a diff); stage 6 raises `RLIMIT_NOFILE` (one descriptor per directory level while sealing), then `Running`.
- **Loop**: `RunStep` runs a step; `Finish`, `Shutdown` or a signal end it (`storage::finish`, then the VM ends); `Signal`/`WindowSize` between steps are ignored; a second `Config`, an exec or file message (kiln-shim's), or a step past the checks below is a protocol violation (`InitFailed`).
- **Checks per step** (the host checked them; a mismatch is a violation): every `lowersAdded` disk is an `erofs` entry not stacked yet; every mount's disk is an `ext4` entry (read-only if the disk is); no target has `.` or `..`; `lowers + lowersAdded ≤ MAX_BUILD_LOWERS` (R2).
- **The step's overlay**: the lowers top first, a new upper and work directory named after the lower index the upper becomes (`/kiln/rw/u/<k>`, so names stay three digits however many steps fail), `xino=on,redirect_dir=off,index=off,metacopy=off`, one mount(2) call, `nodev` (G-S c). `the_most_build_lowers_fit_in_a_page` checks 230 lowers with the longest names fit one page.
- **The step's mounts**, through `openat2(RESOLVE_IN_ROOT | RESOLVE_NO_MAGICLINKS)` and the new mount API (`fsopen`/`fsmount`/`open_tree` + `move_mount`): `/proc` (clone of init's), `/sys` (fresh sysfs, read-only, P4), `/dev` (`minimal_dev`, G-S b: `null`, `zero`, `full`, `random`, `urandom`, `tty` made with `mknodat` + `chmodat` 0666, `pts`/`shm` directories, `ptmx` → `pts/ptmx`, `fd`/`stdin`/`stdout`/`stderr` links, then the superblock reconfigured read-only through `fspick`), `devpts` (`newinstance,ptmxmode=0666,mode=0620,gid=5`) on `/dev/pts`, a 1777 tmpfs on `/dev/shm`, the cache disks (ext4, `nodev,nosuid`, grown to the device once per VM when read-write), then one 4 MiB `nodev` tmpfs holding `/etc/hostname`, `/etc/hosts`, `/etc/resolv.conf` (0644, root; only where the image has a regular file or nothing, never through a symlink) and the secrets (0400, `uid:gid`, R4), each bind-mounted onto its target. Mount points that had to be created are stubs, removed (last first) after the step; a stub the step filled stays.
- **The step**: stdout and stderr are pipes relayed to fresh connections on ports 1026 and 1027 (EOF ends the output; R7). The child's pre-exec hook (allocation-free): `setsid`; `unshare(CLONE_NEWNS)`, `/` made `rprivate`; `chdir /kiln/root`, `pivot_root(".", ".")`, detach the old root, `chdir /` (G-S d); drop every capability outside `STEP_CAPS` from the bounding set (Docker's defaults less `MKNOD`, G-S a; `EINVAL` past the kernel's last capability is ignored); create and enter the workdir as root; `setgroups([gid])`, `setresgid`, `setresuid`. A failed hook step reports its name through a pipe: the step exits 125 with `kiln-init: <what>: <error>` on its stderr; a missing command 127, a non-executable 126 (as for the main process). `Signal` goes to the step's process group. When the step's process exits: `kill_all` (every process but init, so `setsid` escapes too), `reap_all`, drain the relays, `StepExited`.
- **Seal** (exit 0): detach the mounts and stubs, unmount the overlay (detach if busy), then `Output::seal`: walk the upper in byte order with descriptors (`NOFOLLOW`), char 0:0 → `.wh.<name>`, `trusted.overlay.opaque=y` → `.wh..wh..opq` only where some lower has the path, hard links by `(dev, ino)`, xattrs through `keep_xattr`, every mtime through `clamp_mtime(epoch)` (R3: copied-up parents included, since every entry goes through it), sockets skipped; positional writes hashed as they go, refused past the disk's end (`InitFailed`); `fsync`; `StepSealed`; the upper becomes the top lower. Any other exit: the upper is removed. The work directory is always removed.

- [ ] **Step 1: Modify `crates/kiln-init/Cargo.toml`**

Apply (unified diff against the previous task's tree):

```diff
--- a/crates/kiln-init/Cargo.toml
+++ b/crates/kiln-init/Cargo.toml
@@ -15,6 +15,8 @@ kiln-proto = { path = "../kiln-proto" }
 [target.'cfg(target_os = "linux")'.dependencies]
 rustix = { version = "1.1.5", features = ["fs", "mount", "net", "pipe", "process", "pty", "stdio", "system", "termios", "thread"] }
 signal-hook = { version = "0.4.4", default-features = false, features = ["iterator"] }
+# The seal hashes each diff (linux::seal).
+sha2 = "0.11.0"
 
 [dev-dependencies]
 kiln-erofs = { path = "../kiln-erofs" }
```

- [ ] **Step 2: Create `crates/kiln-init/src/linux/build.rs`**

```rust
//! Build mode (protocol version 3, spec §9.8): kiln-init runs the host's
//! `RunStep`s one at a time and seals each successful step's changes.
//!
//! Disks: `vda` init, `vdb` scratch, `vdc` the output disk, then `Config.layers`
//! base layers (bottom first), then `Config.disks` (erofs layers a step may stack,
//! ext4 cache disks a step may mount). Nothing is hot-plugged.
//!
//! Init never pivots: it keeps its own root, and each step gets a fresh overlay at
//! `/kiln/root` (the lowers so far, a new upper and work directory on scratch;
//! `nodev`), with `/proc`, a read-only `/sys`, a minimal `/dev` (read-only, no block
//! devices: `null`, `zero`, `full`, `random`, `urandom`, `tty`, `ptmx` and the
//! `fd`/`stdin`/`stdout`/`stderr` links), `/dev/pts`, `/dev/shm`, its cache
//! disks, the `/etc` files and its secrets mounted in it. Every mount point that had to be
//! created is a "stub", removed again after the step, so none reaches its diff;
//! secrets and the `/etc` files live on a detached tmpfs and are bind-mounted
//! onto their targets, so they are never written to the upper. Paths inside the
//! root are resolved with `RESOLVE_IN_ROOT`, so the image's symlinks cannot lead
//! a mount out of it. The step runs in its own mount namespace, pivoted into the
//! overlay with init's root detached, with Docker's default capability bounding
//! set less `CAP_MKNOD` (no `CAP_SYS_ADMIN`, so it cannot mount; no
//! `CAP_MKNOD`, so it cannot make a device node; no `CAP_SYS_PTRACE`, so it cannot
//! reach init's root through `/proc/1/root`), as its uid and gid, in its own
//! session and process group, in its working directory (created as root when
//! missing; that is part of the step's diff, as Docker's WORKDIR). So a step,
//! root or not, cannot open the output disk or a layer disk (ruling 1).
//!
//! When the step's process exits, every process but init is killed (stragglers,
//! and anything that left the group), its output is drained, and `StepExited`
//! is sent. Then the step's mounts are detached, the stubs removed and the overlay
//! unmounted. After exit 0 the upper is sealed onto the output disk
//! ([`super::seal`]), `StepSealed` is sent, and the upper becomes the topmost
//! lower; otherwise it is discarded and the build continues from the state before
//! the step (the host decides whether it does).

use std::collections::BTreeSet;
use std::ffi::CString;
use std::fs::File;
use std::io::Write;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, Sender};

use kiln_proto::{
    Config, DiskKind, GuestMessage, HostMessage, MAX_BUILD_LOWERS, RunStep, StepExited, StepSealed, port,
};
use rustix::fs::{AtFlags, FileType, Mode, OFlags, ResolveFlags};
use rustix::io::Errno;
use rustix::mount::{
    FsMountFlags, FsOpenFlags, FsPickFlags, MountAttrFlags, MountFlags, MoveMountFlags, OpenTreeFlags, UnmountFlags,
};
use rustix::pipe::PipeFlags;
use rustix::process::{Gid, Pid, Uid};
use rustix::thread::CapabilitySet;
use signal_hook::consts::SIGCHLD;

use super::process::{connect, give, pipe, watch_signals};
use super::seal::Output;
use super::storage::{self, mkdir};
use super::{Event, enter, host_ok, identity, relay, send, sys};
use crate::disks::disk_name;
use crate::error::{Context, Failure, Result};
use crate::passwd::Identity;

const ROOT: &str = "/kiln/root";

/// Stages 3 to 6, `Running`, then steps until `Finish`.
pub fn run(config: &Config, events: Sender<Event>, rx: &Receiver<Event>) -> Result<()> {
    enter(3)?;
    let rw = storage::mount_base(config, Config::OUTPUT_DISK as usize + 1)?;
    for dir in ["/kiln/rw/u", "/kiln/rw/w", "/kiln/disks", ROOT] {
        mkdir(dir)?;
    }
    enter(4)?;
    // Init's own descriptor: no step ever sees a block device.
    let output = Output::open(&format!("/dev/{}", disk_name(Config::OUTPUT_DISK as usize)))?;
    enter(5)?;
    identity::network(config)?;
    enter(6)?;
    watch_signals(events)?;
    // One descriptor per directory level while sealing.
    if let Some(max) = rustix::process::getrlimit(rustix::process::Resource::Nofile).maximum {
        let _ = rustix::process::setrlimit(
            rustix::process::Resource::Nofile,
            rustix::process::Rlimit {
                current: Some(max),
                maximum: Some(max),
            },
        );
    }
    host_ok()?;
    send(&GuestMessage::Running)?;
    let mut b = Builder {
        config,
        lowers: (0..config.layers as usize).map(crate::overlay::layer_dir).collect(),
        stacked: BTreeSet::new(),
        grown: BTreeSet::new(),
        output,
        epoch: config.source_date_epoch,
    };
    loop {
        match rx.recv().expect("the signal thread holds a sender") {
            Event::Host(HostMessage::RunStep(step)) => b.step(&step, rx)?,
            Event::Host(HostMessage::Finish) => break,
            // Nothing runs between steps.
            Event::Host(HostMessage::Signal(_) | HostMessage::WindowSize(_)) => {}
            Event::Host(HostMessage::Shutdown(_)) => break,
            Event::Host(HostMessage::Config(_)) => return Err(Failure::msg("protocol violation: a second Config")),
            // kiln-shim's messages: a build VM has no execs and no volumes.
            Event::Host(
                HostMessage::ExecStart(_)
                | HostMessage::ExecSignal(_)
                | HostMessage::ExecWindowSize(_)
                | HostMessage::FileUpdate(_),
            ) => {
                return Err(Failure::msg(
                    "protocol violation: an exec or file message in build mode",
                ));
            }
            Event::Signal(SIGCHLD) => {
                sys::reap();
            }
            Event::Signal(_) => break,
            Event::HostError(e) => return Err(Failure::msg(format!("protocol violation: {e}"))),
            Event::HostClosed => return Err(Failure::msg("the host closed the control connection")),
        }
    }
    storage::finish(&rw);
    Ok(())
}

struct Builder<'a> {
    config: &'a Config,
    /// The root's lower directories, bottom first: base layers, stacked layers
    /// and sealed uppers.
    lowers: Vec<String>,
    /// Disks stacked so far (each at most once).
    stacked: BTreeSet<u32>,
    /// Cache disks grown to their device's size.
    grown: BTreeSet<u32>,
    output: Output,
    /// `Config.sourceDateEpoch`: the latest mtime a sealed entry may have.
    epoch: Option<u64>,
}

impl Builder<'_> {
    fn step(&mut self, step: &RunStep, rx: &Receiver<Event>) -> Result<()> {
        self.check(step)?;
        for &disk in &step.lowers_added {
            let dir = format!("/kiln/disks/{disk}");
            mkdir(&dir)?;
            let dev = format!("/dev/{}", disk_name(disk as usize));
            rustix::mount::mount(dev.as_str(), dir.as_str(), "erofs", MountFlags::RDONLY, None)
                .context(format!("mount layer {dev}"))?;
            self.stacked.insert(disk);
            self.lowers.push(dir);
        }
        // Named after the lower it becomes once sealed: a discarded upper's
        // name is used again, so every name in the mount data stays short.
        let k = self.lowers.len();
        let (upper, work) = (format!("/kiln/rw/u/{k}"), format!("/kiln/rw/w/{k}"));
        mkdir(&upper)?;
        mkdir(&work)?;
        mkdir(crate::overlay::EMPTY_LOWER)?;
        let data = crate::overlay::step_mount_data(&self.lowers, &upper, &work)?;
        let data = CString::new(data).expect("no NUL in paths");
        // `nodev`, defence in depth: a step has no CAP_MKNOD, and a device node a
        // lower layer carries cannot be opened in the guest either.
        rustix::mount::mount("overlay", ROOT, "overlay", MountFlags::NODEV, data.as_c_str())
            .context("mount the step's overlay")?;

        let mut mounts = Mounts::new()?;
        let ran = self
            .mount_all(step, &mut mounts)
            .and_then(|()| run_command(self.config, step, rx));
        mounts.teardown();
        match rustix::mount::unmount(ROOT, UnmountFlags::empty()) {
            Ok(()) => {}
            Err(e) => {
                eprintln!("kiln-init: unmount the step's overlay: {e}; detaching it");
                rustix::mount::unmount(ROOT, UnmountFlags::DETACH).context("detach the step's overlay")?;
            }
        }
        let code = ran?;
        if code == Some(0) {
            let sealed = self.output.seal(&upper, &self.lowers, self.epoch)?;
            send(&GuestMessage::StepSealed(StepSealed {
                id: step.id,
                offset: sealed.offset,
                len: sealed.written.len,
                sha256: sealed.sha256,
                entries: sealed.written.entries,
            }))?;
            self.lowers.push(upper);
        } else {
            std::fs::remove_dir_all(&upper).context(format!("discard {upper}"))?;
        }
        std::fs::remove_dir_all(&work).context(format!("remove {work}"))?;
        Ok(())
    }

    /// The host checked the step against the VM's disks; a step that does not
    /// match them is a protocol violation.
    fn check(&self, step: &RunStep) -> Result<()> {
        let bad = |why: String| Err(Failure::msg(format!("protocol violation: RunStep {}: {why}", step.id)));
        if self.lowers.len() + step.lowers_added.len() > MAX_BUILD_LOWERS {
            return bad(format!(
                "{} lower layers and {} more (at most {MAX_BUILD_LOWERS})",
                self.lowers.len(),
                step.lowers_added.len()
            ));
        }
        for &d in &step.lowers_added {
            match self.config.build_disk(d) {
                Some(disk) if disk.kind == DiskKind::Erofs && !self.stacked.contains(&d) => {}
                _ => return bad(format!("disk {d} is not an erofs disk left to stack")),
            }
        }
        for m in &step.mounts {
            match self.config.build_disk(m.disk) {
                Some(disk) if disk.kind == DiskKind::Ext4 && (m.read_only || !disk.read_only) => {}
                _ => return bad(format!("disk {} is not a cache disk it may mount so", m.disk)),
            }
        }
        for t in step
            .mounts
            .iter()
            .map(|m| &m.target)
            .chain(step.secrets.iter().map(|s| &s.target))
        {
            if t.split('/').any(|c| c == "." || c == "..") {
                return bad(format!("target {t:?} has . or .. in it"));
            }
        }
        Ok(())
    }

    /// The API filesystems, the `/etc` files, the cache disks and the secrets.
    fn mount_all(&mut self, step: &RunStep, m: &mut Mounts) -> Result<()> {
        m.attach(clone_tree("/proc")?, "/proc", true)?;
        m.attach(new_fs("sysfs", None, &[], &[], true)?, "/sys", true)?;
        m.attach(minimal_dev()?, "/dev", true)?;
        m.attach(
            new_fs(
                "devpts",
                None,
                &[("ptmxmode", "0666"), ("mode", "0620"), ("gid", "5")],
                &["newinstance"],
                false,
            )?,
            "/dev/pts",
            true,
        )?;
        m.attach(
            new_fs("tmpfs", None, &[("mode", "1777")], &[], false)?,
            "/dev/shm",
            true,
        )?;

        for mount in &step.mounts {
            let dev = format!("/dev/{}", disk_name(mount.disk as usize));
            let fs = new_fs("ext4", Some(&dev), &[], &[], mount.read_only)?;
            if !mount.read_only && self.grown.insert(mount.disk) {
                let root = rustix::fs::openat(
                    &fs,
                    c".",
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .context(format!("open the cache disk {dev}"))?;
                storage::grow_to_device(&root, &dev)?;
            }
            m.attach(fs, &mount.target, true)?;
        }

        // The `/etc` files and the secrets: one tmpfs, never part of the upper.
        let net = self.config.network.as_ref();
        let hostname = &self.config.hostname;
        let mut files: Vec<(String, Vec<u8>, u32, u32, u32)> = vec![
            (
                "/etc/hostname".into(),
                crate::etc::hostname(hostname).into_bytes(),
                0,
                0,
                0o644,
            ),
            (
                "/etc/hosts".into(),
                crate::etc::hosts(hostname, net).into_bytes(),
                0,
                0,
                0o644,
            ),
            (
                "/etc/resolv.conf".into(),
                crate::etc::resolv_conf(net).into_bytes(),
                0,
                0,
                0o644,
            ),
        ];
        let etc = files.len();
        for s in &step.secrets {
            files.push((s.target.clone(), s.data.clone(), step.uid, step.gid, 0o400));
        }
        let tmp = new_fs("tmpfs", None, &[("mode", "0700"), ("size", "4m")], &[], false)?;
        for (i, (target, data, uid, gid, mode)) in files.iter().enumerate() {
            // The image's own /etc file stays when it is a symlink (or under one).
            if i < etc && !m.plain(target)? {
                continue;
            }
            let name = format!("f{i}");
            let fd = rustix::fs::openat(
                &tmp,
                name.as_str(),
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
                Mode::from_raw_mode(*mode),
            )
            .context("create a file on the step's tmpfs")?;
            rustix::fs::fchown(&fd, Some(Uid::from_raw(*uid)), Some(Gid::from_raw(*gid))).context("chown a secret")?;
            File::from(fd).write_all(data).context("write a secret")?;
            let tree = rustix::mount::open_tree(
                &tmp,
                name.as_str(),
                OpenTreeFlags::OPEN_TREE_CLONE | OpenTreeFlags::OPEN_TREE_CLOEXEC,
            )
            .context("bind a file of the step's tmpfs")?;
            m.attach(tree, target, false)?;
        }
        Ok(())
    }
}

/// A detached copy of init's mount at `path`.
fn clone_tree(path: &str) -> Result<OwnedFd> {
    rustix::mount::open_tree(
        rustix::fs::CWD,
        path,
        OpenTreeFlags::OPEN_TREE_CLONE | OpenTreeFlags::OPEN_TREE_CLOEXEC,
    )
    .context(format!("clone the mount of {path}"))
}

/// A new detached filesystem.
fn new_fs(fstype: &str, source: Option<&str>, opts: &[(&str, &str)], flags: &[&str], ro: bool) -> Result<OwnedFd> {
    let what = format!("mount {fstype}{}", source.map(|s| format!(" {s}")).unwrap_or_default());
    let fs = rustix::mount::fsopen(fstype, FsOpenFlags::FSOPEN_CLOEXEC).context(&what)?;
    if let Some(s) = source {
        rustix::mount::fsconfig_set_string(&fs, "source", s).context(&what)?;
    }
    for (k, v) in opts {
        rustix::mount::fsconfig_set_string(&fs, *k, *v).context(format!("{what}: {k}={v}"))?;
    }
    for f in flags.iter().copied().chain(ro.then_some("ro")) {
        rustix::mount::fsconfig_set_flag(&fs, f).context(format!("{what}: {f}"))?;
    }
    rustix::mount::fsconfig_create(&fs).context(&what)?;
    let mut attrs = MountAttrFlags::MOUNT_ATTR_NOSUID | MountAttrFlags::MOUNT_ATTR_NODEV;
    if ro {
        attrs |= MountAttrFlags::MOUNT_ATTR_RDONLY;
    }
    if fstype == "devpts" {
        attrs = MountAttrFlags::MOUNT_ATTR_NOSUID | MountAttrFlags::MOUNT_ATTR_NOEXEC;
    }
    rustix::mount::fsmount(&fs, FsMountFlags::FSMOUNT_CLOEXEC, attrs).context(&what)
}

/// The character devices a step's `/dev` holds (ruling 1): no block devices,
/// so not the output disk or a layer disk, whatever the step's uid.
const DEVICES: [(&str, u32, u32); 6] = [
    ("null", 1, 3),
    ("zero", 1, 5),
    ("full", 1, 7),
    ("random", 1, 8),
    ("urandom", 1, 9),
    ("tty", 5, 0),
];

/// A step's `/dev`: a small tmpfs with [`DEVICES`], `pts` and `shm` (mounted
/// over next), `ptmx` and the `fd`, `stdin`, `stdout` and `stderr` links.
fn minimal_dev() -> Result<OwnedFd> {
    let what = "mount the step's /dev";
    let fs = rustix::mount::fsopen("tmpfs", FsOpenFlags::FSOPEN_CLOEXEC).context(what)?;
    for (k, v) in [("mode", "0755"), ("size", "64k"), ("nr_inodes", "64")] {
        rustix::mount::fsconfig_set_string(&fs, k, v).context(format!("{what}: {k}={v}"))?;
    }
    rustix::mount::fsconfig_create(&fs).context(what)?;
    let dev = rustix::mount::fsmount(
        &fs,
        FsMountFlags::FSMOUNT_CLOEXEC,
        MountAttrFlags::MOUNT_ATTR_NOSUID | MountAttrFlags::MOUNT_ATTR_NOEXEC,
    )
    .context(what)?;
    for (name, major, minor) in DEVICES {
        rustix::fs::mknodat(
            &dev,
            name,
            FileType::CharacterDevice,
            Mode::from_raw_mode(0o666),
            rustix::fs::makedev(major, minor),
        )
        .context(format!("mknod /dev/{name}"))?;
        // mknod applies init's umask.
        rustix::fs::chmodat(&dev, name, Mode::from_raw_mode(0o666), AtFlags::empty())
            .context(format!("chmod /dev/{name}"))?;
    }
    for dir in ["pts", "shm"] {
        rustix::fs::mkdirat(&dev, dir, Mode::from_raw_mode(0o755)).context(format!("mkdir /dev/{dir}"))?;
    }
    for (name, target) in [
        ("ptmx", "pts/ptmx"),
        ("fd", "/proc/self/fd"),
        ("stdin", "/proc/self/fd/0"),
        ("stdout", "/proc/self/fd/1"),
        ("stderr", "/proc/self/fd/2"),
    ] {
        rustix::fs::symlinkat(target, &dev, name).context(format!("symlink /dev/{name}"))?;
    }
    // Populated: read-only from now on (the superblock, private to this step;
    // rustix has no mount_setattr), so no step can add a node to it. `pts` and
    // `shm` are mounts of their own over it.
    let fs = rustix::mount::fspick(&dev, "", FsPickFlags::FSPICK_EMPTY_PATH | FsPickFlags::FSPICK_CLOEXEC)
        .context("reopen the step's /dev")?;
    rustix::mount::fsconfig_set_flag(&fs, "ro").context("make the step's /dev read-only")?;
    rustix::mount::fsconfig_reconfigure(&fs).context("make the step's /dev read-only")?;
    Ok(dev)
}

/// The step's mounts and the mount points created for them.
struct Mounts {
    root: OwnedFd,
    /// Created mount points: parent, name, whether a directory.
    stubs: Vec<(OwnedFd, CString, bool)>,
    /// Attached mounts, each fd on its mount's root.
    attached: Vec<OwnedFd>,
}

impl Mounts {
    fn new() -> Result<Self> {
        let root = rustix::fs::open(ROOT, OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())
            .context("open the step's root")?;
        Ok(Self {
            root,
            stubs: Vec::new(),
            attached: Vec::new(),
        })
    }

    fn resolve(&self, rel: &str, extra: ResolveFlags) -> rustix::io::Result<OwnedFd> {
        let rel = if rel.is_empty() { "." } else { rel };
        rustix::fs::openat2(
            &self.root,
            rel,
            OFlags::PATH | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::IN_ROOT | ResolveFlags::NO_MAGICLINKS | extra,
        )
    }

    /// Whether `path` in the root is a regular file or absent, with no symlink on the way.
    fn plain(&self, path: &str) -> Result<bool> {
        match self.resolve(path.trim_start_matches('/'), ResolveFlags::NO_SYMLINKS) {
            Ok(fd) => {
                Ok(rustix::fs::fstat(&fd).is_ok_and(|st| FileType::from_raw_mode(st.st_mode) == FileType::RegularFile))
            }
            Err(Errno::NOENT) => Ok(true),
            Err(Errno::LOOP | Errno::NOTDIR) => Ok(false),
            Err(e) => Err(e).context(format!("look up {path}")),
        }
    }

    /// An `O_PATH` descriptor of `path` in the root, a directory or a regular
    /// file, created (and recorded as a stub) when missing.
    fn target(&mut self, path: &str, dir: bool) -> Result<OwnedFd> {
        let comps: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
        let mut parent = self
            .resolve("", ResolveFlags::empty())
            .context("open the step's root")?;
        for (i, c) in comps.iter().enumerate() {
            let last = i + 1 == comps.len();
            let prefix = comps[..=i].join("/");
            match self.resolve(&prefix, ResolveFlags::empty()) {
                Ok(fd) => parent = fd,
                Err(Errno::NOENT) => {
                    let as_dir = !last || dir;
                    if as_dir {
                        rustix::fs::mkdirat(&parent, *c, Mode::from_raw_mode(0o755))
                    } else {
                        rustix::fs::openat(
                            &parent,
                            *c,
                            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                            Mode::from_raw_mode(0o644),
                        )
                        .map(drop)
                    }
                    .context(format!("create the mount point {path}"))?;
                    let name = CString::new(*c).expect("no NUL in targets");
                    let fd = rustix::fs::openat(
                        &parent,
                        *c,
                        OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    )
                    .context(format!("open the mount point {path}"))?;
                    self.stubs.push((parent, name, as_dir));
                    parent = fd;
                }
                Err(e) => return Err(e).context(format!("look up {path}")),
            }
        }
        let ty = rustix::fs::fstat(&parent)
            .map(|st| FileType::from_raw_mode(st.st_mode))
            .context(format!("stat {path}"))?;
        let want = if dir {
            FileType::Directory
        } else {
            FileType::RegularFile
        };
        if ty != want {
            return Err(Failure::msg(format!(
                "the mount point {path} is not a {}",
                if dir { "directory" } else { "regular file" }
            )));
        }
        Ok(parent)
    }

    fn attach(&mut self, mnt: OwnedFd, path: &str, dir: bool) -> Result<()> {
        let target = self.target(path, dir)?;
        rustix::mount::move_mount(
            &mnt,
            "",
            &target,
            "",
            MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH | MoveMountFlags::MOVE_MOUNT_T_EMPTY_PATH,
        )
        .context(format!("mount on {path}"))?;
        self.attached.push(mnt);
        Ok(())
    }

    /// Detaches the mounts (last first), then removes the stubs (last first);
    /// a stub the step put something in stays, as its own change.
    fn teardown(mut self) {
        for m in self.attached.drain(..).rev() {
            let path = format!("/proc/self/fd/{}", m.as_raw_fd());
            if let Err(e) = rustix::mount::unmount(path.as_str(), UnmountFlags::DETACH) {
                eprintln!("kiln-init: detach a step mount: {e}");
            }
        }
        for (parent, name, dir) in self.stubs.drain(..).rev() {
            let flags = if dir { AtFlags::REMOVEDIR } else { AtFlags::empty() };
            let _ = rustix::fs::unlinkat(&parent, name.as_c_str(), flags);
        }
    }
}

/// What the child's setup was doing when it failed.
const STEP_HOOK: [&str; 9] = [
    "setsid",
    "unshare the mount namespace",
    "pivot into the step's root",
    "drop capabilities",
    "create the working directory",
    "enter the working directory",
    "setgroups",
    "setresgid",
    "setresuid",
];

/// The bounding set a step keeps: Docker's default capabilities without
/// `MKNOD` (the guest has no device cgroup to stop a node from being opened):
/// `CHOWN`, `DAC_OVERRIDE`, `FOWNER`, `FSETID`, `KILL`, `SETGID`, `SETUID`,
/// `SETPCAP`, `NET_BIND_SERVICE`, `NET_RAW`, `SYS_CHROOT`, `AUDIT_WRITE`, `SETFCAP`.
const STEP_CAPS: u64 = (1 << 0)
    | (1 << 1)
    | (1 << 3)
    | (1 << 4)
    | (1 << 5)
    | (1 << 6)
    | (1 << 7)
    | (1 << 8)
    | (1 << 10)
    | (1 << 13)
    | (1 << 18)
    | (1 << 29)
    | (1 << 31);

/// Runs the command; returns its exit code (`None` when a signal killed it),
/// once `StepExited` is sent.
fn run_command(config: &Config, step: &RunStep, rx: &Receiver<Event>) -> Result<Option<i32>> {
    let id = Identity {
        uid: step.uid,
        gid: step.gid,
        groups: vec![step.gid],
        home: if step.uid == 0 { "/root".into() } else { "/".into() },
    };
    let mut outputs = Vec::new();
    let mut ends = Vec::new();
    for (name, p) in [("stdout", port::STDOUT), ("stderr", port::STDERR)] {
        let (r, w) = pipe()?;
        give(&w, &id)?;
        outputs.push(relay::spawn(name, r, connect(p)?, |v: super::sys::Vsock| {
            v.shutdown_write()
        })?);
        ends.push(w);
    }
    let mut err = ends.pop().expect("two pipes");
    let out = ends.pop().expect("two pipes");
    let env = crate::env::build(&step.env, &id.home, &config.hostname, false);
    let argv = step.command.argv();
    let mut cmd = Command::new(argv[0]);
    cmd.args(&argv[1..])
        .env_clear()
        .envs(env.iter().filter_map(|e| e.split_once('=')))
        .stdin(Stdio::null())
        .stdout(out.try_clone().context("dup the stdout pipe")?)
        .stderr(err.try_clone().context("dup the stderr pipe")?);
    drop(out);
    let (hook_failed, report) = rustix::pipe::pipe_with(PipeFlags::CLOEXEC | PipeFlags::NONBLOCK).context("pipe")?;
    step_hook(&mut cmd, &id, &step.workdir, report);
    let spawned = cmd.spawn();
    drop(cmd);
    let code = match spawned {
        Err(e) => {
            let (code, msg) = match failed_hook_step(&hook_failed) {
                Some(step) => (125, format!("kiln-init: {step}: {e}\n")),
                None => (
                    if e.raw_os_error() == Some(2) { 127 } else { 126 },
                    format!("kiln-init: exec {}: {e}\n", argv[0]),
                ),
            };
            let _ = err.write_all(msg.as_bytes());
            drop(err);
            for r in outputs {
                let _ = r.join();
            }
            send(&GuestMessage::StepExited(StepExited {
                id: step.id,
                code: Some(code),
                signal: None,
            }))?;
            return Ok(Some(code));
        }
        Ok(child) => {
            drop(err);
            let pid = Pid::from_raw(child.id() as i32).expect("a child's pid is positive");
            wait(pid, rx)?
        }
    };
    sys::kill_all();
    sys::reap_all();
    for r in outputs {
        let _ = r.join();
    }
    let (code, signal) = match (code.exit_status(), code.terminating_signal()) {
        (Some(c), _) => (Some(c), None),
        (None, s) => (None, s),
    };
    send(&GuestMessage::StepExited(StepExited {
        id: step.id,
        code,
        signal,
    }))?;
    Ok(code)
}

/// Waits for the step's process; `Signal` goes to its process group.
fn wait(pid: Pid, rx: &Receiver<Event>) -> Result<rustix::process::WaitStatus> {
    loop {
        match rx.recv().expect("the signal thread holds a sender") {
            Event::Signal(SIGCHLD) => {
                for (p, status) in sys::reap() {
                    if p == pid {
                        return Ok(status);
                    }
                }
            }
            Event::Host(HostMessage::Signal(s)) => {
                if let Some(sig) = rustix::process::Signal::from_named_raw(s.sig) {
                    let _ = rustix::process::kill_process_group(pid, sig);
                }
            }
            Event::Host(HostMessage::WindowSize(_)) => {}
            Event::Host(other) => {
                sys::kill_all();
                return Err(Failure::msg(format!(
                    "protocol violation: {} while a step runs",
                    match other {
                        HostMessage::RunStep(_) => "RunStep",
                        HostMessage::Finish => "Finish",
                        HostMessage::Shutdown(_) => "Shutdown",
                        HostMessage::Config(_) => "Config",
                        _ => "an exec or file message",
                    }
                )));
            }
            Event::Signal(_) => {
                sys::kill_all();
                return Err(Failure::msg("init was asked to stop during a step"));
            }
            Event::HostError(e) => return Err(Failure::msg(format!("protocol violation: {e}"))),
            Event::HostClosed => return Err(Failure::msg("the host closed the control connection")),
        }
    }
}

/// In the child: a new session; a mount namespace of its own, pivoted into the
/// step's root with init's root detached; the bounding set cut to
/// [`STEP_CAPS`]; the working directory (created as root when missing); then
/// groups, gid and uid.
fn step_hook(cmd: &mut Command, id: &Identity, workdir: &str, report: OwnedFd) {
    let mut prefixes = Vec::new();
    let mut acc = String::new();
    for c in workdir.split('/').filter(|c| !c.is_empty()) {
        acc.push('/');
        acc.push_str(c);
        prefixes.push(CString::new(acc.clone()).expect("no NUL in workdir"));
    }
    let wd = CString::new(workdir).expect("no NUL in workdir");
    let groups: Vec<Gid> = id.groups.iter().map(|&g| Gid::from_raw(g)).collect();
    let (uid, gid) = (Uid::from_raw(id.uid), Gid::from_raw(id.gid));
    let hook = move || -> std::io::Result<()> {
        let step = |n: u8, r: rustix::io::Result<()>| {
            r.map_err(|e| {
                let _ = rustix::io::write(&report, &[n]);
                std::io::Error::from(e)
            })
        };
        step(0, rustix::process::setsid().map(drop))?;
        // SAFETY: only a new mount namespace (not `CLONE_FILES`): no descriptor
        // table is unshared, and the child is single-threaded.
        #[allow(unsafe_code)]
        step(1, unsafe {
            rustix::thread::unshare_unsafe(rustix::thread::UnshareFlags::NEWNS)
        })?;
        step(
            1,
            rustix::mount::mount_change(
                c"/",
                rustix::mount::MountPropagationFlags::PRIVATE | rustix::mount::MountPropagationFlags::REC,
            ),
        )?;
        // runc's `pivot_root(".", ".")`: the old root stacks under the new one
        // and is detached at once.
        step(2, rustix::process::chdir(c"/kiln/root"))?;
        step(2, rustix::process::pivot_root(c".", c"."))?;
        step(2, rustix::mount::unmount(c".", UnmountFlags::DETACH))?;
        step(2, rustix::process::chdir(c"/"))?;
        for n in 0..64u32 {
            if STEP_CAPS & (1 << n) == 0 {
                match rustix::thread::remove_capability_from_bounding_set(CapabilitySet::from_bits_retain(1 << n)) {
                    // Past the kernel's last capability.
                    Ok(()) | Err(Errno::INVAL) => {}
                    Err(e) => step(3, Err(e))?,
                }
            }
        }
        for p in &prefixes {
            match rustix::fs::mkdir(p.as_c_str(), Mode::from_raw_mode(0o755)) {
                Ok(()) | Err(Errno::EXIST) => {}
                Err(e) => step(4, Err(e))?,
            }
        }
        step(5, rustix::process::chdir(wd.as_c_str()))?;
        step(6, rustix::thread::set_thread_groups(&groups))?;
        step(7, rustix::thread::set_thread_res_gid(gid, gid, gid))?;
        step(8, rustix::thread::set_thread_res_uid(uid, uid, uid))?;
        Ok(())
    };
    // SAFETY: the hook runs in the forked child before exec. It only makes raw
    // system calls through rustix on values moved in beforehand (C strings and
    // ids prepared in the parent; a failure writes one byte to a pipe and
    // converts an errno): no allocation, locks or state shared with the parent's
    // threads.
    #[allow(unsafe_code)]
    unsafe {
        cmd.pre_exec(hook);
    }
}

/// The setup step the hook reported on `pipe`, if it failed.
fn failed_hook_step(pipe: &OwnedFd) -> Option<&'static str> {
    let mut b = [0u8; 1];
    match rustix::io::read(pipe, &mut b) {
        Ok(1) => STEP_HOOK.get(usize::from(b[0])).copied(),
        _ => None,
    }
}
```

- [ ] **Step 3: Modify `crates/kiln-init/src/linux/identity.rs`**

Apply (unified diff against the previous task's tree):

```diff
--- a/crates/kiln-init/src/linux/identity.rs
+++ b/crates/kiln-init/src/linux/identity.rs
@@ -13,11 +13,18 @@ use crate::error::{Context, Failure, Result};
 use crate::{etc, netlink};
 
 pub fn configure(config: &Config) -> Result<()> {
-    rustix::system::sethostname(config.hostname.as_bytes()).context("sethostname")?;
     let net = config.network.as_ref();
     write_etc("/etc/hostname", &etc::hostname(&config.hostname))?;
     write_etc("/etc/hosts", &etc::hosts(&config.hostname, net))?;
     write_etc("/etc/resolv.conf", &etc::resolv_conf(net))?;
+    network(config)
+}
+
+/// The hostname, `lo` and `eth0`, without the `/etc` files (build mode mounts
+/// them per step instead, so that they never reach a step's diff).
+pub fn network(config: &Config) -> Result<()> {
+    rustix::system::sethostname(config.hostname.as_bytes()).context("sethostname")?;
+    let net = config.network.as_ref();
     let mut rtnl = Rtnl::open()?;
     let lo = rtnl.index("lo")?;
     rtnl.request("bring up lo", |seq| netlink::link_up(seq, lo))?;
```

- [ ] **Step 4: Modify `crates/kiln-init/src/linux/mod.rs`**

Apply (unified diff against the previous task's tree):

```diff
--- a/crates/kiln-init/src/linux/mod.rs
+++ b/crates/kiln-init/src/linux/mod.rs
@@ -3,11 +3,13 @@
 //! `kiln-init: <stage>: <error>` on the console, sends `InitFailed` when the
 //! channel is up, and ends the VM with `exitMethod`, or `reboot()` before `Config`.
 
+mod build;
 mod control;
 mod identity;
 mod process;
 mod relay;
 mod root;
+mod seal;
 mod storage;
 mod supervise;
 pub mod sys;
@@ -67,6 +69,11 @@ fn boot() -> Result<()> {
     let (events, rx) = channel();
     enter(2)?;
     let (config, config_at, files) = control::connect(events.clone())?;
+    if config.mode == kiln_proto::Mode::Build {
+        // `Config::validate` refuses volumes in build mode, so `files` is empty.
+        drop(files);
+        return build::run(&config, events, &rx);
+    }
     enter(3)?;
     let scratch = storage::mount_all(&config)?;
     let mut volumes = volumes::Volumes::prepare(&config, &files)?;
```

- [ ] **Step 5: Modify `crates/kiln-init/src/linux/process.rs`**

Apply (unified diff against the previous task's tree):

```diff
--- a/crates/kiln-init/src/linux/process.rs
+++ b/crates/kiln-init/src/linux/process.rs
@@ -380,7 +380,7 @@ fn read_optional(path: &str) -> Result<String> {
 }
 
 /// SIGCHLD, and SIGINT (Ctrl-Alt-Del) or SIGTERM as `Shutdown`, go to the supervisor.
-fn watch_signals(events: Sender<Event>) -> Result<()> {
+pub(super) fn watch_signals(events: Sender<Event>) -> Result<()> {
     let mut signals = Signals::new([SIGCHLD, SIGINT, SIGTERM]).context("install signal handlers")?;
     std::thread::Builder::new()
         .name("signals".into())
@@ -395,18 +395,18 @@ fn watch_signals(events: Sender<Event>) -> Result<()> {
     Ok(())
 }
 
-fn connect(port: u32) -> Result<Vsock> {
+pub(super) fn connect(port: u32) -> Result<Vsock> {
     Vsock::connect(HOST_CID, port).context(format!("connect to the host on vsock port {port}"))
 }
 
-fn pipe() -> Result<(File, File)> {
+pub(super) fn pipe() -> Result<(File, File)> {
     let (r, w) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).context("pipe")?;
     Ok((File::from(r), File::from(w)))
 }
 
 /// Gives the workload's user its stdio, as runc does: a non-root user could
 /// otherwise not reopen `/dev/stdout` or `/proc/self/fd/N`.
-fn give(fd: impl AsFd, id: &Identity) -> Result<()> {
+pub(super) fn give(fd: impl AsFd, id: &Identity) -> Result<()> {
     rustix::fs::fchown(fd, Some(Uid::from_raw(id.uid)), Some(Gid::from_raw(id.gid))).context("chown the stdio")
 }
```

- [ ] **Step 6: Create `crates/kiln-init/src/linux/seal.rs`**

```rust
//! Sealing a build step (spec §9.8 build mode): the overlay's upper directory,
//! once the overlay is unmounted, becomes an OCI tar diff appended to the output
//! disk. Overlay whiteouts (character devices 0:0) become `.wh.<name>`, opaque
//! directories (`trusted.overlay.opaque`) get `.wh..wh..opq`, inodes with more
//! than one link become hard links to their first path, and the overlay's own
//! `trusted.overlay.*` attributes are dropped (only `user.*` and
//! `security.capability` are kept: [`crate::diff::keep_xattr`]). With a
//! `SOURCE_DATE_EPOCH`, every entry's mtime is clamped to it, copied-up parent
//! directories included ([`crate::diff::clamp_mtime`]). Sockets cannot be in a layer and
//! are skipped. Names are walked in byte order, so the same upper gives the same
//! diff.

use std::collections::HashMap;
use std::ffi::CStr;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::os::fd::OwnedFd;

use rustix::fs::{AtFlags, FileType, Mode, OFlags, SeekFrom, Stat};
use rustix::io::Errno;
use sha2::{Digest, Sha256};

use crate::diff::{Kind, Node, TarWriter, Written, clamp_mtime, join, keep_xattr, opaque, whiteout};
use crate::error::{Context, Failure, Result};

const OPAQUE_XATTR: &CStr = c"trusted.overlay.opaque";

/// The output disk: where the next diff goes.
pub struct Output {
    dev: OwnedFd,
    size: u64,
    next: u64,
}

/// Where a diff landed.
pub struct Sealed {
    pub offset: u64,
    pub written: Written,
    pub sha256: String,
}

impl Output {
    pub fn open(path: &str) -> Result<Self> {
        let dev = rustix::fs::open(path, OFlags::RDWR | OFlags::CLOEXEC, Mode::empty())
            .context(format!("open the output disk {path}"))?;
        let size = rustix::fs::seek(&dev, SeekFrom::End(0)).context("size of the output disk")?;
        Ok(Self { dev, size, next: 0 })
    }

    /// Writes the diff of `upper` at the next aligned offset and flushes the disk.
    /// `lowers` are the directories below it: overlayfs marks every directory it
    /// creates in a merged directory opaque, and the marker is only kept where a
    /// lower has something it hides. `epoch` clamps every entry's mtime.
    pub fn seal(&mut self, upper: &str, lowers: &[String], epoch: Option<u64>) -> Result<Sealed> {
        let offset = self.next;
        let w = DiskWriter {
            dev: &self.dev,
            pos: offset,
            end: self.size,
            hash: Sha256::new(),
        };
        let mut tar = TarWriter::new(BufWriter::with_capacity(1 << 20, w));
        let root = rustix::fs::open(
            upper,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .context(format!("open {upper}"))?;
        let mut links = HashMap::new();
        let lowers = lowers
            .iter()
            .map(|l| {
                rustix::fs::open(
                    l.as_str(),
                    OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .context(format!("open {l}"))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut w = Walk {
            tar: &mut tar,
            links: &mut links,
            lowers: &lowers,
            epoch,
        };
        walk_dir(&mut w, &root, &[])?;
        let (buf, written) = tar.finish(kiln_proto::DIFF_ALIGN).map_err(io_failure)?;
        let w = buf
            .into_inner()
            .map_err(|e| io_failure(io::Error::other(e.to_string())))?;
        let sha256 = hex(&w.hash.finalize());
        rustix::fs::fsync(&self.dev).context("flush the output disk")?;
        self.next = offset + written.len;
        Ok(Sealed {
            offset,
            written,
            sha256,
        })
    }
}

fn io_failure(e: io::Error) -> Failure {
    Failure::os("write the step's diff", &e)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Positional writes to the output disk, hashed, refused past its end.
struct DiskWriter<'a> {
    dev: &'a OwnedFd,
    pos: u64,
    end: u64,
    hash: Sha256,
}

impl Write for DiskWriter<'_> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        if self.pos + b.len() as u64 > self.end {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                format!("the output disk ({} bytes) is full", self.end),
            ));
        }
        let n = rustix::io::pwrite(self.dev, b, self.pos)?;
        self.hash.update(&b[..n]);
        self.pos += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn fail(path: &[u8], what: &str, e: impl Into<io::Error>) -> Failure {
    Failure::os(format!("seal: {what} {}", String::from_utf8_lossy(path)), &e.into())
}

/// The directory's entries other than `.` and `..`, in byte order.
fn names(dir: &OwnedFd, path: &[u8]) -> Result<Vec<Vec<u8>>> {
    let d = rustix::fs::openat(
        dir,
        c".",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| fail(path, "open", e))?;
    let mut names = Vec::new();
    for entry in rustix::fs::Dir::read_from(&d).map_err(|e| fail(path, "read", e))? {
        let entry = entry.map_err(|e| fail(path, "read", e))?;
        let name = entry.file_name().to_bytes();
        if name != b"." && name != b".." {
            names.push(name.to_vec());
        }
    }
    names.sort();
    Ok(names)
}

/// The extended attributes a diff keeps ([`keep_xattr`]) of an open file.
fn xattrs(fd: &OwnedFd, path: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut list = vec![0u8; 64 * 1024];
    let n = match rustix::fs::flistxattr(fd, &mut list[..]) {
        Ok(n) => n,
        Err(Errno::NOTSUP) => return Ok(Vec::new()),
        Err(e) => return Err(fail(path, "list the attributes of", e)),
    };
    let mut out = Vec::new();
    for name in list[..n].split(|&b| b == 0).filter(|n| !n.is_empty()) {
        if !keep_xattr(name) {
            continue;
        }
        let cname = std::ffi::CString::new(name).expect("split on NUL");
        let mut value = vec![0u8; 64 * 1024];
        let len = rustix::fs::fgetxattr(fd, cname.as_c_str(), &mut value[..])
            .map_err(|e| fail(path, "read an attribute of", e))?;
        value.truncate(len);
        out.push((name.to_vec(), value));
    }
    Ok(out)
}

fn is_opaque(fd: &OwnedFd) -> bool {
    let mut v = [0u8; 4];
    matches!(rustix::fs::fgetxattr(fd, OPAQUE_XATTR, &mut v[..]), Ok(1) if v[0] == b'y')
}

fn base(path: Vec<u8>, st: &Stat, kind: Kind, epoch: Option<u64>) -> Node {
    Node {
        path,
        kind,
        mode: st.st_mode & 0o7777,
        uid: u64::from(st.st_uid),
        gid: u64::from(st.st_gid),
        mtime: clamp_mtime(st.st_mtime, epoch),
        xattrs: Vec::new(),
    }
}

type Links = HashMap<(u64, u64), Vec<u8>>;

struct Walk<'a, W: Write> {
    tar: &'a mut TarWriter<W>,
    links: &'a mut Links,
    lowers: &'a [OwnedFd],
    epoch: Option<u64>,
}

impl<W: Write> Walk<'_, W> {
    /// Whether a lower layer has something at `path`.
    fn in_lowers(&self, path: &[u8]) -> bool {
        self.lowers
            .iter()
            .any(|l| rustix::fs::statat(l, path, AtFlags::SYMLINK_NOFOLLOW).is_ok())
    }
}

fn walk_dir<W: Write>(w: &mut Walk<'_, W>, dir: &OwnedFd, path: &[u8]) -> Result<()> {
    for name in names(dir, path)? {
        let child = join(path, &name);
        let st =
            rustix::fs::statat(dir, name.as_slice(), AtFlags::SYMLINK_NOFOLLOW).map_err(|e| fail(&child, "stat", e))?;
        let ty = FileType::from_raw_mode(st.st_mode);
        let rdev = (rustix::fs::major(st.st_rdev), rustix::fs::minor(st.st_rdev));
        if ty == FileType::CharacterDevice && rdev == (0, 0) {
            w.tar
                .entry(&Node::marker(whiteout(path, &name)), Some(&mut io::empty()))
                .map_err(io_failure)?;
            continue;
        }
        if ty != FileType::Directory && st.st_nlink > 1 {
            let key = (st.st_dev, st.st_ino);
            if let Some(first) = w.links.get(&key) {
                let node = base(child, &st, Kind::HardLink(first.clone()), w.epoch);
                w.tar.entry(&node, None).map_err(io_failure)?;
                continue;
            }
            w.links.insert(key, child.clone());
        }
        match ty {
            FileType::Directory => {
                let fd = rustix::fs::openat(
                    dir,
                    name.as_slice(),
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|e| fail(&child, "open", e))?;
                let mut node = base(child.clone(), &st, Kind::Dir, w.epoch);
                node.xattrs = xattrs(&fd, &child)?;
                w.tar.entry(&node, None).map_err(io_failure)?;
                if is_opaque(&fd) && w.in_lowers(&child) {
                    w.tar
                        .entry(&Node::marker(opaque(&child)), Some(&mut io::empty()))
                        .map_err(io_failure)?;
                }
                walk_dir(w, &fd, &child)?;
            }
            FileType::RegularFile => {
                let fd = rustix::fs::openat(
                    dir,
                    name.as_slice(),
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|e| fail(&child, "open", e))?;
                let mut node = base(child.clone(), &st, Kind::File(st.st_size as u64), w.epoch);
                node.xattrs = xattrs(&fd, &child)?;
                let mut file = File::from(fd);
                w.tar.entry(&node, Some(&mut file)).map_err(io_failure)?;
            }
            FileType::Symlink => {
                let target = rustix::fs::readlinkat(dir, name.as_slice(), Vec::new())
                    .map_err(|e| fail(&child, "read the link", e))?;
                let node = base(child, &st, Kind::Symlink(target.into_bytes()), w.epoch);
                w.tar.entry(&node, None).map_err(io_failure)?;
            }
            FileType::CharacterDevice => {
                w.tar
                    .entry(&base(child, &st, Kind::Char(rdev.0, rdev.1), w.epoch), None)
                    .map_err(io_failure)?;
            }
            FileType::BlockDevice => {
                w.tar
                    .entry(&base(child, &st, Kind::Block(rdev.0, rdev.1), w.epoch), None)
                    .map_err(io_failure)?;
            }
            FileType::Fifo => w
                .tar
                .entry(&base(child, &st, Kind::Fifo, w.epoch), None)
                .map_err(io_failure)?,
            // A layer cannot hold a socket.
            FileType::Socket | FileType::Unknown => {}
        }
    }
    Ok(())
}
```

- [ ] **Step 7: Modify `crates/kiln-init/src/linux/storage.rs`**

Apply (unified diff against the previous task's tree):

```diff
--- a/crates/kiln-init/src/linux/storage.rs
+++ b/crates/kiln-init/src/linux/storage.rs
@@ -9,13 +9,13 @@ use rustix::io::Errno;
 use rustix::mount::{FsPickFlags, MountFlags, mount};
 
 use super::{open_dir, sys};
-use crate::disks::{SCRATCH, layer_device};
+use crate::disks::{SCRATCH, disk_name};
 use crate::error::{Context, Failure, Result};
 use crate::overlay::{EMPTY_LOWER, UPPER, WORK, layer_dir, mount_data};
 
 const BLOCK: u64 = 4096;
 
-fn mkdir(path: &str) -> Result<()> {
+pub(super) fn mkdir(path: &str) -> Result<()> {
     match rustix::fs::mkdir(path, Mode::from_raw_mode(0o755)) {
         Ok(()) | Err(Errno::EXIST) => Ok(()),
         Err(e) => Err(e).context(format!("mkdir {path}")),
@@ -25,6 +25,18 @@ fn mkdir(path: &str) -> Result<()> {
 /// Mounts everything under `/kiln` and returns a descriptor of the scratch
 /// filesystem's root, which outlives the pivot (see [`finish`]).
 pub fn mount_all(config: &Config) -> Result<OwnedFd> {
+    let rw = mount_base(config, 2)?;
+    mkdir(UPPER)?;
+    mkdir(WORK)?;
+    let data = std::ffi::CString::new(mount_data(config.layers as usize)?).expect("no NUL in paths");
+    mount("overlay", "/kiln/root", "overlay", MountFlags::empty(), data.as_c_str())
+        .context("mount the root overlay")?;
+    Ok(rw)
+}
+
+/// `/kiln`, the base layers (from disk index `first`) and the grown scratch
+/// disk, without the overlay: run mode adds it, build mode mounts one per step.
+pub fn mount_base(config: &Config, first: usize) -> Result<OwnedFd> {
     mount(
         "tmpfs",
         "/kiln",
@@ -38,22 +50,23 @@ pub fn mount_all(config: &Config) -> Result<OwnedFd> {
     }
     let layers = config.layers as usize;
     for n in 0..layers {
-        let (dev, dir) = (layer_device(n), layer_dir(n));
+        let (dev, dir) = (format!("/dev/{}", disk_name(first + n)), layer_dir(n));
         mkdir(&dir)?;
         mount(dev.as_str(), dir.as_str(), "erofs", MountFlags::RDONLY, None).context(format!("mount layer {dev}"))?;
     }
     mount(SCRATCH, "/kiln/rw", "ext4", MountFlags::empty(), c"noinit_itable").context(format!("mount {SCRATCH}"))?;
     let rw = open_dir("/kiln/rw")?;
     grow(&rw, config.scratch.size_bytes)?;
-    mkdir(UPPER)?;
-    mkdir(WORK)?;
-    let data = std::ffi::CString::new(mount_data(layers)?).expect("no NUL in paths");
-    mount("overlay", "/kiln/root", "overlay", MountFlags::empty(), data.as_c_str())
-        .context("mount the root overlay")?;
     Ok(rw)
 }
 
 /// Grows the scratch filesystem online to `size` bytes (spec §9.4).
+pub(super) fn grow_to_device(rw: &OwnedFd, dev: &str) -> Result<()> {
+    let d = rustix::fs::open(dev, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty()).context(format!("open {dev}"))?;
+    let size = rustix::fs::seek(&d, SeekFrom::End(0)).context(format!("size of {dev}"))?;
+    sys::ext4_resize(rw, size / BLOCK).context(format!("grow {dev} to {size} bytes"))
+}
+
 fn grow(rw: &OwnedFd, size: u64) -> Result<()> {
     let dev = rustix::fs::open(SCRATCH, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
         .context(format!("open {SCRATCH}"))?;
```

- [ ] **Step 8: Modify `crates/kiln-init/src/overlay.rs`**

Apply (unified diff against the previous task's tree):

```diff
--- a/crates/kiln-init/src/overlay.rs
+++ b/crates/kiln-init/src/overlay.rs
@@ -34,6 +34,26 @@ pub fn mount_data(layers: usize) -> Result<String> {
     Ok(data)
 }
 
+/// A build step's overlay (spec §9.8 build mode): `lowers` bottom first (base
+/// layers, stacked layers, sealed uppers), named top first, over `upper`.
+pub fn step_mount_data(lowers: &[String], upper: &str, work: &str) -> Result<String> {
+    let lower = if lowers.is_empty() {
+        EMPTY_LOWER.to_string()
+    } else {
+        lowers.iter().rev().cloned().collect::<Vec<_>>().join(":")
+    };
+    let data =
+        format!("lowerdir={lower},upperdir={upper},workdir={work},xino=on,redirect_dir=off,index=off,metacopy=off");
+    if data.len() > MAX_DATA {
+        return Err(Failure::msg(format!(
+            "{} lower layers exceed what one overlay mount can name ({} bytes)",
+            lowers.len(),
+            data.len()
+        )));
+    }
+    Ok(data)
+}
+
 #[cfg(test)]
 mod tests {
     use super::*;
@@ -48,6 +68,25 @@ mod tests {
         assert!(mount_data(0).unwrap().starts_with("lowerdir=/kiln/empty,"));
     }
 
+    /// Ruling 2: a build step's overlay names up to `MAX_BUILD_LOWERS` lowers,
+    /// with the longest names kiln-init gives them (three-digit indexes, the
+    /// longest prefix), in one page.
+    #[test]
+    fn the_most_build_lowers_fit_in_a_page() {
+        let max = kiln_proto::MAX_BUILD_LOWERS;
+        let lowers: Vec<String> = (0..max).map(|i| layer_dir(100 + i % 900)).collect();
+        let (upper, work) = (format!("/kiln/rw/u/{max}"), format!("/kiln/rw/w/{max}"));
+        let data = step_mount_data(&lowers, &upper, &work).unwrap();
+        assert!(data.len() <= MAX_DATA, "{}", data.len());
+        let more: Vec<String> = (0..max + 10).map(|i| layer_dir(100 + i)).collect();
+        assert!(step_mount_data(&more, &upper, &work).is_err());
+        assert!(
+            step_mount_data(&[], &upper, &work)
+                .unwrap()
+                .starts_with("lowerdir=/kiln/empty,")
+        );
+    }
+
     #[test]
     fn the_largest_layer_count_fits_in_a_page() {
         let max = kiln_proto::MAX_LAYERS as usize;
```

- [ ] **Step 9: Check (macOS)**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: clean; 1 more test than after Task 2 (`overlay::tests::the_most_build_lowers_fit_in_a_page`). The Linux module does not build on macOS.

- [ ] **Step 10: Check (Linux), build static, and commit**

Run (Lima): `cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && cargo build --release --target aarch64-unknown-linux-musl -p kiln-init && file $CARGO_TARGET_DIR/aarch64-unknown-linux-musl/release/kiln-init`
Expected: clean, all pass, `statically linked`. The boot tests for this task are Task 6's (they need Tasks 4 and 5's host).

```bash
git add -A && git commit -m "feat(kiln-init): build mode: a fresh overlay per step, a minimal read-only /dev, secrets on a tmpfs, steps pivoted into their own mount namespace with a reduced bounding set, sealing onto the output disk"
```

---

### Task 4: `kiln-run`: build mode's host model: `BuildSpec`, `RunStep`, the device budget, the lowers cap, the `Config` and `RunStep` mapping, and the guest's message order

**Files:**
- Create: `crates/kiln-run/src/build.rs`
- Modify: `crates/kiln-run/src/error.rs`, `crates/kiln-run/src/lib.rs`

**Interfaces:**
- Consumes: Task 1's protocol types; `kiln_run::config::{GUEST, GATEWAY, PREFIX}` (P6); `VmmKind`.
- Produces (`kiln_run::build`, every platform; the API in `m3e-findings.txt` plus the delta in Design decisions):
  - `FIXED_DEVICES` (4), `STEP_GRACE` (10 s), `KILL_WAIT` (5 s), `device_budget(available, network) -> usize`.
  - `BuildSpec { vmm, kernel, init, layers, root, cache_disks, output, output_size, network, memory_mib, cpus, scratch_bytes, boot_timeout, source_date_epoch }` with `check(available_devices)`, `layer_disk(i)`, `config(exit_method) -> Config`, `lowers(&stacked, sealed) -> usize`, `proto_step(&step, &stacked, sealed) -> Result<kiln_proto::RunStep>`.
  - `RunStep { id, command, env: BTreeMap, workdir, uid, gid, lowers_added: Vec<usize>, mounts: Vec<String>, secrets: Vec<(String, Secret)>, network, timeout }`, `Secret(Vec<u8>)` (redacting `Debug`), `Stream::{Stdout, Stderr}`, `Exit::{Code, Signal, TimedOut}`, `StepOutcome { exit, sealed: Option<StepSealed> }`.
  - `BuildProtocol` (`start_step`, `end_step`, `accept -> BuildEvent::{Boot, Exited, Sealed, Failed}`).
  - `kiln_run::Error::TooManyLowers { step, lowers, max }` (R2, potter's distinct variant).

- **`check`**: `root ≤ layers`; layers + cache disks within `device_budget(available, network)` and within `MAX_LAYERS`; `output_size` a positive multiple of 4096; cache targets absolute and unique.
- **`config`**: `mode: Build`, `layers: root`, `disks` = the non-root layers (`erofs`, read-only) then the cache disks (`ext4`, read-write), hostname `kiln-build`, `network` IPv4 only when `network` (P6), `sourceDateEpoch` from the spec (R3), no services, volumes or pod disk.
- **`proto_step`**: `lowers_added` are indices `≥ root`, `< layers`, not stacked yet, unique, mapped to disk indices; the lowers cap (`lowers(stacked, sealed) + lowers_added.len() > MAX_BUILD_LOWERS` → `TooManyLowers`, nothing sent); mounts name cache targets; env becomes `KEY=value`; `egress = step.network && spec.network`; `timeoutMs` saturates; the encoded message must fit one frame (secrets inline, ~44 KiB).
- **`BuildProtocol`**: `Hello` 3 or later, once (a lower one is refused: "build mode needs version 3"); `Stage` 3–6 increasing; `Running` once; for the step the host sent, `StepExited` with its id once, then after code 0 `StepSealed` with its id; `InitFailed` anywhere after `Hello`, then nothing. Everything else is out of order.

- [ ] **Step 1: Create `crates/kiln-run/src/build.rs`**

```rust
//! Build mode's host side, the parts that do not need a VM (spec §9.8): what a
//! build VM and a step are ([`BuildSpec`], [`RunStep`]), the device budget, the
//! `Config` and `RunStep` messages they become, and the order the guest's build
//! messages must come in ([`BuildProtocol`]). The VM itself is
//! [`BuildVm`](crate::BuildVm) (Linux).
//!
//! Disks, in order (nothing is hot-plugged, so a layer a step stacks later is
//! attached at boot): `vda` the init layer, `vdb` scratch, `vdc` the output disk
//! (raw, created and sized by the caller), `layers` (erofs, bottom first; the
//! first `root` form the boot root), then `cache_disks` (ext4).

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::Duration;

use kiln_proto::{
    BuildDisk, Command, Config, DiskKind, ExitMethod, GuestMessage, MAX_BUILD_LOWERS, MAX_FRAME, Mode, Network,
    Process, Scratch, StepExited, StepMount, StepSealed,
};

use crate::error::{Error, Result};
use crate::options::VmmKind;

/// The devices every build VM has: init layer, scratch, output disk, vsock. A VM
/// with `network` has a NIC too.
pub const FIXED_DEVICES: usize = 4;
/// After a step's timeout the host signals its process group with SIGKILL; when
/// the step has not ended this long after, the host kills the VM.
pub const STEP_GRACE: Duration = Duration::from_secs(10);
/// How long a killed VMM gets to exit before the host stops waiting for it. So
/// `run_step` returns within `timeout + STEP_GRACE + KILL_WAIT`.
pub const KILL_WAIT: Duration = Duration::from_secs(5);

/// The disks a build VM can hold besides its fixed devices (layers and cache
/// disks together), given the VMM's available devices.
pub fn device_budget(available_devices: u32, network: bool) -> usize {
    (available_devices as usize).saturating_sub(FIXED_DEVICES + usize::from(network))
}

/// One build VM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildSpec {
    pub vmm: VmmKind,
    /// The kernel and the static kiln-init binary (this kiln pins neither yet).
    pub kernel: PathBuf,
    pub init: PathBuf,
    /// Read-only erofs layer disks, bottom first.
    pub layers: Vec<PathBuf>,
    /// The first `root` layers are the boot root; the others are attached and
    /// only stacked when a step's `lowers_added` names them.
    pub root: usize,
    /// ext4 cache disks and the one target each is mounted on (read-write).
    pub cache_disks: Vec<(PathBuf, String)>,
    /// The output disk the caller created: raw, `output_size` bytes, a
    /// multiple of 4096. kiln never reads it.
    pub output: PathBuf,
    pub output_size: u64,
    /// A NIC (vmkit's restricted egress); each step turns egress on or off.
    pub network: bool,
    pub memory_mib: u32,
    pub cpus: u8,
    /// The scratch disk holding every step's upper (a multiple of 4096, at least 64 MiB).
    pub scratch_bytes: u64,
    pub boot_timeout: Duration,
    /// `SOURCE_DATE_EPOCH`: when set, every sealed entry's mtime is clamped to
    /// at most this (copied-up parent directories included); otherwise the
    /// guest's times are kept.
    pub source_date_epoch: Option<u64>,
}

/// A secret's value; never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(pub Vec<u8>);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Secret(<{} bytes>)", self.0.len())
    }
}

/// Which of a step's output streams bytes came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

/// One build step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunStep {
    pub id: u32,
    pub command: Command,
    pub env: BTreeMap<String, String>,
    /// Absolute; created as root when missing.
    pub workdir: String,
    pub uid: u32,
    pub gid: u32,
    /// Indices into `BuildSpec.layers` (at or above `root`, each once per VM)
    /// stacked on the root before the step, bottom first.
    pub lowers_added: Vec<usize>,
    /// Targets of the cache disks this step mounts.
    pub mounts: Vec<String>,
    /// Secret targets and values: on a tmpfs in the guest, never in the diff.
    pub secrets: Vec<(String, Secret)>,
    /// Whether the step may reach the network (a VM without one has none).
    pub network: bool,
    /// For the command and its seal together.
    pub timeout: Duration,
}

/// How a step ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Code(i32),
    Signal(i32),
    /// The step outlived its timeout and was killed (with the VM, when it did not end).
    TimedOut,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepOutcome {
    pub exit: Exit,
    /// After `Exit::Code(0)`: where the guest says the diff is. Guest-asserted:
    /// passed through unchecked but for its shape and id.
    pub sealed: Option<StepSealed>,
}

impl BuildSpec {
    /// Checks the spec before anything is created.
    pub fn check(&self, available_devices: u32) -> Result<()> {
        let refuse = |m: String| Err(Error::refused(m));
        if self.root > self.layers.len() {
            return refuse(format!("root {} of {} layers", self.root, self.layers.len()));
        }
        let disks = self.layers.len() + self.cache_disks.len();
        let budget = device_budget(available_devices, self.network);
        if disks > budget {
            return refuse(format!(
                "{disks} layer and cache disks, and this VMM has room for {budget} besides its fixed devices"
            ));
        }
        if disks > kiln_proto::MAX_LAYERS as usize {
            return refuse(format!(
                "{disks} layer and cache disks (at most {})",
                kiln_proto::MAX_LAYERS
            ));
        }
        if self.output_size == 0 || !self.output_size.is_multiple_of(kiln_proto::DIFF_ALIGN) {
            return refuse(format!(
                "output disk size {} (a positive multiple of 4096)",
                self.output_size
            ));
        }
        let mut targets = BTreeSet::new();
        for (_, t) in &self.cache_disks {
            if !t.starts_with('/') || !targets.insert(t) {
                return refuse(format!("cache disk target {t:?}: absolute and unique"));
            }
        }
        Ok(())
    }

    /// The disk index of layer `i`.
    pub fn layer_disk(i: usize) -> u32 {
        Config::OUTPUT_DISK + 1 + i as u32
    }

    /// The guest's `Config`.
    pub fn config(&self, exit_method: ExitMethod) -> Config {
        let disks = self.layers[self.root..]
            .iter()
            .map(|_| BuildDisk {
                kind: DiskKind::Erofs,
                read_only: true,
            })
            .chain(self.cache_disks.iter().map(|_| BuildDisk {
                kind: DiskKind::Ext4,
                read_only: false,
            }))
            .collect();
        Config {
            process: Process::default(),
            stop_signal: kiln_proto::signal::DEFAULT_STOP,
            tty: None,
            interactive: false,
            hostname: "kiln-build".into(),
            network: self.network.then(guest_network),
            layers: self.root as u32,
            scratch: Scratch {
                size_bytes: self.scratch_bytes,
            },
            exit_method,
            shutdown_grace_secs: 10,
            // kiln-shim's volumes and pod disk are run mode only.
            volumes: Vec::new(),
            pod_disk: None,
            services: Vec::new(),
            ready_timeout_ms: None,
            mode: Mode::Build,
            disks,
            source_date_epoch: self.source_date_epoch,
        }
    }

    /// The lower layers a step's overlay has before its `lowers_added`: the
    /// root layers, the layers stacked so far and the steps sealed so far.
    pub fn lowers(&self, stacked: &BTreeSet<usize>, sealed: usize) -> usize {
        self.root + stacked.len() + sealed
    }

    /// The protocol `RunStep` for `step`; `stacked` holds the layers stacked so
    /// far and `sealed` counts the steps sealed so far. A step that would mount
    /// more than [`MAX_BUILD_LOWERS`] lowers is [`Error::TooManyLowers`].
    pub fn proto_step(&self, step: &RunStep, stacked: &BTreeSet<usize>, sealed: usize) -> Result<kiln_proto::RunStep> {
        let mut seen = BTreeSet::new();
        for &i in &step.lowers_added {
            if i < self.root || i >= self.layers.len() || stacked.contains(&i) || !seen.insert(i) {
                return Err(Error::refused(format!(
                    "step {}: layer {i} is not one left to stack (layers {}..{})",
                    step.id,
                    self.root,
                    self.layers.len()
                )));
            }
        }
        let lowers = self.lowers(stacked, sealed) + step.lowers_added.len();
        if lowers > MAX_BUILD_LOWERS {
            return Err(Error::TooManyLowers {
                step: step.id,
                lowers,
                max: MAX_BUILD_LOWERS,
            });
        }
        let mounts =
            step.mounts
                .iter()
                .map(|t| {
                    let j = self.cache_disks.iter().position(|(_, ct)| ct == t).ok_or_else(|| {
                        Error::refused(format!("step {}: no cache disk has the target {t:?}", step.id))
                    })?;
                    Ok(StepMount {
                        disk: Self::layer_disk(self.layers.len() + j),
                        target: t.clone(),
                        read_only: false,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
        let msg = kiln_proto::RunStep {
            id: step.id,
            command: step.command.clone(),
            env: step.env.iter().map(|(k, v)| format!("{k}={v}")).collect(),
            workdir: step.workdir.clone(),
            uid: step.uid,
            gid: step.gid,
            lowers_added: step.lowers_added.iter().map(|&i| Self::layer_disk(i)).collect(),
            mounts,
            secrets: step
                .secrets
                .iter()
                .map(|(t, s)| kiln_proto::Secret {
                    target: t.clone(),
                    data: s.0.clone(),
                })
                .collect(),
            egress: step.network && self.network,
            timeout_ms: u32::try_from(step.timeout.as_millis()).unwrap_or(u32::MAX).max(1),
        };
        msg.validate()?;
        // Secrets travel inline: the whole message must fit one frame.
        let size = 1 + serde_json::to_vec(&msg).map(|v| v.len()).unwrap_or(usize::MAX);
        if size > MAX_FRAME as usize {
            return Err(Error::refused(format!(
                "step {}: the step's message is {size} bytes, more than a control frame's {MAX_FRAME} \
                 (secrets travel inline: at most about 44 KiB of them)",
                step.id
            )));
        }
        Ok(msg)
    }
}

/// vmkit's tap network, as the guest configures `eth0`: IPv4 only (potter spec
/// §9.5 drops IPv6 for builds), DNS through the gateway.
fn guest_network() -> Network {
    use crate::config::{GATEWAY, GUEST, PREFIX};
    Network {
        address: GUEST,
        prefix_len: PREFIX,
        gateway: GATEWAY,
        dns: vec![GATEWAY],
        search: Vec::new(),
        options: Vec::new(),
        ipv6: None,
    }
}

/// Where the guest is in build mode's message order:
///
/// `Hello` (version 3 or later), `Stage` 3–6 increasing, `Running`, then for
/// each step the host sent: `StepExited` with its id, and after a code of 0,
/// `StepSealed` with its id. `InitFailed` may come at any point after `Hello`
/// and ends the VM. Nothing else is accepted, so the guest has at most one
/// message to queue per state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuildProtocol {
    hello: bool,
    stage: u8,
    running: bool,
    /// The step sent and not yet finished.
    step: Option<u32>,
    exited: Option<StepExited>,
    failed: bool,
}

/// What a guest message did to a step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildEvent {
    /// Hello, a stage, or `Running`.
    Boot,
    Exited(StepExited),
    Sealed(StepSealed),
    Failed(kiln_proto::InitFailed),
}

impl BuildProtocol {
    pub fn running(&self) -> bool {
        self.running
    }

    pub fn failed(&self) -> bool {
        self.failed
    }

    /// The host sends `RunStep { id }`: only when running and between steps.
    pub fn start_step(&mut self, id: u32) -> std::result::Result<(), String> {
        if !self.running || self.step.is_some() || self.failed {
            return Err("a step can only start between steps".into());
        }
        self.step = Some(id);
        self.exited = None;
        Ok(())
    }

    /// The step is over (its outcome was returned, or it was abandoned).
    pub fn end_step(&mut self) {
        self.step = None;
        self.exited = None;
    }

    pub fn accept(&mut self, msg: &GuestMessage) -> std::result::Result<BuildEvent, String> {
        if self.failed {
            return Err("a message after InitFailed".into());
        }
        match msg {
            GuestMessage::Hello(h) => {
                if self.hello || h.protocol < 3 || h.protocol > kiln_proto::PROTOCOL_VERSION {
                    return Err(format!("Hello {} (build mode needs version 3)", h.protocol));
                }
                self.hello = true;
                self.stage = 2;
                return Ok(BuildEvent::Boot);
            }
            _ if !self.hello => return Err("a message before Hello".into()),
            GuestMessage::InitFailed(f) => {
                self.failed = true;
                return Ok(BuildEvent::Failed(f.clone()));
            }
            GuestMessage::Stage(s) if !self.running && s.n > self.stage && (3..=6).contains(&s.n) => {
                self.stage = s.n;
                return Ok(BuildEvent::Boot);
            }
            GuestMessage::Running if !self.running => {
                self.running = true;
                return Ok(BuildEvent::Boot);
            }
            GuestMessage::StepExited(e) if self.step == Some(e.id) && self.exited.is_none() => {
                self.exited = Some(*e);
                return Ok(BuildEvent::Exited(*e));
            }
            GuestMessage::StepSealed(s)
                if self.step == Some(s.id) && self.exited.is_some_and(|e| e.code == Some(0)) =>
            {
                let sealed = s.clone();
                self.step = None;
                return Ok(BuildEvent::Sealed(sealed));
            }
            _ => {}
        }
        Err(format!(
            "{} out of order (stage {}, running {}, step {:?})",
            match msg {
                GuestMessage::Stage(s) => format!("Stage {}", s.n),
                GuestMessage::StepExited(e) => format!("StepExited for step {}", e.id),
                GuestMessage::StepSealed(s) => format!("StepSealed for step {}", s.id),
                GuestMessage::Running => "Running".into(),
                _ => "a run-mode message".into(),
            },
            self.stage,
            self.running,
            self.step
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_proto::{Hello, InitFailed, Stage};

    fn spec() -> BuildSpec {
        BuildSpec {
            vmm: VmmKind::Firecracker,
            kernel: "/k".into(),
            init: "/i".into(),
            layers: vec!["/l0".into(), "/l1".into(), "/l2".into()],
            root: 2,
            cache_disks: vec![("/c0".into(), "/root/.cache".into())],
            output: "/out".into(),
            output_size: 1 << 30,
            network: false,
            memory_mib: 512,
            cpus: 1,
            scratch_bytes: 1 << 30,
            boot_timeout: Duration::from_secs(30),
            source_date_epoch: None,
        }
    }

    fn step() -> RunStep {
        RunStep {
            id: 7,
            command: Command::Shell("true".into()),
            env: BTreeMap::from([("A".into(), "1".into())]),
            workdir: "/".into(),
            uid: 0,
            gid: 0,
            lowers_added: vec![2],
            mounts: vec!["/root/.cache".into()],
            secrets: vec![("/run/secrets/x".into(), Secret(b"s3cret".to_vec()))],
            network: true,
            timeout: Duration::from_secs(5),
        }
    }

    #[test]
    fn the_budget_counts_the_nic_only_with_a_network() {
        assert_eq!(device_budget(17, false), 13);
        assert_eq!(device_budget(17, true), 12);
        assert_eq!(device_budget(3, true), 0);
        spec().check(17).unwrap();
        assert!(spec().check(7).is_err(), "4 disks do not fit in 7 - 4");
        assert!(BuildSpec { root: 4, ..spec() }.check(17).is_err());
        assert!(
            BuildSpec {
                output_size: 100,
                ..spec()
            }
            .check(17)
            .is_err()
        );
    }

    #[test]
    fn disks_follow_the_layers_and_steps_name_them_by_index() {
        let s = spec();
        let c = s.config(ExitMethod::Reboot);
        c.validate().unwrap();
        assert_eq!((c.layers, c.disks.len()), (2, 2));
        assert_eq!(c.build_disk(BuildSpec::layer_disk(2)).unwrap().kind, DiskKind::Erofs);
        assert_eq!(c.build_disk(BuildSpec::layer_disk(3)).unwrap().kind, DiskKind::Ext4);
        let m = s.proto_step(&step(), &BTreeSet::new(), 0).unwrap();
        assert_eq!(m.lowers_added, [5]);
        assert_eq!(m.mounts[0].disk, 6);
        assert_eq!(m.env, ["A=1"]);
        assert!(!m.egress, "no network in the VM, no egress");
        assert_eq!(m.timeout_ms, 5000);
        assert!(s.proto_step(&step(), &BTreeSet::from([2]), 0).is_err(), "stacked twice");
        let base = RunStep {
            lowers_added: vec![1],
            ..step()
        };
        assert!(s.proto_step(&base, &BTreeSet::new(), 0).is_err(), "a root layer");
        let unknown = RunStep {
            mounts: vec!["/x".into()],
            ..step()
        };
        assert!(s.proto_step(&unknown, &BTreeSet::new(), 0).is_err());
        assert!(!format!("{:?}", step()).contains("s3cret"));
    }

    /// Ruling 2: a step past the lowers cap is its own error, before anything is
    /// sent; the count is root layers, stacked layers and sealed steps.
    #[test]
    fn a_step_past_the_lowers_cap_is_refused_with_its_own_error() {
        let s = spec();
        assert_eq!(s.lowers(&BTreeSet::from([2]), 5), 8);
        let at_cap = MAX_BUILD_LOWERS - s.root - 1;
        s.proto_step(&step(), &BTreeSet::new(), at_cap).unwrap();
        let err = s.proto_step(&step(), &BTreeSet::new(), at_cap + 1).unwrap_err();
        assert!(
            matches!(
                err,
                Error::TooManyLowers {
                    step: 7,
                    lowers: 231,
                    max: MAX_BUILD_LOWERS
                }
            ),
            "{err:?}"
        );
        assert!(err.to_string().contains("boot a new build VM"), "{err}");
    }

    #[test]
    fn source_date_epoch_reaches_the_config() {
        let c = BuildSpec {
            source_date_epoch: Some(1_700_000_000),
            ..spec()
        }
        .config(ExitMethod::Reboot);
        c.validate().unwrap();
        assert_eq!(c.source_date_epoch, Some(1_700_000_000));
        assert_eq!(spec().config(ExitMethod::Reboot).source_date_epoch, None);
    }

    #[test]
    fn secrets_must_fit_a_frame() {
        let big = RunStep {
            secrets: vec![("/s".into(), Secret(vec![7; 50_000]))],
            ..step()
        };
        let err = spec().proto_step(&big, &BTreeSet::new(), 0).unwrap_err().to_string();
        assert!(err.contains("inline"), "{err}");
        let ok = RunStep {
            secrets: vec![("/s".into(), Secret(vec![7; 40_000]))],
            ..step()
        };
        spec().proto_step(&ok, &BTreeSet::new(), 0).unwrap();
    }

    fn boot(p: &mut BuildProtocol) {
        for m in [
            GuestMessage::Hello(Hello { protocol: 3 }),
            GuestMessage::Stage(Stage { n: 3 }),
            GuestMessage::Stage(Stage { n: 6 }),
            GuestMessage::Running,
        ] {
            assert_eq!(p.accept(&m), Ok(BuildEvent::Boot));
        }
    }

    fn exited(id: u32, code: i32) -> GuestMessage {
        GuestMessage::StepExited(StepExited {
            id,
            code: Some(code),
            signal: None,
        })
    }

    fn sealed(id: u32) -> GuestMessage {
        GuestMessage::StepSealed(StepSealed {
            id,
            offset: 0,
            len: 4096,
            sha256: "0".repeat(64),
            entries: 1,
        })
    }

    #[test]
    fn steps_exit_then_seal_with_their_own_id() {
        let mut p = BuildProtocol::default();
        assert!(p.start_step(1).is_err(), "not running yet");
        boot(&mut p);
        p.start_step(1).unwrap();
        assert!(p.start_step(2).is_err(), "one step at a time");
        assert!(p.accept(&exited(2, 0)).is_err(), "another step's id");
        let mut q = p.clone();
        assert!(matches!(p.accept(&exited(1, 0)), Ok(BuildEvent::Exited(_))));
        assert!(p.accept(&exited(1, 0)).is_err(), "twice");
        assert!(p.accept(&sealed(2)).is_err());
        assert!(matches!(p.accept(&sealed(1)), Ok(BuildEvent::Sealed(_))));
        assert!(p.accept(&sealed(1)).is_err(), "after the step");
        p.start_step(2).unwrap();
        assert!(matches!(p.accept(&exited(2, 1)), Ok(BuildEvent::Exited(_))));
        assert!(p.accept(&sealed(2)).is_err(), "no seal after a failure");
        p.end_step();
        p.start_step(3).unwrap();
        assert!(q.accept(&sealed(1)).is_err(), "a seal before the exit");
        assert!(matches!(
            q.accept(&GuestMessage::InitFailed(InitFailed::new(7, None, "x"))),
            Ok(BuildEvent::Failed(_))
        ));
        assert!(q.accept(&exited(1, 0)).is_err());
    }

    #[test]
    fn boot_messages_come_in_order_and_once() {
        let mut p = BuildProtocol::default();
        assert!(p.accept(&GuestMessage::Hello(Hello { protocol: 2 })).is_err());
        let mut p = BuildProtocol::default();
        boot(&mut p);
        assert!(p.accept(&GuestMessage::Running).is_err());
        assert!(p.accept(&GuestMessage::Stage(Stage { n: 6 })).is_err());
        assert!(p.accept(&GuestMessage::Hello(Hello { protocol: 3 })).is_err());
        assert!(p.accept(&exited(1, 0)).is_err(), "no step sent");
    }
}
```

- [ ] **Step 2: Modify `crates/kiln-run/src/error.rs`**

Apply (unified diff against the previous task's tree):

```diff
--- a/crates/kiln-run/src/error.rs
+++ b/crates/kiln-run/src/error.rs
@@ -19,6 +19,15 @@ pub enum Error {
     Refused(String),
     #[error("invalid {what}: {reason}")]
     Invalid { what: &'static str, reason: String },
+    /// A build step would mount more lower layers than a build VM's overlay can
+    /// name (`kiln_proto::MAX_BUILD_LOWERS`): base layers, stacked layers and
+    /// one sealed upper per successful step. Refused before anything is sent, so
+    /// the VM is still usable: finish it, then squash layers or boot a new one.
+    #[error(
+        "step {step} would mount {lowers} lower layers, more than a build VM's {max}: squash layers or boot a new \
+         build VM"
+    )]
+    TooManyLowers { step: u32, lowers: usize, max: usize },
     /// SIGINT or SIGTERM arrived during setup: the run was abandoned (exit 137).
     #[error("interrupted before the guest started")]
     Interrupted,
```

- [ ] **Step 3: Modify `crates/kiln-run/src/lib.rs`**

Apply (unified diff against the previous task's tree):

```diff
--- a/crates/kiln-run/src/lib.rs
+++ b/crates/kiln-run/src/lib.rs
@@ -6,6 +6,7 @@
 //! instead ([`LIMA_INSTRUCTIONS`]).
 #![forbid(unsafe_code)]
 
+pub mod build;
 pub mod config;
 pub mod console;
 mod error;
```

- [ ] **Step 4: Check (macOS) and commit**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p kiln-run --lib build`
Expected: clean; 7 tests, among them `a_step_past_the_lowers_cap_is_refused_with_its_own_error` (231 lowers → `TooManyLowers { step: 7, lowers: 231, max: 230 }`) and `source_date_epoch_reaches_the_config`; the workspace has 7 more tests than after Task 3. On Linux (Lima) clippy and the workspace's tests are clean too.

```bash
git add -A && git commit -m "feat(kiln-run): build mode's host model: BuildSpec, RunStep, the device budget, Config and RunStep mapping, and the guest's message order"
```

---

### Task 5: `kiln-run`: `BuildVm`: boot a build VM, run steps with bounded deadlines and per-step output streams, pass seals through

**Files:**
- Create: `crates/kiln-run/src/linux/build.rs`
- Modify: `crates/kiln-run/src/build.rs` (re-export), `crates/kiln-run/src/linux/{mod,session}.rs`

**Interfaces:**
- Consumes: Task 4's model; M3b-1/M3b-2's `rundir::{RunDir, RunInfo (with persist: None), ScratchDir, ScratchInfo}`, `ConsoleRelay`, `session::bind` (made `pub(super)`), `run::{CMDLINE, backend, exit_method, vmm_identity}`, `protocol::version_refusal`; vmkit's `VmSpec { .., tap: None }`, `NetSpec::default()`.
- Produces: `kiln_run::build::{BuildVm, END_TIMEOUT, max_devices}` (re-exported from `kiln_run::build` on Linux, and `kiln_run::BuildVm` through `linux::*`): `BuildVm { start(&Store, BuildSpec), run_step(&mut self, RunStep, &mut dyn FnMut(Stream, &[u8])) -> Result<StepOutcome>, output_path(), run_dir(), finish(self) }`, `END_TIMEOUT` (30 s), `max_devices(&dyn Vmm, network) -> usize`.

- **`start`**: discover the VMM, `spec.check(available_devices)`, the output disk's length must equal `output_size` (kiln never reads it), the init layer from the static binary (`--init`'s way), run and scratch directories (kept with `KILN_KEEP_RUN_DIR=1`), the scratch disk, the disks in order (init, scratch, output, layers, cache disks), vsock, a NIC only with `network`; listeners on ports 1024, 1026, 1027; boot bounded by `boot_timeout` (the console's tail in the error).
- **Threads**: a listener (control served once; stream connections go to `run_step`), the control reader (answers `Hello` with `Config` after `version_refusal`, then starts the writer), a writer with a bounded queue and a 5 s send timeout, one reader per step stream, all into one bounded channel that only the caller's thread drains.
- **`run_step`**: a dead VM refuses; `proto_step` (a refusal, `TooManyLowers` included, sends nothing and keeps the VM); `RunStep`; at `timeout` `Signal 9`, at `timeout + STEP_GRACE` kill the VM (`TimedOut`); the step is done when `StepExited`, both streams' EOF and (after 0) `StepSealed` are in; a timed-out step kills the VM even if it answered (R7); a sealed step counts one more lower. Returns within `timeout + STEP_GRACE + KILL_WAIT`. Any `Err` but `TooManyLowers` (and the spec checks) means the VM is gone.
- **`finish`**: `Finish`, then kill after `END_TIMEOUT`. `Drop` kills the VM and removes the run and scratch directories.

- [ ] **Step 1: Modify `crates/kiln-run/src/build.rs`**

Apply (unified diff against the previous task's tree):

```diff
--- a/crates/kiln-run/src/build.rs
+++ b/crates/kiln-run/src/build.rs
@@ -21,6 +21,9 @@ use kiln_proto::{
 use crate::error::{Error, Result};
 use crate::options::VmmKind;
 
+#[cfg(target_os = "linux")]
+pub use crate::linux::{BuildVm, END_TIMEOUT, max_devices};
+
 /// The devices every build VM has: init layer, scratch, output disk, vsock. A VM
 /// with `network` has a NIC too.
 pub const FIXED_DEVICES: usize = 4;
```

- [ ] **Step 2: Create `crates/kiln-run/src/linux/build.rs`**

```rust
//! A build VM (spec §9.8): boots `Config { mode: build }`, then runs one step at
//! a time and reports where each step's diff landed.
//!
//! Threads, as in a run's session ([`super::session`]): a listener over the
//! control, stdout and stderr ports (the control port is served once; each step
//! connects stdout and stderr anew, and a connection while no step runs, or a
//! second one in a step, is a violation); the control reader, which answers
//! `Hello` with `Config` and passes every guest message through
//! [`BuildProtocol`]; the control writer, the only thread that writes host
//! messages, from a bounded queue with a send timeout; and one reader per step
//! stream. They all feed one bounded channel, which only `run_step`, on the
//! caller's thread, drains: output goes to the caller's sink there.
//!
//! Every wait is bounded: `start` by the boot timeout; `run_step` signals the
//! step's process group with SIGKILL at its timeout and kills the VM
//! [`STEP_GRACE`] later, then waits at most [`KILL_WAIT`] for the VMM;
//! `finish` gives the guest [`END_TIMEOUT`] to end the VM. A protocol violation
//! kills the VM, and the `BuildVm` refuses further steps.
//!
//! kiln never reads the output disk, and passes `StepSealed` through after
//! checking only its shape and id: the caller checks the range (within the
//! disk, aligned, after the previous diff, zeros after the tar's end) and hashes
//! what it reads.

use std::collections::BTreeSet;
use std::io::{ErrorKind, Read};
use std::os::fd::OwnedFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use kiln_proto::{Config, GuestMessage, HostMessage, Signal, port, read_message, write_message};
use kiln_store::Store;
use rustix::event::{PollFd, PollFlags, poll};
use vmkit::{Disk, NetSpec, Vm, VmSpec, VsockSpec};

use super::console::ConsoleRelay;
use super::run::{CMDLINE, backend, exit_method, vmm_identity};
use super::session::bind;
use crate::build::{BuildEvent, BuildProtocol, BuildSpec, Exit, KILL_WAIT, RunStep, STEP_GRACE, StepOutcome, Stream};
use crate::console::{TAIL_LINES, tail};
use crate::error::{Error, Result};
use crate::rundir::{self, RUN_INFO_VERSION, RunDir, RunInfo, ScratchDir, ScratchInfo};

/// How long the guest has to end the VM after `Finish`.
pub const END_TIMEOUT: Duration = Duration::from_secs(30);
const QUEUE: usize = 256;
const WRITE_QUEUE: usize = 16;
const SEND_TIMEOUT: Duration = Duration::from_secs(5);
const TICK: Duration = Duration::from_millis(20);
/// The most bytes one output event carries.
const CHUNK: usize = 64 * 1024;

/// The largest `BuildSpec` a VMM can hold: its devices left for layer and
/// cache disks once the fixed ones (and a NIC with `network`) are counted.
pub fn max_devices(vmm: &dyn vmkit::Vmm, network: bool) -> usize {
    crate::build::device_budget(vmm.capabilities().available_devices(), network)
}

enum Event {
    Guest(GuestMessage),
    Violation(String),
    ControlClosed,
    Stream(u32, UnixStream),
    Output(Stream, Vec<u8>),
    Eof(Stream),
}

/// A running build VM.
pub struct BuildVm {
    spec: BuildSpec,
    vm: Box<dyn Vm>,
    events: Receiver<Event>,
    /// For the output readers.
    tx: SyncSender<Event>,
    out: SyncSender<HostMessage>,
    proto: BuildProtocol,
    stacked: BTreeSet<usize>,
    /// Steps sealed so far: each sealed upper is one more lower.
    sealed: usize,
    /// Why the VM is unusable, once it is.
    dead: Option<String>,
    wake: OwnedFd,
    listener: Option<JoinHandle<()>>,
    console: Option<ConsoleRelay>,
    readers: Vec<JoinHandle<()>>,
    /// Dropped after the VM, in this order.
    scratch_dir: Option<ScratchDir>,
    run_dir: Option<RunDir>,
}

impl BuildVm {
    /// Creates the run's directories, the init layer and the scratch disk,
    /// boots the VM and waits for the guest to be ready (`Running`).
    pub fn start(store: &Store, spec: BuildSpec) -> Result<Self> {
        let vmm = backend(spec.vmm).discover()?;
        spec.check(vmm.capabilities().available_devices())?;
        let len = std::fs::metadata(&spec.output)
            .map_err(|e| Error::refused(format!("the output disk {}: {e}", spec.output.display())))?
            .len();
        if len != spec.output_size {
            return Err(Error::refused(format!(
                "the output disk {} has {len} bytes, not {}",
                spec.output.display(),
                spec.output_size
            )));
        }
        let bin = std::fs::read(&spec.init)?;
        let init_layer = kiln_image::init_layer(&bin, &store.tmp_dir())?;

        let base = rundir::base_dir()?;
        let mut run_dir = RunDir::create(&base)?;
        let keep = std::env::var_os("KILN_KEEP_RUN_DIR").is_some_and(|v| v == "1");
        if keep {
            run_dir.keep();
        }
        let scratch_base = rundir::scratch_base(store.root())?;
        let scratch_path = scratch_base.join(&run_dir.id);
        let mut info = RunInfo {
            version: RUN_INFO_VERSION,
            id: run_dir.id.clone(),
            boot_id: rundir::boot_id(),
            hostname: rundir::hostname(),
            kiln: rundir::identity(std::process::id()).ok_or_else(|| Error::refused("cannot read /proc/self/stat"))?,
            vmm: None,
            vmm_kind: spec.vmm.name().into(),
            image: "(build)".into(),
            custom_kernel: Some(spec.kernel.display().to_string()),
            custom_init: Some(spec.init.display().to_string()),
            keep,
            scratch_dir: Some(scratch_path.display().to_string()),
            persist: None,
        };
        run_dir.write_info(&info)?;
        let mut scratch_dir = ScratchDir::create(&scratch_base, &ScratchInfo::from(&info))?;
        if keep {
            scratch_dir.keep();
        }
        let init = run_dir.path.join("init.erofs");
        std::fs::write(&init, &init_layer)?;
        let scratch = scratch_dir.path.join("scratch.img");
        kiln_image::scratch::create_scratch(&scratch, spec.scratch_bytes)?;

        let console = ConsoleRelay::start(&run_dir.path)?;
        let mut disks = vec![
            Disk {
                path: init,
                read_only: true,
            },
            Disk {
                path: scratch,
                read_only: false,
            },
            Disk {
                path: spec.output.clone(),
                read_only: false,
            },
        ];
        disks.extend(spec.layers.iter().map(|p| Disk {
            path: p.clone(),
            read_only: true,
        }));
        disks.extend(spec.cache_disks.iter().map(|(p, _)| Disk {
            path: p.clone(),
            read_only: false,
        }));
        let vm_spec = VmSpec {
            kernel: spec.kernel.clone(),
            initramfs: None,
            cmdline: CMDLINE.map(String::from).to_vec(),
            disks,
            vcpus: spec.cpus,
            memory_mib: spec.memory_mib,
            vsock: Some(VsockSpec {
                guest_cid: kiln_proto::GUEST_CID,
            }),
            // IPv4 only, vmkit's restricted egress; each step turns it on or off.
            net: spec.network.then(NetSpec::default),
            tap: None,
            console_log: console.fifo.clone(),
            run_dir: run_dir.path.clone(),
        };
        let config = spec.config(exit_method(vmm.as_ref()));
        config.validate()?;
        let mut vm = vmm.create(&vm_spec)?;
        info.vmm = vmm_identity(&run_dir.path);
        run_dir.write_info(&info)?;
        scratch_dir.write_info(&ScratchInfo::from(&info))?;

        let sock = vm
            .vsock_socket()
            .ok_or_else(|| Error::refused("the VM has no vsock device"))?
            .to_path_buf();
        let listeners = [port::CONTROL, port::STDOUT, port::STDERR]
            .iter()
            .map(|&p| Ok((p, bind(&sock, p)?)))
            .collect::<Result<Vec<_>>>()?;
        let (tx, events) = sync_channel(QUEUE);
        let (out, out_rx) = sync_channel(WRITE_QUEUE);
        let (wake_r, wake) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).map_err(std::io::Error::from)?;
        let listener = {
            let tx = tx.clone();
            std::thread::Builder::new()
                .name("build-listener".into())
                .spawn(move || listen(listeners, &wake_r, config, &tx, out_rx))?
        };
        if let Err(e) = vm.start() {
            let _ = rustix::io::write(&wake, b"x");
            let _ = listener.join();
            return Err(e.into());
        }
        let mut b = BuildVm {
            spec,
            vm,
            events,
            tx,
            out,
            proto: BuildProtocol::default(),
            stacked: BTreeSet::new(),
            sealed: 0,
            dead: None,
            wake,
            listener: Some(listener),
            console: Some(console),
            readers: Vec::new(),
            scratch_dir: Some(scratch_dir),
            run_dir: Some(run_dir),
        };
        let deadline = Instant::now() + b.spec.boot_timeout;
        while !b.proto.running() {
            if Instant::now() >= deadline {
                b.kill(format!(
                    "boot timeout: the guest was not ready {}s after the VM started",
                    b.spec.boot_timeout.as_secs()
                ));
            }
            if let Some(why) = &b.dead {
                return Err(Error::refused(format!(
                    "the build VM failed to boot: {why}{}",
                    b.console_tail()
                )));
            }
            b.turn(deadline, &mut |_, _| {});
        }
        Ok(b)
    }

    /// The output disk, for the caller to read each step's diff from.
    pub fn output_path(&self) -> &Path {
        &self.spec.output
    }

    /// The run directory (console log, VMM logs).
    pub fn run_dir(&self) -> Option<&Path> {
        self.run_dir.as_ref().map(|d| d.path.as_path())
    }

    /// Runs `step`, streaming its output to `sink`. Returns within
    /// `step.timeout + STEP_GRACE + KILL_WAIT`. An error means the VM is gone (a
    /// protocol violation, the guest failed, the VMM ended): no more steps.
    pub fn run_step(&mut self, step: RunStep, sink: &mut dyn FnMut(Stream, &[u8])) -> Result<StepOutcome> {
        if let Some(why) = &self.dead {
            return Err(Error::refused(format!("the build VM has ended: {why}")));
        }
        // A refusal here (the lowers cap included) sends nothing: the VM stays usable.
        let msg = self.spec.proto_step(&step, &self.stacked, self.sealed)?;
        self.proto.start_step(step.id).map_err(Error::refused)?;
        self.stacked.extend(step.lowers_added.iter().copied());
        self.send(HostMessage::RunStep(Box::new(msg)));
        let start = Instant::now();
        let deadline = start + step.timeout;
        let kill_at = deadline + STEP_GRACE;
        let mut timed_out = false;
        let mut s = StepState::default();
        loop {
            if self.dead.is_none() && !timed_out && Instant::now() >= deadline {
                timed_out = true;
                self.send(HostMessage::Signal(Signal { sig: 9 }));
            }
            if self.dead.is_none() && Instant::now() >= kill_at {
                self.kill(format!(
                    "step {} did not end within {}s of its {}s timeout",
                    step.id,
                    STEP_GRACE.as_secs(),
                    step.timeout.as_secs()
                ));
                return Ok(StepOutcome {
                    exit: Exit::TimedOut,
                    sealed: None,
                });
            }
            if let Some(why) = &self.dead {
                return Err(Error::refused(format!(
                    "step {}: {why}{}",
                    step.id,
                    self.console_tail()
                )));
            }
            if let Some(outcome) = s.done(timed_out) {
                self.proto.end_step();
                if timed_out {
                    // The guest's state after a killed step is not one the caller knows.
                    self.kill(format!("step {} timed out", step.id));
                }
                if outcome.sealed.is_some() {
                    self.sealed += 1;
                }
                return Ok(outcome);
            }
            let until = if timed_out { kill_at } else { deadline };
            self.step_turn(until, &mut s, sink);
        }
    }

    /// `Finish`: the guest syncs and ends the VM; killed after [`END_TIMEOUT`].
    pub fn finish(mut self) -> Result<()> {
        if self.dead.is_some() {
            return Ok(());
        }
        self.send(HostMessage::Finish);
        let deadline = Instant::now() + END_TIMEOUT;
        loop {
            match self.vm.wait_timeout(TICK) {
                Ok(Some(_)) => {
                    self.dead = Some("finished".into());
                    return Ok(());
                }
                Ok(None) if Instant::now() < deadline => {}
                _ => {
                    self.kill("the guest did not end the VM after Finish".into());
                    return Err(Error::refused(format!(
                        "the build VM did not end within {}s of Finish; it was killed",
                        END_TIMEOUT.as_secs()
                    )));
                }
            }
            // Messages after Finish are of no interest; keep the channel moving.
            while self.events.try_recv().is_ok() {}
        }
    }

    fn send(&mut self, msg: HostMessage) {
        match self.out.try_send(msg) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => self.kill("the guest is not reading its control channel".into()),
            Err(TrySendError::Disconnected(_)) => self.kill("the control connection is gone".into()),
        }
    }

    /// Kills the VM (once) and records why; waits at most [`KILL_WAIT`] for it.
    fn kill(&mut self, why: String) {
        if self.dead.is_some() {
            return;
        }
        self.dead = Some(why);
        let _ = self.vm.kill();
        let _ = self.vm.wait_timeout(KILL_WAIT);
    }

    fn console_tail(&self) -> String {
        let Some(c) = &self.console else {
            return String::new();
        };
        let lines = tail(&c.log, TAIL_LINES);
        if lines.is_empty() {
            String::new()
        } else {
            format!("\nthe console's end:\n{}", lines.join("\n"))
        }
    }

    /// One wait for an event outside a step (boot).
    fn turn(&mut self, until: Instant, sink: &mut dyn FnMut(Stream, &[u8])) {
        let mut s = StepState {
            active: false,
            ..StepState::default()
        };
        self.step_turn(until, &mut s, sink);
    }

    /// Waits up to a tick (or `until`) for one event and handles it; notices the VM's end.
    fn step_turn(&mut self, until: Instant, s: &mut StepState, sink: &mut dyn FnMut(Stream, &[u8])) {
        if self.dead.is_none() {
            match self.vm.wait_timeout(Duration::ZERO) {
                Ok(Some(end)) => {
                    self.dead = Some(format!("the VM ended ({:?})", end.reason));
                    return;
                }
                Ok(None) => {}
                Err(e) => {
                    self.kill(format!("waiting for the VMM: {e}"));
                    return;
                }
            }
        }
        let wait = TICK.min(until.saturating_duration_since(Instant::now()));
        let event = match self.events.recv_timeout(wait) {
            Ok(e) => e,
            Err(RecvTimeoutError::Timeout) => return,
            Err(RecvTimeoutError::Disconnected) => {
                self.kill("the listener stopped".into());
                return;
            }
        };
        match event {
            Event::Guest(msg) => match self.proto.accept(&msg) {
                Ok(BuildEvent::Boot) => {}
                Ok(BuildEvent::Exited(e)) => s.exited = Some(e),
                Ok(BuildEvent::Sealed(seal)) => s.sealed = Some(seal),
                Ok(BuildEvent::Failed(f)) => self.kill(f.describe()),
                Err(why) => self.kill(format!("guest protocol violation: {why}")),
            },
            Event::Violation(why) => self.kill(why),
            Event::ControlClosed => self.kill("the guest closed the control connection".into()),
            Event::Stream(p, stream) => {
                let which = if p == port::STDOUT {
                    Stream::Stdout
                } else {
                    Stream::Stderr
                };
                let slot = match which {
                    Stream::Stdout => &mut s.stdout,
                    Stream::Stderr => &mut s.stderr,
                };
                if !s.active || *slot != StreamState::Waiting {
                    self.kill(format!("an unexpected connection on vsock port {p}"));
                    return;
                }
                *slot = StreamState::Open;
                let tx = self.tx.clone();
                match std::thread::Builder::new()
                    .name("build-output".into())
                    .spawn(move || read_stream(stream, which, &tx))
                {
                    Ok(h) => self.readers.push(h),
                    Err(e) => self.kill(format!("spawn an output reader: {e}")),
                }
            }
            Event::Output(which, bytes) => sink(which, &bytes),
            Event::Eof(which) => match which {
                Stream::Stdout => s.stdout = StreamState::Done,
                Stream::Stderr => s.stderr = StreamState::Done,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum StreamState {
    #[default]
    Waiting,
    Open,
    Done,
}

#[derive(Debug)]
struct StepState {
    /// A step is running: its stream connections are expected.
    active: bool,
    stdout: StreamState,
    stderr: StreamState,
    exited: Option<kiln_proto::StepExited>,
    sealed: Option<kiln_proto::StepSealed>,
}

impl Default for StepState {
    fn default() -> Self {
        Self {
            active: true,
            stdout: StreamState::Waiting,
            stderr: StreamState::Waiting,
            exited: None,
            sealed: None,
        }
    }
}

impl StepState {
    /// The step's outcome, once its exit, both streams' EOF and (after exit 0) its seal are in.
    fn done(&self, timed_out: bool) -> Option<StepOutcome> {
        let e = self.exited?;
        if self.stdout != StreamState::Done || self.stderr != StreamState::Done {
            return None;
        }
        let exit = match (timed_out, e.code, e.signal) {
            (true, _, _) => Exit::TimedOut,
            (false, Some(c), _) => Exit::Code(c),
            (false, None, Some(sig)) => Exit::Signal(sig),
            (false, None, None) => unreachable!("validated: a code or a signal"),
        };
        if e.code == Some(0) {
            let sealed = self.sealed.clone()?;
            return Some(StepOutcome {
                exit: if timed_out { Exit::TimedOut } else { exit },
                sealed: (!timed_out).then_some(sealed),
            });
        }
        Some(StepOutcome { exit, sealed: None })
    }
}

impl Drop for BuildVm {
    fn drop(&mut self) {
        if self.dead.is_none() {
            let _ = self.vm.kill();
            let _ = self.vm.wait_timeout(KILL_WAIT);
        }
        let _ = rustix::io::write(&self.wake, b"x");
        if let Some(l) = self.listener.take() {
            let _ = l.join();
        }
        // Unblocks readers waiting on a full channel.
        while self.events.try_recv().is_ok() {}
        if let Some(c) = self.console.take() {
            c.finish(Duration::from_secs(2));
        }
        drop(self.scratch_dir.take());
        drop(self.run_dir.take());
    }
}

/// Accepts on the control, stdout and stderr ports until woken. The control
/// port is served once; stream connections go to `run_step`, which decides.
fn listen(
    listeners: Vec<(u32, UnixListener)>,
    wake: &OwnedFd,
    config: Config,
    tx: &SyncSender<Event>,
    out: Receiver<HostMessage>,
) {
    let mut out = Some(out);
    let mut control = false;
    loop {
        let mut fds: Vec<PollFd<'_>> = listeners
            .iter()
            .map(|(_, l)| PollFd::new(l, PollFlags::IN))
            .chain([PollFd::new(wake, PollFlags::IN)])
            .collect();
        match poll(&mut fds, None) {
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => {
                let _ = tx.send(Event::Violation(format!("vsock listener: {e}")));
                return;
            }
        }
        let ready: Vec<bool> = fds.iter().map(|f| !f.revents().is_empty()).collect();
        drop(fds);
        if ready.last().copied().unwrap_or(true) {
            return;
        }
        for (i, (p, l)) in listeners.iter().enumerate() {
            if !ready[i] {
                continue;
            }
            let stream = match l.accept() {
                Ok((s, _)) => s,
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => continue,
                Err(e) => {
                    let _ = tx.send(Event::Violation(format!("accept on vsock port {p}: {e}")));
                    return;
                }
            };
            if stream.set_nonblocking(false).is_err() {
                continue;
            }
            if *p != port::CONTROL {
                if tx.send(Event::Stream(*p, stream)).is_err() {
                    return;
                }
                continue;
            }
            if control {
                let _ = tx.send(Event::Violation("a second connection on the control port".into()));
                continue;
            }
            control = true;
            let (config, ctx, out) = (config.clone(), tx.clone(), out.take().expect("served once"));
            let spawned = std::thread::Builder::new()
                .name("build-control".into())
                .spawn(move || read_control(stream, &config, &ctx, out));
            if let Err(e) = spawned {
                let _ = tx.send(Event::Violation(format!("spawn the control reader: {e}")));
            }
        }
    }
}

/// Reads guest messages; answers a version 3 `Hello` with `Config` and then
/// starts the writer. The main loop checks the order ([`BuildProtocol`]).
fn read_control(stream: UnixStream, config: &Config, tx: &SyncSender<Event>, out: Receiver<HostMessage>) {
    let violation = |why: String| {
        let _ = tx.send(Event::Violation(why));
    };
    if let Err(e) =
        rustix::net::sockopt::set_socket_timeout(&stream, rustix::net::sockopt::Timeout::Send, Some(SEND_TIMEOUT))
    {
        return violation(format!("control socket: setting its send timeout: {e}"));
    }
    let mut reader = match stream.try_clone() {
        Ok(r) => r,
        Err(e) => return violation(format!("control socket: {e}")),
    };
    let mut writer = Some((stream, out));
    loop {
        let msg = match read_message::<_, GuestMessage>(&mut reader) {
            Ok(Some(m)) => m,
            Ok(None) => {
                let _ = tx.send(Event::ControlClosed);
                return;
            }
            Err(e) => return violation(format!("guest protocol error: {e}")),
        };
        let hello = matches!(msg, GuestMessage::Hello(_));
        let version = if let GuestMessage::Hello(h) = &msg {
            h.protocol
        } else {
            0
        };
        if tx.send(Event::Guest(msg)).is_err() {
            return;
        }
        if hello && let Some((mut w, out)) = writer.take() {
            if let Some(why) = crate::protocol::version_refusal(version, config) {
                return violation(why);
            }
            if let Err(e) = write_message(&mut w, &HostMessage::Config(Box::new(config.clone()))) {
                return violation(format!("sending Config: {e}"));
            }
            let tx = tx.clone();
            let _ = std::thread::Builder::new()
                .name("build-writer".into())
                .spawn(move || write_control(w, &out, &tx));
        }
    }
}

/// Writes queued host messages; a stalled send is a violation.
fn write_control(mut w: UnixStream, out: &Receiver<HostMessage>, tx: &SyncSender<Event>) {
    for msg in out {
        if let Err(e) = write_message(&mut w, &msg) {
            if let kiln_proto::ProtoError::Io(io) = &e
                && matches!(io.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
            {
                let _ = tx.send(Event::Violation("the guest is not reading its control channel".into()));
            }
            return;
        }
    }
}

/// Streams one step's output to the main loop, then its EOF.
fn read_stream(mut from: UnixStream, which: Stream, tx: &SyncSender<Event>) {
    let mut buf = vec![0u8; CHUNK];
    loop {
        match from.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if tx.send(Event::Output(which, buf[..n].to_vec())).is_err() {
                    return;
                }
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    let _ = tx.send(Event::Eof(which));
}
```

- [ ] **Step 3: Modify `crates/kiln-run/src/linux/mod.rs`**

Apply (unified diff against the previous task's tree):

```diff
--- a/crates/kiln-run/src/linux/mod.rs
+++ b/crates/kiln-run/src/linux/mod.rs
@@ -1,11 +1,13 @@
 //! The Linux runtime: vmkit, the session, the terminal and signals.
 
+mod build;
 mod console;
 mod run;
 mod session;
 mod signals;
 pub mod tty;
 
+pub use build::{BuildVm, END_TIMEOUT, max_devices};
 pub use console::ConsoleRelay;
 pub use run::{CMDLINE, Report, Run, backend, exit_method, net_spec, vm_spec, vmm_identity};
 pub use session::{EXIT_KILLED, Handle, Outcome, Session, SessionOptions, Streams};
```

- [ ] **Step 4: Modify `crates/kiln-run/src/linux/session.rs`**

Apply (unified diff against the previous task's tree):

```diff
--- a/crates/kiln-run/src/linux/session.rs
+++ b/crates/kiln-run/src/linux/session.rs
@@ -434,7 +434,7 @@ fn port_path(base: &Path, p: u32) -> PathBuf {
     PathBuf::from(format!("{}_{p}", base.display()))
 }
 
-fn bind(base: &Path, p: u32) -> Result<UnixListener> {
+pub(super) fn bind(base: &Path, p: u32) -> Result<UnixListener> {
     use std::os::unix::fs::FileTypeExt;
     let path = port_path(base, p);
     match std::fs::symlink_metadata(&path) {
```

- [ ] **Step 5: Check (macOS, Linux) and commit**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace` (macOS: no change in counts; Lima: clean, all pass).

```bash
git add -A && git commit -m "feat(kiln-run): BuildVm: boot a build VM, run steps with bounded deadlines and per-step output streams, pass seals through"
```

---

### Task 6: Boot tests: two steps sealed, stacked and converted; no way to the output disk; `SOURCE_DATE_EPOCH`; mixed versions; a timeout; a hostile seal

**Files:**
- Create: `crates/kiln-run/tests/build.rs`
- Modify: `crates/kiln-run/Cargo.toml` (dev-dependency `sha2`), `crates/kiln-run/tests/support/mod.rs`, `crates/kiln-testguest/src/main.rs`, `scripts/boot-tests.sh`

**Interfaces:**
- Consumes: Tasks 1–5; the boot suite's `support` (`Case`, `fixtures`, `env`, `require`, `BOOT`).
- Produces: `support::{base_tar, convert_tars, tar_mtimes}`, `Env.init_v2` (`KILN_TEST_INIT_V2`); `hostile` answers a build-mode `Config` by sealing its first step at `[u64::MAX - 4095, +1 TiB)`; `scripts/boot-tests.sh` runs `--test build` too.

Each case runs on both VMMs and skips itself without the suite's environment (unless `KILN_REQUIRE_KVM_TESTS=1`):
- `build_steps_seal_stack_and_convert`: step 1 (files, a hard link, 200-byte names, a cache write, a secret, stdout and stderr apart, a new workdir), step 2 (stacks a layer, whiteout, opaque directory, reads the cache back), a failed step 3 (discarded), step 4 as uid 1000 in a new workdir with a secret that is `400 1000 1000` (R4), a missing command (127). Every diff is read as potter reads it (in bounds, aligned, packed, hashed, zeros after the end), holds no secret bytes, no mount point, `/etc` file or cache content (only `/root`, copied up, R7); kiln converts base + diff 1 + diff 2 into an image that runs.
- `steps_cannot_reach_the_output_disk` (R1, G-S): as root, `/dev` is exactly `fd full null ptmx pts random shm stderr stdin stdout tty urandom zero`; no `/dev/vdc`; `mount -t devtmpfs` fails; `mknod` of `vdc`'s major:minor fails in `/dev` and in `/tmp`, and so does opening it; `/proc/1/root/dev/vdc` cannot be read; `/sys` cannot be written.
- `source_date_epoch_clamps_the_diff` (R3): with `source_date_epoch = 1e9` every entry, `root/` (copied up for the cache mount point) included, has mtime exactly 1e9; without it, every mtime is later.
- `a_version_2_guest_is_refused_build_mode`: with `KILN_TEST_INIT_V2`, `BuildVm::start` fails naming version 3 (skipped without it, P7).
- `a_step_past_its_timeout_is_killed`: a 5 s timeout returns `TimedOut` within `5 s + STEP_GRACE + KILL_WAIT` with the output the step wrote before it; the next step is refused (5 s, not the prototype's 2 s: under parallel load the first output line sometimes missed a 2 s deadline).
- `a_seal_outside_the_disk_is_passed_through` (`KILN_TEST_HOSTILE`): kiln passes the bogus range through; the caller's check refuses it.

- [ ] **Step 1: Modify `crates/kiln-run/Cargo.toml`**

Apply (unified diff against the previous task's tree):

```diff
--- a/crates/kiln-run/Cargo.toml
+++ b/crates/kiln-run/Cargo.toml
@@ -24,4 +24,5 @@ vmkit.workspace = true
 [dev-dependencies]
 kiln-erofs = { path = "../kiln-erofs" }
 kiln-oci = { path = "../kiln-oci" }
+sha2 = "0.11.0"
 tempfile = "3.27.0"
```

- [ ] **Step 2: Create `crates/kiln-run/tests/build.rs`**

```rust
//! Build mode on real VMs (spec §9.8): steps stacked and sealed onto the output
//! disk, read back and converted by kiln into an image that runs. See `support`
//! for what the suite needs; without it every test is skipped.
#![cfg(target_os = "linux")]

mod support;

use std::collections::BTreeMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kiln_erofs::{Image, LayerWriter, Limits};
use kiln_proto::{Command, StepSealed};
use kiln_run::build::{BuildSpec, Exit, KILL_WAIT, RunStep, STEP_GRACE, Secret, StepOutcome, Stream};
use kiln_run::{BuildVm, VmmKind};
use kiln_store::Store;
use support::{Case, fixtures};
use vmkit::Backend;

const SECRET: &[u8] = b"kiln-secret-7f3a9c";
const OUTPUT: u64 = 256 << 20;

fn kind(backend: Backend) -> VmmKind {
    match backend {
        Backend::Firecracker => VmmKind::Firecracker,
        Backend::CloudHypervisor => VmmKind::CloudHypervisor,
    }
}

/// A build VM over `layers` with one cache disk at `/root/.cache`.
fn spec(c: &Case, backend: Backend, layers: Vec<PathBuf>, root: usize) -> BuildSpec {
    let env = support::env().unwrap();
    let dir = c.dir.path();
    let output = dir.join("output.img");
    std::fs::File::create(&output).unwrap().set_len(OUTPUT).unwrap();
    let cache = dir.join("cache.img");
    kiln_image::scratch::create_scratch(&cache, 128 << 20).unwrap();
    BuildSpec {
        vmm: kind(backend),
        kernel: env.kernel.clone(),
        init: env.init.clone(),
        layers,
        root,
        cache_disks: vec![(cache, "/root/.cache".into())],
        output,
        output_size: OUTPUT,
        network: false,
        memory_mib: 512,
        cpus: 1,
        scratch_bytes: 1 << 30,
        boot_timeout: support::BOOT,
        source_date_epoch: None,
    }
}

fn store() -> Store {
    Store::open(fixtures().dir.join("store")).unwrap()
}

fn sh(id: u32, script: &str) -> RunStep {
    RunStep {
        id,
        command: Command::Shell(script.into()),
        env: BTreeMap::new(),
        workdir: "/".into(),
        uid: 0,
        gid: 0,
        lowers_added: Vec::new(),
        mounts: Vec::new(),
        secrets: Vec::new(),
        network: false,
        timeout: Duration::from_secs(60),
    }
}

/// Runs a step, collecting its output by stream.
fn run(vm: &mut BuildVm, step: RunStep) -> (StepOutcome, String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let o = vm
        .run_step(step, &mut |s, b| match s {
            Stream::Stdout => out.extend_from_slice(b),
            Stream::Stderr => err.extend_from_slice(b),
        })
        .expect("the step runs");
    (
        o,
        String::from_utf8_lossy(&out).into(),
        String::from_utf8_lossy(&err).into(),
    )
}

/// The sealed range, as potter reads it: in bounds, aligned, after the
/// previous diff, hashed by the reader, zeros after the tar's end.
fn read_diff(output: &Path, s: &StepSealed, after: u64) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    use std::io::{Read, Seek, SeekFrom};
    assert!(
        s.offset.is_multiple_of(4096) && s.len.is_multiple_of(4096) && s.offset >= after,
        "{s:?}"
    );
    assert!(s.offset + s.len <= OUTPUT, "{s:?}");
    let mut f = std::fs::File::open(output).unwrap();
    f.seek(SeekFrom::Start(s.offset)).unwrap();
    let mut buf = vec![0u8; s.len as usize];
    f.read_exact(&mut buf).unwrap();
    let hex: String = Sha256::digest(&buf).iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(hex, s.sha256, "the guest's hash matches what was read");
    buf
}

/// The diff's paths (as kiln's converter reads them), whiteouts as `.wh.` names.
fn diff_paths(tar: &[u8]) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let mut w = LayerWriter::new(Cursor::new(Vec::new()), dir.path(), Limits::default()).unwrap();
    w.append_tar(tar).unwrap();
    let (out, _) = w.finish(&BTreeMap::new()).unwrap();
    let mut img = Image::open(Cursor::new(out.into_inner())).unwrap();
    let mut paths = Vec::new();
    let mut queue = vec![(String::new(), img.root_nid())];
    while let Some((path, nid)) = queue.pop() {
        let info = img.inode(nid).unwrap();
        if info.is_dir() {
            let opaque = img
                .xattrs(nid)
                .unwrap()
                .into_keys()
                .any(|k| k.full_name().ends_with(b"overlay.opaque"));
            if opaque {
                paths.push(format!("{path}/.wh..wh..opq"));
            }
            for e in img.read_dir(nid).unwrap() {
                queue.push((format!("{path}/{}", String::from_utf8_lossy(&e.name)), e.nid));
            }
        }
        if info.is_whiteout() {
            let (parent, name) = path.rsplit_once('/').unwrap();
            paths.push(format!("{parent}/.wh.{name}"));
        } else if !path.is_empty() {
            paths.push(path);
        }
    }
    paths.sort();
    paths
}

fn build_steps_seal_stack_and_convert(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    let f = fixtures();
    // The base image, then a layer stacked only by step 2.
    let layers = vec![f.base[0].clone(), f.stack[3].clone()];
    let spec = spec(&c, backend, layers, 1);
    let output = spec.output.clone();
    let started = Instant::now();
    let mut vm = BuildVm::start(&store(), spec).expect("boot the build VM");
    let booted = started.elapsed();
    let long = "n".repeat(200);

    // Step 1: files, a hard link, long names, a cache write, a secret, a FIFO,
    // stdout and stderr apart, a working directory created as root.
    let mut s1 = sh(
        1,
        &format!(
            "set -e; pwd; mkdir -p /data /opq/deep; echo a > /data/a; echo b > /data/b; ln /data/b /data/hard; \
             echo x > /opq/x; mkdir -p /data/{long}; echo long > /data/{long}/{long}; \
             cat /run/secrets/token; echo cached > /root/.cache/c; echo err >&2; touch here"
        ),
    );
    s1.workdir = "/work/sub".into();
    s1.secrets = vec![("/run/secrets/token".into(), Secret(SECRET.to_vec()))];
    s1.mounts = vec!["/root/.cache".into()];
    let t1 = Instant::now();
    let (o1, out1, err1) = run(&mut vm, s1);
    let step1 = t1.elapsed();
    assert_eq!(o1.exit, Exit::Code(0), "{out1}\n{err1}");
    assert_eq!(out1, format!("/work/sub\n{}", String::from_utf8_lossy(SECRET)));
    assert_eq!(err1, "err\n");
    let seal1 = o1.sealed.expect("sealed");
    assert_eq!((seal1.id, seal1.offset), (1, 0));

    // Step 2: stack layer 1, delete a lower file (whiteout), replace a lower
    // directory (opaque), read the cache disk back.
    let mut s2 = sh(
        2,
        "set -e; cat /layers/03; cat /root/.cache/c; rm /data/a; rm -rf /opq; mkdir /opq; echo y > /opq/y; \
         test -f /work/sub/here",
    );
    s2.lowers_added = vec![1];
    s2.mounts = vec!["/root/.cache".into()];
    let (o2, out2, err2) = run(&mut vm, s2);
    assert_eq!(o2.exit, Exit::Code(0), "{out2}\n{err2}");
    assert_eq!(out2, "3\ncached\n");
    let seal2 = o2.sealed.expect("sealed");
    assert_eq!(seal2.offset, seal1.offset + seal1.len, "diffs are packed, aligned");

    // Step 3 fails: its changes are discarded.
    let (o3, _, _) = run(&mut vm, sh(3, "echo bad > /f; exit 3"));
    assert_eq!(
        o3,
        StepOutcome {
            exit: Exit::Code(3),
            sealed: None
        }
    );

    // Step 4: as uid 1000 in a new working directory, on top of 1 and 2 only.
    // Its secret is mode 0400, owned by the step's uid and gid (ruling 4).
    let mut s4 = sh(
        4,
        "id -u; id -g; pwd; test ! -e /f && test ! -e /data/a && cat /opq/y; stat -c '%a %u %g' /run/secrets/t",
    );
    (s4.uid, s4.gid, s4.workdir) = (1000, 1000, "/home/app/w".into());
    s4.secrets = vec![("/run/secrets/t".into(), Secret(SECRET.to_vec()))];
    let (o4, out4, err4) = run(&mut vm, s4);
    assert_eq!(o4.exit, Exit::Code(0), "{out4}\n{err4}");
    assert_eq!(out4, "1000\n1000\n/home/app/w\ny\n400 1000 1000\n");
    let seal4 = o4.sealed.expect("sealed");
    // A command that does not exist: 127, with kiln-init's message on stderr.
    let mut missing = sh(5, "x");
    missing.command = Command::Argv(vec!["/nonexistent".into()]);
    let (o5, _, err5) = run(&mut vm, missing);
    assert_eq!(o5.exit, Exit::Code(127), "{err5}");
    assert!(err5.contains("exec /nonexistent"), "{err5}");
    vm.finish().expect("the guest ends the VM");

    let d1 = read_diff(&output, &seal1, 0);
    let d2 = read_diff(&output, &seal2, seal1.offset + seal1.len);
    let d4 = read_diff(&output, &seal4, seal2.offset + seal2.len);
    for (d, s) in [(&d1, &seal1), (&d2, &seal2), (&d4, &seal4)] {
        assert!(
            !d.windows(SECRET.len()).any(|w| w == SECRET),
            "a secret in diff {}",
            s.id
        );
    }
    let p1 = diff_paths(&d1);
    let want1 = [
        "/data".to_string(),
        "/data/a".into(),
        "/data/b".into(),
        "/data/hard".into(),
        format!("/data/{long}"),
        format!("/data/{long}/{long}"),
        "/opq".into(),
        "/opq/deep".into(),
        "/opq/x".into(),
        // Copied up when the cache disk's mount point was made (and removed).
        "/root".into(),
        "/work".into(),
        "/work/sub".into(),
        "/work/sub/here".into(),
    ];
    let mut want1 = want1.to_vec();
    want1.sort();
    assert_eq!(p1, want1, "no mount points, /etc files, secrets or cache content");
    assert_eq!(seal1.entries, want1.len() as u64);
    let p2 = diff_paths(&d2);
    assert_eq!(
        p2,
        ["/data", "/data/.wh.a", "/opq", "/opq/.wh..wh..opq", "/opq/y", "/root"],
        "{p2:?}"
    );
    let p4 = diff_paths(&d4);
    assert_eq!(p4, ["/home", "/home/app", "/home/app/w"], "{p4:?}");

    // kiln converts the base and the sealed diffs into an image that runs.
    let image = support::convert_tars("built", vec![support::base_tar(), d1, d2]);
    let config = c.config(
        &image,
        &[
            "/bin/sh",
            "-c",
            "cat /data/b /data/hard /opq/y; test ! -e /data/a && test ! -e /opq/x && ls /work/sub",
        ],
    );
    let o = c.run(&image, config);
    assert_eq!(o.exit_code(), 0, "{o:?}\n{}", c.tail());
    assert_eq!(o.stdout(), "b\nb\ny\nhere\n");
    eprintln!(
        "{backend:?}: build VM boot {} ms, step 1 {} ms; diffs {} + {} + {} bytes",
        booted.as_millis(),
        step1.as_millis(),
        seal1.len,
        seal2.len,
        seal4.len
    );
}

/// Ruling 1: a root step has no block device in its `/dev`, cannot mount one,
/// cannot make (or open) a node for the output disk in `/dev` or in `/tmp`, and
/// cannot reach init's root (and its `/dev`) through `/proc/1/root`.
fn steps_cannot_reach_the_output_disk(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    let spec = spec(&c, backend, vec![fixtures().base[0].clone()], 1);
    let mut vm = BuildVm::start(&store(), spec).expect("boot the build VM");
    let script = "ls /dev | tr '\\n' ' '; echo; id -u; \
                  test ! -e /dev/vdc && echo no-vdc; \
                  mount -t devtmpfs d /tmp 2>/dev/null || echo no-mount; \
                  d=$(cat /sys/block/vdc/dev); for at in /dev /tmp; do \
                  mknod $at/vdc b ${d%:*} ${d#*:} 2>/dev/null || echo no-mknod-$at; \
                  head -c 1 $at/vdc >/dev/null 2>&1 || echo no-open-$at; rm -f $at/vdc 2>/dev/null; done; \
                  head -c 1 /proc/1/root/dev/vdc >/dev/null 2>&1 || echo no-proc-root; \
                  echo x > /sys/block/vdc/uevent 2>/dev/null || echo no-sys";
    let (o, out, err) = run(&mut vm, sh(1, script));
    assert_eq!(o.exit, Exit::Code(0), "{out}\n{err}");
    assert_eq!(
        out,
        "fd full null ptmx pts random shm stderr stdin stdout tty urandom zero \n0\n\
         no-vdc\nno-mount\nno-mknod-/dev\nno-open-/dev\nno-mknod-/tmp\nno-open-/tmp\nno-proc-root\nno-sys\n",
        "{err}"
    );
    vm.finish().expect("the guest ends the VM");
}

/// Ruling 3: with a `SOURCE_DATE_EPOCH` every entry's mtime is at most it, a
/// parent directory copied up for a mount point included; without one the
/// guest's times are kept.
fn source_date_epoch_clamps_the_diff(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    const EPOCH: u64 = 1_000_000_000;
    let mut mtimes = Vec::new();
    for epoch in [Some(EPOCH), None] {
        // A fresh output and cache disk for each VM.
        for f in ["output.img", "cache.img"] {
            let _ = std::fs::remove_file(c.dir.path().join(f));
        }
        let mut spec = spec(&c, backend, vec![fixtures().base[0].clone()], 1);
        spec.source_date_epoch = epoch;
        let output = spec.output.clone();
        let mut vm = BuildVm::start(&store(), spec).expect("boot the build VM");
        let mut s = sh(1, "mkdir -p /new/dir && echo x > /new/dir/f && echo c > /root/.cache/c");
        s.mounts = vec!["/root/.cache".into()];
        let (o, out, err) = run(&mut vm, s);
        assert_eq!(o.exit, Exit::Code(0), "{out}\n{err}");
        vm.finish().expect("the guest ends the VM");
        let diff = read_diff(&output, &o.sealed.expect("sealed"), 0);
        mtimes.push(support::tar_mtimes(&diff));
    }
    let names = |m: &[(String, u64)]| m.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>();
    assert_eq!(names(&mtimes[0]), ["new/", "new/dir/", "new/dir/f", "root/"]);
    assert_eq!(names(&mtimes[0]), names(&mtimes[1]));
    assert!(mtimes[0].iter().all(|(_, t)| *t == EPOCH), "{:?}", mtimes[0]);
    assert!(mtimes[1].iter().all(|(_, t)| *t > EPOCH), "{:?}", mtimes[1]);
}

/// A build VM needs a version 3 `kiln-init`; an older one (`KILN_TEST_INIT_V2`,
/// built from the base of this branch) is refused at boot, naming the version.
fn a_version_2_guest_is_refused_build_mode(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    let Some(v2) = support::env().unwrap().init_v2.clone() else {
        eprintln!("skipped: KILN_TEST_INIT_V2 is unset");
        return;
    };
    let mut spec = spec(&c, backend, vec![fixtures().base[0].clone()], 1);
    spec.init = v2;
    let err = BuildVm::start(&store(), spec).err().expect("refused").to_string();
    assert!(err.contains("build mode needs") && err.contains("version 3"), "{err}");
}

fn a_step_past_its_timeout_is_killed(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    let spec = spec(&c, backend, vec![fixtures().base[0].clone()], 1);
    let mut vm = BuildVm::start(&store(), spec).expect("boot the build VM");
    let mut s = sh(1, "echo started; sleep 600");
    s.timeout = Duration::from_secs(5);
    let t = Instant::now();
    let (o, out, _) = run(&mut vm, s);
    let took = t.elapsed();
    assert_eq!(
        o,
        StepOutcome {
            exit: Exit::TimedOut,
            sealed: None
        }
    );
    assert_eq!(out, "started\n");
    assert!(took < Duration::from_secs(5) + STEP_GRACE + KILL_WAIT, "{took:?}");
    let err = vm.run_step(sh(2, "true"), &mut |_, _| {}).unwrap_err().to_string();
    assert!(err.contains("timed out"), "{err}");
    eprintln!("{backend:?}: a 5 s timeout returned after {} ms", took.as_millis());
}

/// A hostile guest seals its step far outside the output disk: kiln passes the
/// claim through, and the caller's range check (here `read_diff`'s) refuses it.
fn a_seal_outside_the_disk_is_passed_through(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    let Some(hostile) = support::env().unwrap().hostile.clone() else {
        assert!(
            !support::require(),
            "KILN_REQUIRE_KVM_TESTS=1 but KILN_TEST_HOSTILE is unset"
        );
        return;
    };
    let mut spec = spec(&c, backend, vec![fixtures().base[0].clone()], 1);
    // The init layer is built from this binary: the hostile program is PID 1.
    spec.init = hostile;
    let mut vm = BuildVm::start(&store(), spec).expect("boot the hostile build VM");
    let (o, _, _) = run(&mut vm, sh(1, "true"));
    let s = o.sealed.expect("passed through");
    assert_eq!((s.offset, s.len), (u64::MAX - 4095, 1 << 40));
    assert!(
        s.offset.checked_add(s.len).is_none_or(|end| end > OUTPUT),
        "the caller must refuse it"
    );
}

macro_rules! boot_tests {
    ($($name:ident),* $(,)?) => {
        mod firecracker {
            $( #[test] fn $name() { super::$name(vmkit::Backend::Firecracker) } )*
        }
        mod cloud_hypervisor {
            $( #[test] fn $name() { super::$name(vmkit::Backend::CloudHypervisor) } )*
        }
    };
}

boot_tests!(
    build_steps_seal_stack_and_convert,
    steps_cannot_reach_the_output_disk,
    source_date_epoch_clamps_the_diff,
    a_version_2_guest_is_refused_build_mode,
    a_step_past_its_timeout_is_killed,
    a_seal_outside_the_disk_is_passed_through,
);
```

- [ ] **Step 3: Modify `crates/kiln-run/tests/support/mod.rs`**

Apply (unified diff against the previous task's tree):

```diff
--- a/crates/kiln-run/tests/support/mod.rs
+++ b/crates/kiln-run/tests/support/mod.rs
@@ -7,6 +7,7 @@
 //!   KILN_TEST_KERNEL=<vmkit kernel>   KILN_TEST_INIT=<static kiln-init>
 //!   KILN_TEST_HOSTILE=<static `hostile` from kiln-testguest>   KILN_TEST_BUSYBOX=<static busybox, default /bin/busybox>
 //!   KILN_TEST_INIT_V1=<a static kiln-init speaking protocol version 1> (optional: the mixed-install tests)
+//!   KILN_TEST_INIT_V2=<a static kiln-init speaking protocol version 2> (optional: build mode's mixed-install test)
 //! Without the kernel and init each test is skipped, unless KILN_REQUIRE_KVM_TESTS=1.
 //! KILN_TEST_KEEP=1 keeps every run directory (console and VMM logs); KILN_TEST_VCPUS
 //! sets the guests' vCPUs (default 1).
@@ -43,6 +44,7 @@ pub struct Env {
     pub hostile: Option<PathBuf>,
     pub busybox: PathBuf,
     pub init_v1: Option<PathBuf>,
+    pub init_v2: Option<PathBuf>,
 }
 
 pub fn require() -> bool {
@@ -60,6 +62,7 @@ pub fn env() -> Option<&'static Env> {
                 hostile: get("KILN_TEST_HOSTILE"),
                 busybox: get("KILN_TEST_BUSYBOX").unwrap_or_else(|| "/bin/busybox".into()),
                 init_v1: get("KILN_TEST_INIT_V1"),
+                init_v2: get("KILN_TEST_INIT_V2"),
             }),
             _ => {
                 assert!(
@@ -182,6 +185,63 @@ const APPLETS: [&str; 30] = [
     "wc",
 ];
 
+/// Each entry's path and mtime in a ustar/PAX archive (PAX `path` and `mtime`
+/// records apply to the next entry; PAX headers are not entries).
+pub fn tar_mtimes(tar: &[u8]) -> Vec<(String, u64)> {
+    let field = |h: &[u8], a: usize, b: usize| {
+        let s: String = h[a..b].iter().take_while(|&&c| c != 0).map(|&c| c as char).collect();
+        u64::from_str_radix(s.trim(), 8).unwrap_or(0)
+    };
+    let mut out = Vec::new();
+    let (mut at, mut pax_path, mut pax_mtime) = (0, None, None);
+    while at + 512 <= tar.len() && tar[at..at + 512].iter().any(|&b| b != 0) {
+        let h = &tar[at..at + 512];
+        let size = field(h, 124, 136) as usize;
+        let data = &tar[at + 512..at + 512 + size];
+        if h[156] == b'x' {
+            for rec in String::from_utf8_lossy(data).lines() {
+                if let Some((_, kv)) = rec.split_once(' ')
+                    && let Some((k, v)) = kv.split_once('=')
+                {
+                    match k {
+                        "path" => pax_path = Some(v.to_string()),
+                        "mtime" => pax_mtime = v.split('.').next().and_then(|t| t.parse().ok()),
+                        _ => {}
+                    }
+                }
+            }
+        } else {
+            let name: String = h[..100].iter().take_while(|&&c| c != 0).map(|&c| c as char).collect();
+            let prefix: String = h[345..500]
+                .iter()
+                .take_while(|&&c| c != 0)
+                .map(|&c| c as char)
+                .collect();
+            let path = pax_path.take().unwrap_or(if prefix.is_empty() {
+                name
+            } else {
+                format!("{prefix}/{name}")
+            });
+            out.push((path, pax_mtime.take().unwrap_or(field(h, 136, 148))));
+        }
+        at += 512 + size.div_ceil(512) * 512;
+    }
+    out
+}
+
+/// The base layer's tar (busybox, users, `/etc` symlinks), as the fixtures convert it.
+pub fn base_tar() -> Vec<u8> {
+    base_layer(&std::fs::read(&env().expect("checked by Case::new").busybox).expect("read the static busybox"))
+}
+
+/// Converts layer tars with kiln, as `kiln convert` would, into erofs layers in
+/// the fixtures' store, lowest first.
+pub fn convert_tars(name: &str, tars: Vec<Vec<u8>>) -> Vec<PathBuf> {
+    let dir = &fixtures().dir;
+    let store = Store::open(dir.join("store")).unwrap();
+    convert(&store, dir, name, tars)
+}
+
 fn base_layer(busybox: &[u8]) -> Vec<u8> {
     let dir = Opts::default().mode(0o755);
     let mut t = TarBuilder::new();
```

- [ ] **Step 4: Modify `crates/kiln-testguest/src/main.rs`**

Apply (unified diff against the previous task's tree):

```diff
--- a/crates/kiln-testguest/src/main.rs
+++ b/crates/kiln-testguest/src/main.rs
@@ -5,7 +5,8 @@
 //!   port a second time and sends `Hello`, as a reset guest would.
 //! - As PID 1 (the init layer built around it instead of kiln-init), it speaks the
 //!   protocol itself and misbehaves as `Config.process.cmd[0]` says:
-//!   `stage-flood`, `exited-then-hang`, `second-stdout`, `no-read`, `hang-before-running`.
+//!   `stage-flood`, `exited-then-hang`, `second-stdout`, `no-read`, `hang-before-running`;
+//!   with a build-mode `Config`, it seals its first step far outside the output disk.
 //!
 //! The host must end the VM in every case.
 //!
@@ -44,6 +45,7 @@ fn main() {
         return;
     }
     let mode = match read_message::<_, HostMessage>(&mut control) {
+        Ok(Some(HostMessage::Config(c))) if c.mode == kiln_proto::Mode::Build => "build-seal-outside".to_string(),
         Ok(Some(HostMessage::Config(c))) => c.process.cmd.first().cloned().unwrap_or_default(),
         _ => String::new(),
     };
@@ -68,6 +70,38 @@ fn main() {
             std::thread::sleep(Duration::from_secs(3600));
         }
         "no-read" => boot(&mut send),
+        // Build mode: answers the first step with a seal far outside the output
+        // disk; the host passes it through (its caller checks the range).
+        "build-seal-outside" => {
+            for n in 3..=6 {
+                let _ = send(GuestMessage::Stage(Stage { n }));
+            }
+            let _ = send(GuestMessage::Running);
+            let mut reader = control.try_clone().expect("dup the control socket");
+            if let Ok(Some(HostMessage::RunStep(step))) = read_message::<_, HostMessage>(&mut reader) {
+                // Empty output: connect and close both streams.
+                drop(Vsock::connect(HOST_CID, port::STDOUT));
+                drop(Vsock::connect(HOST_CID, port::STDERR));
+                let _ = write_message(
+                    &mut control,
+                    &GuestMessage::StepExited(kiln_proto::StepExited {
+                        id: step.id,
+                        code: Some(0),
+                        signal: None,
+                    }),
+                );
+                let _ = write_message(
+                    &mut control,
+                    &GuestMessage::StepSealed(kiln_proto::StepSealed {
+                        id: step.id,
+                        offset: u64::MAX - 4095,
+                        len: 1 << 40,
+                        sha256: "ab".repeat(32),
+                        entries: 0,
+                    }),
+                );
+            }
+        }
         _ => {
             let _ = send(GuestMessage::Stage(Stage { n: 3 }));
         }
```

- [ ] **Step 5: Modify `scripts/boot-tests.sh`**

Apply (unified diff against the previous task's tree):

```diff
--- a/scripts/boot-tests.sh
+++ b/scripts/boot-tests.sh
@@ -37,4 +37,4 @@ export KILN_TEST_KERNEL=$kernel
 export KILN_TEST_INIT=$target/$musl/release/kiln-init
 export KILN_TEST_HOSTILE=$target/$musl/release/hostile
 export KILN_REQUIRE_KVM_TESTS=1
-cargo test -p kiln-run --test boot -p kiln --test run "$@"
+cargo test -p kiln-run --test boot --test build -p kiln --test run "$@"
```

- [ ] **Step 6: Run the build suite (Linux, Firecracker)**

Run (Lima, `sg kvm`): build a version 2 `kiln-init` from `m3d-services` (Environment, above) and export `KILN_TEST_INIT_V2`; then `scripts/boot-tests.sh /Users/alfonso/Github/Personal/playground/vmkit/out/vmlinux-6.18.54-aarch64 firecracker:: -- --test-threads 2` and, for the whole suite, the same without the filter.
Expected: `tests/build.rs` 6/6 on Firecracker (Task 7 adds the seventh), `boot.rs` and `kiln`'s `run.rs` unchanged from the baseline. Cloud Hypervisor: run `cloud_hypervisor::` serially (`--test-threads 1`), rerun failures up to three times; a failure that is a boot timeout with an empty console or a stalled step is the nested stall, anything else is a bug.

- [ ] **Step 7: Commit**

```bash
git add -A && git commit -m "test(kiln-run): build mode boot tests: two steps sealed, stacked and converted into a running image; a step timeout; a hostile seal"
```

---

### Task 7: `kiln-run`: per-step egress through vmkit's `Vm::set_egress` (last code task)

**Files:**
- Modify: `Cargo.toml` (the vmkit pin), `crates/kiln-run/src/linux/build.rs`, `crates/kiln-run/tests/build.rs`

**Interfaces:**
- Consumes (vmkit `feature/runtime-egress`, head 7a680a4, `.scratch-notes/vmkit-set-egress-report.md`):
  - `vmkit::EgressGate::{Policy, Closed}` (`Policy` is the default: the VM's own `NetSpec.egress`/`allow`; `Closed`: nothing the guest sends gets through, no DNS, connections opened while open are cut too; port forwards still reach the guest).
  - `vmkit::Vm::set_egress(&mut self, gate: EgressGate) -> vmkit::Result<()>` (a provided trait method; one atomic `nft` transaction in the VM's namespace through the helper's new `vmkit-sandbox nft` stage; idempotent; works from `create` on).
  - `vmkit::Error::{EgressUnavailable(&'static str), EgressFailed(String)}` (`Error` is `non_exhaustive`).
- Produces: `RunStep.network` enforced per step (R5). No new kiln API: `run_step`'s contract is unchanged (an egress failure is an `Err` and the VM is gone).

Ruling 5 with vmkit's API as reported (it is `set_egress(EgressGate)`, not `set_egress(Egress)`: `Egress` has no "nothing at all" mode, and a gate in front of the VM's own policy restores exactly that policy when reopened).
- **Pin:** the workspace's vmkit becomes `feature/runtime-egress`'s merge rev on vmkit `main` (it sits on b1baf3a, kiln's current pin, plus five commits). Until vmkit's PR merges, validate against the branch head 7a680a4 (the port used a local `file://` git URL to `/Users/alfonso/Github/Personal/playground/vmkit-egress`); the diff below shows the final form with the placeholder `<VMKIT_RUNTIME_EGRESS_MERGE_REV>`: replace it with the merge commit's full hash, then `cargo update -p vmkit`. Do not merge kiln with a branch or `file://` pin.
- **Boot closed:** a build VM with `network` closes its gate right after `create` (before `start`), so nothing gets out until a step asks for the network.
- **Per step:** before sending `RunStep`, `gate = if step.network { Policy } else { Closed }`, and `set_egress` only when it differs from the current gate. The switch counts inside the step's timeout. A failure kills the VM and returns the vmkit error (`Error::Vmm`).
- **Semantics for potter** (stricter than the prototype's `nsenter` table and than potter's "identical semantics" ask, by the coordinator's ruling on the vmkit side): a step without the network has no DNS either, and a connection an earlier network step left open (a `setsid` straggler is killed anyway, R7) is cut. Port forwards are not used by build VMs.
- **Deleted:** nothing to delete on this base: the prototype's `kiln-run/src/linux/egress.rs` (`nsenter` + `table inet kilnstep`) is not ported. kiln no longer needs `nsenter` or `nft` on the host's `PATH`; vmkit's helper needs host `nft`, as before for `--net`.
- **Deploy note:** the `vmkit-sandbox` helper must be the new build (it gains the `nft` stage); an old helper makes `set_egress` fail with `EgressFailed` ("usage: …"). In the Lima VM, cargo may keep a stale `$CARGO_TARGET_DIR/debug/vmkit-sandbox` from the old pin (same package, same version): build the helper into a fresh target dir, or `cargo clean -p vmkit`, before the boot suite.

- [ ] **Step 1: Modify `Cargo.toml`**

Apply (unified diff against the previous task's tree):

```diff
--- a/Cargo.toml
+++ b/Cargo.toml
@@ -3,10 +3,11 @@ resolver = "3"
 members = ["crates/*"]
 
 # The VMMs run on Linux only; elsewhere `kiln run` explains how to use a Lima VM. Pinned to
-# vmkit with TapAttach (vmkit PR #13, kiln's containerd shim), on vmkit main after guest
-# IPv6 egress (vmkit PR #12) and snapshot, restore and lazy restore (vmkit PR #11).
+# vmkit with the runtime egress gate (`Vm::set_egress`, build mode's per-step egress), on vmkit
+# main after TapAttach (vmkit PR #13, kiln's containerd shim), guest IPv6 egress (vmkit PR #12)
+# and snapshot, restore and lazy restore (vmkit PR #11).
 [workspace.dependencies]
-vmkit = { git = "https://github.com/AlfonsoCampodonico/vmkit", rev = "b1baf3a0712209dbe7c7659186ad02b702524d3e" }
+vmkit = { git = "https://github.com/AlfonsoCampodonico/vmkit", rev = "<VMKIT_RUNTIME_EGRESS_MERGE_REV>" }
 
 [workspace.package]
 edition = "2024"
```

- [ ] **Step 2: Modify `crates/kiln-run/src/linux/build.rs`**

Apply (unified diff against the previous task's tree):

```diff
--- a/crates/kiln-run/src/linux/build.rs
+++ b/crates/kiln-run/src/linux/build.rs
@@ -34,7 +34,7 @@ use std::time::{Duration, Instant};
 use kiln_proto::{Config, GuestMessage, HostMessage, Signal, port, read_message, write_message};
 use kiln_store::Store;
 use rustix::event::{PollFd, PollFlags, poll};
-use vmkit::{Disk, NetSpec, Vm, VmSpec, VsockSpec};
+use vmkit::{Disk, EgressGate, NetSpec, Vm, VmSpec, VsockSpec};
 
 use super::console::ConsoleRelay;
 use super::run::{CMDLINE, backend, exit_method, vmm_identity};
@@ -80,6 +80,9 @@ pub struct BuildVm {
     stacked: BTreeSet<usize>,
     /// Steps sealed so far: each sealed upper is one more lower.
     sealed: usize,
+    /// The VM's egress gate (with `network`): closed from boot, opened for the
+    /// steps that ask for the network (vmkit's `Vm::set_egress`).
+    gate: EgressGate,
     /// Why the VM is unusable, once it is.
     dead: Option<String>,
     wake: OwnedFd,
@@ -185,6 +188,11 @@ impl BuildVm {
         let config = spec.config(exit_method(vmm.as_ref()));
         config.validate()?;
         let mut vm = vmm.create(&vm_spec)?;
+        // Boot closed: no step has asked for the network yet.
+        let spec_network = spec.network;
+        if spec_network {
+            vm.set_egress(EgressGate::Closed)?;
+        }
         info.vmm = vmm_identity(&run_dir.path);
         run_dir.write_info(&info)?;
         scratch_dir.write_info(&ScratchInfo::from(&info))?;
@@ -220,6 +228,11 @@ impl BuildVm {
             proto: BuildProtocol::default(),
             stacked: BTreeSet::new(),
             sealed: 0,
+            gate: if spec_network {
+                EgressGate::Closed
+            } else {
+                EgressGate::Policy
+            },
             dead: None,
             wake,
             listener: Some(listener),
@@ -264,12 +277,28 @@ impl BuildVm {
         if let Some(why) = &self.dead {
             return Err(Error::refused(format!("the build VM has ended: {why}")));
         }
+        // The deadline counts from here: the egress change is part of the step's time.
+        let start = Instant::now();
         // A refusal here (the lowers cap included) sends nothing: the VM stays usable.
         let msg = self.spec.proto_step(&step, &self.stacked, self.sealed)?;
+        if self.spec.network {
+            let gate = if step.network {
+                EgressGate::Policy
+            } else {
+                EgressGate::Closed
+            };
+            if gate != self.gate {
+                // A failure leaves the gate as it was; the VM is not used again.
+                if let Err(e) = self.vm.set_egress(gate) {
+                    self.kill(format!("setting step {}'s egress: {e}", step.id));
+                    return Err(e.into());
+                }
+                self.gate = gate;
+            }
+        }
         self.proto.start_step(step.id).map_err(Error::refused)?;
         self.stacked.extend(step.lowers_added.iter().copied());
         self.send(HostMessage::RunStep(Box::new(msg)));
-        let start = Instant::now();
         let deadline = start + step.timeout;
         let kill_at = deadline + STEP_GRACE;
         let mut timed_out = false;
```

- [ ] **Step 3: Modify `crates/kiln-run/tests/build.rs`**

Apply (unified diff against the previous task's tree):

```diff
--- a/crates/kiln-run/tests/build.rs
+++ b/crates/kiln-run/tests/build.rs
@@ -404,6 +404,31 @@ fn a_seal_outside_the_disk_is_passed_through(backend: Backend) {
     );
 }
 
+/// Egress is per step in a VM with a network: off, on, off again, through
+/// vmkit's `Vm::set_egress` (needs pasta, nft and the internet: KILN_TEST_NET=1).
+/// Closed means no DNS either, and no packet of a connection an earlier step
+/// opened.
+fn egress_follows_each_step(backend: Backend) {
+    let Some(c) = Case::new(backend) else { return };
+    if std::env::var_os("KILN_TEST_NET").is_none_or(|v| v != "1") {
+        assert!(!support::require(), "KILN_REQUIRE_KVM_TESTS=1 but KILN_TEST_NET is not");
+        return;
+    }
+    let mut spec = spec(&c, backend, vec![fixtures().base[0].clone()], 1);
+    spec.network = true;
+    let mut vm = BuildVm::start(&store(), spec).expect("boot the build VM");
+    let probe = "printf 'HEAD / HTTP/1.0\\r\\n\\r\\n' | nc -w 5 1.1.1.1 80 | head -c 8; echo";
+    let mut got = Vec::new();
+    for (id, network) in [(1, false), (2, true), (3, false)] {
+        let mut s = sh(id, probe);
+        s.network = network;
+        let (o, out, err) = run(&mut vm, s);
+        assert_eq!(o.exit, Exit::Code(0), "{out}\n{err}");
+        got.push(out);
+    }
+    assert_eq!(got, ["\n", "HTTP/1.1\n", "\n"], "egress off, on, off");
+}
+
 macro_rules! boot_tests {
     ($($name:ident),* $(,)?) => {
         mod firecracker {
@@ -422,4 +447,5 @@ boot_tests!(
     a_version_2_guest_is_refused_build_mode,
     a_step_past_its_timeout_is_killed,
     a_seal_outside_the_disk_is_passed_through,
+    egress_follows_each_step,
 );
```

- [ ] **Step 4: Check and run the egress case**

Run: macOS `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace` (no change in counts); Lima clippy and tests; then (Lima, `sg kvm`) with the new helper: `KILN_TEST_NET=1 scripts/boot-tests.sh /Users/alfonso/Github/Personal/playground/vmkit/out/vmlinux-6.18.54-aarch64 egress_follows_each_step`.
Expected: `firecracker::egress_follows_each_step` passes (off, on, off: `""`, `"HTTP/1.1"`, `""`); `tests/build.rs` 7/7 on Firecracker. Cloud Hypervisor: judged as in Task 6; "egress on" is re-checked on x86_64 CI (ruling 6).

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(kiln-run): per-step egress through vmkit Vm::set_egress; pin vmkit with the runtime egress gate"
```

---

### Task 8: `docs/format.md` and spec rev 2.16: control protocol version 3, build mode

**Files:**
- Modify: `docs/format.md`, `docs/superpowers/specs/2026-09-30-kiln-design.md`

**Interfaces:**
- Consumes: Tasks 1–7.
- Produces: the normative description of build mode (`docs/format.md` is normative for §7 and §9.5): `Config.mode`, `disks`, `sourceDateEpoch`; types 17–20; the disks and the device budget; `RunStep`'s fields and bounds; the lowers cap; a step's mounts and isolation (G-S); sealing (xattrs, the clamp, layout); what the caller must check; message order; deadlines; the version rule for build mode; kiln-shim and services. Spec rev 2.16 records the rulings, settles the note on potter §9.3 (`Hello` first; `Running` is potter's `Ready`), and marks M3e planned.

- [ ] **Step 1: Modify `docs/format.md`**

Apply (unified diff against the previous task's tree):

```diff
--- a/docs/format.md
+++ b/docs/format.md
@@ -1,6 +1,6 @@
 # kiln formats
 
-This document is normative. It defines the **kiln image** (schema versions 1 and 2, provisional until M3 adds kernel and init layers), the **kiln snapshot** (schema version 1) and its lazy index, the **erofs profile** (format version 1), the **guest** that `kiln-init` sets up (devices, init layer, scratch disk, boot sequence) and the **control protocol** (versions 1 and 2) between `kiln-init` and the host.
+This document is normative. It defines the **kiln image** (schema versions 1 and 2, provisional until M3 adds kernel and init layers), the **kiln snapshot** (schema version 1) and its lazy index, the **erofs profile** (format version 1), the **guest** that `kiln-init` sets up (devices, init layer, scratch disk, boot sequence) and the **control protocol** (versions 1 to 3) between `kiln-init` and the host.
 
 ## kiln image, schema versions 1 and 2
 
@@ -373,7 +373,7 @@ Growing online to 64 GiB works on Firecracker. On Cloud Hypervisor 53 the resize
 
 ## Control protocol
 
-This section defines versions 1 and 2. Version 2 adds `Config.services`, `Config.readyTimeoutMs` and `Exited.service`, and widens `Config.shutdownGraceSecs` to the stop that follows a process's exit, and nothing else; a version adds message types, optional fields and values, and never changes what an earlier version defines (spec §9.8).
+This section defines versions 1 to 3. Version 2 adds `Config.services`, `Config.readyTimeoutMs` and `Exited.service`, and widens `Config.shutdownGraceSecs` to the stop that follows a process's exit, and nothing else; version 3 adds build mode (`Config.mode`, `Config.disks`, `Config.sourceDateEpoch`, `RunStep`, `Finish`, `StepExited`, `StepSealed`, below) and nothing else; a version adds message types, optional fields and values, and never changes what an earlier version defines (spec §9.8).
 
 Version 1 was frozen at the first `kiln-init` release. It includes what the kiln-shim exec and volumes work added before any release (spec §9.8): the exec message types 10 to 16, vsock port 1029, `Config.volumes` and `Config.podDisk`, and `Config.network.search`, `options` and `ipv6`. Those are specified for now by `docs/superpowers/specs/2026-10-30-kiln-shim-exec-volumes-design.md` (and the IPv6 design, `docs/superpowers/specs/2026-10-21-guest-ipv6.md`), not in the tables below.
 
@@ -399,7 +399,7 @@ A frame is a `u32` little-endian length, then a `u8` message type, then the payl
 
 | Type | Name | Direction | Payload |
 |---|---|---|---|
-| 1 | `Hello` | guest → host | `{"protocol": 2}`: the highest protocol version the guest implements, at least 1. |
+| 1 | `Hello` | guest → host | `{"protocol": 3}`: the highest protocol version the guest implements, at least 1. |
 | 2 | `Config` | host → guest | The run's configuration (below). |
 | 3 | `Stage` | guest → host | `{"n": 3}`: init entered stage `n` (1–7). Sent for stages 3 to 7; stages 1 and 2 precede it. |
 | 4 | `Running` | guest → host | `{}`: the main process started (after every service was ready). |
@@ -408,6 +408,12 @@ A frame is a `u32` little-endian length, then a `u8` message type, then the payl
 | 7 | `Shutdown` | host → guest | `{"graceSecs": 10}`. |
 | 8 | `Signal` | host → guest | `{"sig": 1}`: a signal number from 1 to 31. |
 | 9 | `WindowSize` | host → guest | `{"rows": 24, "cols": 80}` (each 0–65535). |
+| 17 | `RunStep` | host → guest | Version 3, build mode: run one step (below). |
+| 18 | `Finish` | host → guest | Version 3, build mode: `{}`; the build is over, sync and end the VM. |
+| 19 | `StepExited` | guest → host | Version 3, build mode: `{"id": 1, "code": 0}` or `{"id": 1, "signal": 9}`: the step's command ended (exactly one of `code` 0–255 and `signal` 1–64) and its output reached EOF. |
+| 20 | `StepSealed` | guest → host | Version 3, build mode: `{"id": 1, "offset": 0, "len": 8192, "sha256": "<64 lowercase hex>", "entries": 5}` (below). |
+
+Types 10 to 16 are version 1's exec and file messages (above). Build mode's types follow them.
 
 `Config` (camelCase keys; optional fields are omitted when unset, and a reader also accepts `null` for them):
 
@@ -437,7 +443,7 @@ The whole `Config` is one frame, and a frame is at most 65536 bytes including th
 ### Session
 
 1. The guest connects port 1024 and sends `Hello`. The host answers the first `Hello` with `Config`, once per VM.
-   - **Versions.** A host supports every version from 1 to its own (2). A `Hello` naming a higher version is a violation, so a version 1 host refuses a version 2 `kiln-init`; that is by design, since `kiln run` is to boot the `kiln-init` pinned to it (spec §8.1), the two shipping together. Until kiln pins a released `kiln-init`, every run names one with `--init`, so mixed versions are expected, and this gate handles them. The host sends only what the guest's version defines for services: when the run has services (they need version 2) and the guest speaks version 1, the host sends no `Config`, kills the VM and reports 125 with an error naming the version required. The exec and volume additions of version 1 are not gated on the guest's version. A guest may use only what its version defines: `Exited.service` from a version 1 guest, or naming a service the run does not have, is a violation. The guest decodes strictly, so a version 1 `kiln-init` refuses a version 2 field (`InitFailed` at stage 2).
+   - **Versions.** A host supports every version from 1 to its own (3). A `Hello` naming a higher version is a violation, so a version 1 host refuses a version 2 `kiln-init`; that is by design, since `kiln run` is to boot the `kiln-init` pinned to it (spec §8.1), the two shipping together. Until kiln pins a released `kiln-init`, every run names one with `--init`, so mixed versions are expected, and this gate handles them. The host sends only what the guest's version defines for services: when the run has services (they need version 2) and the guest speaks version 1, or the VM is a build VM (version 3) and the guest speaks an earlier version, the host sends no `Config`, kills the VM and reports 125 with an error naming the version required. The exec and volume additions of version 1 are not gated on the guest's version. A guest may use only what its version defines: `Exited.service` from a version 1 guest, or naming a service the run does not have, is a violation. The guest decodes strictly, so a version 1 `kiln-init` refuses a version 2 field (`InitFailed` at stage 2).
    - **kiln-shim** sends no services in M3d: it cannot see the image's `dev.kiln.services` label (it never reaches the OCI runtime spec), so under it an image with services runs without them. This is a known gap; the M4 follow-up has the shim read the image config through containerd's image service and either pass the services or refuse.
 2. The guest sends `Stage` for stages 3 to 7, `Running` once the main process has started, then `Exited`, or `InitFailed` at any point. The host sends `Shutdown`, `Signal` and `WindowSize` at any time after `Config`.
 
@@ -445,6 +451,62 @@ The host checks the order of the guest's messages: `Stage` numbers strictly incr
 
 The host kills the VM, and reports 125, on any violation: a message out of that order, a second connection to port 1024 (or to any port it serves), a second `Hello` (a reset guest starting over), any message before `Hello`, a protocol version it does not support, or any framing or payload error. The guest treats a framing or payload error, a second `Config`, or the host closing port 1024 as a failure (InitFailed, then the VM ends).
 
+### Build mode (version 3)
+
+`Config.mode` is `"run"` (the default, omitted) or `"build"`. In build mode `process` is empty, `tty`, `interactive`, `services`, `readyTimeoutMs`, `volumes` and `podDisk` are unset (a build VM has no services and no kiln-shim pod fields), and the guest runs the host's steps instead of a main process. A host sends a build-mode `Config` only to a guest whose `Hello` names version 3 or later: against an older guest it sends no `Config`, kills the VM, and the build VM fails to start with an error naming the version required. A version 1 or 2 guest would refuse the fields anyway (strict decoding). kiln-shim never sends a build-mode `Config`; a build-mode guest treats `ExecStart`, `ExecSignal`, `ExecWindowSize` and `FileUpdate` as violations.
+
+**Disks**, in order, nothing hot-plugged: `vda` init layer, `vdb` scratch, `vdc` the **output disk** (raw, created and sized by the host's caller, a multiple of 4096 bytes), then `Config.layers` base layers (erofs, bottom first: the boot root), then `Config.disks`, a list of `{"kind": "erofs"|"ext4", "readOnly": bool}`, one per further disk in order. `erofs` disks are layers a step stacks (`lowersAdded`), `ext4` disks are cache disks a step mounts. `layers` plus the entries of `disks` are at most 128. Disk indexes count from `vda` = 0. The host's caller budgets disks against the VMM's devices: the fixed ones are the init layer, scratch, the output disk and vsock, plus a NIC when the VM has a network.
+
+**`Config` fields** (version 3, build mode only; refused in run mode):
+
+| Field | Type | Rules |
+|---|---|---|
+| `mode` | `"run"` or `"build"`, optional | Omitted when `"run"`. |
+| `disks` | list, optional | As above. Omitted when empty. |
+| `sourceDateEpoch` | integer, optional | Seconds since the epoch. When present, every entry of every sealed diff has an mtime of at most this value, copied-up parent directories included; when absent the guest's times are kept. |
+
+**`RunStep`** (camelCase):
+
+| Field | Type | Rules |
+|---|---|---|
+| `id` | integer | The step's id (u32); the guest's answers carry it. |
+| `command` | `{"argv": [..]}` or `{"shell": ".."}` | A non-empty argv with a non-empty argv[0], or a script run as `/bin/sh -c <script>`. |
+| `env` | list of `KEY=value` | As `process.env`; the guest adds `PATH`, `HOSTNAME` and `HOME` (`/root` for uid 0, else `/`) when unset. |
+| `workdir` | string | Absolute; created as root when missing (that is part of the step's diff), then entered. |
+| `uid`, `gid` | integers | Not 4294967295. The step runs as `uid:gid` with `gid` as its only group. |
+| `lowersAdded` | list of disk indexes, optional | `erofs` entries of `Config.disks`, each at most once per VM: stacked on the root before the step, bottom first. |
+| `mounts` | list of `{"disk", "target", "readOnly"}`, optional | `ext4` entries of `Config.disks` mounted at `target` for the step (read-only if the disk is). At most 64. |
+| `secrets` | list of `{"target", "data"}`, optional | `data` is standard base64 (padded, strict). The file appears at `target`, mode 0400, owned by `uid:gid`, on a tmpfs: never in the upper or the output. At most 64. |
+| `egress` | boolean | Whether the step may reach the network. The host enforces it in the VM's network namespace; the guest does not act on it. |
+| `timeoutMs` | integer | At least 1. The host's deadline for the step and its seal; the guest does not act on it. |
+
+Targets are absolute, not `/`, without `.` or `..` components, and unique across `mounts` and `secrets`. Secrets travel inline: the whole `RunStep` is one frame, so a step's secrets are at most about 44 KiB together, and the host refuses a larger step before sending it.
+
+**Lower layers.** A step's overlay has as lower directories the base layers, the layers stacked so far and one sealed upper per successful step, at most 230 (`MAX_BUILD_LOWERS`): the guest names them all in one mount(2) data page. The host refuses a `RunStep` that would mount more with its own error, before sending anything, so the VM stays usable; its caller squashes layers or boots a new VM. The guest treats such a step as a violation.
+
+**A step**, in the guest:
+- A fresh overlay (`nodev`) is mounted with every lower so far, the newest on top, and a new upper and work directory on scratch.
+- Mounted in it, with every path resolved inside the root (`RESOLVE_IN_ROOT`, so the image's symlinks cannot lead a mount out of it): `/proc`; a read-only `sysfs` on `/sys`; a minimal **`/dev`** (a read-only tmpfs holding only the character devices `null`, `zero`, `full`, `random`, `urandom` and `tty`, the links `ptmx` → `pts/ptmx`, `fd`, `stdin`, `stdout` and `stderr`, a `devpts` on `/dev/pts` and a tmpfs on `/dev/shm`): no block device, so neither the output disk nor a layer disk; the cache disks (`nodev`); `/etc/hostname`, `/etc/hosts` and `/etc/resolv.conf`, each only where the image has a regular file or nothing, never through a symlink; and the secrets (`nodev`).
+- Mount points that had to be created are removed after the step, so none reaches the diff. A parent directory that overlayfs copied up to create one stays in the diff (`/root` for a cache disk at `/root/.cache`), as BuildKit's do.
+- The guest connects ports 1026 and 1027 anew for each step: they carry the step's stdout and stderr, and their EOF ends its output. A connection while no step runs, or a second one in a step, is a violation.
+- The command runs in a mount namespace of its own, pivoted into the overlay with init's root detached, in its own session and process group, with a capability bounding set of Docker's defaults less `CAP_MKNOD` (`CHOWN`, `DAC_OVERRIDE`, `FOWNER`, `FSETID`, `KILL`, `SETGID`, `SETUID`, `SETPCAP`, `NET_BIND_SERVICE`, `NET_RAW`, `SYS_CHROOT`, `AUDIT_WRITE`, `SETFCAP`). So a step, root or not, cannot mount, cannot make a device node, and cannot reach init's root through `/proc/1/root`. `Signal` goes to the step's process group.
+- When the command's process exits, every other process in the guest is killed, the output is drained, and `StepExited` is sent. Then the step's mounts are detached and the overlay unmounted.
+
+**Sealing**, after `code` 0: the upper becomes an OCI tar diff (ustar, with PAX records for long paths and link targets, large ids, sizes and times, and `SCHILY.xattr.*`):
+- an overlay whiteout (character device 0:0) becomes `.wh.<name>`; an opaque directory (`trusted.overlay.opaque`) gets `.wh..wh..opq`, kept only where a lower layer has that path;
+- inodes with several links become hard links to their first path; sockets are skipped;
+- extended attributes: `user.*` (except `user.overlay.*`) and `security.capability` are kept; everything else (`trusted.*`, other `security.*`, `system.*`) is dropped;
+- with `sourceDateEpoch`, every entry's mtime is clamped to it;
+- paths are relative, never absolute or with `..`; names are walked in byte order.
+
+The diff is written to the output disk at `offset`, the end of the previous diff (0 for the first), and padded with zeros to a multiple of 4096: `len` includes the padding, so `[offset, offset + len)` holds the tar, its two end blocks, then only zeros. `sha256` is that range's digest and `entries` its tar entries (PAX headers not counted). The disk is flushed before `StepSealed` is sent. The upper then becomes the topmost lower. After any other exit there is no seal: the upper is discarded, and the next step starts from the state before the failed one. A diff that does not fit the output disk is a failure (`InitFailed`).
+
+`StepSealed` is guest-asserted. The host passes it through after checking only its shape and that `id` is the step's; its caller must check that the range lies within the output disk, is aligned, starts after the previous diff, holds only zeros after the tar's end, and must hash what it reads itself. Steps cannot open the output disk, but the caller should still read each diff before the next step runs.
+
+**Order**: the guest sends `Hello`, `Stage` 3 to 6, `Running` (ready for steps), then for each `RunStep`: `StepExited` with its id and, after `code` 0, `StepSealed` with its id. The host sends one `RunStep` at a time, after the previous step's last answer; `Signal` at any time; `Finish` between steps. `InitFailed` may come at any point and ends the VM. Anything else is a violation, and the host kills the VM. A `RunStep`, `Finish` or `Shutdown` while a step runs is a violation on the guest's side.
+
+**Deadlines**: at `timeoutMs` the host sends `Signal` 9 (the step's process group); if the step has not ended 10 seconds later it kills the VM. Either way the step is reported as timed out and the VM is not used again: the caller boots a new one.
+
 ### Exit codes
 
 The host reports, as Docker does:
```

- [ ] **Step 2: Modify `docs/superpowers/specs/2026-09-30-kiln-design.md`**

Apply (unified diff against the previous task's tree):

```diff
--- a/docs/superpowers/specs/2026-09-30-kiln-design.md
+++ b/docs/superpowers/specs/2026-09-30-kiln-design.md
@@ -1,7 +1,7 @@
 # kiln — Design Spec
 
-**Date:** 2026-09-30 (rev 2.15: 2026-11-19)
-**Status:** Draft rev 2.15, awaiting review
+**Date:** 2026-09-30 (rev 2.16: 2026-11-20)
+**Status:** Draft rev 2.16, awaiting review
 **Scope:** `kiln` (OCI image / Dockerfile → bootable microVM image builder) and the v1 scope of `vmkit` (shared VMM crate)
 **Related:**
 - `../../../../ROADMAP.md`: the three-repo roadmap (`vmkit`, `kiln`, `potter`).
@@ -117,6 +117,15 @@
   - §9.8 and §12 (M4): `kiln-shim` does not support services in M3d and sends none, because image labels never reach the OCI runtime spec. Known gap; the M4 follow-up has the shim read the image config through containerd's image service and either pass the services or refuse.
   - §16.5: `kiln rebase` re-derives `services` from the merged `dev.kiln.services` label and recomputes `schemaVersion`; a merged label that does not parse refuses the rebase.
   - §12: M3d is done, in review on `m3d-services`.
+
+- **rev 2.16 (2026-11-20):** M3e (build mode) is planned (`docs/superpowers/plans/2026-11-20-m3e-build-mode.md`); the spec records what its prototype and rulings settled (`docs/format.md` is normative):
+  - §9.8: build mode is protocol version 3. Its message types are 17 to 20 (`RunStep`, `Finish`, `StepExited`, `StepSealed`), after version 1's exec and file types 10 to 16. `Hello` comes first and the host answers it with `Config { mode: build }` (the note on potter §9.3 is settled that way); the guest's `Running` is potter's `Ready`.
+  - §9.8 and §9.1: a build VM's disks are init, scratch, the output disk (`vdc`), the base layers, then `Config.disks` (erofs layers a step stacks, ext4 cache disks it mounts), all attached at boot. A step's stdout and stderr are fresh connections to ports 1026 and 1027 per step; its exit is `StepExited`, a message of its own. A `RunStep`, secrets included, is one 64 KiB frame.
+  - §9.8, isolation (ruling 1 and the security review): a step sees a minimal read-only `/dev` with no block devices, a read-only `/sys`, runs in its own mount namespace pivoted into the overlay, with Docker's default bounding set less `CAP_MKNOD`; the overlay, cache disks and tmpfs mounts are `nodev`. A root step cannot open the output disk or a layer disk.
+  - §9.8: at most 230 lower layers per build VM; the host refuses a step past it with its own error (ruling 2). `Config.sourceDateEpoch` clamps every sealed entry's mtime, copied-up directories included (ruling 3). Secrets are mode 0400, owned by the step's uid:gid (ruling 4). Seals keep `user.*` and `security.capability` attributes only.
+  - §9.3 and §9.8: per-step egress goes through vmkit's runtime `Vm::set_egress` (ruling 5).
+  - Accepted deviations (ruling 7): per-step stdio connections; a timed-out step kills the VM; directories copied up for mount points stay in the diff.
+  - kiln-shim never boots build mode, and a build VM has no services, volumes or pod disk.
 ---
 
 ## 1. Goals
@@ -792,7 +801,7 @@ kiln-init is pinned to the kiln release that boots it (§8.1), and potter and `k
   - appends the diff to the raw output disk at the next 4 KiB-aligned offset;
   - remounts with the sealed upper as the topmost lower and a fresh upper. Mounts use per-layer `lowerdir+` or mount(2), never one long `lowerdir` string.
 - The host trusts none of `StepSealed`'s values. potter reads the range itself, checks its bounds, hashes it, and ingests it through kiln's untrusted tar path (potter spec §9.3, P4).
-- Designed in M3e: the session order (§9.5's `Hello`-first rule stands; see the note below), the build-mode device order (output disk, cache-mount disks) against §9.1, how a step's stdout and stderr are carried, how a step's `Exited` is told apart from the main process's, and the size bound on `RunStep`.
+- Designed in M3e (rev 2.16, normative in `docs/format.md`): `Hello` first, then `Config { mode: build }`, `Running` as the guest's ready; message types 17 to 20; the disks (output disk `vdc`, base layers, then `Config.disks`, nothing hot-plugged); a step's stdout and stderr as fresh connections to ports 1026 and 1027; `StepExited` as the step's exit; one 64 KiB frame per `RunStep`, secrets inline; a minimal read-only `/dev`, a per-step mount namespace and a reduced bounding set (no `CAP_SYS_ADMIN`, `CAP_MKNOD`, `CAP_SYS_PTRACE`); at most 230 lowers; `Config.sourceDateEpoch`; per-step egress through vmkit's `Vm::set_egress`.
 
 **Exec, guest stats and file updates (M4):**
 - **Exec** (implemented at version 1, `2026-10-30-kiln-shim-exec-volumes-design.md` §2):
@@ -811,7 +820,7 @@ kiln-init is pinned to the kiln release that boots it (§8.1), and potter and `k
   - the `Config.network` additions the shim needs for the pod's IP, routes and DNS (shim spec §3.2);
   - the dm-verity root hashes the shim passes to kiln-init (shim spec §3.1, S4).
 
-**Note on potter spec §9.3:** it has the host send `Config{mode: Build}` and the guest reply with `Hello`, then `Ready`. Under §9.5 the guest's `Hello` always comes first and the host answers it with `Config`. M3e keeps that order unless the user decides otherwise.
+**Note on potter spec §9.3:** it has the host send `Config{mode: Build}` and the guest reply with `Hello`, then `Ready`. Under §9.5 the guest's `Hello` always comes first and the host answers it with `Config`. M3e keeps that order (rev 2.16); the guest's `Running` is potter's `Ready`.
 
 ---
 
@@ -910,7 +919,7 @@ One parametrised suite runs against both backends, using a test guest (the kerne
 | **M3b: run** (planned as **M3b-1**, the runtime and CLI, and **M3b-2**, pinned artifacts, `--persist`, `--net` and Lima) | `kiln` | `kiln run` (§9.7): the host runtime, its CLI flags, host signal handling and console tails; kernel and init layers in images with their pins and run-time verification (§6.5, §8.1); the Lima template (§10). | Yes | Planned (M3b-1: `docs/superpowers/plans/2026-10-06-m3b1-run.md`; M3b-2: `docs/superpowers/plans/2026-10-07-m3b2-artifacts-persist-net.md`) |
 | **M3d: services** | `kiln` | kiln-proto and `kiln-init` `services` (§9.8), as a protocol version bump. Needed by potter P2 (PHP web images). | Yes | Done, in review on branch `m3d-services` (`docs/superpowers/plans/2026-10-11-m3d-services.md`) |
 | **M3c: build** | `kiln` | `kiln build` backend dispatch: potter, BuildKit, Podman or Buildah (§6). The end-to-end matrix (§11.4), the kernel and init swap tests (§11.5), the release pipeline and the docs (§15). | Yes | Planned |
-| **M3e: build mode** | `kiln` | kiln-proto `Config.mode = Build`, the build-mode messages and `kiln-init` build mode (§9.8), as a protocol version bump. Lands with potter P4 (the VM executor). | Yes | Planned |
+| **M3e: build mode** | `kiln` | kiln-proto `Config.mode = Build`, the build-mode messages and `kiln-init` build mode (§9.8), as a protocol version bump. Lands with potter P4 (the VM executor). | Yes | Planned (rev 2.16) |
 | **M4: `kiln-shim`** (slip) | `kiln` | The containerd shim, in the phases of the shim spec (§8 there): **S0** a KVM node-group spike, nested C8i against metal, with kiln's bench and end-to-end matrix (after M3b); **S1** kiln-proto exec, `Stats`, `OomKill` and `FileUpdate` (§9.8; after M3a); **S2** media type alignment with containerd 2.3's erofs snapshotter, plus a test (§5.1; after M1); **S3** `kiln-shim`: lifecycle, networking, logs, exec, stats, projected volumes (after S1, S2 and M3b); **S4** resize, verity, init containers (after S3; uses the vmkit hotplug capabilities in §4.1). The shim spec's S5, a platform canary, is a rollout step, not kiln work. **Follow-up to M3d:** `kiln-shim` passes no services, as it cannot see the image's `dev.kiln.services` label; it reads the image config through containerd's image service and either passes the services or refuses the pod (§9.8). | Yes | Planned |
 | **sagger** (agent sandbox) | `kiln`, `vmkit` | Snapshot, restore and fork of `kiln run` guests: `kiln-init` reconnects after restore, on `vmkit`'s snapshot and restore. | Yes | Needs a spec |
 | **sherd** and **raku** | `kiln` (sherd), `vmkit` (raku) | Snapshot artifacts in OCI registries with chunk dedup (sherd); lazy restore below the VMM (raku); `vmkit` snapshot and restore. | Yes | First cut done (`2026-10-21-lazy-restore-design.md`) |
```

- [ ] **Step 3: Check and commit**

Run: `git diff --check` and a read-through against Tasks 1–7 (every number in the docs: 17–20, 230, 128, 64, 44 KiB, 4096, 10 s, 1026/1027, the capability list, the `/dev` list, matches the code).

```bash
git add -A && git commit -m "docs: control protocol version 3, build mode, in format.md and spec rev 2.16"
```

- [ ] **Step 4: The whole validation**

Run on the final tree: macOS fmt, clippy, workspace tests; Lima clippy, workspace tests, `scripts/boot-tests.sh` with `KILN_TEST_NET=1` and `KILN_TEST_INIT_V2` on Firecracker (whole suite), then Cloud Hypervisor serially with reruns. Report the counts to the controller; the PR body carries none (PR style).

---

## Spec coverage (M3e)

| Spec item | Task |
|---|---|
| §9.8 build mode: `Config.mode = Run \| Build`, the layer and mount table (`layers`, `disks`), limits | 1, 4 |
| §9.8 versioning rules 1–4 for version 3: additions only (types 17–20, optional fields); the host refuses build mode to a guest below 3 before `Config`; strict decoding in older guests | 1, 5, 6 |
| §9.8 `RunStep{id, argv\|shell, env, workdir, user, lowers_added, mounts, secrets, timeout_ms}` (designed here: `uid`/`gid`, disk indexes, `egress`, one frame, secrets inline ~44 KiB) | 1, 4 |
| §9.8 the guest streams the step's output, then its exit (designed here: per-step connections on 1026/1027, `StepExited`, R7) | 3, 5 |
| §9.8 secrets on a tmpfs never part of the upper (R4: 0400, uid:gid) | 3, 6 |
| §9.8 seal: kill the group and stragglers, unmount, OCI tar diff (whiteouts, opaque), 4 KiB-aligned append, the sealed upper as the top lower via mount(2) | 2, 3 |
| §9.8 the host trusts none of `StepSealed` (shape and id only; potter checks the range and hashes) | 4, 5, 6 |
| §9.8 designed here: session order (`Hello` first), device order (output `vdc`, base layers, `Config.disks`) | 1, 3, 4, 8 |
| §9.1 devices and the budget (`FIXED_DEVICES` 4 + NIC, `max_devices`) | 4, 5 |
| potter §9.3 VM shape, deadlines (`run_step` within timeout + grace), teardown (`finish`, `Drop`) | 5 |
| potter §9.3/§9.5 per-step network (R5: `Vm::set_egress`; IPv4 only) | 4, 7 |
| potter §8.4 trust ordering: a `vm`-class layer comes from a step that cannot touch the output disk or another layer (R1, G-S) | 3, 6 |
| Rulings R2 (lowers cap, distinct error), R3 (`SOURCE_DATE_EPOCH`), potter's xattr answer | 1–4, 6 |
| `docs/format.md` and spec rev 2.16 | 8 |

**Left to later milestones:** potter P4b (`BuildBackend` on `kiln_run::build`, using the API delta above); kiln pinning a released `kiln-init` and kernel for build VMs (`BuildSpec.kernel`/`init` stay explicit); a shim-driven build (M4 and later); lifting the lowers cap with `lowerdir+` through `fsconfig` (the M1b-1 finding still holds for long strings, so it would need per-layer `lowerdir+` calls).

**Send potter** (with the merge rev): the API delta in Design decisions, the `EgressGate` semantics of Task 7 (no DNS in a closed step), and that steps lack `CAP_MKNOD`.
