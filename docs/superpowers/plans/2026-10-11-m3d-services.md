# M3d: kiln-init `services` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** a kiln image can declare services, processes that `kiln-init` starts in order before the main process, each ready before the next, and stops in reverse order with its own stop signal (spec §9.8 `services`, milestone M3d). This is what potter P2 needs for PHP web images: php-fpm a service ready on its socket and stopped with SIGQUIT, nginx the main process (potter spec §6.1.1). It ships as control protocol version 2 and image config schema version 2, so a kiln or `kiln-init` that predates M3d refuses what it cannot run instead of running an image without its services.

**Architecture:**
- **Protocol version 2** (Task 1): `kiln_proto::Config` gains `services` and `readyTimeoutMs`, `Exited` gains `service`; a `Config` without services is byte for byte a version 1 `Config`. `PROTOCOL_VERSION` becomes 2.
- **Version negotiation** (Task 2): the host records the version of the guest's `Hello`, refuses before `Config` a run whose `Config` needs a later version (125), checks `Exited.service`, and sets `readyTimeoutMs` from what remains of the boot timeout.
- **The image side** (Tasks 3, 4): `kiln_oci::ContainerConfig` carries the OCI labels; `kiln convert` (and potter, through `convert_resolved`) turns the `dev.kiln.services` label into `KilnConfig.services`, written with `schemaVersion` 2. potter needs nothing else from kiln (Task 4).
- **The host** (Task 5): `kiln run` passes the image's services to the guest with the run's `--env`, and says which service ended a run.
- **The guest** (Task 6): stage 6 starts the services, each with its own user, environment and working directory, and waits for each to be ready; stage 7 ends the run with the first exit or `Shutdown` and stops the rest in reverse start order, splitting the grace period between them (ruling 3).
- **Tests:** kiln-proto unit and property tests (Task 1); the host's version rules against the scripted guest (Task 2); the label, the schema version and potter's route (Task 4); `config::build` (Task 5); the share of the grace period (Task 6); seven boot cases on both VMMs, among them potter's PHP shape, a stuck main process and mixed versions (Task 7); `kiln run` end to end with busybox `httpd` as a service (Task 8).

**Tech Stack:**
- Rust 2024, in the existing workspace. No new external dependency; `kiln-image` depends on `kiln-proto` (in the workspace) from Task 4.
- Tools (Tasks 7, 8 on Linux): as M3b-1: Firecracker 1.17, Cloud Hypervisor 53, vmkit's 6.18.54 kernel, a static busybox (with `nc` and `httpd`), vmkit's `vmkit-sandbox` with its AppArmor profile. The mixed-version boot case also needs a static `kiln-init` built from kiln `main` (9c891ff), named by `KILN_TEST_INIT_V1`.

**Spec:** `docs/superpowers/specs/2026-09-30-kiln-design.md` (rev 2.10; rev 2.11, committed with this plan, records this plan's rulings). This plan implements milestone M3d (§12):
- §9.8 `services`: what M3d was to design (a service's `process`, readiness timeouts, how `Exited` reports a service, whether `kiln run` exposes services) and the versioning rules 1 to 4;
- §5.1 the config's `services` and `schemaVersion` 2 (ruling 1);
- §9.5 protocol version 2's messages;
- §9.6 stages 6 and 7 with services.

**Spec rev 2.11** (committed with this plan):
- Images declare services with the `dev.kiln.services` label of their OCI config (a JSON array of `{name, process, ready, stopSignal}`); `kiln convert` checks it and stores it as the kiln config's `services`, so the image digest covers it; a bad label fails the conversion. `kiln inspect` lists the services; `kiln run` has no flag for them (§5.1, §9.8).
- A config with services has `schemaVersion` 2, one without keeps 1; readers accept 2 only with services, 1 only without. A kiln older than M3d accepts only 1, so it refuses an image with services instead of running it without them (ruling 1, §5.1).
- Control protocol version 2: `Config.services`, `Config.readyTimeoutMs`, `Exited.service`. A v1 host refuses a v2 `kiln-init`'s `Hello`; that is by design, since `kiln run` boots the `kiln-init` pinned to it (§8.1), and the two ship together (ruling 2, §9.5, §9.8).
- Readiness: `unix:<path>` and `tcp:<port>` are met by a connection that is accepted (connect, then close at once), `none` at once. `tcp:` probes `127.0.0.1` only, so a service must listen on the loopback or the wildcard address (ruling 4); the probes may show up in the service's log (ruling 5). `readyTimeoutMs` bounds all services from `Config`'s arrival; the host sets it to what remains of the boot timeout less 2 s (§9.6, §9.8).
- Stopping: the first process to exit (a service or the main process), or `Shutdown`, ends the run; the others are stopped in reverse start order, each with its own stop signal; the grace period is split evenly between the processes still to stop, and one that has not exited when its share ends gets SIGKILL before the next gets its stop signal. The whole sequence stays within the grace period, and the host's stop deadline is unchanged (ruling 3, §9.6). `Exited` carries the status of the process whose exit ended the run (naming it when it was a service), or after `Shutdown` the main process's; `kiln run` exits with that status and names the service (§9.5, §9.7).
- Services share the main process's stdout and stderr (with `-t`, the terminal, never as their controlling terminal or input); `kiln run --env` values reach every service as well as the main process; signals the host forwards go to the main process only (§9.6, §9.7).

It builds on kiln `main` (9c891ff, M3b-1). M3b-2 (`m3b2-run`) is not merged yet; Known gaps lists the conflicts expected when one of the two is rebased on the other.

## Global Constraints

- **Repository:**
  - `/Users/alfonso/Github/Personal/playground/kiln` (GitHub `AlfonsoCampodonico/kiln`, private).
  - Work on a new branch `m3d-services` created from `main` (9c891ff).
  - Do not push; the controller asks the user.
- **One PR** (ruling 6): Task 1 sets `PROTOCOL_VERSION` to 2, so a `kiln-init` built at Tasks 1 to 5 announces version 2 and accepts `Config.services` but ignores them until Task 6. Merge Tasks 1–9 together.
- **Commits:** plain messages with no `Co-Authored-By`, `Claude-Session` or other attribution lines.
- **Crates:** every crate root but `kiln-init`'s keeps `#![forbid(unsafe_code)]`; no new `unsafe`. No `expect()`, `unwrap()` or panicking index on the host's run path; tests may. `kiln-run`'s Linux code stays `#[cfg(target_os = "linux")]`, so `cargo test --all` works on macOS.
- **Compatibility:** a run without services sends a version 1 `Config` (byte-identical), so a version 1 `kiln-init` keeps working; an image without services keeps `schemaVersion` 1 and its digest (the golden digest test is unchanged).
- **Exit codes** (spec §9.5): unchanged. The refusals this plan adds (a service-needing run against a version 1 guest, a version violation) exit 125; a service that ends the run gives its own status.
- **Environment:**
  - macOS for every task's checks.
  - Linux checks (every task) in the Lima VM `vmkit` (Ubuntu 26.04 aarch64, nested KVM, Rust 1.99): `limactl shell vmkit`, `cd` to the repository, `export CARGO_TARGET_DIR=$HOME/target-m3d` (the AppArmor profile covers `/home/*/target-*/debug/vmkit-sandbox`). KVM runs need the `kvm` group (`sg kvm -c '…'`), Firecracker and Cloud Hypervisor on `PATH` (`$HOME/bin` there), `busybox-static`, and the kernel `/Users/alfonso/Github/Personal/playground/vmkit/out/vmlinux-6.18.54-aarch64`.
- **Formatting:** `rustfmt.toml` sets `max_width = 120`. The plan's code is already `cargo fmt`-clean. CI runs fmt, clippy (`-D warnings`) and the tests on ubuntu-24.04 and macos-15, and the boot job runs `scripts/boot-tests.sh` with `KILN_TEST_NET=1` on x86_64 KVM.
- **Validation:**
  - This plan was generated from task commits built on the validated prototype `m3d-proto` (a995565; findings T1–T9, with rulings 1 and 3 folded into Tasks 4, 6, 7 and 9) and replayed task by task, mechanically, on a fresh clone of kiln `main` (9c891ff) on macOS (Apple Silicon). After every task the replayed tree, `Cargo.lock` included, is identical to the task commit it was generated from.
  - After every task, fmt, clippy (`-D warnings`) and the whole workspace's tests passed on macOS: 400, 402, 402, 408, 410, 410, 410, 410, 410 tests after Tasks 1–9.
  - After every task, fmt, clippy and the tests passed on Linux (Lima `vmkit`, Ubuntu 26.04 aarch64, Rust 1.99): 499, 504, 504, 510, 512, 513, 527, 528, 528 tests (the boot and `kiln run` cases skip themselves without their environment).
  - The fail-first steps were checked: Tasks 1 and 4's tests do not compile on the previous task's tree (macOS); Task 2's `tests/session.rs` on Task 1's tree fails exactly `a_version_1_guest_is_refused_a_config_with_services` and `a_version_2_guest_gets_services_and_reports_a_service_exit` (Linux).
  - `kiln-init` and `hostile` build static for aarch64 (`statically linked`) and x86_64 (`static-pie linked`, rust-lld), clippy clean; a version 1 `kiln-init` built from `main` (9c891ff) served `KILN_TEST_INIT_V1`.
  - **Firecracker 1.17** (Task 9's tree, nested, 4 test threads, `KILN_TEST_NET=1`, `KILN_TEST_INIT_V1` set): the boot suite 30/30 (the 7 service cases included, `a_version_1_guest_and_services` run, not skipped) and `kiln run` end to end 12/12. Task 7's service command, as written, 7/7.
  - **Ruling 3's test bites:** `a_stuck_main_process_does_not_cost_a_service_its_stop_signal` against the prototype's `kiln-init` (serial stop, one shared deadline) fails on Firecracker (`["nginx-up"]`, no `fpm-quit`: php-fpm was SIGKILLed at the deadline), and passes with Task 6's.
  - **Cloud Hypervisor 53** (same): boot suite 10/30 and `kiln run` end to end 10/12 in the full run; every failure a stall (boot timeouts, mostly with an empty console or before stage 6; VMs not ending after their final message). Serial reruns of the failed service cases: `a_stuck_main_process_does_not_cost_a_service_its_stop_signal`, `shutdown_stops_the_services_in_reverse_order` and `services_from_the_image_label` passed on their 1st try, `a_version_1_guest_and_services` on its 2nd; `a_service_exiting_first_ends_the_run` and `a_service_that_is_not_ready_fails_the_boot` stalled in three tries each (boot timeouts at or before stage 5, and once a VM that did not end after the expected `InitFailed`). Both passed on Cloud Hypervisor in the prototype's runs (findings); `services_start_in_order_and_stop_in_reverse` and `services_in_tty_mode_write_to_the_terminal` passed in the full run. The Lima VM was shared with another agent's runs throughout.

## Review Focus

Inputs and conditions no other test targets that will bite a real user, most likely first. Each has a pinned test in the task named.

1. **potter's PHP image under `docker stop`-like shutdown, with an nginx that does not stop.** Expected: php-fpm still gets SIGQUIT (graceful drain) once nginx's share of the grace period ends, and the run ends within the grace period.
   - Pinned in Task 6 (`the_grace_period_is_split_evenly_between_the_processes_left`) and Task 7 (`a_stuck_main_process_does_not_cost_a_service_its_stop_signal`, `shutdown_stops_the_services_in_reverse_order`).
2. **An image with services run by a kiln from before M3d.** Expected: refused (`unsupported kiln image schema version 2`), never run without its services (ruling 1).
   - Pinned in Task 4 (`the_config_schema_version_must_match_its_services`, `the_services_label_becomes_the_configs_services`).
3. **A service listening on `::1` only, or one that crashes at start.** Expected: the boot fails at stage 6 naming the service (not ready by the deadline, or exited before it was ready), before the host's boot timeout; exec failures give 127 or 126 as for the main process.
   - Pinned in Task 7 (`a_service_that_is_not_ready_fails_the_boot`).
4. **php-fpm dying while nginx serves.** Expected: the run ends with php-fpm's status, `kiln run` says which service ended it, nginx is stopped.
   - Pinned in Task 7 (`a_service_exiting_first_ends_the_run`) and Task 8 (`services_from_the_image_label`).
5. **Mixed installs** (an older `kiln-init` named with `--init`). Expected: runs without services work; a run with services is refused before `Config` with the version it needs (125); the old guest refuses a version 2 `Config` sent anyway.
   - Pinned in Task 2 (`tests/session.rs`) and Task 7 (`a_version_1_guest_and_services`, with `KILN_TEST_INIT_V1`).
6. **An `APP_KEY` given with `kiln run --env`.** Expected: it reaches php-fpm as well as nginx.
   - Pinned in Task 5 (`services_get_the_run_time_environment_and_a_readiness_placeholder`) and Task 8.

## Design decisions (rulings made while prototyping and planning)

Controller rulings (`.scratch-notes/m3d-rulings.md`):
- **R1.** An image whose config has non-empty `services` is written with `schemaVersion: 2`; images without services stay at 1. Readers reject unknown versions (spec §5.1), so an older kiln refuses the image instead of silently running it without its services. Recorded in `docs/format.md` and spec rev 2.11.
- **R2.** A version 1 host refusing a version 2 `kiln-init` is by design: `kiln run` always boots the init pinned to it (§8.1), so host and init ship together. Documented in `docs/format.md`.
- **R3.** Processes stop in reverse start order; the grace period is split evenly between the processes still running; each gets its own stop signal, and if it has not exited when its share ends it gets SIGKILL and the next gets its stop signal. The whole sequence stays within the grace period; the host's stop deadline is unchanged. Test: a main process that ignores SIGTERM plus a service that logs SIGQUIT; the service sees SIGQUIT.
- **R4.** TCP readiness probes `127.0.0.1` only; services must listen on the loopback or the wildcard address (documented).
- **R5.** The connect-and-close probe may show up in a service's log; accepted and documented.
- **R6.** One PR, tasks as the prototype's split (findings), with R1 and R3 folded into the tasks they change (Tasks 4, 6, 7, 9).

Rulings made while planning (not in the prototype):
- **P1. Readers require the version that matches the services, both ways** (R1): version 1 with services and version 2 without are refused, as well as any other version. `kiln convert` always writes the matching version (`schema_version_for`), so the rule is exact and needs no "at least" reading; a version 1 config listing services is precisely what an older kiln would run without them.
- **P2. The share is recomputed at each step** (R3): a process's share is what remains of the grace period divided by the number of processes still to stop, itself included. A process that exits early leaves its unused time to those after it; with one process its share is the whole grace period, which is version 1's behaviour exactly. A later `Shutdown` that shortens the deadline also caps the current share at its share of what remains.
- **P3. SIGKILL at the end of a share goes to the process only, not its process group**, as its stop signal does; whatever it left behind is killed with everything else once every supervised process has exited (or at the deadline).
- **P4. The mixed-version boot case skips itself without `KILN_TEST_INIT_V1`**, so `scripts/boot-tests.sh` (and CI) does not build an old `kiln-init`; this plan's validation built one from `main` (9c891ff).

From the prototype (findings), kept:
- **D1.** A service's `process` is the main process's without `stopSignal` (the service's own `stopSignal` sits beside it); unknown fields are refused at every level, including `restart`. Names `[a-z0-9-]{1,32}`, unique, at most 16.
- **D2.** Run-time `--env` values are appended to every service's environment as to the main process's (Docker has no equivalent; it is what a Laravel `APP_KEY` needs in php-fpm).
- **D3.** `readyTimeoutMs` is the host's: what remains of the boot timeout less 2 s when `Config` is sent, so the guest's `InitFailed` naming the service arrives before the host's boot timeout. There is no per-service timeout and no image-level knob.
- **D4.** With `-t`, services write to the pseudo-terminal as plain stdout and stderr (stdin `/dev/null`, never their controlling terminal); without it they share the main process's pipes, made writable by all (0622) when a service runs as another uid so it can reopen `/dev/stdout`.
- **D5.** `Exited` reports the first exit (naming the service), or after `Shutdown` the main process's status; a service killed at the deadline after the main process ended is not reported.
- **D6.** Host signals (`Signal`) go to the main process only, and are dropped once it has exited.

**Known gaps:**
- **Rebasing onto M3b-2** (`m3b2-run`, ef32f24 when this plan was written): a trial merge conflicts in `crates/kiln-image/src/error.rs` (both add variants at the end), `crates/kiln-init/src/linux/supervise.rs` (M3b-2 edits the module comment Task 6 rewrites), `crates/kiln-run/src/config.rs` (M3b-2's `Config.network` next to Task 5's `build` signature and services) and `docs/format.md` (the config's `kernel`/`init` lines and `resolv.conf` next to this plan's lines). The rest merges cleanly, but three behaviours meet: M3b-2 moves the scratch disk's sync before `Exited` (it stays after `supervise::run` returns, so services are stopped first); M3b-2's host stop deadline becomes the grace period plus the end bound (ruling 3's sequence stays inside the grace period, so it holds); and `convert`'s `KilnConfig` gains M3b-2's `kernel`/`init` beside this plan's `schema_version_for`. Whichever lands second reruns the boot suite.
- During stage 6, init reaps services with `waitpid(pid)`; orphans a service leaves before stage 7 stay zombies until stage 7 starts (harmless for short startups).
- A service bound only to `::1` or to the guest's network address is never ready (R4; documented, pinned by the not-ready case).
- The mixed-version boot case runs only with `KILN_TEST_INIT_V1` (P4); CI does not run it.
- Cloud Hypervisor 53 under nested virtualization stalls guests (spec §14): judged by pass rate with reruns; Firecracker is the reference. Re-check Cloud Hypervisor on x86_64 CI before merging.

## File Structure

```
crates/kiln-proto/src/{config,exit,message,lib}.rs   Service, Ready, Config.services/readyTimeoutMs, Exited.service, v2   Task 1
crates/kiln-proto/tests/props.rs                                                                                       Task 1
crates/kiln-run/src/protocol.rs                       Protocol::new, version(), version_refusal                         Tasks 1, 2
crates/kiln-run/src/linux/session.rs                  the refusal before Config; readyTimeoutMs                         Tasks 1, 2, 7
crates/kiln-run/tests/session.rs                                                                                       Tasks 1, 2
crates/kiln-oci/src/types.rs                          ContainerConfig.labels, label()                                   Task 3
crates/kiln-image/src/types.rs                        LABEL_SERVICES, Service, ServiceProcess, schema_version_for       Task 4
crates/kiln-image/src/{convert,load,error}.rs, tests/convert.rs                                                        Task 4
crates/kiln-run/src/{config,prepare}.rs, linux/run.rs build(.., services, ..), Prepared.services                       Task 5
crates/kiln/src/{main,run_cmd}.rs                     inspect's service lines (4); the service report (5)
crates/kiln-init/src/linux/{process,supervise,sys,mod}.rs   start services; supervise them                             Task 6 (+ 1)
crates/kiln-run/tests/{boot.rs,support/mod.rs}, crates/kiln-testguest/src/main.rs                                      Task 7 (+ 1)
crates/kiln/tests/run.rs                                                                                               Task 8
docs/format.md                                                                                                         Task 9
```

Each step below gives a file's whole content, or exact replacements in it ("In `path`, replace … with …", each old text occurring exactly once at that point). Cargo updates `Cargo.lock` on the first build.

---

### Task 1: `kiln-proto`: protocol version 2 with `Config.services`, `readyTimeoutMs` and `Exited.service`

**Files:**
- Modify: `crates/kiln-init/src/linux/supervise.rs`, `crates/kiln-proto/src/config.rs`, `crates/kiln-proto/src/exit.rs`, `crates/kiln-proto/src/lib.rs`, `crates/kiln-proto/src/message.rs`, `crates/kiln-proto/tests/props.rs`, `crates/kiln-run/src/config.rs`, `crates/kiln-run/src/linux/session.rs`, `crates/kiln-run/src/protocol.rs`, `crates/kiln-run/tests/session.rs`, `crates/kiln-run/tests/support/driver.rs`, `crates/kiln-run/tests/support/mod.rs`, `crates/kiln-testguest/src/main.rs`

**Interfaces:**
- Consumes: version 1's `Config`, `Process`, `Exited`, `Hello`, `ProtoError::invalid`, `signal::{parse_stop_signal, name, DEFAULT_STOP}`, `sanitize::clean_line`.
- Produces:
  - `kiln_proto::{Service { name, process: Process, ready: Ready, stop_signal: i32 }, Ready::{None, Unix(String), Tcp(u16)}}` (`Ready` is carried as the string `none` / `unix:<path>` / `tcp:<port>`, with `Display`, `TryFrom<String>`, `is_none()`, `validate()`); `MAX_SERVICES` (16), `MAX_SERVICE_NAME` (32), `MAX_SOCKET_PATH` (107); `valid_service_name(&str) -> bool`; `validate_services(&[Service]) -> Result<()>`.
  - `Config { services: Vec<Service> (omitted when empty), ready_timeout_ms: Option<u32> (present exactly with services), .. }`; `Config::required_version() -> u32` (2 with services, else 1).
  - `Exited { signaled, code, service: Option<String> }` (no longer `Copy`); `Exited::main(signaled, code)`; `Exited::describe_service() -> Option<String>`.
  - `PROTOCOL_VERSION = 2`.
  - `kiln_run::protocol::Protocol::exited() -> Option<&Exited>`; a guest `Hello` of 1 or 2 is accepted (still 1 for any `Config`: Task 2 negotiates).

The wire format of protocol version 2 (spec §9.8 rule 1): only additions. A `Config` without services serialises exactly as version 1's (`services` and `readyTimeoutMs` omitted), so a version 1 `kiln-init` still accepts it, and `required_version()` says which version a `Config` needs. Decoding stays strict: unknown fields are refused at every level, inside a service too.

- A service is a name, a `Process` validated as the main process's is, a readiness and a stop signal (1–31). `ready` is always present on the wire; the image format (Task 4) lets it default to `none`.
- `readyTimeoutMs` is 1 to `u32::MAX` and present exactly when services are: the host sets it (Task 2), the guest uses it as its deadline (Task 6).
- `Exited.service` names the service whose exit ended the run (validated as a name; `null` reads as absent). `describe_service()` is the line `kiln run` prints for it (Task 5).
- The other crates follow: `Exited::main` where they built `Exited`, the new `Config` fields in literals, and the host accepts `Hello` 1 or 2 (a 3 is now the unsupported example).

- [ ] **Step 1: Modify `crates/kiln-proto/tests/props.rs`**

Round trips with services (any valid set) and with `Exited.service`; every encoded `Config` decodes back and validates.

In `crates/kiln-proto/tests/props.rs`, replace:
```rust
    Scratch, Shutdown, Signal, Stage, WindowSize, read_message, write_message,
```
with:
```rust
    Ready, Scratch, Service, Shutdown, Signal, Stage, WindowSize, read_message, write_message,
```

In `crates/kiln-proto/tests/props.rs`, replace:
```rust
        (0i32..=255).prop_map(|code| GuestMessage::Exited(Exited { signaled: false, code })),
        (1i32..=64).prop_map(|code| GuestMessage::Exited(Exited { signaled: true, code })),
        (1u8..=7, proptest::option::of(1i32..4096), "\\PC{0,200}")
            .prop_map(|(s, e, m)| GuestMessage::InitFailed(InitFailed::new(s, e, &m))),
    ]
```
with:
```rust
        (0i32..=255).prop_map(|code| GuestMessage::Exited(Exited::main(false, code))),
        (1i32..=64).prop_map(|code| GuestMessage::Exited(Exited::main(true, code))),
        (any::<bool>(), 1i32..=64, "[a-z0-9-]{1,32}").prop_map(|(signaled, code, name)| {
            GuestMessage::Exited(Exited {
                service: Some(name),
                ..Exited::main(signaled, code)
            })
        }),
        (1u8..=7, proptest::option::of(1i32..4096), "\\PC{0,200}")
            .prop_map(|(s, e, m)| GuestMessage::InitFailed(InitFailed::new(s, e, &m))),
    ]
}

fn ready() -> impl Strategy<Value = Ready> {
    prop_oneof![
        Just(Ready::None),
        "/[a-z/.]{0,60}".prop_map(Ready::Unix),
        (1u16..).prop_map(Ready::Tcp),
    ]
}

fn services() -> impl Strategy<Value = Vec<Service>> {
    proptest::collection::vec(
        (
            proptest::collection::vec("[a-z/]{1,10}", 1..3),
            proptest::collection::vec("[A-Z_]{1,8}=[^\u{0}]{0,10}", 0..3),
            proptest::option::of("/[a-z/]{0,20}"),
            proptest::option::of("[a-z0-9]{1,8}(:[a-z0-9]{1,8})?"),
            ready(),
            1i32..=31,
        ),
        0..4,
    )
    .prop_map(|v| {
        v.into_iter()
            .enumerate()
            .map(|(i, (cmd, env, working_dir, user, ready, stop_signal))| Service {
                name: format!("svc-{i}"),
                process: Process {
                    entrypoint: Vec::new(),
                    cmd,
                    env,
                    working_dir,
                    user,
                },
                ready,
                stop_signal,
            })
            .collect()
    })
```

In `crates/kiln-proto/tests/props.rs`, replace:
```rust
    )
        .prop_map(|(entrypoint, cmd, env, working_dir, user, sig, more)| {
            let (stop_signal, tty, interactive, hostname) = sig;
            let (layers, blocks, poweroff, grace, net) = more;
            Config {
                process: Process {
                    entrypoint,
                    cmd,
                    env,
                    working_dir,
                    user,
                },
                stop_signal,
                tty: tty.then_some(WindowSize { rows: 24, cols: 80 }),
                interactive,
                hostname,
                network: net.then(|| Network {
                    address: Ipv4Addr::new(172, 30, 0, 2),
                    prefix_len: 30,
                    gateway: Ipv4Addr::new(172, 30, 0, 1),
                    dns: vec![Ipv4Addr::new(172, 30, 0, 1)],
                }),
                layers,
                scratch: Scratch {
                    size_bytes: blocks * 4096,
                },
                exit_method: if poweroff {
                    ExitMethod::Poweroff
                } else {
                    ExitMethod::Reboot
                },
                shutdown_grace_secs: grace,
            }
        })
```
with:
```rust
        (services(), 1u32..),
    )
        .prop_map(
            |(entrypoint, cmd, env, working_dir, user, sig, more, (services, ready_ms))| {
                let (stop_signal, tty, interactive, hostname) = sig;
                let (layers, blocks, poweroff, grace, net) = more;
                Config {
                    process: Process {
                        entrypoint,
                        cmd,
                        env,
                        working_dir,
                        user,
                    },
                    stop_signal,
                    tty: tty.then_some(WindowSize { rows: 24, cols: 80 }),
                    interactive,
                    hostname,
                    network: net.then(|| Network {
                        address: Ipv4Addr::new(172, 30, 0, 2),
                        prefix_len: 30,
                        gateway: Ipv4Addr::new(172, 30, 0, 1),
                        dns: vec![Ipv4Addr::new(172, 30, 0, 1)],
                    }),
                    layers,
                    scratch: Scratch {
                        size_bytes: blocks * 4096,
                    },
                    exit_method: if poweroff {
                        ExitMethod::Poweroff
                    } else {
                        ExitMethod::Reboot
                    },
                    shutdown_grace_secs: grace,
                    ready_timeout_ms: (!services.is_empty()).then_some(ready_ms),
                    services,
                }
            },
        )
```

In `crates/kiln-proto/tests/props.rs`, replace:
```rust
            "process", "stopSignal", "interactive", "hostname", "layers", "scratch", "exitMethod", "shutdownGraceSecs",
        ])
```
with:
```rust
            "process", "stopSignal", "interactive", "hostname", "layers", "scratch", "exitMethod", "shutdownGraceSecs",
            "services", "readyTimeoutMs", "service", "ready", "name",
        ])
```

In `crates/kiln-proto/tests/props.rs`, replace:
```rust
        1 => "[a-z]{0,6}".prop_map(|s| format!("\"{s}\"")),
        1 => Just("{}".to_string()),
```
with:
```rust
        1 => "[a-z]{0,6}".prop_map(|s| format!("\"{s}\"")),
        1 => proptest::sample::select(vec!["\"unix:/s\"", "\"tcp:80\"", "\"tcp:0\"", "\"none\"", "\"unix:x\""])
            .prop_map(String::from),
        1 => Just("{}".to_string()),
```

- [ ] **Step 2: Run the tests to see them fail**

Run:
```bash
cargo test -q -p kiln-proto --test props
```
Expected: FAIL to compile: `kiln_proto::{Ready, Service}`, `Config::services` and `Exited::service` do not exist yet.

- [ ] **Step 3: Modify `crates/kiln-proto/src/config.rs`**

`Service`, `Ready`, the limits and their validation; `Config`'s two new fields, `validate()` and `required_version()`. Unit tests: a `Config` without services is a version 1 `Config` byte for byte; `Ready`'s three forms; each invalid service refused; unknown service fields refused.

In `crates/kiln-proto/src/config.rs`, replace:
```rust
const MAX_DNS: usize = 3;

```
with:
```rust
const MAX_DNS: usize = 3;
/// The most services a run may have (protocol version 2).
pub const MAX_SERVICES: usize = 16;
/// The longest service name, in bytes.
pub const MAX_SERVICE_NAME: usize = 32;
/// The longest `unix:` readiness path, in bytes: `sun_path` holds 108 with its NUL.
pub const MAX_SOCKET_PATH: usize = 107;

```

In `crates/kiln-proto/src/config.rs`, replace:
```rust
    /// The grace period when init gets SIGINT (Ctrl-Alt-Del) instead of `Shutdown`.
    pub shutdown_grace_secs: u32,
```
with:
```rust
    /// The grace period when init gets SIGINT (Ctrl-Alt-Del) instead of `Shutdown`,
    /// and when a process's exit stops the others (protocol version 2).
    pub shutdown_grace_secs: u32,
    /// Protocol version 2: processes started, in order, before the main process,
    /// each waiting for the previous one's readiness. Omitted when empty, so a
    /// `Config` without services is a version 1 `Config`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<Service>,
    /// Protocol version 2, present exactly when `services` is not empty: how long,
    /// from `Config`'s arrival, the guest allows every service to become ready.
    /// The host derives it from what remains of its boot timeout, so that a
    /// service that never becomes ready is reported (`InitFailed` naming it)
    /// before the host's own boot timeout ends the VM.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_timeout_ms: Option<u32>,
}

/// A process kiln-init starts before the main process and stops after it
/// (protocol version 2; spec §9.8 `services`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Service {
    /// `[a-z0-9-]{1,32}`, unique in the run; names the service in failures and in `Exited`.
    pub name: String,
    /// As the main process's: argv, the final environment, working directory, user.
    pub process: Process,
    /// When the service counts as started.
    pub ready: Ready,
    /// Sent when the service is stopped (1–31).
    pub stop_signal: i32,
}

/// When a service is ready: `unix:<absolute path>` (a stream socket there accepts
/// a connection), `tcp:<port>` (127.0.0.1:<port> accepts one), or `none` (as soon
/// as it has been started). Carried as that string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Ready {
    None,
    Unix(String),
    Tcp(u16),
}

impl Ready {
    pub fn is_none(&self) -> bool {
        matches!(self, Ready::None)
    }

    /// Checks the value's own rules (the parser applies them too).
    pub fn validate(&self) -> std::result::Result<(), String> {
        match self {
            Ready::None => Ok(()),
            Ready::Tcp(0) => Err("tcp port 0".into()),
            Ready::Tcp(_) => Ok(()),
            Ready::Unix(p) if !p.starts_with('/') => Err(format!("unix socket path {p:?} is not absolute")),
            Ready::Unix(p) if p.contains('\0') => Err("unix socket path contains a NUL byte".into()),
            Ready::Unix(p) if p.len() > MAX_SOCKET_PATH => Err(format!(
                "unix socket path of {} bytes (at most {MAX_SOCKET_PATH})",
                p.len()
            )),
            Ready::Unix(_) => Ok(()),
        }
    }
}

impl TryFrom<String> for Ready {
    type Error = String;

    fn try_from(s: String) -> std::result::Result<Self, String> {
        let r = if s == "none" {
            Ready::None
        } else if let Some(path) = s.strip_prefix("unix:") {
            Ready::Unix(path.to_string())
        } else if let Some(port) = s.strip_prefix("tcp:") {
            // Digits only: no sign, no spaces.
            if port.is_empty() || port.len() > 5 || !port.bytes().all(|b| b.is_ascii_digit()) {
                return Err(format!("ready {s:?}: the port is not a number 1-65535"));
            }
            Ready::Tcp(
                port.parse()
                    .map_err(|_| format!("ready {s:?}: the port is not a number 1-65535"))?,
            )
        } else {
            return Err(format!("ready {s:?} (unix:<path>, tcp:<port> or none)"));
        };
        r.validate().map_err(|e| format!("ready {s:?}: {e}"))?;
        Ok(r)
    }
}

impl From<Ready> for String {
    fn from(r: Ready) -> String {
        r.to_string()
    }
}

impl std::fmt::Display for Ready {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Ready::None => f.write_str("none"),
            Ready::Unix(p) => write!(f, "unix:{p}"),
            Ready::Tcp(port) => write!(f, "tcp:{port}"),
        }
    }
}

/// A valid service name: `[a-z0-9-]{1,32}`.
pub fn valid_service_name(name: &str) -> bool {
    (1..=MAX_SERVICE_NAME).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Checks a run's services: at most [`MAX_SERVICES`], unique valid names, each
/// process as valid as the main process, a valid readiness and stop signal.
/// kiln-image calls it for an image's services, and `Config` for its own.
pub fn validate_services(services: &[Service]) -> Result<()> {
    if services.len() > MAX_SERVICES {
        return Err(invalid(format!("{} services (at most {MAX_SERVICES})", services.len())));
    }
    let mut seen = std::collections::BTreeSet::new();
    for s in services {
        if !valid_service_name(&s.name) {
            return Err(invalid(format!(
                "service name {:?} (1-{MAX_SERVICE_NAME} of [a-z0-9-])",
                s.name
            )));
        }
        if !seen.insert(s.name.as_str()) {
            return Err(invalid(format!("two services named {:?}", s.name)));
        }
        s.process
            .validate()
            .map_err(|e| invalid(format!("service {}: {e}", s.name)))?;
        s.ready
            .validate()
            .map_err(|e| invalid(format!("service {}: ready: {e}", s.name)))?;
        check_signal(s.stop_signal).map_err(|e| invalid(format!("service {}: {e}", s.name)))?;
    }
    Ok(())
}

fn check_signal(sig: i32) -> std::result::Result<(), String> {
    match crate::signal::name(sig) {
        Some(_) => Ok(()),
        None => Err(format!(
            "stopSignal {sig} (signals 1-31 only; real-time signals are not supported)"
        )),
    }
```

In `crates/kiln-proto/src/config.rs`, replace:
```rust
        self.entrypoint.iter().chain(&self.cmd).map(String::as_str).collect()
    }
```
with:
```rust
        self.entrypoint.iter().chain(&self.cmd).map(String::as_str).collect()
    }

    /// The rules for the main process and every service's: a non-empty argv[0],
    /// `KEY=value` entries, an absolute working directory, `user[:group]`, no NUL.
    pub fn validate(&self) -> std::result::Result<(), String> {
        let argv = self.argv();
        if argv.first().is_none_or(|a| a.is_empty()) {
            return Err("no command: entrypoint and cmd are empty".into());
        }
        for a in &argv {
            no_nul("an argument", a)?;
        }
        for e in &self.env {
            no_nul("an environment entry", e)?;
            if e.split_once('=')
                .is_none_or(|(k, _)| k.is_empty() || k.chars().any(char::is_control))
            {
                return Err(format!("environment entry {e:?} is not KEY=value"));
            }
        }
        if let Some(w) = &self.working_dir {
            no_nul("workingDir", w)?;
            if !w.starts_with('/') {
                return Err(format!("workingDir {w:?} is not absolute"));
            }
        }
        if let Some(u) = &self.user {
            no_nul("user", u)?;
            let parts: Vec<&str> = u.split(':').collect();
            if parts.len() > 2 || parts.iter().any(|s| s.is_empty() || s.chars().any(char::is_control)) {
                return Err(format!("user {u:?} is not user[:group]"));
            }
        }
        Ok(())
    }
```

In `crates/kiln-proto/src/config.rs`, replace:
```rust
fn no_nul(what: &str, s: &str) -> Result<()> {
    if s.contains('\0') {
        return Err(invalid(format!("{what} contains a NUL byte")));
```
with:
```rust
fn no_nul(what: &str, s: &str) -> std::result::Result<(), String> {
    if s.contains('\0') {
        return Err(format!("{what} contains a NUL byte"));
```

In `crates/kiln-proto/src/config.rs`, replace:
```rust
        let p = &self.process;
        let argv = p.argv();
        if argv.first().is_none_or(|a| a.is_empty()) {
            return Err(invalid("no command: entrypoint and cmd are empty"));
        }
        for a in &argv {
            no_nul("an argument", a)?;
        }
        for e in &p.env {
            no_nul("an environment entry", e)?;
            if e.split_once('=')
                .is_none_or(|(k, _)| k.is_empty() || k.chars().any(char::is_control))
            {
                return Err(invalid(format!("environment entry {e:?} is not KEY=value")));
            }
        }
        if let Some(w) = &p.working_dir {
            no_nul("workingDir", w)?;
            if !w.starts_with('/') {
                return Err(invalid(format!("workingDir {w:?} is not absolute")));
            }
        }
        if let Some(u) = &p.user {
            no_nul("user", u)?;
            let parts: Vec<&str> = u.split(':').collect();
            if parts.len() > 2 || parts.iter().any(|s| s.is_empty() || s.chars().any(char::is_control)) {
                return Err(invalid(format!("user {u:?} is not user[:group]")));
            }
        }
        if crate::signal::name(self.stop_signal).is_none() {
            return Err(invalid(format!(
                "stopSignal {} (signals 1-31 only; real-time signals are not supported)",
                self.stop_signal
            )));
        }
```
with:
```rust
        self.process.validate().map_err(invalid)?;
        check_signal(self.stop_signal).map_err(invalid)?;
```

In `crates/kiln-proto/src/config.rs`, replace:
```rust
            n.validate()?;
        }
        Ok(())
    }
}
```
with:
```rust
            n.validate()?;
        }
        validate_services(&self.services)?;
        match (self.services.is_empty(), self.ready_timeout_ms) {
            (true, Some(_)) => return Err(invalid("readyTimeoutMs without services")),
            (false, None) => return Err(invalid("services without readyTimeoutMs")),
            (false, Some(0)) => return Err(invalid("readyTimeoutMs 0")),
            _ => {}
        }
        Ok(())
    }

    /// The lowest protocol version that defines everything this `Config` uses
    /// (spec §9.8 rule 3): 2 with services, else 1.
    pub fn required_version(&self) -> u32 {
        if self.services.is_empty() { 1 } else { 2 }
    }
}
```

In `crates/kiln-proto/src/config.rs`, replace:
```rust
            shutdown_grace_secs: 10,
        }
    }
```
with:
```rust
            shutdown_grace_secs: 10,
            services: Vec::new(),
            ready_timeout_ms: None,
        }
    }

    fn fpm() -> Service {
        Service {
            name: "php-fpm".into(),
            process: Process {
                entrypoint: vec!["php-fpm".into()],
                cmd: vec!["-F".into()],
                env: vec!["APP_ENV=production".into()],
                working_dir: Some("/app".into()),
                user: None,
            },
            ready: Ready::Unix("/run/php-fpm.sock".into()),
            stop_signal: 3,
        }
    }

    fn with_services(services: Vec<Service>) -> Config {
        Config {
            services,
            ready_timeout_ms: Some(20_000),
            ..config()
        }
    }

    #[test]
    fn a_config_without_services_is_a_version_1_config() {
        let c = config();
        assert_eq!(c.required_version(), 1);
        let json = serde_json::to_string(&c).unwrap();
        assert!(!json.contains("services") && !json.contains("readyTimeoutMs"), "{json}");
        let c = with_services(vec![fpm()]);
        c.validate().unwrap();
        assert_eq!(c.required_version(), 2);
        let json = serde_json::to_string(&c).unwrap();
        assert!(
            json.contains(r#""services":[{"name":"php-fpm","process":{"#)
                && json.contains(r#""ready":"unix:/run/php-fpm.sock","stopSignal":3}]"#)
                && json.contains(r#""readyTimeoutMs":20000"#),
            "{json}"
        );
        assert_eq!(serde_json::from_str::<Config>(&json).unwrap(), c);
    }

    #[test]
    fn ready_parses_and_prints_its_three_forms() {
        for (text, ready) in [
            ("none", Ready::None),
            ("unix:/run/php-fpm.sock", Ready::Unix("/run/php-fpm.sock".into())),
            ("tcp:9000", Ready::Tcp(9000)),
            ("tcp:65535", Ready::Tcp(65535)),
        ] {
            assert_eq!(Ready::try_from(text.to_string()).unwrap(), ready);
            assert_eq!(ready.to_string(), text);
        }
        let long = format!("unix:/{}", "s".repeat(MAX_SOCKET_PATH));
        for bad in [
            "",
            "None",
            "unix:",
            "unix:run/x",
            "unix:/a\0b",
            "tcp:",
            "tcp:0",
            "tcp:65536",
            "tcp:+80",
            "tcp: 80",
            "tcp:80x",
            "tcp:0080000",
            "http:80",
            &long,
        ] {
            assert!(Ready::try_from(bad.to_string()).is_err(), "{bad:?} was accepted");
        }
        let ok = format!("unix:/{}", "s".repeat(MAX_SOCKET_PATH - 1));
        Ready::try_from(ok).unwrap();
    }

    #[test]
    fn each_invalid_service_is_refused() {
        type Mutation = Box<dyn Fn(&mut Vec<Service>)>;
        let cases: Vec<(&str, Mutation)> = vec![
            ("empty name", Box::new(|s| s[0].name = String::new())),
            ("upper case", Box::new(|s| s[0].name = "PHP".into())),
            ("underscore", Box::new(|s| s[0].name = "php_fpm".into())),
            ("long name", Box::new(|s| s[0].name = "a".repeat(MAX_SERVICE_NAME + 1))),
            ("duplicate", Box::new(|s| s.push(fpm()))),
            (
                "too many",
                Box::new(|s| {
                    *s = (0..=MAX_SERVICES)
                        .map(|i| Service {
                            name: format!("s{i}"),
                            ..fpm()
                        })
                        .collect()
                }),
            ),
            ("no argv", Box::new(|s| s[0].process = Process::default())),
            (
                "relative workdir",
                Box::new(|s| s[0].process.working_dir = Some("app".into())),
            ),
            ("bad user", Box::new(|s| s[0].process.user = Some("a:b:c".into()))),
            ("bad env", Box::new(|s| s[0].process.env = vec!["X".into()])),
            ("stop signal 0", Box::new(|s| s[0].stop_signal = 0)),
            ("stop signal 34", Box::new(|s| s[0].stop_signal = 34)),
            (
                "relative socket",
                Box::new(|s| s[0].ready = Ready::Unix("run/x".into())),
            ),
            ("port 0", Box::new(|s| s[0].ready = Ready::Tcp(0))),
        ];
        for (name, mutate) in cases {
            let mut services = vec![fpm()];
            mutate(&mut services);
            let err = with_services(services).validate().unwrap_err().to_string();
            assert!(!err.is_empty(), "{name}");
        }
        let max: Vec<Service> = (0..MAX_SERVICES)
            .map(|i| Service {
                name: format!("s{i}"),
                ..fpm()
            })
            .collect();
        with_services(max).validate().unwrap();
        let mut c = with_services(vec![fpm()]);
        c.ready_timeout_ms = None;
        assert!(c.validate().unwrap_err().to_string().contains("readyTimeoutMs"));
        c.ready_timeout_ms = Some(0);
        assert!(c.validate().is_err());
        let mut c = config();
        c.ready_timeout_ms = Some(1);
        assert!(c.validate().unwrap_err().to_string().contains("without services"));
    }

    #[test]
    fn unknown_service_fields_are_refused() {
        let mut v = serde_json::to_value(with_services(vec![fpm()])).unwrap();
        v["services"][0]["restart"] = "always".into();
        assert!(serde_json::from_value::<Config>(v).is_err());
        let mut v = serde_json::to_value(with_services(vec![fpm()])).unwrap();
        v["services"][0]["ready"] = "tcp:x".into();
        assert!(serde_json::from_value::<Config>(v).is_err());
    }
```

- [ ] **Step 4: Modify `crates/kiln-proto/src/message.rs`**

`Exited.service`, validated as a name.

In `crates/kiln-proto/src/message.rs`, replace:
```rust
/// The main process ended, and its stdout and stderr reached EOF. With
/// `signaled`, `code` is the signal that killed it. Guest-asserted (spec §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
```
with:
```rust
/// The run ended, and its stdout and stderr reached EOF. With `signaled`, `code`
/// is the signal that killed the process. Without `service` the status is the
/// main process's; with it (protocol version 2), that service exited first and
/// ended the run, and the status is the service's. Guest-asserted (spec §3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
```

In `crates/kiln-proto/src/message.rs`, replace:
```rust
    pub code: i32,
}
```
with:
```rust
    pub code: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
}

impl Exited {
    /// The main process's status.
    pub fn main(signaled: bool, code: i32) -> Self {
        Self {
            signaled,
            code,
            service: None,
        }
    }
}
```

In `crates/kiln-proto/src/message.rs`, replace:
```rust
                if ok {
                    Ok(())
                } else {
                    Err(ProtoError::invalid(
                        "Exited",
                        format!("code {} (signaled: {})", e.code, e.signaled),
                    ))
```
with:
```rust
                if !ok {
                    return Err(ProtoError::invalid(
                        "Exited",
                        format!("code {} (signaled: {})", e.code, e.signaled),
                    ));
                }
                match &e.service {
                    Some(name) if !crate::config::valid_service_name(name) => {
                        Err(ProtoError::invalid("Exited", "service name"))
                    }
                    _ => Ok(()),
```

In `crates/kiln-proto/src/message.rs`, replace:
```rust
        assert!(GuestMessage::decode(EXITED, br#"{"signaled":true,"code":9}"#).is_ok());
        assert!(GuestMessage::decode(INIT_FAILED, br#"{"stage":6,"errno":0,"message":""}"#).is_err());
```
with:
```rust
        assert!(GuestMessage::decode(EXITED, br#"{"signaled":true,"code":9}"#).is_ok());
        assert!(GuestMessage::decode(EXITED, br#"{"signaled":false,"code":1,"service":"php-fpm"}"#).is_ok());
        assert!(GuestMessage::decode(EXITED, br#"{"signaled":false,"code":1,"service":null}"#).is_ok());
        assert!(GuestMessage::decode(EXITED, br#"{"signaled":false,"code":1,"service":"Bad Name"}"#).is_err());
        assert!(GuestMessage::decode(EXITED, br#"{"signaled":false,"code":1,"service":""}"#).is_err());
        assert!(GuestMessage::decode(INIT_FAILED, br#"{"stage":6,"errno":0,"message":""}"#).is_err());
```

- [ ] **Step 5: Modify `crates/kiln-proto/src/exit.rs`**

The report line for a service that ended the run.

In `crates/kiln-proto/src/exit.rs`, replace:
```rust
        if self.signaled { 128 + self.code } else { self.code }
    }
```
with:
```rust
        if self.signaled { 128 + self.code } else { self.code }
    }

    /// When a service ended the run: one sanitised line for the user (T8),
    /// `service <name> exited with <code>` or `... was killed by SIG<name>`.
    pub fn describe_service(&self) -> Option<String> {
        let name = clean_line(self.service.as_deref()?);
        Some(if self.signaled {
            let sig = crate::signal::name(self.code).map_or_else(|| self.code.to_string(), |n| format!("SIG{n}"));
            format!("service {name} was killed by {sig}, which ended the run")
        } else {
            format!("service {name} exited with {}, which ended the run", self.code)
        })
    }
```

In `crates/kiln-proto/src/exit.rs`, replace:
```rust
        assert_eq!(
            Exited {
                signaled: false,
                code: 3
            }
            .exit_code(),
            3
        );
        assert_eq!(
            Exited {
                signaled: true,
                code: 9
            }
            .exit_code(),
            137
```
with:
```rust
        assert_eq!(Exited::main(false, 3).exit_code(), 3);
        assert_eq!(Exited::main(true, 9).exit_code(), 137);
        let svc = Exited {
            service: Some("php-fpm".into()),
            ..Exited::main(false, 70)
        };
        assert_eq!(svc.exit_code(), 70);
        assert_eq!(
            svc.describe_service().unwrap(),
            "service php-fpm exited with 70, which ended the run"
        );
        assert_eq!(Exited::main(false, 0).describe_service(), None);
        let killed = Exited {
            service: Some("web".into()),
            ..Exited::main(true, 9)
        };
        assert_eq!(
            killed.describe_service().unwrap(),
            "service web was killed by SIGKILL, which ended the run"
```

- [ ] **Step 6: Modify `crates/kiln-proto/src/lib.rs`**

Exports; version 2.

In `crates/kiln-proto/src/lib.rs`, replace:
```rust
pub use config::{Config, ExitMethod, MAX_LAYERS, MIN_SCRATCH_BYTES, Network, Process, Scratch};
```
with:
```rust
pub use config::{
    Config, ExitMethod, MAX_LAYERS, MAX_SERVICE_NAME, MAX_SERVICES, MAX_SOCKET_PATH, MIN_SCRATCH_BYTES, Network,
    Process, Ready, Scratch, Service, valid_service_name, validate_services,
};
```

In `crates/kiln-proto/src/lib.rs`, replace:
```rust
/// The protocol version a guest announces in [`Hello`].
pub const PROTOCOL_VERSION: u32 = 1;
```
with:
```rust
/// The highest protocol version this crate implements: what a guest announces in
/// [`Hello`], and the highest a host accepts (spec §9.8). Version 2 adds
/// `Config.services`, `Config.readyTimeoutMs` and `Exited.service`.
pub const PROTOCOL_VERSION: u32 = 2;
```

- [ ] **Step 7: Modify `crates/kiln-run/src/protocol.rs`**

`Hello` 1 to 2; `exited()` borrows.

In `crates/kiln-run/src/protocol.rs`, replace:
```rust
                if h.protocol != PROTOCOL_VERSION {
                    return v(format!("unsupported guest protocol {}", h.protocol));
```
with:
```rust
                if !(1..=PROTOCOL_VERSION).contains(&h.protocol) {
                    return v(format!(
                        "unsupported guest protocol {} (this kiln speaks 1 to {PROTOCOL_VERSION})",
                        h.protocol
                    ));
```

In `crates/kiln-run/src/protocol.rs`, replace:
```rust
                self.exited = Some(*e);
```
with:
```rust
                self.exited = Some(e.clone());
```

In `crates/kiln-run/src/protocol.rs`, replace:
```rust
    pub fn exited(&self) -> Option<Exited> {
        self.exited
```
with:
```rust
    pub fn exited(&self) -> Option<&Exited> {
        self.exited.as_ref()
```

In `crates/kiln-run/src/protocol.rs`, replace:
```rust
        GuestMessage::Exited(Exited { signaled: false, code })
```
with:
```rust
        GuestMessage::Exited(Exited::main(false, code))
```

In `crates/kiln-run/src/protocol.rs`, replace:
```rust
        let (_, why) = run(&[GuestMessage::Hello(Hello { protocol: 2 })]).unwrap_err();
        assert!(why.0.contains("unsupported guest protocol 2"), "{why}");
```
with:
```rust
        let (_, why) = run(&[GuestMessage::Hello(Hello { protocol: 3 })]).unwrap_err();
        assert!(why.0.contains("unsupported guest protocol 3"), "{why}");
```

- [ ] **Step 8: Modify `crates/kiln-run/src/config.rs`**

The new fields, empty.

In `crates/kiln-run/src/config.rs`, replace:
```rust
        shutdown_grace_secs: opts.stop_timeout,
    };
```
with:
```rust
        shutdown_grace_secs: opts.stop_timeout,
        services: Vec::new(),
        ready_timeout_ms: None,
    };
```

- [ ] **Step 9: Modify `crates/kiln-run/src/linux/session.rs`**

`exited()` borrows.

In `crates/kiln-run/src/linux/session.rs`, replace:
```rust
            exited: self.proto.exited(),
```
with:
```rust
            exited: self.proto.exited().cloned(),
```

- [ ] **Step 10: Modify `crates/kiln-run/tests/session.rs`**

`Exited::main`; version 3 is the unsupported one.

In `crates/kiln-run/tests/session.rs`, replace:
```rust
        shutdown_grace_secs: 10,
    }
```
with:
```rust
        shutdown_grace_secs: 10,
        services: Vec::new(),
        ready_timeout_ms: None,
    }
```

In `crates/kiln-run/tests/session.rs`, replace:
```rust
    send(c, GuestMessage::Exited(Exited { signaled: false, code }));
```
with:
```rust
    send(c, GuestMessage::Exited(Exited::main(false, code)));
```

In `crates/kiln-run/tests/session.rs`, replace:
```rust
        send(
            &mut c,
            GuestMessage::Exited(Exited {
                signaled: true,
                code: 9,
            }),
        );
```
with:
```rust
        send(&mut c, GuestMessage::Exited(Exited::main(true, 9)));
```

In `crates/kiln-run/tests/session.rs`, replace:
```rust
        send(&mut c, GuestMessage::Hello(Hello { protocol: 2 }));
```
with:
```rust
        send(&mut c, GuestMessage::Hello(Hello { protocol: 3 }));
```

In `crates/kiln-run/tests/session.rs`, replace:
```rust
    assert_eq!(o.violation.as_deref(), Some("unsupported guest protocol 2"));
```
with:
```rust
    assert_eq!(
        o.violation.as_deref(),
        Some("unsupported guest protocol 3 (this kiln speaks 1 to 2)")
    );
```

In `crates/kiln-run/tests/session.rs`, replace:
```rust
            send(
                &mut c,
                GuestMessage::Exited(Exited {
                    signaled: true,
                    code: 15,
                }),
            );
```
with:
```rust
            send(&mut c, GuestMessage::Exited(Exited::main(true, 15)));
```

- [ ] **Step 11: Modify `crates/kiln-run/tests/support/driver.rs`**

`Exited` is no longer `Copy`.

In `crates/kiln-run/tests/support/driver.rs`, replace:
```rust
        self.run.exited
```
with:
```rust
        self.run.exited.clone()
```

- [ ] **Step 12: Modify `crates/kiln-run/tests/support/mod.rs`**

The new fields, empty.

In `crates/kiln-run/tests/support/mod.rs`, replace:
```rust
            shutdown_grace_secs: 10,
        }
```
with:
```rust
            shutdown_grace_secs: 10,
            services: Vec::new(),
            ready_timeout_ms: None,
        }
```

- [ ] **Step 13: Modify `crates/kiln-init/src/linux/supervise.rs`**

`Exited::main`.

In `crates/kiln-init/src/linux/supervise.rs`, replace:
```rust
        (Some(code), _) => Exited { signaled: false, code },
        (None, Some(sig)) => Exited {
            signaled: true,
            code: sig,
        },
```
with:
```rust
        (Some(code), _) => Exited::main(false, code),
        (None, Some(sig)) => Exited::main(true, sig),
```

- [ ] **Step 14: Modify `crates/kiln-testguest/src/main.rs`**

`Exited::main`.

In `crates/kiln-testguest/src/main.rs`, replace:
```rust
            let _ = send(GuestMessage::Exited(Exited {
                signaled: false,
                code: 0,
            }));
```
with:
```rust
            let _ = send(GuestMessage::Exited(Exited::main(false, 0)));
```

- [ ] **Step 15: Run the crate's tests**

Run:
```bash
cargo test -q -p kiln-proto
```
Expected: all pass (34 tests on macOS, 34 on Linux), among them the `config` cases `a_config_without_services_is_a_version_1_config` and `each_invalid_service_is_refused`.

- [ ] **Step 16: Format, lint and run the whole workspace**

Run:
```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test --all -q
```
Expected: no clippy warnings; every test passes (400 tests).

- [ ] **Step 17: Lint and test on Linux**

In the Lima VM (`limactl shell vmkit`, then `cd` to the repository and `export CARGO_TARGET_DIR=$HOME/target-m3d`):

Run (Linux):
```bash
cargo clippy --all-targets -- -D warnings && cargo test --all -q
```
Expected: no clippy warnings; every test passes (499 tests; the boot and `kiln run` cases skip themselves without their environment).

- [ ] **Step 18: Commit**

```bash
git add -A crates
git commit -m 'feat(kiln-proto): protocol version 2 with Config.services, readyTimeoutMs and Exited.service'
```

### Task 2: `kiln-run`: negotiate the guest's protocol version; refuse services to a version 1 guest

**Files:**
- Modify: `crates/kiln-run/src/linux/session.rs`, `crates/kiln-run/src/protocol.rs`, `crates/kiln-run/tests/session.rs`

**Interfaces:**
- Consumes: Task 1's `Config::required_version`, `Exited::service`, `PROTOCOL_VERSION`; M3b-1's `Protocol`, `Session`, `SessionOptions::boot_timeout`.
- Produces:
  - `kiln_run::protocol::Protocol::new(&Config)` (replaces `Default`), `Protocol::version() -> u32` (the guest's `Hello`); `version_refusal(guest: u32, &Config) -> Option<String>` ("the image's services need guest protocol version 2, and the guest's kiln-init speaks version 1: …").
  - The session sends `Config` only when the guest's version allows it, else the VM is killed as a violation (125); with services, `readyTimeoutMs` is set when `Config` is sent.
  - `Exited.service` from a version 1 guest, or naming a service the run lacks, is a violation.

§9.8 rules 2 and 3 on the host.

- The host remembers the version the guest's `Hello` announced (1 or 2). A run whose `Config` needs a later version (services need 2) gets no `Config`: the VM is killed and the run reports 125 with an error naming the version required, which is how an older `kiln-init` named with `--init` meets an image with services.
- A guest may use only what its version defines: `Exited.service` needs version 2 and must name one of the run's services.
- `readyTimeoutMs` (D3) is computed when `Config` is sent: what remains of the boot timeout less a 2 s margin (`READY_MARGIN`), at least 1 ms; `u32::MAX` without a boot deadline. So the guest's own `InitFailed` naming a service that is never ready reaches the host before the boot timeout ends the VM.

- [ ] **Step 1: Modify `crates/kiln-run/tests/session.rs`**

Against the scripted guest: a version 1 guest refused a `Config` with services (125, the message, no `Config` read); a version 2 guest getting services with a `readyTimeoutMs` below the boot timeout and reporting a service's exit; a version 1 guest without services as before.

In `crates/kiln-run/tests/session.rs`, replace:
```rust
    Config, EXIT_INFRA, ExitMethod, Exited, GuestMessage, Hello, HostMessage, InitFailed, Process, Scratch, Shutdown,
    Stage, WindowSize, port, read_message, write_message,
```
with:
```rust
    Config, EXIT_INFRA, ExitMethod, Exited, GuestMessage, Hello, HostMessage, InitFailed, Process, Ready, Scratch,
    Service, Shutdown, Stage, WindowSize, port, read_message, write_message,
```

In `crates/kiln-run/tests/session.rs`, replace:
```rust
    assert_eq!(io.err.bytes(), b"done\n");
}
```
with:
```rust
    assert_eq!(io.err.bytes(), b"done\n");
}

fn with_service() -> Config {
    Config {
        services: vec![Service {
            name: "php-fpm".into(),
            process: Process {
                cmd: vec!["php-fpm".into()],
                ..Process::default()
            },
            ready: Ready::Unix("/run/php-fpm.sock".into()),
            stop_signal: 3,
        }],
        ready_timeout_ms: Some(10_000),
        ..config()
    }
}

/// Spec §9.8 rule 3: a version 1 guest never gets a `Config` with services; the
/// run ends with 125 naming the version it needs.
#[test]
fn a_version_1_guest_is_refused_a_config_with_services() {
    let got = Arc::new(Mutex::new(None));
    let seen = got.clone();
    let vm = fake(move |sock, killed| {
        let mut c = connect(&sock, port::CONTROL);
        send(&mut c, GuestMessage::Hello(Hello { protocol: 1 }));
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        *seen.lock().unwrap() = Some(read_message::<_, HostMessage>(&mut c).map_err(|e| e.to_string()));
        wait_killed(&killed);
    });
    let (st, _) = streams(None);
    let o = Session::start(vm, with_service(), st, opts())
        .unwrap()
        .finish_within(LIMIT);
    assert_eq!(o.exit_code, EXIT_INFRA, "{o:?}");
    let why = o.violation.unwrap();
    assert!(
        why.contains("need guest protocol version 2") && why.contains("speaks version 1"),
        "{why}"
    );
    // The guest read EOF or an error, never a Config.
    let got = got.lock().unwrap().take().unwrap();
    assert!(!matches!(got, Ok(Some(HostMessage::Config(_)))), "{got:?}");
}

/// A version 2 guest gets the services and a readiness deadline within the boot
/// timeout; a service that ends the run is reported with its status.
#[test]
fn a_version_2_guest_gets_services_and_reports_a_service_exit() {
    let got = Arc::new(Mutex::new(None));
    let seen = got.clone();
    let vm = fake(move |sock, _| {
        let mut c = connect(&sock, port::CONTROL);
        send(&mut c, GuestMessage::Hello(Hello { protocol: 2 }));
        let Ok(Some(HostMessage::Config(cfg))) = read_message::<_, HostMessage>(&mut c) else {
            return;
        };
        *seen.lock().unwrap() = Some(*cfg);
        boot(&mut c);
        drop((connect(&sock, port::STDOUT), connect(&sock, port::STDERR)));
        send(
            &mut c,
            GuestMessage::Exited(Exited {
                service: Some("php-fpm".into()),
                ..Exited::main(false, 70)
            }),
        );
    });
    let (st, _) = streams(None);
    let o = Session::start(vm, with_service(), st, opts())
        .unwrap()
        .finish_within(LIMIT);
    assert_eq!((o.violation.as_deref(), o.exit_code), (None, 70), "{o:?}");
    assert_eq!(o.exited.unwrap().service.as_deref(), Some("php-fpm"));
    let cfg = got.lock().unwrap().take().unwrap();
    assert_eq!(cfg.services, with_service().services);
    // The boot timeout is 10 s, less the 2 s margin, less what the boot took.
    let ms = cfg.ready_timeout_ms.unwrap();
    assert!((6_000..=8_000).contains(&ms), "{ms}");
}

/// A version 1 guest without services: the `Config` it gets is a version 1
/// `Config` (no `services`, no `readyTimeoutMs`), byte for byte as before.
#[test]
fn a_version_1_guest_without_services_runs_as_before() {
    let vm = fake(move |sock, _| {
        let mut c = connect(&sock, port::CONTROL);
        handshake(&mut c);
        boot(&mut c);
        drop((connect(&sock, port::STDOUT), connect(&sock, port::STDERR)));
        exited(&mut c, 0);
    });
    let (st, _) = streams(None);
    let o = Session::start(vm, config(), st, opts()).unwrap().finish_within(LIMIT);
    assert_eq!((o.violation.as_deref(), o.exit_code), (None, 0), "{o:?}");
}
```

- [ ] **Step 2: Run the tests to see them fail**

Run:
```bash
cargo test -q -p kiln-run --test session
```
Expected: on Linux, `a_version_1_guest_is_refused_a_config_with_services` and `a_version_2_guest_gets_services_and_reports_a_service_exit` FAIL (the host sends `Config` regardless of the guest's version and leaves `readyTimeoutMs` unset); on macOS the session tests are Linux-only and nothing runs.

- [ ] **Step 3: Modify `crates/kiln-run/src/protocol.rs`**

The version, the refusal and the `Exited.service` checks, with unit tests.

In `crates/kiln-run/src/protocol.rs`, replace:
```rust

use kiln_proto::{EXIT_INFRA, Exited, GuestMessage, InitFailed, PROTOCOL_VERSION};
```
with:
```rust
//!
//! Versions (spec §9.8): the guest's `Hello` names the highest version it
//! speaks; the host accepts 1 to [`PROTOCOL_VERSION`]. A guest may use only what
//! its version defines (`Exited.service` needs 2, and must name one of the run's
//! services), and the host sends only what that version defines: a run whose
//! `Config` needs more ends before `Config` is sent ([`version_refusal`]).

use kiln_proto::{Config, EXIT_INFRA, Exited, GuestMessage, InitFailed, PROTOCOL_VERSION};
```

In `crates/kiln-run/src/protocol.rs`, replace:
```rust
pub struct Protocol {
    hello: bool,
```
with:
```rust
pub struct Protocol {
    /// The run's service names: the only ones `Exited.service` may name.
    services: Vec<String>,
    /// The version the guest's `Hello` announced (0 before it).
    version: u32,
    hello: bool,
```

In `crates/kiln-run/src/protocol.rs`, replace:
```rust
impl Protocol {
    /// Checks `msg` against the sequence and records it.
```
with:
```rust
impl Protocol {
    /// A session for a run with these services (none: [`Protocol::default`]).
    pub fn new(config: &Config) -> Self {
        Self {
            services: config.services.iter().map(|s| s.name.clone()).collect(),
            ..Self::default()
        }
    }

    /// Checks `msg` against the sequence and records it.
```

In `crates/kiln-run/src/protocol.rs`, replace:
```rust
                }
                self.hello = true;
```
with:
```rust
                }
                self.version = h.protocol;
                self.hello = true;
```

In `crates/kiln-run/src/protocol.rs`, replace:
```rust
                    return v(format!("Exited at stage {}", self.stage));
                }
```
with:
```rust
                    return v(format!("Exited at stage {}", self.stage));
                }
                if let Some(name) = &e.service {
                    if self.version < 2 {
                        return v(format!("Exited names a service from a protocol {} guest", self.version));
                    }
                    if !self.services.contains(name) {
                        return v(format!("Exited names service {name:?}, which this run does not have"));
                    }
                }
```

In `crates/kiln-run/src/protocol.rs`, replace:
```rust

    pub fn init_failed(&self) -> Option<&InitFailed> {
```
with:
```rust

    /// The protocol version the guest announced (0 before `Hello`).
    pub fn version(&self) -> u32 {
        self.version
    }

    pub fn init_failed(&self) -> Option<&InitFailed> {
```

In `crates/kiln-run/src/protocol.rs`, replace:
```rust

fn name(msg: &GuestMessage) -> &'static str {
```
with:
```rust

/// Why the host must not send `config` to a guest that speaks `guest`: it needs a
/// later version (spec §9.8 rule 3). The run then ends with 125, before `Config`.
pub fn version_refusal(guest: u32, config: &Config) -> Option<String> {
    let needed = config.required_version();
    (guest < needed).then(|| {
        format!(
            "the image's services need guest protocol version {needed}, and the guest's kiln-init speaks \
             version {guest}: boot it with a kiln-init from this kiln release"
        )
    })
}

fn name(msg: &GuestMessage) -> &'static str {
```

In `crates/kiln-run/src/protocol.rs`, replace:
```rust
    use kiln_proto::{Hello, Stage};

    fn hello() -> GuestMessage {
        GuestMessage::Hello(Hello { protocol: 1 })
```
with:
```rust
    use kiln_proto::{ExitMethod, Hello, Process, Ready, Scratch, Service, Stage};

    fn hello() -> GuestMessage {
        GuestMessage::Hello(Hello { protocol: 1 })
    }

    fn config(services: &[&str]) -> Config {
        Config {
            process: Process {
                cmd: vec!["nginx".into()],
                ..Process::default()
            },
            stop_signal: 15,
            tty: None,
            interactive: false,
            hostname: "h".into(),
            network: None,
            layers: 1,
            scratch: Scratch { size_bytes: 64 << 20 },
            exit_method: ExitMethod::Reboot,
            shutdown_grace_secs: 10,
            ready_timeout_ms: (!services.is_empty()).then_some(1000),
            services: services
                .iter()
                .map(|n| Service {
                    name: n.to_string(),
                    process: Process {
                        cmd: vec!["php-fpm".into()],
                        ..Process::default()
                    },
                    ready: Ready::None,
                    stop_signal: 3,
                })
                .collect(),
        }
    }

    fn service_exited(name: &str, code: i32) -> GuestMessage {
        GuestMessage::Exited(Exited {
            service: Some(name.into()),
            ..Exited::main(false, code)
        })
    }

    #[test]
    fn versions_one_and_two_are_accepted_and_the_config_needs_its_own() {
        for v in [1, 2] {
            let mut p = Protocol::default();
            assert_eq!(
                p.accept(&GuestMessage::Hello(Hello { protocol: v })),
                Ok(Accepted::Hello)
            );
            assert_eq!(p.version(), v);
        }
        let mut p = Protocol::default();
        let why = p.accept(&GuestMessage::Hello(Hello { protocol: 3 })).unwrap_err();
        assert!(
            why.0.contains("unsupported guest protocol 3 (this kiln speaks 1 to 2)"),
            "{why}"
        );
        assert_eq!(version_refusal(1, &config(&[])), None);
        assert_eq!(version_refusal(2, &config(&["php-fpm"])), None);
        let why = version_refusal(1, &config(&["php-fpm"])).unwrap();
        assert!(why.contains("version 2") && why.contains("speaks version 1"), "{why}");
    }

    #[test]
    fn a_service_ending_the_run() {
        let c = config(&["php-fpm"]);
        let boot = |p: &mut Protocol, version: u32| {
            for m in [
                GuestMessage::Hello(Hello { protocol: version }),
                stage(6),
                GuestMessage::Running,
                stage(7),
            ] {
                p.accept(&m).unwrap();
            }
        };
        let mut p = Protocol::new(&c);
        boot(&mut p, 2);
        p.accept(&service_exited("php-fpm", 70)).unwrap();
        assert_eq!(p.exit_code(), 70);
        assert_eq!(p.exited().unwrap().service.as_deref(), Some("php-fpm"));
        // A name the run does not have, or any name from a version 1 guest, is a lie.
        let mut p = Protocol::new(&c);
        boot(&mut p, 2);
        let why = p.accept(&service_exited("nginx", 0)).unwrap_err();
        assert!(why.0.contains("does not have"), "{why}");
        let mut p = Protocol::new(&c);
        boot(&mut p, 1);
        let why = p.accept(&service_exited("php-fpm", 0)).unwrap_err();
        assert!(why.0.contains("protocol 1 guest"), "{why}");
        let mut p = Protocol::default();
        boot(&mut p, 2);
        assert!(p.accept(&service_exited("php-fpm", 0)).is_err());
```

- [ ] **Step 4: Modify `crates/kiln-run/src/linux/session.rs`**

The refusal before `Config`; `readyTimeoutMs` from the boot deadline.

Replace the contents of `crates/kiln-run/src/linux/session.rs` with:
```rust
//! The host side of one guest session (spec §9.5, §9.7, T9).
//!
//! Threads, and what bounds them:
//! - **listener**: one `poll(2)` over every served vsock port and a wake pipe (no
//!   timed polling). The first connection on a port is served; a second one on
//!   any port is a violation (D-5).
//! - **control reader**: reads guest frames through the sequence state machine
//!   ([`Protocol`], D-1) and stops at the first violation, so at most nine guest
//!   messages are ever queued for the main loop. It answers the first `Hello`
//!   with `Config`, once. Like the output streams, it is drained to EOF after
//!   the VM ends: the guest's last messages may still be in flight then.
//! - **control writer**: the only thread that writes host messages, from a
//!   bounded queue, with a send timeout. A full queue or a stalled send is a
//!   violation, and no other thread ever waits on the control socket (D-2).
//! - **relays**: stdout, stderr and the terminal are streamed to their sinks as
//!   they arrive, never buffered whole (D-6); stdin is copied to the guest with
//!   EOF as a half-close.
//! - **main loop** ([`Session::pump`]): handles events, polls the VMM's end, and
//!   enforces every deadline: the boot timeout, the bound after a requested stop
//!   (until the guest's final message), the bound for the VM to end after the
//!   guest's final message or after the control connection closes (D-3), and the
//!   bound for a killed VMM to exit, after which kiln stops waiting for it.
//!   Killing the VMM never depends on control I/O, and a VMM that is already gone
//!   is not an error (D-6).
//!
//! After the VM ends, the listener accepts what is still in its backlog once, and
//! the control connection has a fixed bound to reach EOF. When the VMM really
//! exited, what remains of the output is finite: the output streams are drained
//! to EOF however long kiln's own sinks take to take it (a pager whose user is
//! reading, a stopped terminal), as a pipeline would; only reading is bounded, so a
//! stream that brings nothing for the same bound while none of them is writing is
//! abandoned. When kiln gave up on a VMM it could not see end (the kill did not
//! take, or waiting failed), the VMM may still be writing, so the output streams
//! get the fixed bound too; so do they when the user asks to stop (SIGINT,
//! SIGTERM, `Ctrl-]`) after the VM's end, since a sink that never takes the
//! output would otherwise hold kiln. A stream abandoned with bytes pending is
//! warned about.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{ErrorKind, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError, channel, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use kiln_proto::{
    Config, EXIT_INFRA, Exited, GuestMessage, HostMessage, InitFailed, Shutdown, Signal, WindowSize, port,
    read_message, write_message,
};
use rustix::event::{PollFd, PollFlags, poll};
use vmkit::{EndReason, Vm, VmEnd};

use crate::error::{Error, Result};
use crate::escape::{Escape, Key};
use crate::protocol::{Accepted, Protocol, version_refusal};

/// How many host messages may wait for the guest to read them.
const QUEUE: usize = 64;
/// How long one control write may block before the guest counts as not reading.
const SEND_TIMEOUT: Duration = Duration::from_secs(5);
/// How often the main loop looks for the VMM's end (vmkit has no pollable handle).
const VM_POLL: Duration = Duration::from_millis(20);
/// What the guest's readiness deadline for services leaves of the boot timeout,
/// for its `InitFailed` to reach the host before the boot timeout ends the VM.
const READY_MARGIN: Duration = Duration::from_secs(2);
/// The exit code of a VM the user killed (second SIGINT, `Ctrl-]` `k`), as `docker kill`.
pub const EXIT_KILLED: i32 = 137;

/// Where the guest's stdio goes and comes from.
pub struct Streams {
    /// With `interactive`: relayed to the guest's stdin (or terminal).
    pub stdin: Option<Box<dyn Read + Send>>,
    /// The guest's stdout, or its terminal with `tty`.
    pub stdout: Box<dyn Write + Send>,
    pub stderr: Box<dyn Write + Send>,
}

/// Timeouts and switches.
#[derive(Debug, Clone)]
pub struct SessionOptions {
    /// From starting the VM to `Running` (spec §9.7).
    pub boot_timeout: Duration,
    /// The grace period sent with `Shutdown`.
    pub stop_timeout: u32,
    /// How long past the grace period the host waits before killing the VM itself.
    pub stop_margin: Duration,
    /// After `Exited`, `InitFailed` or the control connection closing: how long the
    /// guest has to end the VM (it syncs and remounts the scratch disk first, D-3).
    pub end_timeout: Duration,
    /// After the VM ended: how long the control connection has to reach EOF, and
    /// how long the output streams may go without reading a byte (while none is
    /// writing to its sink) before they are abandoned; after kiln gave up on the
    /// VMM, how long the output streams have in all.
    pub drain_timeout: Duration,
    /// After the VMM was killed: how long to wait for it to exit before giving up
    /// on it (the run then ends, with a warning).
    pub kill_wait: Duration,
    /// Interpret `Ctrl-]` sequences on the terminal's input (`-t`).
    pub escape_keys: bool,
    /// Raw bytes to answer `Hello` with instead of `Config` (tests of the guest's T9).
    #[doc(hidden)]
    pub reply: Option<Vec<u8>>,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            boot_timeout: crate::options::DEFAULT_BOOT_TIMEOUT,
            stop_timeout: crate::options::DEFAULT_STOP_TIMEOUT,
            stop_margin: Duration::from_secs(5),
            end_timeout: Duration::from_secs(10),
            drain_timeout: Duration::from_secs(2),
            kill_wait: Duration::from_secs(5),
            escape_keys: false,
            reply: None,
        }
    }
}

/// What the writer sends.
enum Out {
    Msg(HostMessage),
    Raw(Vec<u8>),
    Stop,
}

enum Event {
    Guest(GuestMessage),
    Violation(String),
    ControlClosed,
    /// A drained stream reached EOF (it has left [`Drain::open`]): wakes the main loop.
    Eof,
    Interrupt,
    Terminate,
    Shutdown,
    Kill,
}

/// Controls a running session from other threads (signals, the terminal).
#[derive(Clone)]
pub struct Handle {
    events: Sender<Event>,
    out: SyncSender<Out>,
    /// A full queue was reported: one violation, however many sends find it full.
    full: Arc<AtomicBool>,
}

impl Handle {
    /// SIGINT: the first asks the guest to stop, the second kills the VM.
    pub fn interrupt(&self) {
        let _ = self.events.send(Event::Interrupt);
    }

    /// SIGTERM: asks the guest to stop.
    pub fn terminate(&self) {
        let _ = self.events.send(Event::Terminate);
    }

    /// `Ctrl-]` `q`: asks the guest to stop.
    pub fn shutdown(&self) {
        let _ = self.events.send(Event::Shutdown);
    }

    /// `Ctrl-]` `k`: kills the VM.
    pub fn kill(&self) {
        let _ = self.events.send(Event::Kill);
    }

    /// Forwards a signal (1–31) to the guest's main process.
    pub fn signal(&self, sig: i32) {
        self.queue(Out::Msg(HostMessage::Signal(Signal { sig })), true);
    }

    /// The terminal was resized. Dropped when the queue is full: a later resize follows.
    pub fn window_size(&self, size: WindowSize) {
        self.queue(Out::Msg(HostMessage::WindowSize(size)), false);
    }

    /// Any host message (tests).
    #[doc(hidden)]
    pub fn send(&self, msg: HostMessage) {
        self.queue(Out::Msg(msg), true);
    }

    /// Raw bytes on the control connection, after `Config` (tests of the guest's T9).
    #[doc(hidden)]
    pub fn send_raw(&self, bytes: Vec<u8>) {
        self.queue(Out::Raw(bytes), true);
    }

    fn queue(&self, out: Out, must: bool) {
        match self.out.try_send(out) {
            Ok(()) | Err(TrySendError::Disconnected(_)) => {}
            Err(TrySendError::Full(_)) if must => {
                if !self.full.swap(true, Ordering::SeqCst) {
                    let _ = self
                        .events
                        .send(Event::Violation("the guest is not reading its control channel".into()));
                }
            }
            Err(TrySendError::Full(_)) => {}
        }
    }
}

/// How a session ended.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// What `kiln run` exits with (spec §9.5).
    pub exit_code: i32,
    pub exited: Option<Exited>,
    pub init_failed: Option<InitFailed>,
    /// The stages the guest reported, in order.
    pub stages: Vec<u8>,
    pub hello: bool,
    pub running: bool,
    /// Why the host killed the VM, when the guest broke the protocol or a timeout.
    pub violation: Option<String>,
    /// The user killed the VM.
    pub killed: bool,
    pub end: VmEnd,
    /// Things the user should know (a forced kill after the final message, ...).
    pub warnings: Vec<String>,
    /// From starting the VM to `Hello` and to `Running`.
    pub hello_after: Option<Duration>,
    pub running_after: Option<Duration>,
    /// From starting the VM to the VMM's exit, and to the end of its output.
    pub ended_after: Option<Duration>,
    pub drained_after: Duration,
}

/// One running guest and the host side of its protocol.
pub struct Session {
    vm: Box<dyn Vm>,
    events: Receiver<Event>,
    handle: Handle,
    wake: OwnedFd,
    listener: Option<JoinHandle<()>>,
    opts: SessionOptions,
    proto: Protocol,
    stages: Vec<u8>,
    violation: Option<String>,
    killed: bool,
    warnings: Vec<String>,
    end: Option<VmEnd>,
    ended_at: Option<Instant>,
    /// kiln gave up on the VMM ([`lost`]): it may still be running.
    gave_up: bool,
    /// The user asked to stop after the VM's end (a sink that never takes the
    /// output would hold kiln otherwise): the output gets the fixed bound.
    cut: bool,
    started: Instant,
    hello_at: Option<Instant>,
    running_at: Option<Instant>,
    boot_deadline: Option<Instant>,
    stop_deadline: Option<Instant>,
    end_deadline: Option<Instant>,
    /// What started the end deadline, for its warning.
    end_after: &'static str,
    /// When the VMM was first killed.
    killed_at: Option<Instant>,
    interrupts: u32,
    drain: Arc<Drain>,
    /// The last count of moved bytes seen, and when it last changed.
    moved: (u64, Instant),
}

/// The connections drained after the VM ends, and the bytes the output streams
/// have moved; shared with the threads that serve them.
#[derive(Default)]
struct Drain {
    /// Ports connected and not yet at EOF: the listener adds a port before serving
    /// it, so the main loop never sees a connection it does not wait for.
    open: Mutex<BTreeMap<u32, Open>>,
    /// Bytes the output streams have read and written.
    moved: AtomicU64,
    /// The listener has accepted what was in its backlog after the VM's end (or
    /// has stopped).
    swept: AtomicBool,
}

/// A connection being drained.
#[derive(Default)]
struct Open {
    /// An output stream in a write to its sink.
    writing: bool,
    /// An output stream's socket, to tell whether bytes are pending when it is abandoned.
    socket: Option<UnixStream>,
}

impl Drain {
    fn ports(&self) -> std::sync::MutexGuard<'_, BTreeMap<u32, Open>> {
        self.open.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn moved(&self, n: usize) {
        self.moved.fetch_add(n as u64, Ordering::Relaxed);
    }

    fn writing(&self, p: u32, writing: bool) {
        if let Some(o) = self.ports().get_mut(&p) {
            o.writing = writing;
        }
    }

    /// The output streams still open with bytes they have not delivered: in a
    /// write to their sink, or unread on their socket.
    fn pending(&self) -> Vec<u32> {
        self.ports()
            .iter()
            .filter(|&(&p, o)| {
                p != port::CONTROL
                    && (o.writing
                        || o.socket
                            .as_ref()
                            .is_some_and(|s| rustix::io::ioctl_fionread(s).is_ok_and(|n| n > 0)))
            })
            .map(|(&p, _)| p)
            .collect()
    }
}

/// The Unix socket path the VMM forwards guest port `p` to.
fn port_path(base: &Path, p: u32) -> PathBuf {
    PathBuf::from(format!("{}_{p}", base.display()))
}

fn bind(base: &Path, p: u32) -> Result<UnixListener> {
    use std::os::unix::fs::FileTypeExt;
    let path = port_path(base, p);
    match std::fs::symlink_metadata(&path) {
        Ok(m) if m.file_type().is_socket() => std::fs::remove_file(&path)?,
        Ok(_) => {
            return Err(Error::refused(format!("{} exists and is not a socket", path.display())));
        }
        Err(_) => {}
    }
    let l = UnixListener::bind(&path)?;
    l.set_nonblocking(true)?;
    Ok(l)
}

impl Session {
    /// Listens on the ports `config` needs, then starts the guest.
    pub fn start(vm: Box<dyn Vm>, config: Config, streams: Streams, opts: SessionOptions) -> Result<Self> {
        let base = vm
            .vsock_socket()
            .ok_or_else(|| Error::refused("the VM has no vsock device"))?
            .to_path_buf();
        let mut ports = vec![port::CONTROL];
        if config.tty.is_some() {
            ports.push(port::TTY);
        } else {
            if config.interactive {
                ports.push(port::STDIN);
            }
            ports.extend([port::STDOUT, port::STDERR]);
        }
        let listeners = ports
            .iter()
            .map(|&p| Ok((p, bind(&base, p)?)))
            .collect::<Result<Vec<_>>>()?;
        let (tx, events) = channel();
        let (out_tx, out_rx) = sync_channel(QUEUE);
        let (stream_tx, stream_rx) = channel();
        let (wake_r, wake) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).map_err(std::io::Error::from)?;
        let drain = Arc::new(Drain::default());
        let proto = Protocol::new(&config);
        let boot_deadline = Arc::new(OnceLock::new());
        spawn("control-writer", {
            let tx = tx.clone();
            move || writer(&stream_rx, &out_rx, &tx)
        })?;
        let serve = Serve {
            config,
            boot_deadline: boot_deadline.clone(),
            reply: opts.reply.clone(),
            events: tx.clone(),
            writer: stream_tx,
            stdin: streams.stdin,
            stdout: Some(streams.stdout),
            stderr: Some(streams.stderr),
            escape_keys: opts.escape_keys,
            drain: drain.clone(),
        };
        let listener = spawn("vsock-listener", move || listen(listeners, &wake_r, serve))?;
        let handle = Handle {
            events: tx,
            out: out_tx,
            full: Arc::new(AtomicBool::new(false)),
        };
        let now = Instant::now();
        let mut s = Session {
            vm,
            events,
            handle,
            wake,
            listener: Some(listener),
            opts,
            proto,
            stages: Vec::new(),
            violation: None,
            killed: false,
            warnings: Vec::new(),
            end: None,
            ended_at: None,
            gave_up: false,
            cut: false,
            started: now,
            hello_at: None,
            running_at: None,
            boot_deadline: None,
            stop_deadline: None,
            end_deadline: None,
            end_after: "",
            killed_at: None,
            interrupts: 0,
            drain,
            moved: (0, now),
        };
        s.vm.start()?;
        s.started = Instant::now();
        // Too far to represent (a huge --boot-timeout): no deadline.
        s.boot_deadline = s.started.checked_add(s.opts.boot_timeout);
        let _ = boot_deadline.set(s.boot_deadline);
        Ok(s)
    }

    pub fn handle(&self) -> Handle {
        self.handle.clone()
    }

    /// The guest has started its main process.
    pub fn running(&self) -> bool {
        self.proto.running()
    }

    /// The VM has ended.
    pub fn ended(&self) -> bool {
        self.end.is_some()
    }

    /// Runs the session until `done()` holds, the VM ends, or `timeout` passes;
    /// returns `done()`.
    pub fn pump(&mut self, timeout: Duration, mut done: impl FnMut(&Self) -> bool) -> bool {
        let until = Instant::now().checked_add(timeout);
        loop {
            if done(self) {
                return true;
            }
            if self.end.is_some() || until.is_some_and(|u| Instant::now() >= u) {
                return done(self);
            }
            self.step(until);
        }
    }

    /// Waits for `Running`; false if the VM ended (or was killed for a boot timeout) first.
    pub fn wait_running(&mut self) -> bool {
        self.pump(Duration::from_secs(365 * 24 * 3600), |s| s.proto.running());
        self.proto.running()
    }

    /// Runs to the end of the VM and its output.
    pub fn finish(self) -> Outcome {
        self.finish_within(None)
    }

    /// Like [`Session::finish`], killing the VM after `limit` (a test's overall bound).
    pub fn finish_within(mut self, limit: Option<Duration>) -> Outcome {
        let limit = limit.and_then(|l| Instant::now().checked_add(l));
        loop {
            self.note_moved();
            if self.done() {
                break;
            }
            if limit.is_some_and(|l| Instant::now() >= l) && self.end.is_none() {
                self.violate("timed out (the test's limit)".into());
                if let Ok(end) = self.vm.wait_timeout(Duration::from_secs(10)) {
                    self.set_end(end);
                }
                if self.end.is_none() {
                    self.give_up();
                }
            }
            self.step(limit);
        }
        // Late events (the control thread may have read a final message just now).
        while let Ok(e) = self.events.try_recv() {
            self.handle_event(e);
        }
        for p in self.drain.pending() {
            self.warnings.push(format!(
                "the guest's {} was cut off: {}",
                stream_name(p),
                if self.gave_up {
                    format!(
                        "kiln stopped waiting for it {:.1}s after it gave up on the VMM",
                        self.opts.drain_timeout.as_secs_f64()
                    )
                } else if self.cut {
                    "kiln was asked to stop while its own output was not taking it".to_string()
                } else {
                    format!(
                        "it brought nothing for {:.1}s after the VM ended",
                        self.opts.drain_timeout.as_secs_f64()
                    )
                }
            ));
        }
        self.outcome()
    }

    /// The VM has ended, the listener has accepted what was left in its backlog,
    /// and every drained connection is at EOF or past its bound: the control
    /// connection a fixed one from the VM's end; the output streams, after a real
    /// end, one from the last byte they moved that runs only while none of them is
    /// writing to its sink, and after kiln gave up on the VMM or the user asked to
    /// stop, the fixed one.
    fn done(&self) -> bool {
        let Some(at) = self.ended_at else {
            return false;
        };
        let bound = self.opts.drain_timeout;
        // Bounded too, should the listener be stuck.
        if !self.drain.swept.load(Ordering::SeqCst) && at.elapsed() < bound {
            return false;
        }
        let (control, streams, writing) = {
            let open = self.drain.ports();
            (
                open.contains_key(&port::CONTROL),
                open.keys().any(|&p| p != port::CONTROL),
                open.values().any(|o| o.writing),
            )
        };
        // Read after the writing flags: a write that just ended has counted its bytes.
        let unmoved = self.drain.moved.load(Ordering::Relaxed) == self.moved.0 && self.moved.1.elapsed() >= bound;
        let streams_done = if self.gave_up || self.cut {
            at.elapsed() >= bound
        } else {
            !writing && unmoved
        };
        (!control || at.elapsed() >= bound) && (!streams || streams_done)
    }

    /// Restarts the output streams' drain bound when they have moved bytes.
    fn note_moved(&mut self) {
        let moved = self.drain.moved.load(Ordering::Relaxed);
        if moved != self.moved.0 {
            self.moved = (moved, Instant::now());
        }
    }

    /// One turn of the main loop: deadlines, the VM's state, at most one event.
    fn step(&mut self, until: Option<Instant>) {
        let now = Instant::now();
        if self.end.is_none() {
            if self.boot_deadline.is_some_and(|d| now >= d) {
                self.boot_deadline = None;
                self.violate(format!(
                    "boot timeout: the guest was not running {}s after the VM started",
                    self.opts.boot_timeout.as_secs()
                ));
            }
            if self.stop_deadline.is_some_and(|d| now >= d) {
                self.stop_deadline = None;
                self.warnings.push(format!(
                    "the guest did not stop within {}s of the request; the VM was killed",
                    self.opts.stop_timeout
                ));
                self.kill_vm();
            }
            if self.end_deadline.is_some_and(|d| now >= d) {
                self.end_deadline = None;
                self.warnings.push(format!(
                    "{}the VM did not end within {:.1}s{}; it was killed",
                    if self.end_after == CONTROL_CLOSED {
                        "the control connection closed, and "
                    } else {
                        ""
                    },
                    self.opts.end_timeout.as_secs_f64(),
                    if self.end_after == CONTROL_CLOSED {
                        String::new()
                    } else {
                        format!(" of {}", self.end_after)
                    }
                ));
                self.kill_vm();
            }
            match self.vm.wait_timeout(Duration::ZERO) {
                Ok(end) => self.set_end(end),
                Err(e) => {
                    self.warnings.push(format!("waiting for the VMM: {e}"));
                    self.give_up();
                }
            }
            if self.end.is_none() && self.killed_at.is_some_and(|k| now >= k + self.opts.kill_wait) {
                self.warnings.push(format!(
                    "the VMM did not exit within {:.1}s of being killed; kiln stopped waiting for it",
                    self.opts.kill_wait.as_secs_f64()
                ));
                self.give_up();
            }
        }
        self.note_moved();
        if self.done() {
            return;
        }
        // After the VM's end too: the drain bounds move with the output.
        let mut wait = VM_POLL;
        if let Some(u) = until {
            wait = wait.min(u.saturating_duration_since(now));
        }
        match self.events.recv_timeout(wait) {
            Ok(e) => self.handle_event(e),
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {}
        }
    }

    fn set_end(&mut self, end: Option<VmEnd>) {
        if self.end.is_none() && end.is_some() {
            let now = Instant::now();
            self.end = end;
            self.ended_at = Some(now);
            self.moved = (self.drain.moved.load(Ordering::Relaxed), now);
            // Nothing new can connect now: the listener accepts what is in its backlog.
            let _ = rustix::io::write(&self.wake, &[SWEEP]);
        }
    }

    /// kiln stops waiting for a VMM it cannot see end (it may still run).
    fn give_up(&mut self) {
        if self.end.is_none() {
            self.gave_up = true;
            self.set_end(Some(lost()));
        }
    }

    fn handle_event(&mut self, e: Event) {
        let now = Instant::now();
        match e {
            Event::Guest(msg) => {
                match self.proto.accept(&msg) {
                    Ok(Accepted::Hello | Accepted::Recorded) => {}
                    Ok(Accepted::Ignored) => return,
                    Err(v) => {
                        // The reader checked it already; this cannot differ.
                        self.violate(v.0);
                        return;
                    }
                }
                match msg {
                    GuestMessage::Hello(_) => self.hello_at = Some(now),
                    GuestMessage::Stage(s) => self.stages.push(s.n),
                    GuestMessage::Running => {
                        self.running_at = Some(now);
                        self.boot_deadline = None;
                    }
                    GuestMessage::Exited(_) | GuestMessage::InitFailed(_) => {
                        // The guest has stopped: what remains is the end bound, not
                        // the stop request's.
                        self.stop_deadline = None;
                        self.expect_end(now, FINAL_MESSAGE);
                    }
                }
            }
            Event::Violation(why) => self.violate(why),
            Event::ControlClosed => self.expect_end(now, CONTROL_CLOSED),
            // The drain state is shared; this only wakes the loop.
            Event::Eof => {}
            // After the VM's end only the output is left, and a sink that never
            // takes it (a pager left open) could hold kiln forever: an interrupt,
            // a stop or a kill now ends the wait, within the fixed drain bound.
            Event::Interrupt | Event::Terminate | Event::Shutdown | Event::Kill if self.end.is_some() => {
                self.cut = true;
            }
            Event::Interrupt => {
                self.interrupts += 1;
                if self.interrupts >= 2 {
                    self.user_kill();
                } else {
                    self.request_stop();
                }
            }
            Event::Terminate | Event::Shutdown => self.request_stop(),
            Event::Kill => self.user_kill(),
        }
    }

    /// The guest is done talking: it must end the VM soon (D-3).
    fn expect_end(&mut self, now: Instant, after: &'static str) {
        self.boot_deadline = None;
        if self.end_deadline.is_none() {
            self.end_deadline = now.checked_add(self.opts.end_timeout);
            self.end_after = after;
        }
    }

    fn request_stop(&mut self) {
        // After the guest's final message it is already ending the VM.
        if self.end.is_some() || self.proto.terminal() {
            return;
        }
        if !self.proto.hello() {
            // Nothing runs yet that could stop gracefully.
            self.user_kill();
            return;
        }
        let grace = self.opts.stop_timeout;
        self.handle
            .queue(Out::Msg(HostMessage::Shutdown(Shutdown { grace_secs: grace })), true);
        let at = Instant::now() + Duration::from_secs(u64::from(grace)) + self.opts.stop_margin;
        self.stop_deadline = Some(self.stop_deadline.map_or(at, |d| d.min(at)));
    }

    fn user_kill(&mut self) {
        if self.end.is_none() {
            self.killed = true;
            self.kill_vm();
        }
    }

    /// Records the first violation and kills the VM (T9).
    fn violate(&mut self, why: String) {
        if self.violation.is_none() && !self.killed {
            self.violation = Some(why);
        }
        self.kill_vm();
    }

    fn kill_vm(&mut self) {
        if self.end.is_some() {
            return;
        }
        if let Err(e) = self.vm.kill() {
            self.warnings.push(format!("killing the VMM: {e}"));
        }
        self.killed_at.get_or_insert_with(Instant::now);
    }

    fn outcome(&mut self) -> Outcome {
        // A user's kill after the guest's final message keeps what it reported
        // (the app's code, or init's 127, 126 or 125).
        let exit_code = if self.violation.is_some() {
            EXIT_INFRA
        } else if self.killed && !self.proto.terminal() {
            EXIT_KILLED
        } else {
            self.proto.exit_code()
        };
        Outcome {
            exit_code,
            exited: self.proto.exited().cloned(),
            init_failed: self.proto.init_failed().cloned(),
            stages: self.stages.clone(),
            hello: self.proto.hello(),
            running: self.proto.running(),
            violation: self.violation.clone(),
            killed: self.killed,
            end: self.end.unwrap_or_else(lost),
            warnings: std::mem::take(&mut self.warnings),
            hello_after: self.hello_at.map(|t| t - self.started),
            running_after: self.running_at.map(|t| t - self.started),
            ended_after: self.ended_at.map(|t| t - self.started),
            drained_after: self.started.elapsed(),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Not run to its end (a failure after the start, in kiln or a test): the VM
        // must not outlive the session.
        if self.end.is_none() {
            let _ = self.vm.kill();
            let _ = self.vm.wait_timeout(self.opts.kill_wait);
        }
        let _ = rustix::io::write(&self.wake, &[STOP]);
        let _ = self.handle.out.try_send(Out::Stop);
        if let Some(l) = self.listener.take() {
            let _ = l.join();
        }
    }
}

/// What the wake pipe tells the listener: accept the backlog once, or stop.
const SWEEP: u8 = b's';
const STOP: u8 = b'x';

fn stream_name(p: u32) -> &'static str {
    match p {
        port::STDOUT => "stdout",
        port::STDERR => "stderr",
        port::TTY => "terminal output",
        _ => "output",
    }
}

/// What started the end deadline.
const FINAL_MESSAGE: &str = "the guest's final message";
const CONTROL_CLOSED: &str = "the control connection closing";

/// The end reported when vmkit could not tell.
fn lost() -> VmEnd {
    VmEnd {
        reason: EndReason::Killed,
        code: None,
        signal: None,
    }
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> Result<JoinHandle<()>> {
    Ok(std::thread::Builder::new().name(name.into()).spawn(f)?)
}

/// What the listener hands each first connection.
struct Serve {
    config: Config,
    /// The boot timeout's end, once the VM has started: services' readiness
    /// deadline is derived from it when `Config` is sent.
    boot_deadline: Arc<OnceLock<Option<Instant>>>,
    reply: Option<Vec<u8>>,
    events: Sender<Event>,
    writer: Sender<UnixStream>,
    stdin: Option<Box<dyn Read + Send>>,
    stdout: Option<Box<dyn Write + Send>>,
    stderr: Option<Box<dyn Write + Send>>,
    escape_keys: bool,
    drain: Arc<Drain>,
}

/// Accepts on every port until told to stop; a second connection on a port is a
/// violation. Told to sweep (the VM has ended), it accepts whatever is still in the
/// listeners' backlogs, recording each connection for the drain before the main
/// loop can count the drain done. However it stops, the drain no longer waits for it.
fn listen(listeners: Vec<(u32, UnixListener)>, wake: &OwnedFd, mut serve: Serve) {
    struct Stopped(Arc<Drain>);
    impl Drop for Stopped {
        fn drop(&mut self) {
            self.0.swept.store(true, Ordering::SeqCst);
        }
    }
    let _stopped = Stopped(serve.drain.clone());
    let mut seen = BTreeSet::new();
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
                let _ = serve.events.send(Event::Violation(format!("vsock listener: {e}")));
                return;
            }
        }
        let ready: Vec<bool> = fds.iter().map(|f| !f.revents().is_empty()).collect();
        drop(fds);
        // The wake pipe is last.
        if ready.last().copied().unwrap_or(true) {
            let mut b = [0u8; 1];
            match rustix::io::read(wake, &mut b) {
                Ok(1) if b == [SWEEP] => {
                    for (p, l) in &listeners {
                        while accept(*p, l, &mut seen, &mut serve) == Accept::Served {}
                    }
                    serve.drain.swept.store(true, Ordering::SeqCst);
                    continue;
                }
                Err(rustix::io::Errno::INTR | rustix::io::Errno::AGAIN) => continue,
                // STOP, or the session is gone.
                _ => return,
            }
        }
        for (i, (p, l)) in listeners.iter().enumerate() {
            if ready.get(i).copied().unwrap_or(false) && accept(*p, l, &mut seen, &mut serve) == Accept::Failed {
                return;
            }
        }
    }
}

#[derive(PartialEq, Eq)]
enum Accept {
    Served,
    /// Nothing to accept.
    Empty,
    /// The listener cannot go on (reported as a violation).
    Failed,
}

/// Accepts one connection on port `p`, if any, and serves it.
fn accept(p: u32, l: &UnixListener, seen: &mut BTreeSet<u32>, serve: &mut Serve) -> Accept {
    let stream = match l.accept() {
        Ok((s, _)) => s,
        Err(e) if e.kind() == ErrorKind::WouldBlock => return Accept::Empty,
        Err(e) if e.kind() == ErrorKind::Interrupted => return Accept::Served,
        Err(e) => {
            let _ = serve
                .events
                .send(Event::Violation(format!("accept on vsock port {p}: {e}")));
            return Accept::Failed;
        }
    };
    if !seen.insert(p) {
        let _ = serve
            .events
            .send(Event::Violation(format!("a second connection on vsock port {p}")));
        return Accept::Served;
    }
    if stream.set_nonblocking(false).is_err() {
        return Accept::Served;
    }
    // The control connection and the output streams are drained to EOF after the
    // VM ends; stdin need not be. Recorded before they are served.
    if p == port::CONTROL {
        serve.drain.ports().insert(p, Open::default());
    } else if [port::STDOUT, port::STDERR, port::TTY].contains(&p) {
        let socket = stream.try_clone().ok();
        serve.drain.ports().insert(p, Open { writing: false, socket });
    }
    if let Err(e) = serve.dispatch(p, stream) {
        let _ = serve.events.send(Event::Violation(format!("vsock port {p}: {e}")));
    }
    Accept::Served
}

impl Serve {
    fn dispatch(&mut self, p: u32, stream: UnixStream) -> Result<()> {
        let events = self.events.clone();
        let drain = self.drain.clone();
        match p {
            port::CONTROL => {
                let (config, reply, writer) = (self.config.clone(), self.reply.take(), self.writer.clone());
                let deadline = self.boot_deadline.get().copied().flatten();
                spawn("control-reader", move || {
                    control(stream, &config, deadline, reply, &events, &writer, &drain)
                })?;
            }
            port::STDIN => {
                if let Some(input) = self.stdin.take() {
                    spawn("stdin", move || copy_in(input, stream))?;
                }
            }
            port::STDOUT | port::STDERR | port::TTY => {
                let sink = if p == port::STDERR {
                    self.stderr.take()
                } else {
                    self.stdout.take()
                };
                if p == port::TTY
                    && let Some(input) = self.stdin.take()
                {
                    let (to, escape) = (stream.try_clone()?, self.escape_keys);
                    // So a guest not reading its terminal cannot hold up the escape keys.
                    rustix::net::sockopt::set_socket_timeout(
                        &to,
                        rustix::net::sockopt::Timeout::Send,
                        Some(SEND_TIMEOUT),
                    )
                    .map_err(std::io::Error::from)?;
                    let events = events.clone();
                    spawn("tty-in", move || tty_in(input, to, escape, &events))?;
                }
                if let Some(sink) = sink {
                    spawn("output", move || copy_out(stream, sink, p, &drain, &events))?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// Reads guest messages through the state machine; answers the first `Hello`.
/// Reports the end of the control connection as `Eof`, however it ended.
fn control(
    stream: UnixStream,
    config: &Config,
    boot_deadline: Option<Instant>,
    reply: Option<Vec<u8>>,
    events: &Sender<Event>,
    writer: &Sender<UnixStream>,
    drain: &Drain,
) {
    read_control(stream, config, boot_deadline, reply, events, writer);
    drain.ports().remove(&port::CONTROL);
    let _ = events.send(Event::Eof);
}

fn read_control(
    stream: UnixStream,
    config: &Config,
    boot_deadline: Option<Instant>,
    reply: Option<Vec<u8>>,
    events: &Sender<Event>,
    writer: &Sender<UnixStream>,
) {
    let violation = |why: String| {
        let _ = events.send(Event::Violation(why));
    };
    if let Err(e) =
        rustix::net::sockopt::set_socket_timeout(&stream, rustix::net::sockopt::Timeout::Send, Some(SEND_TIMEOUT))
    {
        // Without it a guest that stops reading could block the writer: refuse the session.
        return violation(format!("control socket: setting its send timeout: {e}"));
    }
    let (mut reader, mut w) = match stream.try_clone() {
        Ok(r) => (r, stream),
        Err(e) => return violation(format!("control socket: {e}")),
    };
    let mut proto = Protocol::new(config);
    loop {
        let msg = match read_message::<_, GuestMessage>(&mut reader) {
            Ok(Some(m)) => m,
            Ok(None) => {
                let _ = events.send(Event::ControlClosed);
                return;
            }
            Err(e) => return violation(format!("guest protocol error: {e}")),
        };
        match proto.accept(&msg) {
            // After InitFailed: not the main loop's concern.
            Ok(Accepted::Ignored) => continue,
            Ok(Accepted::Hello) => {
                // The main loop learns of Hello before the guest gets Config, so
                // nothing the guest does next (a terminal's Ctrl-] q, say) can
                // reach it first and count as before Hello.
                if events.send(Event::Guest(msg)).is_err() {
                    return;
                }
                // Only what the guest's version defines is sent (spec §9.8).
                if let Some(why) = version_refusal(proto.version(), config) {
                    return violation(why);
                }
                let sent = match &reply {
                    None => {
                        let mut config = config.clone();
                        if !config.services.is_empty() {
                            config.ready_timeout_ms = Some(ready_timeout(boot_deadline, Instant::now()));
                        }
                        write_message(&mut w, &HostMessage::Config(Box::new(config))).map_err(|e| e.to_string())
                    }
                    Some(raw) => w.write_all(raw).map_err(|e| e.to_string()),
                };
                if let Err(e) = sent {
                    return violation(format!("sending Config: {e}"));
                }
                match w.try_clone() {
                    Ok(c) => {
                        let _ = writer.send(c);
                    }
                    Err(e) => return violation(format!("control socket: {e}")),
                }
            }
            Ok(Accepted::Recorded) => {
                if events.send(Event::Guest(msg)).is_err() {
                    return;
                }
            }
            Err(v) => return violation(v.0),
        }
    }
}

/// The guest's deadline for its services to become ready, from `Config`'s
/// arrival: what remains of the boot timeout, less [`READY_MARGIN`] (at least
/// 1 ms; without a boot deadline, the most `Config` can carry).
fn ready_timeout(boot_deadline: Option<Instant>, now: Instant) -> u32 {
    match boot_deadline {
        None => u32::MAX,
        Some(d) => {
            let left = d.saturating_duration_since(now).saturating_sub(READY_MARGIN);
            u32::try_from(left.as_millis()).unwrap_or(u32::MAX).max(1)
        }
    }
}

/// Writes queued host messages once `Config` has gone out.
fn writer(stream: &Receiver<UnixStream>, out: &Receiver<Out>, events: &Sender<Event>) {
    let Ok(mut s) = stream.recv() else { return };
    for item in out {
        let r = match item {
            Out::Stop => return,
            Out::Msg(m) => write_message(&mut s, &m).map_err(|e| match e {
                kiln_proto::ProtoError::Io(io) => io,
                other => std::io::Error::other(other.to_string()),
            }),
            Out::Raw(b) => s.write_all(&b),
        };
        match r {
            Ok(()) => {}
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                let _ = events.send(Event::Violation("the guest is not reading its control channel".into()));
                return;
            }
            // The VM is gone, or going.
            Err(_) => return,
        }
    }
}

/// Streams guest output to `sink`. A sink that fails (a closed pipe) is dropped,
/// but the guest's output is still read to EOF, so the guest never blocks on it.
/// Every byte read or written counts as movement for the drain bound, and time in
/// a write to the sink does not count against it.
fn copy_out(mut from: UnixStream, mut sink: Box<dyn Write + Send>, p: u32, drain: &Drain, events: &Sender<Event>) {
    let mut buf = vec![0u8; 64 * 1024];
    let mut ok = true;
    loop {
        match from.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                drain.moved(n);
                if ok {
                    // However long the sink takes, the drain waits for it (after a real end).
                    drain.writing(p, true);
                    ok = write_moving(sink.as_mut(), buf.get(..n).unwrap_or_default(), drain);
                    drain.writing(p, false);
                }
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    drain.ports().remove(&p);
    let _ = events.send(Event::Eof);
}

/// `write_all` and `flush`, counting each write's bytes as they go; false when
/// the sink fails.
fn write_moving(sink: &mut dyn Write, mut bytes: &[u8], drain: &Drain) -> bool {
    while !bytes.is_empty() {
        match sink.write(bytes) {
            Ok(0) => return false,
            Ok(n) => {
                drain.moved(n);
                bytes = bytes.get(n..).unwrap_or_default();
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(_) => return false,
        }
    }
    sink.flush().is_ok()
}

/// Copies stdin to the guest; EOF is a half-close.
fn copy_in(mut from: Box<dyn Read + Send>, mut to: UnixStream) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match from.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if to.write_all(buf.get(..n).unwrap_or_default()).is_err() {
                    return;
                }
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    let _ = to.shutdown(std::net::Shutdown::Write);
}

/// Copies the terminal's input to the guest, acting on escape sequences. The
/// keys in each read act before its bytes are written, and a guest that stops
/// reading its terminal loses input rather than holding up the keys: a write that
/// makes no progress within the socket's send timeout drops the rest of its bytes,
/// and later input is offered without waiting until the guest reads again.
fn tty_in(mut from: Box<dyn Read + Send>, mut to: UnixStream, escape: bool, events: &Sender<Event>) {
    let mut keys = Escape::default();
    let mut buf = vec![0u8; 4096];
    let mut stalled = false;
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        let read = buf.get(..n).unwrap_or_default();
        let parts = if escape {
            keys.feed(read)
        } else {
            vec![Key::Data(read.to_vec())]
        };
        for k in &parts {
            match k {
                Key::Data(_) => {}
                Key::Shutdown => {
                    let _ = events.send(Event::Shutdown);
                }
                Key::Kill => {
                    let _ = events.send(Event::Kill);
                }
            }
        }
        for k in parts {
            if let Key::Data(d) = k
                && !write_tty(&mut to, &d, &mut stalled)
            {
                return;
            }
        }
    }
}

/// Writes `bytes` to the guest's terminal; false when the connection is gone.
/// See [`tty_in`] for `stalled`.
fn write_tty(to: &mut UnixStream, mut bytes: &[u8], stalled: &mut bool) -> bool {
    while !bytes.is_empty() {
        let r = if *stalled {
            rustix::net::send(&*to, bytes, rustix::net::SendFlags::DONTWAIT).map_err(std::io::Error::from)
        } else {
            to.write(bytes)
        };
        match r {
            Ok(0) => return false,
            Ok(n) => {
                *stalled = false;
                bytes = bytes.get(n..).unwrap_or_default();
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                *stalled = true;
                return true;
            }
            Err(_) => return false,
        }
    }
    true
}
```

- [ ] **Step 5: Run the crate's tests**

Run:
```bash
cargo test -q -p kiln-run
```
Expected: all pass (34 tests on macOS, 119 on Linux); on Linux `tests/session.rs` includes the three new cases.

- [ ] **Step 6: Format, lint and run the whole workspace**

Run:
```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test --all -q
```
Expected: no clippy warnings; every test passes (402 tests).

- [ ] **Step 7: Lint and test on Linux**

In the Lima VM (`limactl shell vmkit`, then `cd` to the repository and `export CARGO_TARGET_DIR=$HOME/target-m3d`):

Run (Linux):
```bash
cargo clippy --all-targets -- -D warnings && cargo test --all -q
```
Expected: no clippy warnings; every test passes (504 tests; the boot and `kiln run` cases skip themselves without their environment).

- [ ] **Step 8: Commit**

```bash
git add -A crates
git commit -m 'feat(kiln-run): negotiate the guest protocol version and refuse services to a version 1 guest'
```

### Task 3: `kiln-oci`: `ContainerConfig` carries the image's labels

**Files:**
- Modify: `crates/kiln-oci/src/types.rs`

**Interfaces:**
- Consumes: `kiln_oci::ContainerConfig`.
- Produces: `ContainerConfig::labels: Option<BTreeMap<String, String>>` (serde `Labels`, omitted when `None`; JSON `null` reads as `None`) and `ContainerConfig::label(&self, key) -> Option<&str>`.

The OCI config's `Labels`, parsed and serialised, so `kiln convert` can read `dev.kiln.services` (Task 4) and potter can set it on the `ResolvedImage` it builds by hand. The kiln config does not copy labels, so converted images keep their digests.

- [ ] **Step 1: Modify `crates/kiln-oci/src/types.rs`**

The field and its accessor; the config parsing test checks them (a label, a missing key, the round trip under `Labels`, `null`).

In `crates/kiln-oci/src/types.rs`, replace:
```rust
    pub stop_signal: Option<String>,
}
```
with:
```rust
    pub stop_signal: Option<String>,
    /// The image's labels. kiln reads `dev.kiln.services` from them (kiln-image).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<BTreeMap<String, String>>,
}

impl ContainerConfig {
    pub fn label(&self, key: &str) -> Option<&str> {
        self.labels.as_ref()?.get(key).map(String::as_str)
    }
}
```

In `crates/kiln-oci/src/types.rs`, replace:
```rust
        assert_eq!(cc.cmd.unwrap(), vec!["sh"]);
        assert_eq!(cc.stop_signal.as_deref(), Some("SIGQUIT"));
        assert_eq!(cc.working_dir.as_deref(), Some("/w"));
```
with:
```rust
        assert_eq!(cc.cmd.as_deref(), Some(&["sh".to_string()][..]));
        assert_eq!(cc.stop_signal.as_deref(), Some("SIGQUIT"));
        assert_eq!(cc.working_dir.as_deref(), Some("/w"));
        assert_eq!(cc.label("a"), Some("b"));
        assert_eq!(cc.label("c"), None);
        // Serialised back under the OCI name; `null` labels read as none.
        let s = String::from_utf8(canonical_json(&cc)).unwrap();
        assert!(s.contains(r#""Labels":{"a":"b"}"#), "{s}");
        let none: ContainerConfig = serde_json::from_str(r#"{"Labels":null}"#).unwrap();
        assert_eq!(none.labels, None);
        assert!(!String::from_utf8(canonical_json(&none)).unwrap().contains("Labels"));
```

- [ ] **Step 2: Run the crate's tests**

Run:
```bash
cargo test -q -p kiln-oci
```
Expected: all pass (34 tests on macOS, 34 on Linux).

- [ ] **Step 3: Format, lint and run the whole workspace**

Run:
```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test --all -q
```
Expected: no clippy warnings; every test passes (402 tests).

- [ ] **Step 4: Lint and test on Linux**

In the Lima VM (`limactl shell vmkit`, then `cd` to the repository and `export CARGO_TARGET_DIR=$HOME/target-m3d`):

Run (Linux):
```bash
cargo clippy --all-targets -- -D warnings && cargo test --all -q
```
Expected: no clippy warnings; every test passes (504 tests; the boot and `kiln run` cases skip themselves without their environment).

- [ ] **Step 5: Commit**

```bash
git add -A crates
git commit -m 'feat(kiln-oci): ContainerConfig carries the image labels'
```

### Task 4: `kiln-image`: `KilnConfig.services` from the `dev.kiln.services` label, with config schema version 2

**Files:**
- Modify: `crates/kiln-image/Cargo.toml`, `crates/kiln-image/src/convert.rs`, `crates/kiln-image/src/error.rs`, `crates/kiln-image/src/load.rs`, `crates/kiln-image/src/types.rs`, `crates/kiln-image/tests/convert.rs`, `crates/kiln/src/main.rs`
- `Cargo.lock` (cargo updates it)

**Interfaces:**
- Consumes: Task 1's `kiln_proto::{Ready, Service, Process, validate_services, signal::parse_stop_signal}`; Task 3's `ContainerConfig::label`.
- Produces:
  - `kiln_image::types::{LABEL_SERVICES ("dev.kiln.services"), Service { name, process: ServiceProcess, ready: Ready (default none, omitted), stop_signal: Option<String> (default SIGTERM, omitted) }, ServiceProcess { entrypoint, cmd, env, working_dir, user }}` (both `deny_unknown_fields`, camelCase); `Service::to_proto(&self, extra_env) -> Result<kiln_proto::Service, String>`; `validate_services(&[Service]) -> Result<(), String>`; `services_from_label(&str) -> kiln_image::Result<Vec<Service>>`.
  - `KilnConfig::services: Vec<Service>` (omitted when empty); `SCHEMA_VERSION_SERVICES` (2) beside `SCHEMA_VERSION` (1); `schema_version_for(&[Service]) -> u32`.
  - `ImageError::Services(String)` ("invalid dev.kiln.services label: …"), `ImageError::SchemaMismatch { version, services }`.
  - `kiln inspect` prints `service:    <name>  ready <r>  stop <sig>  <argv>` per service.
  - potter's route, unchanged: set the label in `ResolvedImage.config.config.labels` and call `kiln_image::convert_resolved`.

The image side (spec §5.1, potter spec §6.1.1). `convert_image`, which `convert_local`, `convert_registry` and `convert_resolved` all reach, reads the label from the source OCI config before any layer work: a label that does not parse, has an unknown field anywhere (`restart`, a process-level `stopSignal`) or breaks a kiln-proto rule fails the conversion at once. The parsed services land in the kiln config blob, so the image digest covers them; the canonical form omits defaults (`ready: none`, `stopSignal: SIGTERM`).

**Ruling 1:** a config with services is written with `schemaVersion` 2. Every kiln before M3d accepts only 1, so it refuses such an image (`unsupported kiln image schema version 2`) instead of running it without its services. Readers here accept 1 without services and 2 with them, nothing else (P1). Images without services keep version 1 and their digests (the golden digest test is unchanged).

- [ ] **Step 1: Modify `crates/kiln-image/tests/convert.rs`**

The label becomes the config's services with `schemaVersion` 2, and a bad label fails before any layer is cached; a hand-built `ResolvedImage` through `convert_resolved` (potter's route); readers refuse a version that does not match the services, and version 3.

At the end of `crates/kiln-image/tests/convert.rs`, append:
```rust

/// The label route (spec §9.8 `services`, potter spec §6.1.1): `dev.kiln.services`
/// on the OCI config becomes the kiln config's `services`, so the image's digest
/// covers them.
#[test]
fn the_services_label_becomes_the_configs_services() {
    use kiln_image::types::LABEL_SERVICES;
    use kiln_oci::ContainerConfig;
    use kiln_oci::testlayout::LayoutBuilder;
    let label = r#"[{"name":"php-fpm","process":{"cmd":["php-fpm","-F"]},"ready":"unix:/run/php-fpm.sock","stopSignal":"SIGQUIT"}]"#;
    let src = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    let build = |dir: &std::path::Path, label: &str| {
        let mut b = LayoutBuilder::new(dir);
        let cfg = ContainerConfig {
            cmd: Some(vec!["nginx".into(), "-g".into(), "daemon off;".into()]),
            labels: Some([(LABEL_SERVICES.to_string(), label.to_string())].into()),
            ..Default::default()
        };
        let d = b.image(&arm(), &[gz(&base(0o1777))], cfg);
        b.add(d, Some("app")).finish()
    };
    let path = build(src.path(), label);
    let out = convert_local(&store, &path, &req(&[arm()], "web"), &ConvertOptions::default()).unwrap();
    let m = &load(&store, &out.digest).unwrap().entries[0].1;
    assert_eq!(m.config.services.len(), 1);
    assert_eq!(m.config.services[0].name, "php-fpm");
    assert_eq!(m.config.services[0].stop_signal.as_deref(), Some("SIGQUIT"));
    let raw = String::from_utf8(store.read_metadata(&m.manifest.config.digest).unwrap()).unwrap();
    assert!(raw.contains(r#""services":[{"name":"php-fpm","#), "{raw}");
    // Services make the config schema version 2, which a kiln older than M3d refuses.
    assert_eq!(m.config.schema_version, 2);
    assert!(raw.contains(r#""schemaVersion":2,"#), "{raw}");

    // A bad label fails the conversion before any layer is converted.
    let bad = tempfile::tempdir().unwrap();
    let home2 = tempfile::tempdir().unwrap();
    let store2 = Store::open(home2.path()).unwrap();
    let path = build(
        bad.path(),
        r#"[{"name":"php-fpm","process":{"cmd":["x"]},"restart":"always"}]"#,
    );
    let err = convert_local(&store2, &path, &req(&[arm()], "web"), &ConvertOptions::default()).unwrap_err();
    assert!(matches!(err, ImageError::Services(_)), "{err}");
    assert!(err.to_string().contains("unknown field `restart`"), "{err}");
    assert_eq!(cache_entries(&store2), 0);
}

/// potter commits through `convert_resolved` with a `ResolvedImage` it builds
/// itself: the labels on its hand-built config take the same route.
#[test]
fn a_hand_built_resolved_image_carries_services_through_its_labels() {
    use kiln_image::types::LABEL_SERVICES;
    use kiln_image::{StoredLayers, convert_resolved};
    let src = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    let path = layout(src.path(), &[arm()], &[gz(&base(0o1777))]);
    let mut resolved =
        kiln_oci::resolve_local(&store, &kiln_oci::LocalSource::detect(&path).unwrap(), None, &[arm()]).unwrap();
    let cfg = resolved[0].config.config.get_or_insert_with(Default::default);
    cfg.labels = Some(
        [(
            LABEL_SERVICES.to_string(),
            r#"[{"name":"cache","process":{"cmd":["redis-server"]},"ready":"tcp:6379"}]"#.to_string(),
        )]
        .into(),
    );
    let out = convert_resolved(&store, &resolved, None, &ConvertOptions::default(), &StoredLayers).unwrap();
    let m = &load(&store, &out.digest).unwrap().entries[0].1;
    assert_eq!(m.config.services[0].ready, kiln_proto::Ready::Tcp(6379));
}

/// Readers accept schema version 1 without services and 2 with them, nothing else:
/// a version 1 config listing services is what an older kiln would run without them.
#[test]
fn the_config_schema_version_must_match_its_services() {
    use kiln_image::load_manifest;
    use kiln_oci::ImageManifest;
    use kiln_oci::canonical_json;
    let src = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    let path = layout(src.path(), &[arm()], &[gz(&base(0o1777))]);
    let out = convert_local(&store, &path, &req(&[arm()], "plain"), &ConvertOptions::default()).unwrap();
    let m = load(&store, &out.digest).unwrap().entries[0].1.clone();
    assert_eq!(m.config.schema_version, 1);
    // The same manifest, its config changed by `edit`.
    let with = |edit: &dyn Fn(&mut serde_json::Value)| {
        let mut config: serde_json::Value =
            serde_json::from_slice(&store.read_metadata(&m.manifest.config.digest).unwrap()).unwrap();
        edit(&mut config);
        let bytes = canonical_json(&config);
        let mut manifest: ImageManifest = m.manifest.clone();
        manifest.config.digest = store.put_bytes(&bytes).unwrap();
        manifest.config.size = bytes.len() as u64;
        load_manifest(&store, &store.put_bytes(&canonical_json(&manifest)).unwrap())
    };
    let services = serde_json::json!([{"name": "a", "process": {"cmd": ["x"]}}]);
    let ok = with(&|c| {
        c["schemaVersion"] = 2.into();
        c["services"] = services.clone();
    })
    .unwrap();
    assert_eq!(ok.config.services.len(), 1);
    for (edit, want) in [
        (
            Box::new(|c: &mut serde_json::Value| c["services"] = services.clone())
                as Box<dyn Fn(&mut serde_json::Value)>,
            "schema version 1 and 1 services",
        ),
        (
            Box::new(|c| c["schemaVersion"] = 2.into()),
            "schema version 2 and 0 services",
        ),
        (
            Box::new(|c| c["schemaVersion"] = 3.into()),
            "unsupported kiln image schema version 3",
        ),
    ] {
        let err = with(&*edit).unwrap_err();
        assert!(err.to_string().contains(want), "{err}");
    }
}
```

- [ ] **Step 2: Run the tests to see them fail**

Run:
```bash
cargo test -q -p kiln-image --test convert
```
Expected: FAIL to compile: `kiln_image::types::LABEL_SERVICES`, `ImageError::Services` and `KilnConfig::services` do not exist yet, and kiln-image does not depend on kiln-proto.

- [ ] **Step 3: Modify `crates/kiln-image/Cargo.toml`**

kiln-proto's rules and types. `Cargo.lock` follows on the first build.

In `crates/kiln-image/Cargo.toml`, replace:
```toml
kiln-oci = { path = "../kiln-oci" }
kiln-registry = { path = "../kiln-registry" }
```
with:
```toml
kiln-oci = { path = "../kiln-oci" }
kiln-proto = { path = "../kiln-proto" }
kiln-registry = { path = "../kiln-registry" }
```

- [ ] **Step 4: Modify `crates/kiln-image/src/error.rs`**

The label's error and the schema mismatch.

In `crates/kiln-image/src/error.rs`, replace:
```rust
    UnknownSchema(u32),
    #[error("{0} is not a kiln image (use `kiln convert` for OCI images)")]
```
with:
```rust
    UnknownSchema(u32),
    #[error(
        "kiln image config has schema version {version} and {services} services (version 2 is for configs with services, 1 for those without)"
    )]
    SchemaMismatch { version: u32, services: usize },
    #[error("{0} is not a kiln image (use `kiln convert` for OCI images)")]
```

In `crates/kiln-image/src/error.rs`, replace:
```rust
    BadOption(String),
}
```
with:
```rust
    BadOption(String),
    #[error("invalid dev.kiln.services label: {0}")]
    Services(String),
}
```

- [ ] **Step 5: Modify `crates/kiln-image/src/types.rs`**

The label's types, their conversion to the wire, the checks, and the schema version rule. Unit tests: potter's PHP label round trips to its canonical form, `--env` reaches the service, each bad label is refused, services need version 2.

In `crates/kiln-image/src/types.rs`, replace:
```rust
use kiln_oci::ContainerConfig;
use kiln_store::Digest;
```
with:
```rust
use kiln_oci::ContainerConfig;
use kiln_proto::Ready;
use kiln_store::Digest;
```

In `crates/kiln-image/src/types.rs`, replace:
```rust
pub const SCHEMA_VERSION: u32 = 1;
```
with:
```rust
/// The OCI image-config label whose JSON value lists the image's services
/// ([`Service`]); `kiln convert` copies them into [`KilnConfig::services`].
pub const LABEL_SERVICES: &str = "dev.kiln.services";

/// The config's `schemaVersion` for an image without services.
pub const SCHEMA_VERSION: u32 = 1;
/// The config's `schemaVersion` for an image with services: a kiln older than
/// M3d accepts only [`SCHEMA_VERSION`], so it refuses such an image instead of
/// running it without its services.
pub const SCHEMA_VERSION_SERVICES: u32 = 2;

/// The `schemaVersion` a config with these services is written with, and the
/// only one a reader accepts for it.
pub fn schema_version_for(services: &[Service]) -> u32 {
    if services.is_empty() {
        SCHEMA_VERSION
    } else {
        SCHEMA_VERSION_SERVICES
    }
}
```

In `crates/kiln-image/src/types.rs`, replace:
```rust
    pub erofs_format_version: u32,
}
```
with:
```rust
    pub erofs_format_version: u32,
    /// Processes `kiln-init` starts, in order, before the main process (spec
    /// §9.8, protocol version 2). Omitted when empty. A config with services
    /// has `schemaVersion` 2 ([`schema_version_for`]), which a kiln older than
    /// M3d refuses.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<Service>,
}

/// One service: the shape of `dev.kiln.services` entries and of
/// `KilnConfig.services`. Unknown fields are refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Service {
    /// `[a-z0-9-]{1,32}`, unique in the image.
    pub name: String,
    pub process: ServiceProcess,
    /// `unix:<absolute path>`, `tcp:<port>` or `none` (the default, omitted).
    #[serde(default = "ready_none", skip_serializing_if = "Ready::is_none")]
    pub ready: Ready,
    /// A signal name or number, as `STOPSIGNAL` (default SIGTERM, omitted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_signal: Option<String>,
}

fn ready_none() -> Ready {
    Ready::None
}

/// A service's process: the main process's fields without `stopSignal`. Run-time
/// `--env` values are added to every process's environment, services' included.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ServiceProcess {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entrypoint: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cmd: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

impl Service {
    /// The service as `Config` carries it, with `extra_env` appended to its
    /// environment (run-time `--env`). Checks only what the conversion needs
    /// (the stop signal); [`validate_services`] checks the rest.
    pub fn to_proto(&self, extra_env: &[String]) -> std::result::Result<kiln_proto::Service, String> {
        let stop_signal = match &self.stop_signal {
            Some(s) => kiln_proto::signal::parse_stop_signal(s).map_err(|e| format!("service {}: {e}", self.name))?,
            None => kiln_proto::signal::DEFAULT_STOP,
        };
        let p = &self.process;
        Ok(kiln_proto::Service {
            name: self.name.clone(),
            process: kiln_proto::Process {
                entrypoint: p.entrypoint.clone(),
                cmd: p.cmd.clone(),
                env: p.env.iter().chain(extra_env).cloned().collect(),
                working_dir: p.working_dir.clone(),
                user: p.user.clone(),
            },
            ready: self.ready.clone(),
            stop_signal,
        })
    }
}

/// Checks services with the rules `kiln-init` applies (kiln-proto's): at most 16,
/// unique `[a-z0-9-]{1,32}` names, each process as valid as a main process, an
/// absolute socket path or a port 1–65535, a stop signal 1–31.
pub fn validate_services(services: &[Service]) -> std::result::Result<(), String> {
    let wire = services
        .iter()
        .map(|s| s.to_proto(&[]))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    kiln_proto::validate_services(&wire).map_err(|e| match e {
        kiln_proto::ProtoError::Invalid { reason, .. } => reason,
        other => other.to_string(),
    })
}

/// Parses and checks a `dev.kiln.services` label: a JSON array of [`Service`]s.
pub fn services_from_label(value: &str) -> crate::Result<Vec<Service>> {
    let services: Vec<Service> = serde_json::from_str(value).map_err(|e| crate::ImageError::Services(e.to_string()))?;
    validate_services(&services).map_err(crate::ImageError::Services)?;
    Ok(services)
}
```

In `crates/kiln-image/src/types.rs`, replace:
```rust
    }

    #[test]
    fn hex_round_trips() {
```
with:
```rust
    }

    /// potter's PHP web image (potter spec §6.1.1): nginx is the main process,
    /// php-fpm a service ready on its socket and stopped with SIGQUIT.
    const LARAVEL: &str = r#"[{"name":"php-fpm","process":{"cmd":["-F"],"entrypoint":["php-fpm"],"env":["PHP_INI_DIR=/usr/local/etc/php"],"workingDir":"/app"},"ready":"unix:/run/php-fpm.sock","stopSignal":"SIGQUIT"}]"#;

    #[test]
    fn the_services_label_parses_and_round_trips() {
        let s = services_from_label(LARAVEL).unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].ready, Ready::Unix("/run/php-fpm.sock".into()));
        let wire = s[0].to_proto(&["APP_KEY=k".into()]).unwrap();
        assert_eq!((wire.stop_signal, wire.process.user.as_deref()), (3, None));
        assert_eq!(wire.process.env, ["PHP_INI_DIR=/usr/local/etc/php", "APP_KEY=k"]);
        // Canonical JSON of the parsed services is the label (keys sorted, defaults omitted).
        assert_eq!(String::from_utf8(kiln_oci::canonical_json(&s)).unwrap(), LARAVEL);
        let minimal = services_from_label(r#"[{"name":"a","process":{"cmd":["x"]}}]"#).unwrap();
        assert_eq!((&minimal[0].ready, &minimal[0].stop_signal), (&Ready::None, &None));
        assert_eq!(minimal[0].to_proto(&[]).unwrap().stop_signal, 15);
        assert_eq!(
            String::from_utf8(kiln_oci::canonical_json(&minimal)).unwrap(),
            r#"[{"name":"a","process":{"cmd":["x"]}}]"#
        );
    }

    #[test]
    fn bad_services_labels_are_refused() {
        for (bad, why) in [
            ("", "EOF"),
            ("{}", "a sequence"),
            (
                r#"[{"name":"a","process":{"cmd":["x"]},"restart":"always"}]"#,
                "unknown field `restart`",
            ),
            (
                r#"[{"name":"a","process":{"cmd":["x"],"stopSignal":"QUIT"}}]"#,
                "unknown field `stopSignal`",
            ),
            (r#"[{"name":"A","process":{"cmd":["x"]}}]"#, "service name"),
            (r#"[{"name":"a","process":{}}]"#, "no command"),
            (
                r#"[{"name":"a","process":{"cmd":["x"]},"ready":"unix:sock"}]"#,
                "not absolute",
            ),
            (r#"[{"name":"a","process":{"cmd":["x"]},"ready":"tcp:0"}]"#, "tcp:0"),
            (
                r#"[{"name":"a","process":{"cmd":["x"]},"stopSignal":"SIGRTMIN+3"}]"#,
                "real-time",
            ),
            (
                r#"[{"name":"a","process":{"cmd":["x"]}},{"name":"a","process":{"cmd":["y"]}}]"#,
                "two services",
            ),
            (
                r#"[{"name":"a","process":{"cmd":["x"],"workingDir":"app"}}]"#,
                "not absolute",
            ),
        ] {
            let err = services_from_label(bad).unwrap_err().to_string();
            assert!(
                err.starts_with("invalid dev.kiln.services label: ") && err.contains(why),
                "{bad}: {err}"
            );
        }
    }

    #[test]
    fn services_need_schema_version_2() {
        assert_eq!(schema_version_for(&[]), SCHEMA_VERSION);
        let s = services_from_label(LARAVEL).unwrap();
        assert_eq!(schema_version_for(&s), SCHEMA_VERSION_SERVICES);
    }

    #[test]
    fn hex_round_trips() {
```

In `crates/kiln-image/src/types.rs`, replace:
```rust
        };
        let s = String::from_utf8(kiln_oci::canonical_json(&c)).unwrap();
        assert!(
            !s.contains("kernel") && !s.contains("init") && !s.contains("reference"),
```
with:
```rust
            services: Vec::new(),
        };
        let s = String::from_utf8(kiln_oci::canonical_json(&c)).unwrap();
        assert!(
            !s.contains("kernel") && !s.contains("init") && !s.contains("reference") && !s.contains("services"),
```

- [ ] **Step 6: Modify `crates/kiln-image/src/convert.rs`**

The label is read before any layer work; the config's version follows its services.

In `crates/kiln-image/src/convert.rs`, replace:
```rust
    check_platform(&platform)?;
    let layers = &img.manifest.layers;
```
with:
```rust
    check_platform(&platform)?;
    // Before any layer work: a bad label fails the conversion at once.
    let services = match img.config.config.as_ref().and_then(|c| c.label(LABEL_SERVICES)) {
        Some(label) => services_from_label(label)?,
        None => Vec::new(),
    };
    let layers = &img.manifest.layers;
```

In `crates/kiln-image/src/convert.rs`, replace:
```rust
        schema_version: SCHEMA_VERSION,
```
with:
```rust
        schema_version: schema_version_for(&services),
```

In `crates/kiln-image/src/convert.rs`, replace:
```rust
        erofs_format_version: FORMAT_VERSION,
    };
```
with:
```rust
        erofs_format_version: FORMAT_VERSION,
        services,
    };
```

- [ ] **Step 7: Modify `crates/kiln-image/src/load.rs`**

Ruling 1 on reading.

In `crates/kiln-image/src/load.rs`, replace:
```rust
use crate::types::{KILN_ARTIFACT, KILN_CONFIG, KilnConfig, SCHEMA_VERSION};
```
with:
```rust
use crate::types::{
    KILN_ARTIFACT, KILN_CONFIG, KilnConfig, SCHEMA_VERSION, SCHEMA_VERSION_SERVICES, schema_version_for,
};
```

In `crates/kiln-image/src/load.rs`, replace:
```rust
    if config.schema_version != SCHEMA_VERSION {
        return Err(ImageError::UnknownSchema(config.schema_version));
```
with:
```rust
    if config.schema_version != SCHEMA_VERSION && config.schema_version != SCHEMA_VERSION_SERVICES {
        return Err(ImageError::UnknownSchema(config.schema_version));
    }
    // Version 2 exactly when there are services: a version 1 config listing
    // services is one an older kiln would run without them.
    if config.schema_version != schema_version_for(&config.services) {
        return Err(ImageError::SchemaMismatch {
            version: config.schema_version,
            services: config.services.len(),
        });
```

- [ ] **Step 8: Modify `crates/kiln/src/main.rs`**

`kiln inspect` lists the services.

In `crates/kiln/src/main.rs`, replace:
```rust
                }
                let reference = m
```
with:
```rust
                }
                for s in &m.config.services {
                    let sp = &s.process;
                    let argv: Vec<String> = sp.entrypoint.iter().chain(&sp.cmd).cloned().collect();
                    println!(
                        "  service:    {}  ready {}  stop {}  {}",
                        clean_line(&s.name),
                        clean_line(&s.ready.to_string()),
                        clean_line(s.stop_signal.as_deref().unwrap_or("SIGTERM")),
                        list(&argv)
                    );
                }
                let reference = m
```

- [ ] **Step 9: Run the crate's tests**

Run:
```bash
cargo test -q -p kiln-image
```
Expected: all pass (70 tests on macOS, 70 on Linux); `tests/convert.rs` has the three new cases, and `golden_manifest_digest_is_stable` still passes.

- [ ] **Step 10: Format, lint and run the whole workspace**

Run:
```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test --all -q
```
Expected: no clippy warnings; every test passes (408 tests).

- [ ] **Step 11: Lint and test on Linux**

In the Lima VM (`limactl shell vmkit`, then `cd` to the repository and `export CARGO_TARGET_DIR=$HOME/target-m3d`):

Run (Linux):
```bash
cargo clippy --all-targets -- -D warnings && cargo test --all -q
```
Expected: no clippy warnings; every test passes (510 tests; the boot and `kiln run` cases skip themselves without their environment).

- [ ] **Step 12: Commit**

```bash
git add -A crates Cargo.lock
git commit -m 'feat(kiln-image): KilnConfig.services from the dev.kiln.services label, with config schema version 2'
```

### Task 5: `kiln run` passes the image's services to the guest and reports a service that ended the run

**Files:**
- Modify: `crates/kiln-run/src/config.rs`, `crates/kiln-run/src/linux/run.rs`, `crates/kiln-run/src/prepare.rs`, `crates/kiln/src/run_cmd.rs`

**Interfaces:**
- Consumes: Task 4's `KilnConfig::services`, `Service::to_proto`; Task 1's `Exited::describe_service`; M3b-1's `config::build`, `prepare::Prepared`, `check_size` (D-4).
- Produces:
  - `kiln_run::config::build(image: &ImageProcess, services: &[kiln_image::types::Service], opts: &RunOptions, rt: &Runtime) -> Result<Config>`.
  - `kiln_run::prepare::Prepared::services`.
  - `kiln run` prints `kiln: service <name> exited with <n>, which ended the run` (or `was killed by SIG<x>`) and exits with the service's status.

The host side of a run with services. `config::build` turns the image's services into `Config.services`: the run's `--env` values are appended to every service's environment as to the main process's (D2). `readyTimeoutMs` is set to its largest value here, so the 64 KiB size check (D-4) allows for the field; the session sets the real value (Task 2). A `Config` too large for one frame names the services' share in the refusal. `kiln run` has no flag for services: they come from the image.

- [ ] **Step 1: Modify `crates/kiln-run/src/config.rs`**

Services in `build`, with unit tests: `--env` reaches every service and the placeholder deadline is set; oversized services are named in the refusal.

In `crates/kiln-run/src/config.rs`, replace:
```rust
use kiln_image::types::Process as ImageProcess;
```
with:
```rust
use kiln_image::types::{Process as ImageProcess, Service as ImageService};
```

In `crates/kiln-run/src/config.rs`, replace:
```rust
/// Builds and checks the guest's `Config`.
pub fn build(image: &ImageProcess, opts: &RunOptions, rt: &Runtime) -> Result<Config> {
    let mut env = image.env.clone();
    for e in &opts.env {
        match e.split_once('=') {
            Some(_) => env.push(e.clone()),
            // As Docker: `--env KEY` passes kiln's value, and nothing when it is unset.
            None => {
                if let Some((_, v)) = rt.host_env.iter().find(|(k, _)| k == e) {
                    env.push(format!("{e}={v}"));
```
with:
```rust
/// Builds and checks the guest's `Config`. Run-time `--env` values are added to
/// the main process's environment and to every service's.
pub fn build(image: &ImageProcess, services: &[ImageService], opts: &RunOptions, rt: &Runtime) -> Result<Config> {
    let mut overrides = Vec::new();
    for e in &opts.env {
        match e.split_once('=') {
            Some(_) => overrides.push(e.clone()),
            // As Docker: `--env KEY` passes kiln's value, and nothing when it is unset.
            None => {
                if let Some((_, v)) = rt.host_env.iter().find(|(k, _)| k == e) {
                    overrides.push(format!("{e}={v}"));
```

In `crates/kiln-run/src/config.rs`, replace:
```rust
    }
    let cmd = if opts.cmd.is_empty() {
```
with:
```rust
    }
    let mut env = image.env.clone();
    env.extend(overrides.iter().cloned());
    let services = services
        .iter()
        .map(|s| s.to_proto(&overrides))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| Error::refused(format!("the image's services: {e}")))?;
    let cmd = if opts.cmd.is_empty() {
```

In `crates/kiln-run/src/config.rs`, replace:
```rust
        services: Vec::new(),
        ready_timeout_ms: None,
```
with:
```rust
        // The session sets what remains of the boot timeout when it sends
        // `Config`; the largest value is what the size check must allow for.
        ready_timeout_ms: (!services.is_empty()).then_some(u32::MAX),
        services,
```

In `crates/kiln-run/src/config.rs`, replace:
```rust
            Err(Error::refused(format!(
                "the run's configuration encodes to {n} bytes, over the control protocol's \
                 {MAX_FRAME}-byte frame: the command line has {argv} bytes and the environment {env} \
                 bytes ({} variables); pass large values in a file instead",
```
with:
```rust
            let services = if config.services.is_empty() {
                String::new()
            } else {
                let bytes = serde_json::to_vec(&config.services).map_or(0, |v| v.len());
                format!(", and its {} services encode to {bytes} bytes", config.services.len())
            };
            Err(Error::refused(format!(
                "the run's configuration encodes to {n} bytes, over the control protocol's \
                 {MAX_FRAME}-byte frame: the command line has {argv} bytes and the environment {env} \
                 bytes ({} variables){services}; pass large values in a file instead",
```

In `crates/kiln-run/src/config.rs`, replace:
```rust
        let mut o = RunOptions::new("img");
        let c = build(&image(), &o, &rt()).unwrap();
        assert_eq!(c.process.argv(), ["/entry", "default"]);
```
with:
```rust
        let mut o = RunOptions::new("img");
        let c = build(&image(), &[], &o, &rt()).unwrap();
        assert_eq!(c.process.argv(), ["/entry", "default"]);
```

In `crates/kiln-run/src/config.rs`, replace:
```rust
        let c = build(&image(), &o, &rt()).unwrap();
```
with:
```rust
        let c = build(&image(), &[], &o, &rt()).unwrap();
```

In `crates/kiln-run/src/config.rs`, replace:
```rust
        assert!(build(&p, &o, &rt()).unwrap_err().to_string().contains("real-time"));
        let empty = ImageProcess::default();
        assert!(build(&empty, &o, &rt()).unwrap_err().to_string().contains("after --"));
```
with:
```rust
        assert!(build(&p, &[], &o, &rt()).unwrap_err().to_string().contains("real-time"));
        let empty = ImageProcess::default();
        assert!(
            build(&empty, &[], &o, &rt())
                .unwrap_err()
                .to_string()
                .contains("after --")
        );
```

In `crates/kiln-run/src/config.rs`, replace:
```rust
        let err = build(&image(), &o, &rt()).unwrap_err().to_string();
        assert!(err.contains("environment") && err.contains("21 variables"), "{err}");
        o.env.truncate(10);
        build(&image(), &o, &rt()).unwrap();
```
with:
```rust
        let err = build(&image(), &[], &o, &rt()).unwrap_err().to_string();
        assert!(err.contains("environment") && err.contains("21 variables"), "{err}");
        o.env.truncate(10);
        build(&image(), &[], &o, &rt()).unwrap();
```

In `crates/kiln-run/src/config.rs`, replace:
```rust
        assert_eq!(build(&image(), &o, &r).unwrap().process.env, ["A=1", "GOOD=yes"]);
```
with:
```rust
        assert_eq!(build(&image(), &[], &o, &r).unwrap().process.env, ["A=1", "GOOD=yes"]);
    }

    #[test]
    fn services_get_the_run_time_environment_and_a_readiness_placeholder() {
        let services = kiln_image::types::services_from_label(
            r#"[{"name":"php-fpm","process":{"cmd":["php-fpm"],"env":["A=svc"]},"ready":"unix:/run/php-fpm.sock","stopSignal":"SIGQUIT"}]"#,
        )
        .unwrap();
        let mut o = RunOptions::new("img");
        o.env = vec!["APP_KEY=k".into(), "HOSTVAR".into()];
        let c = build(&image(), &services, &o, &rt()).unwrap();
        assert_eq!(c.services.len(), 1);
        assert_eq!(c.services[0].process.env, ["A=svc", "APP_KEY=k", "HOSTVAR=hv"]);
        assert_eq!(c.process.env, ["A=1", "APP_KEY=k", "HOSTVAR=hv"]);
        assert_eq!(c.services[0].stop_signal, 3);
        assert_eq!(c.ready_timeout_ms, Some(u32::MAX));
        assert_eq!(c.required_version(), 2);
        assert_eq!(build(&image(), &[], &o, &rt()).unwrap().ready_timeout_ms, None);
    }

    #[test]
    fn oversized_services_are_named_in_the_refusal() {
        let one = |i: usize| {
            format!(
                r#"{{"name":"s{i}","process":{{"cmd":["x"],"env":["BIG={}"]}}}}"#,
                "x".repeat(5000)
            )
        };
        let label = format!("[{}]", (0..14).map(one).collect::<Vec<_>>().join(","));
        let services = kiln_image::types::services_from_label(&label).unwrap();
        let err = build(&image(), &services, &RunOptions::new("img"), &rt())
            .unwrap_err()
            .to_string();
        assert!(err.contains("its 14 services encode to"), "{err}");
```

- [ ] **Step 2: Modify `crates/kiln-run/src/prepare.rs`**

The image's services.

In `crates/kiln-run/src/prepare.rs`, replace:
```rust
    pub process: Process,
    /// App layers, lowest first, and their blobs' paths.
```
with:
```rust
    pub process: Process,
    /// Started before the main process (spec §9.8).
    pub services: Vec<kiln_image::types::Service>,
    /// App layers, lowest first, and their blobs' paths.
```

In `crates/kiln-run/src/prepare.rs`, replace:
```rust
        process: image.config.process.clone(),
        layers,
```
with:
```rust
        process: image.config.process.clone(),
        services: image.config.services.clone(),
        layers,
```

- [ ] **Step 3: Modify `crates/kiln-run/src/linux/run.rs`**

Passes them to `build`.

In `crates/kiln-run/src/linux/run.rs`, replace:
```rust
            &prepared.process,
            opts,
```
with:
```rust
            &prepared.process,
            &prepared.services,
            opts,
```

- [ ] **Step 4: Modify `crates/kiln/src/run_cmd.rs`**

The service report.

In `crates/kiln/src/run_cmd.rs`, replace:
```rust
        eprintln!("kiln: {}", report.no_final_message());
    }
```
with:
```rust
        eprintln!("kiln: {}", report.no_final_message());
    } else if let Some(line) = o.exited.as_ref().and_then(|e| e.describe_service()) {
        // describe_service() is sanitised already.
        eprintln!("kiln: {}", clean_line(&line));
    }
```

- [ ] **Step 5: Run the crate's tests**

Run:
```bash
cargo test -q -p kiln-run
```
Expected: all pass (36 tests on macOS, 121 on Linux).

- [ ] **Step 6: Format, lint and run the whole workspace**

Run:
```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test --all -q
```
Expected: no clippy warnings; every test passes (410 tests).

- [ ] **Step 7: Lint and test on Linux**

In the Lima VM (`limactl shell vmkit`, then `cd` to the repository and `export CARGO_TARGET_DIR=$HOME/target-m3d`):

Run (Linux):
```bash
cargo clippy --all-targets -- -D warnings && cargo test --all -q
```
Expected: no clippy warnings; every test passes (512 tests; the boot and `kiln run` cases skip themselves without their environment).

- [ ] **Step 8: Commit**

```bash
git add -A crates
git commit -m 'feat: kiln run passes the image services to the guest and reports a service that ended the run'
```

### Task 6: `kiln-init`: start services before the main process; stop everything in reverse order, splitting the grace period

**Files:**
- Modify: `crates/kiln-init/src/linux/mod.rs`, `crates/kiln-init/src/linux/process.rs`, `crates/kiln-init/src/linux/supervise.rs`, `crates/kiln-init/src/linux/sys.rs`

**Interfaces:**
- Consumes: Task 1's `Config::{services, ready_timeout_ms}`, `Ready`, `Exited::service`; M3a's `process::start`, `supervise::run`, `sys::{signal, kill_all, reap_all}`.
- Produces:
  - `process::start(&Config, config_at: Instant, Sender<Event>) -> Result<Started>`; `Started { procs: Vec<Proc>, outputs, tty }` with `Proc { pid, name: Option<String>, stop_signal }`, services first and the main process last; `process::describe(WaitStatus) -> String`.
  - `sys::reap_any() -> Vec<(Pid, WaitStatus)>` (replaces `reap(main)`).
  - `supervise::run` returns `Exited` with `service` set when a service's exit ended the run.

The guest (spec §9.6, §9.8).

- **Stage 6.** Every service's user is resolved first (a failure names the service). Stdio is set up once. Each service is spawned in order: its own session, stdin `/dev/null`, the main process's stdout and stderr pipes (or, with `-t`, the pseudo-terminal as plain stdout and stderr, never its controlling terminal, D4), its own environment built as the main process's (without `TERM`), user, groups and working directory. Then init waits for it to be ready: a connection to its Unix socket or to `127.0.0.1:<port>` accepted, then closed (R4, R5), polling every 5 ms doubling to 50 ms, all bounded by `readyTimeoutMs` from `Config`'s arrival (`config_at`). Not ready by then, exited first, or not executable (errno kept, so 127 and 126 as for the main process): stage 6 fails naming the service. The main process starts last; `Running` still means it started.
- When a service runs as another uid than the main process, the stdout and stderr pipes are made mode 0622 so it can reopen `/dev/stdout`.
- **Stage 7.** The first supervised exit, or `Shutdown` (or SIGINT, SIGTERM to init), ends the run. **Ruling 3:** the processes still running are stopped one at a time in reverse start order (main first), each with its own stop signal; each gets a share of what remains of the grace period, divided evenly between the processes still to stop (P2). One that has not exited when its share ends gets SIGKILL (P3), and the next gets its stop signal; at the end of the grace period everything gets SIGKILL. The grace is `Shutdown`'s, or `Config.shutdownGraceSecs` when an exit ended the run; a later `Shutdown` can only shorten it. Host `Signal`s go to the main process, and are dropped once it has exited (D6). `Exited` carries the first exit's status and the service's name, or after `Shutdown` the main process's (D5). Without services this is version 1's behaviour: the main process's share is the whole grace period.

- [ ] **Step 1: Modify `crates/kiln-init/src/linux/sys.rs`**

Reaping any child: the supervisor now watches several.

In `crates/kiln-init/src/linux/sys.rs`, replace:
```rust
/// Reaps every child without blocking. Returns the wait status of `main` if it was among them.
/// (`wait`, i.e. `waitpid(-1)`: rustix's `waitpid(None, ..)` is `waitpid(0)`, which only
/// sees children in init's process group, and the main process has its own session.)
pub fn reap(main: Pid) -> Option<rustix::process::WaitStatus> {
    let mut found = None;
    loop {
        match rustix::process::wait(rustix::process::WaitOptions::NOHANG) {
            Ok(Some((pid, status))) => {
                if pid == main {
                    found = Some(status);
                }
            }
            Ok(None) | Err(_) => return found,
```
with:
```rust
/// Reaps every child that has exited, without blocking, and returns them.
/// (`wait`, i.e. `waitpid(-1)`: rustix's `waitpid(None, ..)` is `waitpid(0)`, which only
/// sees children in init's process group, and each supervised process has its own session.)
pub fn reap_any() -> Vec<(Pid, rustix::process::WaitStatus)> {
    let mut reaped = Vec::new();
    loop {
        match rustix::process::wait(rustix::process::WaitOptions::NOHANG) {
            Ok(Some(found)) => reaped.push(found),
            Ok(None) | Err(_) => return reaped,
```

- [ ] **Step 2: Modify `crates/kiln-init/src/linux/process.rs`**

Stage 6: services, readiness, then the main process; stdio shared.

In `crates/kiln-init/src/linux/process.rs`, replace:
```rust
//! Stage 6: start the main process (spec §9.6): user and groups from the image,
//! environment, stdio on pipes relayed to vsock or on a pty, working directory,
//! exec. Init keeps no copy of the child's ends of its stdio, so the relays see
//! EOF once every process holding them has exited.
```
with:
```rust
//! Stage 6: start the services, then the main process (spec §9.6, §9.8): user and
//! groups from the image, environment, stdio on pipes relayed to vsock or on a
//! pty, working directory, exec. Each service is started in `Config` order and
//! must be ready before the next one starts; the main process starts last. Init
//! keeps no copy of the processes' ends of their stdio, so the relays see EOF
//! once every process holding them has exited.
```

In `crates/kiln-init/src/linux/process.rs`, replace:
```rust

use kiln_proto::{Config, HOST_CID, WindowSize, port};
use rustix::pipe::PipeFlags;
use rustix::process::{Gid, Pid, Uid};
```
with:
```rust
use std::time::{Duration, Instant};

use kiln_proto::{Config, HOST_CID, Process, Ready, WindowSize, port};
use rustix::fs::Mode;
use rustix::pipe::PipeFlags;
use rustix::process::{Gid, Pid, Uid, WaitOptions, WaitStatus};
```

In `crates/kiln-init/src/linux/process.rs`, replace:
```rust
use super::{Event, relay};
use crate::error::{Context, Failure, Result};
use crate::passwd::{Identity, resolve};

/// The running main process.
pub struct Started {
    pub pid: Pid,
    /// The relays carrying its output; joined to drain it.
```
with:
```rust
use super::{Event, host_ok, relay};
use crate::error::{Context, Failure, Result};
use crate::passwd::{Identity, resolve};

/// One process init supervises: a service, or the main process.
#[derive(Debug, Clone)]
pub struct Proc {
    /// The service's name; `None` for the main process.
    pub name: Option<String>,
    pub pid: Pid,
    pub stop_signal: i32,
}

/// The running services and main process.
pub struct Started {
    /// The services in start order, then the main process (always last).
    pub procs: Vec<Proc>,
    /// The relays carrying their output; joined to drain it.
```

In `crates/kiln-init/src/linux/process.rs`, replace:
```rust
pub fn start(config: &Config, events: Sender<Event>) -> Result<Started> {
    let identity = resolve(
        config.process.user.as_deref(),
        &read_optional("/etc/passwd")?,
        &read_optional("/etc/group")?,
    )?;
    let env = crate::env::build(
        &config.process.env,
        &identity.home,
        &config.hostname,
        config.tty.is_some(),
    );
    let workdir = config.process.working_dir.as_deref().unwrap_or("/");
    std::fs::create_dir_all(workdir).context(format!("create the working directory {workdir}"))?;
    watch_signals(events)?;

    let argv = config.process.argv();
```
with:
```rust
/// The run's stdio before it is handed out: every service gets copies of the
/// output ends, the main process the originals.
enum Stdio_ {
    Pipes {
        /// The read end of the stdin pipe, with `interactive`.
        stdin: Option<File>,
        out: File,
        err: File,
    },
    /// The pty slave: the main process's terminal, and the services' stdout and stderr.
    Pty(OwnedFd),
}

/// How long a readiness check waits between attempts, at most.
const READY_POLL: Duration = Duration::from_millis(50);

/// Starts the services, each ready before the next, then the main process.
/// `config_at` is when `Config` arrived: the services' readiness deadline
/// (`Config.readyTimeoutMs`) counts from it.
pub fn start(config: &Config, config_at: Instant, events: Sender<Event>) -> Result<Started> {
    let passwd = read_optional("/etc/passwd")?;
    let group = read_optional("/etc/group")?;
    let identity = resolve(config.process.user.as_deref(), &passwd, &group)?;
    let service_ids = config
        .services
        .iter()
        .map(|s| {
            resolve(s.process.user.as_deref(), &passwd, &group)
                .map_err(|f| Failure::msg(format!("service {}: {f}", s.name)))
        })
        .collect::<Result<Vec<_>>>()?;
    watch_signals(events)?;

    let tty = config.tty.is_some();
    let (stdio, outputs, master) = match config.tty {
        Some(size) => {
            let (master, slave, outputs) = pty_stdio(size, &identity)?;
            (Stdio_::Pty(slave), outputs, Some(master))
        }
        None => {
            let (stdin, out, err, outputs) = pipe_stdio(config.interactive, &identity)?;
            // A service running as another user must be able to reopen its
            // stdout and stderr (`/dev/stdout`) as the main process can.
            if service_ids.iter().any(|id| id.uid != identity.uid) {
                for f in [&out, &err] {
                    rustix::fs::fchmod(f, Mode::from_raw_mode(0o622)).context("chmod the stdio")?;
                }
            }
            (Stdio_::Pipes { stdin, out, err }, outputs, None)
        }
    };

    let deadline = config
        .ready_timeout_ms
        .map(|ms| config_at + Duration::from_millis(u64::from(ms)));
    let mut procs = Vec::new();
    for (svc, id) in config.services.iter().zip(&service_ids) {
        let env = crate::env::build(&svc.process.env, &id.home, &config.hostname, false);
        let (out, err) = match &stdio {
            Stdio_::Pipes { out, err, .. } => (
                out.try_clone().context("dup the stdout pipe")?,
                err.try_clone().context("dup the stderr pipe")?,
            ),
            Stdio_::Pty(slave) => (File::from(dup(slave)?), File::from(dup(slave)?)),
        };
        let pid = spawn(
            &svc.process,
            &env,
            id,
            (Stdio::null(), out.into(), err.into()),
            false,
            &format!("service {}: ", svc.name),
        )?;
        procs.push(Proc {
            name: Some(svc.name.clone()),
            pid,
            stop_signal: svc.stop_signal,
        });
        wait_ready(&svc.name, &svc.ready, deadline, &procs)?;
    }

    let env = crate::env::build(&config.process.env, &identity.home, &config.hostname, tty);
    let stdio = match stdio {
        Stdio_::Pipes { stdin, out, err } => (stdin.map_or_else(Stdio::null, Stdio::from), out.into(), err.into()),
        Stdio_::Pty(slave) => (
            File::from(dup(&slave)?).into(),
            File::from(dup(&slave)?).into(),
            File::from(slave).into(),
        ),
    };
    let pid = spawn(&config.process, &env, &identity, stdio, tty, "")?;
    procs.push(Proc {
        name: None,
        pid,
        stop_signal: config.stop_signal,
    });
    Ok(Started {
        procs,
        outputs,
        tty: master,
    })
}

/// Starts one process: `what` prefixes its failures (empty for the main process).
fn spawn(
    process: &Process,
    env: &[String],
    identity: &Identity,
    (stdin, stdout, stderr): (Stdio, Stdio, Stdio),
    tty: bool,
    what: &str,
) -> Result<Pid> {
    let workdir = process.working_dir.as_deref().unwrap_or("/");
    std::fs::create_dir_all(workdir).context(format!("{what}create the working directory {workdir}"))?;
    let argv = process.argv();
```

In `crates/kiln-init/src/linux/process.rs`, replace:
```rust
        .current_dir(workdir);
    let (outputs, tty) = match config.tty {
        Some(size) => {
            let (master, outputs) = pty_stdio(&mut cmd, size, &identity)?;
            (outputs, Some(master))
        }
        None => (pipe_stdio(&mut cmd, config.interactive, &identity)?, None),
    };
    let (hook_failed, hook_report) =
        rustix::pipe::pipe_with(PipeFlags::CLOEXEC | PipeFlags::NONBLOCK).context("pipe")?;
    drop_privileges(&mut cmd, &identity, config.tty.is_some(), hook_report);
    let child = cmd.spawn().map_err(|e| match hook_step(&hook_failed) {
        // The error came from the setup before exec, not from exec itself.
        Some(step) => Failure::os(format!("start {}: {step}", argv[0]), &e),
        None => Failure {
            errno: e.raw_os_error(),
            message: format!("exec {}: {e}", argv[0]),
```
with:
```rust
        .current_dir(workdir)
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr);
    let (hook_failed, hook_report) =
        rustix::pipe::pipe_with(PipeFlags::CLOEXEC | PipeFlags::NONBLOCK).context("pipe")?;
    drop_privileges(&mut cmd, identity, tty, hook_report);
    let child = cmd.spawn().map_err(|e| match hook_step(&hook_failed) {
        // The error came from the setup before exec, not from exec itself.
        Some(step) => Failure::os(format!("{what}start {}: {step}", argv[0]), &e),
        None => Failure {
            errno: e.raw_os_error(),
            message: format!("{what}exec {}: {e}", argv[0]),
```

In `crates/kiln-init/src/linux/process.rs`, replace:
```rust
    Ok(Started {
        pid: Pid::from_raw(child.id() as i32).expect("a child's pid is positive"),
        outputs,
        tty,
    })
```
with:
```rust
    Ok(Pid::from_raw(child.id() as i32).expect("a child's pid is positive"))
}

/// Waits until service `name` is ready, failing when the deadline passes, when
/// any process started so far exits, or when the host has failed.
fn wait_ready(name: &str, ready: &Ready, deadline: Option<Instant>, procs: &[Proc]) -> Result<()> {
    let mut pause = Duration::from_millis(5);
    loop {
        for p in procs {
            if let Ok(Some((_, status))) = rustix::process::waitpid(Some(p.pid), WaitOptions::NOHANG) {
                let who = p.name.as_deref().unwrap_or("main");
                let when = if who == name {
                    "before it was ready".to_string()
                } else {
                    format!("while service {name} was starting")
                };
                return Err(Failure::msg(format!("service {who} {} {when}", describe(status))));
            }
        }
        if is_ready(ready) {
            return Ok(());
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(Failure::msg(format!(
                "service {name} was not ready ({ready}) by the deadline the host set (its boot timeout)"
            )));
        }
        host_ok()?;
        std::thread::sleep(pause);
        pause = (pause * 2).min(READY_POLL);
    }
}

/// Whether `ready` holds now: a connection to the socket or port is accepted.
fn is_ready(ready: &Ready) -> bool {
    match ready {
        Ready::None => true,
        Ready::Unix(path) => std::os::unix::net::UnixStream::connect(path).is_ok(),
        Ready::Tcp(port) => {
            let addr = std::net::SocketAddr::from(([127, 0, 0, 1], *port));
            std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok()
        }
    }
}

/// `exited with 3` or `was killed by signal 9`.
pub fn describe(status: WaitStatus) -> String {
    match (status.exit_status(), status.terminating_signal()) {
        (Some(code), _) => format!("exited with {code}"),
        (None, Some(sig)) => format!("was killed by signal {sig}"),
        (None, None) => "ended".into(),
    }
```

In `crates/kiln-init/src/linux/process.rs`, replace:
```rust
/// Pipes relayed to ports 1025–1027. Without `interactive`, stdin is `/dev/null`.
fn pipe_stdio(cmd: &mut Command, interactive: bool, id: &Identity) -> Result<Vec<JoinHandle<()>>> {
    if interactive {
        let (r, w) = pipe()?;
        give(&r, id)?;
        relay::spawn("stdin", connect(port::STDIN)?, w, drop)?;
        cmd.stdin(r);
    } else {
        cmd.stdin(Stdio::null());
    }
    let mut outputs = Vec::new();
```
with:
```rust
fn dup(fd: &OwnedFd) -> Result<OwnedFd> {
    rustix::io::fcntl_dupfd_cloexec(fd, 0).context("dup the pty")
}

/// Pipes relayed to ports 1025–1027: the stdin pipe's read end (with
/// `interactive`), and the stdout and stderr pipes' write ends.
#[allow(clippy::type_complexity)]
fn pipe_stdio(interactive: bool, id: &Identity) -> Result<(Option<File>, File, File, Vec<JoinHandle<()>>)> {
    let stdin = if interactive {
        let (r, w) = pipe()?;
        give(&r, id)?;
        relay::spawn("stdin", connect(port::STDIN)?, w, drop)?;
        Some(r)
    } else {
        None
    };
    let mut outputs = Vec::new();
    let mut ends = Vec::new();
```

In `crates/kiln-init/src/linux/process.rs`, replace:
```rust
        if port == port::STDOUT {
            cmd.stdout(w);
        } else {
            cmd.stderr(w);
        }
    }
    Ok(outputs)
}

/// A pty from the guest's devpts, relayed to port 1028; its slave is the child's stdio.
fn pty_stdio(cmd: &mut Command, size: WindowSize, id: &Identity) -> Result<(OwnedFd, Vec<JoinHandle<()>>)> {
```
with:
```rust
        ends.push(w);
    }
    let err = ends.pop().expect("two pipes");
    let out = ends.pop().expect("two pipes");
    Ok((stdin, out, err, outputs))
}

/// A pty from the guest's devpts, relayed to port 1028: its master and slave.
fn pty_stdio(size: WindowSize, id: &Identity) -> Result<(OwnedFd, OwnedFd, Vec<JoinHandle<()>>)> {
```

In `crates/kiln-init/src/linux/process.rs`, replace:
```rust
    let dup = |fd: &OwnedFd| rustix::io::fcntl_dupfd_cloexec(fd, 0).context("dup the pty");
```
with:
```rust

```

In `crates/kiln-init/src/linux/process.rs`, replace:
```rust
    cmd.stdin(File::from(dup(&slave)?))
        .stdout(File::from(dup(&slave)?))
        .stderr(File::from(slave));
    Ok((master, vec![output]))
```
with:
```rust
    Ok((master, slave, vec![output]))
```

- [ ] **Step 3: Modify `crates/kiln-init/src/linux/supervise.rs`**

Stage 7 with services, and the split grace period (ruling 3), with a unit test for the share.

Replace the contents of `crates/kiln-init/src/linux/supervise.rs` with:
```rust
//! Stage 7: supervise (spec §9.6, §9.8). Reap every child; forward `Signal` to the
//! main process; resize the pty on `WindowSize`.
//!
//! The run ends with the first of its processes (services and the main process)
//! to exit, or with `Shutdown` (SIGINT or SIGTERM to init alike). Then the others
//! are stopped one at a time in reverse start order (so the main process first),
//! each with its own stop signal. The grace period is split evenly between the
//! processes still to stop: one that has not exited when its share ends gets
//! SIGKILL, and the next one gets its stop signal, so a stuck process cannot cost
//! the ones after it their stop signals. When the grace period ends every process
//! gets SIGKILL. Without services this is version 1's behaviour: on `Shutdown` the
//! main process gets its stop signal (its share is the whole grace period), and
//! when it exits everything else is killed at once.
//!
//! The status reported is the first exit's (naming the service when it was one),
//! or after `Shutdown` the main process's. Once every supervised process has
//! exited, every other process is killed (as when a container's PID 1 exits),
//! the output is drained to EOF, and `Exited` is returned.

use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use kiln_proto::{Config, Exited, HostMessage};
use rustix::process::WaitStatus;
use signal_hook::consts::{SIGCHLD, SIGKILL};

use super::process::{Proc, Started, set_window_size};
use super::{Event, sys};
use crate::error::{Failure, Result};

/// Why the processes are being stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ending {
    /// The process at this index exited first: its status is the run's.
    Exit(usize),
    /// The host (or init's SIGINT, SIGTERM) asked: the main process's status is the run's.
    Shutdown,
}

struct Supervisor<'a> {
    config: &'a Config,
    procs: Vec<Proc>,
    status: Vec<Option<WaitStatus>>,
    ending: Option<Ending>,
    /// Who is stopped next, in reverse start order.
    queue: VecDeque<usize>,
    /// The process last sent its stop signal, until it exits, and when its
    /// share of the grace period ends.
    stopping: Option<(usize, Instant)>,
    /// When every process gets SIGKILL.
    deadline: Option<Instant>,
}

pub fn run(config: &Config, started: Started, events: &Receiver<Event>) -> Result<Exited> {
    let Started { procs, outputs, tty } = started;
    let main = procs.len() - 1;
    let mut s = Supervisor {
        config,
        status: vec![None; procs.len()],
        procs,
        ending: None,
        queue: VecDeque::new(),
        stopping: None,
        deadline: None,
    };
    while !s.done() {
        // Checked first, so a flood of events cannot postpone the kill.
        let event = match s.timer() {
            Some(at) if Instant::now() >= at => None,
            Some(at) => match events.recv_timeout(at.saturating_duration_since(Instant::now())) {
                Ok(event) => Some(event),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => {
                    unreachable!("the signal thread holds a sender for the life of the process")
                }
            },
            None => Some(
                events
                    .recv()
                    .expect("the signal thread holds a sender for the life of the process"),
            ),
        };
        match event {
            None => s.expire(),
            Some(Event::Signal(SIGCHLD)) => s.reap(),
            Some(Event::Signal(_)) => s.shutdown(config.shutdown_grace_secs),
            Some(Event::Host(HostMessage::Shutdown(m))) => s.shutdown(m.grace_secs),
            Some(Event::Host(HostMessage::Signal(m))) => {
                if s.status[main].is_none() {
                    sys::signal(s.procs[main].pid, m.sig);
                }
            }
            Some(Event::Host(HostMessage::WindowSize(size))) => {
                if let Some(master) = &tty {
                    set_window_size(master, size)?;
                }
            }
            Some(Event::Host(HostMessage::Config(_))) => {
                return Err(Failure::msg("protocol violation: a second Config"));
            }
            Some(Event::HostError(e)) => return Err(Failure::msg(format!("protocol violation: {e}"))),
            Some(Event::HostClosed) => return Err(Failure::msg("the host closed the control connection")),
        }
    }
    sys::kill_all();
    sys::reap_all();
    for relay in outputs {
        let _ = relay.join();
    }
    drop(tty);
    let first = match s.ending {
        Some(Ending::Exit(i)) => i,
        Some(Ending::Shutdown) | None => main,
    };
    let status = s.status[first].expect("every process has exited");
    let mut e = exited(status);
    e.service = s.procs[first].name.clone();
    Ok(e)
}

impl Supervisor<'_> {
    fn done(&self) -> bool {
        self.status.iter().all(Option::is_some)
    }

    /// Records the exits of supervised processes; the first one starts the end.
    fn reap(&mut self) {
        for (pid, status) in sys::reap_any() {
            let Some(i) = self.procs.iter().position(|p| p.pid == pid) else {
                continue;
            };
            self.status[i] = Some(status);
            if self.ending.is_none() {
                self.ending = Some(Ending::Exit(i));
                self.begin(self.config.shutdown_grace_secs);
            }
        }
        self.advance();
    }

    /// `Shutdown`: stop everything (main first); a later request can shorten
    /// the deadline, never extend it.
    fn shutdown(&mut self, grace: u32) {
        if self.ending.is_none() {
            self.ending = Some(Ending::Shutdown);
            self.begin(grace);
            self.advance();
        } else {
            self.shorten(grace);
        }
    }

    /// Queues every process still running, in reverse start order, and sets
    /// the deadline.
    fn begin(&mut self, grace: u32) {
        self.queue = (0..self.procs.len())
            .rev()
            .filter(|&i| self.status[i].is_none())
            .collect();
        self.shorten(grace);
    }

    fn shorten(&mut self, grace: u32) {
        let now = Instant::now();
        let at = now + Duration::from_secs(u64::from(grace));
        let deadline = self.deadline.map_or(at, |d| d.min(at));
        self.deadline = Some(deadline);
        // The process being stopped gets its share of what is left, if that is sooner.
        if let Some((i, end)) = self.stopping {
            self.stopping = Some((i, end.min(share(now, deadline, self.left() + 1))));
        }
    }

    /// When the loop must act without an event: the end of the current share
    /// or of the grace period, whichever is first.
    fn timer(&self) -> Option<Instant> {
        match (self.stopping, self.deadline) {
            (Some((_, end)), Some(d)) => Some(end.min(d)),
            (_, d) => d,
        }
    }

    /// The timer fired: at the end of the grace period every process gets
    /// SIGKILL; at the end of a share only the process being stopped does, and
    /// the next one gets its stop signal.
    fn expire(&mut self) {
        if self.deadline.is_some_and(|d| Instant::now() >= d) {
            sys::kill_all();
            self.deadline = None;
            self.stopping = None;
            self.queue.clear();
            return;
        }
        if let Some((i, _)) = self.stopping.take()
            && self.status[i].is_none()
        {
            sys::signal(self.procs[i].pid, SIGKILL);
        }
        self.advance();
    }

    /// The processes still queued to stop that are running.
    fn left(&self) -> usize {
        self.queue.iter().filter(|&&i| self.status[i].is_none()).count()
    }

    /// Sends the next stop signal once the process last stopped has exited
    /// (or was killed when its share ended).
    fn advance(&mut self) {
        if self.ending.is_none() {
            return;
        }
        if self.stopping.is_some_and(|(i, _)| self.status[i].is_none()) {
            return;
        }
        self.stopping = None;
        let Some(deadline) = self.deadline else {
            return;
        };
        while let Some(i) = self.queue.pop_front() {
            if self.status[i].is_none() {
                sys::signal(self.procs[i].pid, self.procs[i].stop_signal);
                let now = Instant::now();
                self.stopping = Some((i, share(now, deadline, self.left() + 1)));
                return;
            }
        }
        // Nothing left to stop gracefully: what remains (nothing supervised,
        // only processes they left behind) is killed below.
    }
}

/// The end of one process's share when `left` processes (it included) split
/// what remains of the grace period until `deadline` evenly.
fn share(now: Instant, deadline: Instant, left: usize) -> Instant {
    let left = u32::try_from(left.max(1)).unwrap_or(u32::MAX);
    now + deadline.saturating_duration_since(now) / left
}

fn exited(status: WaitStatus) -> Exited {
    match (status.exit_status(), status.terminating_signal()) {
        (Some(code), _) => Exited::main(false, code),
        (None, Some(sig)) => Exited::main(true, sig),
        (None, None) => unreachable!("waitpid without WUNTRACED reports exits and kills only"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_grace_period_is_split_evenly_between_the_processes_left() {
        let now = Instant::now();
        let deadline = now + Duration::from_secs(9);
        assert_eq!(share(now, deadline, 1), deadline);
        assert_eq!(share(now, deadline, 3), now + Duration::from_secs(3));
        assert_eq!(share(now, deadline, 0), deadline);
        // A deadline already passed leaves no share.
        assert_eq!(share(deadline, now, 2), deadline);
    }
}
```

- [ ] **Step 4: Modify `crates/kiln-init/src/linux/mod.rs`**

`Config`'s arrival starts the readiness clock.

In `crates/kiln-init/src/linux/mod.rs`, replace:
```rust
    let config = control::connect(events.clone())?;
    enter(3)?;
```
with:
```rust
    let config = control::connect(events.clone())?;
    let config_at = std::time::Instant::now();
    enter(3)?;
```

In `crates/kiln-init/src/linux/mod.rs`, replace:
```rust
    let started = process::start(&config, events)?;
```
with:
```rust
    let started = process::start(&config, config_at, events)?;
```

- [ ] **Step 5: Lint and test the guest on Linux**

In the Lima VM (`limactl shell vmkit`, then `cd` to the repository and `export CARGO_TARGET_DIR=$HOME/target-m3d`):

Run (Linux):
```bash
cargo clippy --all-targets -- -D warnings && cargo test -q -p kiln-init
cargo clippy --release --target $(uname -m)-unknown-linux-musl -p kiln-init -- -D warnings
```
Expected: no clippy warnings; kiln-init's 20 tests pass, among them `supervise::tests::the_grace_period_is_split_evenly_between_the_processes_left`.

- [ ] **Step 6: Format, lint and run the whole workspace**

Run:
```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test --all -q
```
Expected: no clippy warnings; every test passes (410 tests).

- [ ] **Step 7: Lint and test on Linux**

In the Lima VM (`limactl shell vmkit`, then `cd` to the repository and `export CARGO_TARGET_DIR=$HOME/target-m3d`):

Run (Linux):
```bash
cargo clippy --all-targets -- -D warnings && cargo test --all -q
```
Expected: no clippy warnings; every test passes (513 tests; the boot and `kiln run` cases skip themselves without their environment).

- [ ] **Step 8: Commit**

```bash
git add -A crates
git commit -m 'feat(kiln-init): start services before the main process; stop everything in reverse order, splitting the grace period'
```

### Task 7: Boot tests: services, the split grace period and mixed protocol versions

**Files:**
- Modify: `crates/kiln-run/src/linux/session.rs`, `crates/kiln-run/tests/boot.rs`, `crates/kiln-run/tests/support/mod.rs`, `crates/kiln-testguest/src/main.rs`

**Interfaces:**
- Consumes: Tasks 1–6; M3b-1's boot harness (`Case`, `Session`, `Reply`, `fixtures`).
- Produces:
  - `support::Env::init_v1` (`KILN_TEST_INIT_V1`, optional) and `Fixtures::init_v1` (an init layer around it).
  - `hostile listen-unix <path>` in kiln-testguest (a Unix-socket service).
  - Seven boot cases on both VMMs. A raw reply (`Reply::Raw`, tests of the guest's T9) skips the host's version refusal.

Boot cases, on both VMMs, each printing marks with the guest's uptime so the order is visible:
- `services_start_in_order_and_stop_in_reverse`: a Unix-socket service and a TCP one with its own user, env and working directory; the main process last; stopped in reverse when the main process exits; the main process's status.
- `a_service_exiting_first_ends_the_run`: its status and name in `Exited`; the main process stopped before the other service.
- `shutdown_stops_the_services_in_reverse_order`: potter's PHP shape (nginx main, php-fpm ready on its socket and stopped with SIGQUIT) plus a service that ignores SIGTERM, stopped last and killed when the grace ends.
- `a_stuck_main_process_does_not_cost_a_service_its_stop_signal` (ruling 3): nginx ignores SIGTERM; after its share (2 s of 4) it is killed and php-fpm logs its SIGQUIT; the status is nginx's SIGKILL (137).
- `a_service_that_is_not_ready_fails_the_boot`: never ready (`tcp:9999`, before the host's boot timeout), exits first, not executable (127).
- `services_in_tty_mode_write_to_the_terminal`.
- `a_version_1_guest_and_services` (P4, skipped without `KILN_TEST_INIT_V1`): a version 1 `kiln-init` runs a run without services, is refused one with services before `Config` (125), and refuses a version 2 `Config` sent raw (`InitFailed` at stage 2).

- [ ] **Step 1: Modify `crates/kiln-testguest/src/main.rs`**

A Unix-socket listener for socket readiness.

In `crates/kiln-testguest/src/main.rs`, replace:
```rust
//! The host must end the VM in every case.
#![forbid(unsafe_code)]
```
with:
```rust
//! The host must end the VM in every case.
//!
//! - `hostile listen-unix <path>`: listens on a Unix stream socket at `<path>`
//!   and accepts connections forever (a service with `ready: unix:<path>`).
#![forbid(unsafe_code)]
```

In `crates/kiln-testguest/src/main.rs`, replace:
```rust

    let hello = GuestMessage::Hello(Hello {
```
with:
```rust

    let args: Vec<String> = std::env::args().collect();
    if let [_, mode, path] = args.as_slice()
        && mode == "listen-unix"
    {
        let listener = std::os::unix::net::UnixListener::bind(path).expect("bind the socket");
        println!("listening on {path}");
        for conn in listener.incoming() {
            drop(conn);
        }
        return;
    }
    let hello = GuestMessage::Hello(Hello {
```

- [ ] **Step 2: Modify `crates/kiln-run/tests/support/mod.rs`**

The optional version 1 kiln-init.

In `crates/kiln-run/tests/support/mod.rs`, replace:
```rust
//!   KILN_TEST_HOSTILE=<static `hostile` from kiln-testguest>   KILN_TEST_BUSYBOX=<static busybox, default /bin/busybox>
//! Without the kernel and init each test is skipped, unless KILN_REQUIRE_KVM_TESTS=1.
```
with:
```rust
//!   KILN_TEST_HOSTILE=<static `hostile` from kiln-testguest>   KILN_TEST_BUSYBOX=<static busybox, default /bin/busybox>
//!   KILN_TEST_INIT_V1=<a static kiln-init speaking protocol version 1> (optional: the mixed-install tests)
//! Without the kernel and init each test is skipped, unless KILN_REQUIRE_KVM_TESTS=1.
```

In `crates/kiln-run/tests/support/mod.rs`, replace:
```rust
    pub busybox: PathBuf,
}
```
with:
```rust
    pub busybox: PathBuf,
    pub init_v1: Option<PathBuf>,
}
```

In `crates/kiln-run/tests/support/mod.rs`, replace:
```rust
                busybox: get("KILN_TEST_BUSYBOX").unwrap_or_else(|| "/bin/busybox".into()),
            }),
```
with:
```rust
                busybox: get("KILN_TEST_BUSYBOX").unwrap_or_else(|| "/bin/busybox".into()),
                init_v1: get("KILN_TEST_INIT_V1"),
            }),
```

In `crates/kiln-run/tests/support/mod.rs`, replace:
```rust
    pub proc_link: Vec<PathBuf>,
}
```
with:
```rust
    pub proc_link: Vec<PathBuf>,
    /// An init layer around a protocol version 1 kiln-init, when one was given.
    pub init_v1: Option<PathBuf>,
}
```

In `crates/kiln-run/tests/support/mod.rs`, replace:
```rust
        });
        let link = TarBuilder::new().symlink("proc", "/dev", &Opts::default()).finish();
```
with:
```rust
        });
        let init_v1 = env.init_v1.as_ref().map(|path| {
            let bin = std::fs::read(path).expect("read KILN_TEST_INIT_V1");
            let p = dir.join("init-v1.erofs");
            std::fs::write(&p, init_layer(&bin, &dir).unwrap()).unwrap();
            p
        });
        let link = TarBuilder::new().symlink("proc", "/dev", &Opts::default()).finish();
```

In `crates/kiln-run/tests/support/mod.rs`, replace:
```rust
            hostile_init,
            proc_link: convert(&store, &dir, "proc-link", vec![base.clone(), link]),
```
with:
```rust
            hostile_init,
            init_v1,
            proc_link: convert(&store, &dir, "proc-link", vec![base.clone(), link]),
```

- [ ] **Step 3: Modify `crates/kiln-run/src/linux/session.rs`**

A raw reply is the test's own bytes: no version refusal.

In `crates/kiln-run/src/linux/session.rs`, replace:
```rust
                // Only what the guest's version defines is sent (spec §9.8).
                if let Some(why) = version_refusal(proto.version(), config) {
                    return violation(why);
                }
                let sent = match &reply {
                    None => {
```
with:
```rust
                let sent = match &reply {
                    None => {
                        // Only what the guest's version defines is sent (spec §9.8).
                        if let Some(why) = version_refusal(proto.version(), config) {
                            return violation(why);
                        }
```

- [ ] **Step 4: Modify `crates/kiln-run/tests/boot.rs`**

The seven cases.

In `crates/kiln-run/tests/boot.rs`, replace:
```rust
    EXIT_CANNOT_INVOKE, EXIT_INFRA, EXIT_NOT_FOUND, HostMessage, MAX_FRAME, Network, Shutdown, Signal, WindowSize,
```
with:
```rust
    EXIT_CANNOT_INVOKE, EXIT_INFRA, EXIT_NOT_FOUND, HostMessage, MAX_FRAME, Network, Ready, Service, Shutdown, Signal,
    WindowSize,
```

In `crates/kiln-run/tests/boot.rs`, replace:
```rust

macro_rules! boot_tests {
```
with:
```rust

/// A service for `Config.services`: `sh -c <script>`.
fn svc(name: &str, script: &str, ready: Ready, stop_signal: i32) -> Service {
    Service {
        name: name.into(),
        process: kiln_proto::Process {
            cmd: vec!["/bin/sh".into(), "-c".into(), script.into()],
            ..kiln_proto::Process::default()
        },
        ready,
        stop_signal,
    }
}

/// `config` with `services`; the session sets the real readiness deadline.
fn with_services(mut config: kiln_proto::Config, services: Vec<Service>) -> kiln_proto::Config {
    config.services = services;
    config.ready_timeout_ms = Some(1);
    config
}

/// The layers with the `hostile` program (its `listen-unix` mode is a socket service).
fn hostile_layers() -> Option<&'static Vec<std::path::PathBuf>> {
    let layers = fixtures().hostile.as_ref();
    assert!(
        layers.is_some() || !support::require(),
        "KILN_REQUIRE_KVM_TESTS=1 but KILN_TEST_HOSTILE is unset"
    );
    layers
}

/// The lines of `out` starting with one of `marks`, in order, without what follows the mark.
fn marks(out: &str, marks: &[&str]) -> Vec<String> {
    out.lines()
        .filter_map(|l| {
            marks
                .iter()
                .find(|m| l.split(' ').next() == Some(**m))
                .map(|m| m.to_string())
        })
        .collect()
}

/// The guest's uptime that a line starting with `mark` carries after it.
fn uptime(out: &str, mark: &str) -> f64 {
    out.lines()
        .find_map(|l| l.strip_prefix(&format!("{mark} ")))
        .and_then(|rest| rest.split(' ').next()?.parse().ok())
        .unwrap_or_else(|| panic!("no {mark} line with an uptime in {out:?}"))
}

/// Prints `<mark> <uptime>`.
const T: &str = r#"$(cut -d" " -f1 /proc/uptime)"#;

/// Spec §9.8 `services`: each service starts in order and is ready (here a Unix
/// socket, then a TCP port) before the next one, with its own user, environment
/// and working directory; the main process starts last. When the main process
/// exits, the services are stopped in reverse order, each with its stop signal,
/// and the run's status is the main process's.
fn services_start_in_order_and_stop_in_reverse(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    let Some(layers) = hostile_layers() else { return };
    let a = svc(
        "a",
        &format!(
            "trap 'echo \"a-stop {T}\"; kill $L; exit 0' TERM; echo a-start; \
             /bin/hostile listen-unix /tmp/a.sock & L=$!; wait"
        ),
        Ready::Unix("/tmp/a.sock".into()),
        15,
    );
    let mut b = svc(
        "b",
        &format!(
            "trap 'echo \"b-stop {T}\"; kill $L; exit 0' USR1; [ -S /tmp/a.sock ] && echo b-sees-a; \
             echo \"b-id $(id -u) $FOO $(pwd)\"; nc -ll -p 9000 -e /bin/cat & L=$!; wait"
        ),
        Ready::Tcp(9000),
        10,
    );
    b.process.user = Some("app".into());
    b.process.env = vec!["FOO=bar".into()];
    b.process.working_dir = Some("/srv".into());
    let main = c.config(
        layers,
        &["/bin/sh", "-c", &format!("echo \"main-up {T}\"; sleep 0.3; exit 3")],
    );
    let o = c.run(layers, with_services(main, vec![a, b]));
    ended_cleanly(&c, &o);
    let out = o.stdout();
    assert_eq!(
        marks(&out, &["a-start", "b-sees-a", "b-id", "main-up", "b-stop", "a-stop"]),
        ["a-start", "b-sees-a", "b-id", "main-up", "b-stop", "a-stop"],
        "{out}\nconsole:\n{}",
        c.tail()
    );
    assert!(out.contains("b-id 1000 bar /srv\n"), "{out}");
    let (up, b_stop, a_stop) = (uptime(&out, "main-up"), uptime(&out, "b-stop"), uptime(&out, "a-stop"));
    assert!(up <= b_stop && b_stop <= a_stop, "{out}");
    assert_eq!(o.exit_code(), 3);
    assert_eq!(o.exited().unwrap().service, None);
}

/// The first process to exit ends the run, even a service: the others are
/// stopped in reverse start order (the main process first), and the service's
/// status is the run's, named in `Exited`.
fn a_service_exiting_first_ends_the_run(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    let a = svc(
        "a",
        &format!("trap 'echo \"a-stop {T}\"; exit 0' TERM; sleep 1000 & wait"),
        Ready::None,
        15,
    );
    let b = svc("b", &format!("sleep 1; echo \"b-exit {T}\"; exit 42"), Ready::None, 15);
    let main = sh(
        &c,
        &format!("trap 'echo \"main-stop {T}\"; exit 0' TERM; echo main-up; sleep 1000 & wait"),
    );
    let o = c.run(&fixtures().base, with_services(main, vec![a, b]));
    ended_cleanly(&c, &o);
    let out = o.stdout();
    assert_eq!(
        marks(&out, &["b-exit", "main-stop", "a-stop"]),
        ["b-exit", "main-stop", "a-stop"],
        "{out}\nconsole:\n{}",
        c.tail()
    );
    assert!(uptime(&out, "main-stop") <= uptime(&out, "a-stop"), "{out}");
    assert_eq!(o.exit_code(), 42);
    assert_eq!(o.exited().unwrap().service.as_deref(), Some("b"));
}

/// potter's PHP web image (potter spec §6.1.1): php-fpm a service ready on its
/// socket and stopped with SIGQUIT, nginx the main process. `Shutdown` stops
/// everything in reverse order (nginx first), each with its own stop signal; a
/// service that ignores its signal, stopped last, is killed when the grace ends;
/// the status is the main process's.
fn shutdown_stops_the_services_in_reverse_order(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    let Some(layers) = hostile_layers() else { return };
    let stubborn = svc("stubborn", "trap '' TERM; sleep 1000 & wait", Ready::None, 15);
    let fpm = svc(
        "php-fpm",
        &format!(
            "trap 'echo \"fpm-quit {T}\"; kill $L; exit 0' QUIT; \
             /bin/hostile listen-unix /tmp/php-fpm.sock & L=$!; wait"
        ),
        Ready::Unix("/tmp/php-fpm.sock".into()),
        3,
    );
    let nginx = c.config(
        layers,
        &[
            "/bin/sh",
            "-c",
            &format!(
                "trap 'echo \"nginx-stop {T}\"; exit 7' TERM; [ -S /tmp/php-fpm.sock ] && echo nginx-up; \
                 sleep 1000 & wait"
            ),
        ],
    );
    let mut s = c.start(
        layers,
        with_services(nginx, vec![stubborn, fpm]),
        Reply::Config,
        Vec::new(),
    );
    assert!(s.wait_output("nginx-up", BOOT), "console:\n{}", c.tail());
    let sent = std::time::Instant::now();
    s.send(&HostMessage::Shutdown(Shutdown { grace_secs: 3 }))
        .expect("send to the guest");
    let o = s.finish(END);
    ended_cleanly(&c, &o);
    let out = o.stdout();
    assert_eq!(
        marks(&out, &["nginx-up", "nginx-stop", "fpm-quit"]),
        ["nginx-up", "nginx-stop", "fpm-quit"],
        "{out}\nconsole:\n{}",
        c.tail()
    );
    assert!(uptime(&out, "nginx-stop") <= uptime(&out, "fpm-quit"), "{out}");
    assert_eq!(o.exit_code(), 7);
    assert_eq!(o.exited().unwrap().service, None);
    // The stubborn service held the run until the grace ended.
    assert!(sent.elapsed() >= Duration::from_secs(3), "{:?}", sent.elapsed());
}

/// The grace period is split between the processes still to stop: a main process
/// that ignores its stop signal is killed when its share (half of it here) ends,
/// and php-fpm still gets its SIGQUIT instead of SIGKILL when the grace ends.
fn a_stuck_main_process_does_not_cost_a_service_its_stop_signal(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    let Some(layers) = hostile_layers() else { return };
    let fpm = svc(
        "php-fpm",
        &format!(
            "trap 'echo \"fpm-quit {T}\"; kill $L; exit 0' QUIT; \
             /bin/hostile listen-unix /tmp/php-fpm.sock & L=$!; wait"
        ),
        Ready::Unix("/tmp/php-fpm.sock".into()),
        3,
    );
    let nginx = c.config(
        layers,
        &[
            "/bin/sh",
            "-c",
            "trap '' TERM; [ -S /tmp/php-fpm.sock ] && echo nginx-up; sleep 1000 & wait",
        ],
    );
    let mut s = c.start(layers, with_services(nginx, vec![fpm]), Reply::Config, Vec::new());
    assert!(s.wait_output("nginx-up", BOOT), "console:\n{}", c.tail());
    let sent = std::time::Instant::now();
    s.send(&HostMessage::Shutdown(Shutdown { grace_secs: 4 }))
        .expect("send to the guest");
    let o = s.finish(END);
    ended_cleanly(&c, &o);
    let out = o.stdout();
    assert_eq!(
        marks(&out, &["nginx-up", "fpm-quit"]),
        ["nginx-up", "fpm-quit"],
        "{out}\nconsole:\n{}",
        c.tail()
    );
    // nginx held its share (2 s) and was killed: the status is its SIGKILL.
    assert!(sent.elapsed() >= Duration::from_secs(2), "{:?}", sent.elapsed());
    assert_eq!(o.exit_code(), 128 + 9);
    assert_eq!(o.exited().unwrap().service, None);
}

/// A service that never becomes ready, exits first, or cannot be executed stops
/// the boot at stage 6 with `InitFailed` naming it; the main process never runs.
fn a_service_that_is_not_ready_fails_the_boot(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    let main = || sh(&c, "echo should-not-run");
    // Long enough for a loaded host's slow boot to leave the readiness wait time.
    let boot_timeout = Duration::from_secs(30);
    let run = |config: kiln_proto::Config| {
        let spec = c.spec(&fixtures().base, &config, None);
        let vm = c.vmm.create(&spec).expect("create the VM");
        let mut opts = c.options();
        opts.boot_timeout = boot_timeout;
        support::driver::Session::start(vm, config, Reply::Config, Vec::new(), opts).finish(END)
    };
    let never = svc("never", "sleep 1000", Ready::Tcp(9999), 15);
    let o = run(with_services(main(), vec![never]));
    ended_cleanly(&c, &o);
    let f = o
        .init_failed()
        .unwrap_or_else(|| panic!("no InitFailed: {o:?}\n{}", c.tail()));
    assert_eq!(f.stage, 6, "{f:?}");
    assert!(f.message.contains("service never was not ready (tcp:9999)"), "{f:?}");
    // The guest's own deadline ended the boot (no violation above), before the host's.
    let ended = o.run.ended_after.expect("the VM ended");
    assert!(ended < boot_timeout, "{ended:?}");
    assert_eq!(o.exit_code(), EXIT_INFRA);
    assert!(!o.running() && o.stdout.is_empty(), "{o:?}");

    let dies = svc("dies", "exit 3", Ready::Unix("/run/x.sock".into()), 15);
    let o = run(with_services(main(), vec![dies]));
    ended_cleanly(&c, &o);
    let f = o.init_failed().expect("InitFailed");
    assert!(
        f.message.contains("service dies exited with 3 before it was ready"),
        "{f:?}"
    );
    assert_eq!(o.exit_code(), EXIT_INFRA);

    let mut missing = svc("missing", "", Ready::None, 15);
    missing.process.cmd = vec!["/nonexistent".into()];
    let o = run(with_services(main(), vec![missing]));
    ended_cleanly(&c, &o);
    let f = o.init_failed().expect("InitFailed");
    assert!(f.message.contains("service missing: exec /nonexistent"), "{f:?}");
    assert_eq!(o.exit_code(), EXIT_NOT_FOUND);
}

/// With `-t` the main process gets the terminal; services write to it too, as
/// neither its controlling terminal nor its input.
fn services_in_tty_mode_write_to_the_terminal(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    let side = svc(
        "side",
        "echo svc-line; [ -t 0 ] || echo svc-no-tty-input; sleep 1000",
        Ready::None,
        15,
    );
    let mut main = sh(&c, "sleep 0.5; echo main-line");
    main.tty = Some(WindowSize { rows: 24, cols: 80 });
    let o = c.run(&fixtures().base, with_services(main, vec![side]));
    ended_cleanly(&c, &o);
    let tty = o.tty();
    assert!(
        tty.contains("svc-line\r\n") && tty.contains("svc-no-tty-input\r\n") && tty.contains("main-line\r\n"),
        "{tty:?}"
    );
    assert_eq!(o.exit_code(), 0);
}

/// Mixed installs (spec §9.8): a version 1 kiln-init still runs a run without
/// services; a run with services is refused before `Config` (125, naming the
/// version); and a version 1 kiln-init refuses a version 2 `Config` (strict decoding).
fn a_version_1_guest_and_services(backend: Backend) {
    let Some(mut c) = Case::new(backend) else { return };
    let Some(v1) = fixtures().init_v1.clone() else {
        eprintln!("skipped: KILN_TEST_INIT_V1 is unset");
        return;
    };
    c.init = Some(v1);
    let o = c.run(&fixtures().base, sh(&c, "echo old; exit 4"));
    ended_cleanly(&c, &o);
    assert_eq!((o.exit_code(), o.stdout()), (4, "old\n".into()));

    let svc_config = with_services(sh(&c, "true"), vec![svc("s", "true", Ready::None, 15)]);
    let o = c.run(&fixtures().base, svc_config.clone());
    assert_eq!(o.exit_code(), EXIT_INFRA, "{o:?}");
    let why = o.violation.as_deref().unwrap_or_default();
    assert!(
        why.contains("need guest protocol version 2") && why.contains("speaks version 1"),
        "{why}"
    );
    assert!(!o.running() && o.init_failed().is_none(), "{o:?}");

    let mut raw = Vec::new();
    let mut sent = svc_config.clone();
    sent.ready_timeout_ms = Some(10_000);
    kiln_proto::write_message(&mut raw, &HostMessage::Config(Box::new(sent))).unwrap();
    let o = c
        .start(&fixtures().base, svc_config, Reply::Raw(raw), Vec::new())
        .finish(END);
    let f = o
        .init_failed()
        .unwrap_or_else(|| panic!("no InitFailed: {o:?}\n{}", c.tail()));
    assert_eq!(f.stage, 2, "{f:?}");
    assert!(f.message.contains("unknown field `services`"), "{f:?}");
}

macro_rules! boot_tests {
```

In `crates/kiln-run/tests/boot.rs`, replace:
```rust
    the_boot_timeout_kills_a_stuck_guest,
);
```
with:
```rust
    the_boot_timeout_kills_a_stuck_guest,
    services_start_in_order_and_stop_in_reverse,
    a_service_exiting_first_ends_the_run,
    shutdown_stops_the_services_in_reverse_order,
    a_stuck_main_process_does_not_cost_a_service_its_stop_signal,
    a_service_that_is_not_ready_fails_the_boot,
    services_in_tty_mode_write_to_the_terminal,
    a_version_1_guest_and_services,
);
```

- [ ] **Step 5: Format, lint and run the whole workspace**

Run:
```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test --all -q
```
Expected: no clippy warnings; every test passes (410 tests).

- [ ] **Step 6: Lint and test on Linux**

In the Lima VM (`limactl shell vmkit`, then `cd` to the repository and `export CARGO_TARGET_DIR=$HOME/target-m3d`):

Run (Linux):
```bash
cargo clippy --all-targets -- -D warnings && cargo test --all -q
```
Expected: no clippy warnings; every test passes (527 tests; the boot and `kiln run` cases skip themselves without their environment).

- [ ] **Step 7: Build a version 1 kiln-init for the mixed-version case**

In the Lima VM, from a worktree of `main` (9c891ff):

Run (Linux):
```bash
git worktree add --detach ../kiln-v1 9c891ff
(cd ../kiln-v1 && CARGO_TARGET_DIR=$HOME/target-m3d-v1 cargo build -q --release --target $(uname -m)-unknown-linux-musl -p kiln-init)
export KILN_TEST_INIT_V1=$HOME/target-m3d-v1/$(uname -m)-unknown-linux-musl/release/kiln-init
```
Expected: the build succeeds.

- [ ] **Step 8: Run the service cases on Firecracker**

In the Lima VM, in an `sg kvm` shell, with Firecracker and Cloud Hypervisor on `PATH`:

Run (Linux, KVM):
```bash
kernel=/Users/alfonso/Github/Personal/playground/vmkit/out/vmlinux-6.18.54-aarch64
scripts/boot-tests.sh $kernel -- --test-threads=2 firecracker::services firecracker::a_service \
  firecracker::shutdown_stops firecracker::a_stuck_main firecracker::a_version_1
```
Expected: the 7 service cases pass (`services_start_in_order_and_stop_in_reverse`, `a_service_exiting_first_ends_the_run`, `shutdown_stops_the_services_in_reverse_order`, `a_stuck_main_process_does_not_cost_a_service_its_stop_signal`, `a_service_that_is_not_ready_fails_the_boot`, `services_in_tty_mode_write_to_the_terminal`, `a_version_1_guest_and_services`); about 50 s in the Lima VM. Without `KILN_TEST_INIT_V1` the last one prints `skipped` and passes.

- [ ] **Step 9: Commit**

```bash
git add -A crates
git commit -m 'test(kiln-run): boot tests for services, the split grace period and mixed protocol versions'
```

### Task 8: `kiln run` end to end: busybox `httpd` as a service from the image's label

**Files:**
- Modify: `crates/kiln/tests/run.rs`

**Interfaces:**
- Consumes: Tasks 3–6 through the `kiln` binary; M3b-1's e2e fixture (`fixture()`, `busybox_layer`, `LayoutBuilder`).
- Produces: `services_from_the_image_label` in `crates/kiln/tests/run.rs`.

The whole path through the CLI: an OCI layout whose config carries `dev.kiln.services` (busybox `httpd` as `web`, ready on `tcp:8080`, and `quitter`, which exits 9 after `$QUIT_AFTER` seconds), converted with `kiln convert`, listed by `kiln inspect`, and run: the main process fetches the page from `127.0.0.1:8080`. Then `--env QUIT_AFTER=1` reaches the service, which ends the run with 9 and is named on stderr.

- [ ] **Step 1: Modify `crates/kiln/tests/run.rs`**

The case.

At the end of `crates/kiln/tests/run.rs`, append:
```rust

/// Spec §9.8 `services` end to end: an OCI image whose `dev.kiln.services` label
/// declares busybox httpd as a service ready on its TCP port; `kiln convert`
/// copies the services into the kiln config, `kiln inspect` lists them, and
/// `kiln run` starts httpd before the main process, which fetches a page from it
/// with nc. A service that exits first ends the run with its status, and kiln
/// names it.
#[test]
fn services_from_the_image_label() {
    let Some(f) = fixture() else { return };
    let label = r#"[
        {"name": "web", "process": {"entrypoint": ["/bin/busybox", "httpd"], "cmd": ["-f", "-p", "8080", "-h", "/www"]}, "ready": "tcp:8080"},
        {"name": "quitter", "process": {"cmd": ["/bin/sh", "-c", "sleep ${QUIT_AFTER:-1000}; exit 9"]}}
    ]"#;
    let mut layer = TarBuilder::new();
    layer
        .dir("www", &Opts::default().mode(0o755))
        .file("www/index.html", b"hello-from-httpd\n", &Opts::default());
    let src = tempfile::tempdir().unwrap();
    let path = src.path().join("web");
    let mut b = LayoutBuilder::new(&path);
    let cfg = ContainerConfig {
        cmd: Some(vec![
            "/bin/sh".into(),
            "-c".into(),
            r#"/bin/busybox printf 'GET / HTTP/1.0\r\n\r\n' | nc 127.0.0.1 8080"#.into(),
        ]),
        labels: Some([("dev.kiln.services".to_string(), label.to_string())].into()),
        ..Default::default()
    };
    let d = b.image(
        &Platform::host(),
        &[
            TestLayer::tar(busybox_layer(&f.env.busybox, "web")),
            TestLayer::tar(layer.finish()),
        ],
        cfg,
    );
    b.add(d, None).finish();
    let out = f.kiln().args(["convert", "--tag", "web"]).arg(&path).finish();
    assert!(out.status.success(), "{}", text(&out.stderr));
    let inspect = f.kiln().args(["inspect", "web"]).finish();
    let shown = text(&inspect.stdout);
    assert!(
        shown.contains("service:    web  ready tcp:8080  stop SIGTERM  /bin/busybox httpd -f -p 8080 -h /www"),
        "{shown}"
    );

    let o = f.output(&["web"]);
    assert_eq!(code(&o), 0, "{}", text(&o.stderr));
    let page = text(&o.stdout);
    assert!(page.contains("200 OK") && page.contains("hello-from-httpd"), "{page}");

    let o = f.output(&["--env", "QUIT_AFTER=1", "web", "--", "/bin/sh", "-c", "sleep 1000"]);
    assert_eq!(code(&o), 9, "{}", text(&o.stderr));
    assert!(
        text(&o.stderr).contains("kiln: service quitter exited with 9, which ended the run"),
        "{}",
        text(&o.stderr)
    );
}
```

- [ ] **Step 2: Run it on Firecracker**

In the Lima VM, in an `sg kvm` shell, with Firecracker and Cloud Hypervisor on `PATH`:

Run (Linux, KVM):
```bash
kernel=/Users/alfonso/Github/Personal/playground/vmkit/out/vmlinux-6.18.54-aarch64
KILN_TEST_VMM=firecracker scripts/boot-tests.sh $kernel services_from_the_image_label
```
Expected: `services_from_the_image_label` passes.

- [ ] **Step 3: Format, lint and run the whole workspace**

Run:
```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test --all -q
```
Expected: no clippy warnings; every test passes (410 tests).

- [ ] **Step 4: Commit**

```bash
git add -A crates
git commit -m 'test(kiln): kiln run of an image whose dev.kiln.services label declares busybox httpd'
```

### Task 9: `docs/format.md`: services, config schema version 2 and control protocol version 2

**Files:**
- Modify: `docs/format.md`

**Interfaces:**
- Consumes: Tasks 1–8.
- Produces: the normative description of the config's `services` and `schemaVersion` 2 (ruling 1), stages 6 and 7 with services (rulings 3, 4, 5), `Config.services`, `Config.readyTimeoutMs`, `Exited.service`, and the host's version rules (ruling 2).

`docs/format.md` is normative for §5.1, §7 and §9.5 (spec §5.1): it gains the services in the image config and the label, `schemaVersion` 2 and the reader's rule, the readiness contract (loopback or wildcard listeners, probes visible in logs), stage 7's split grace period, protocol version 2's fields and the version rules, among them the by-design refusal of a version 2 `kiln-init` by a version 1 host. Then the whole validation.

- [ ] **Step 1: Modify `docs/format.md`**

The image, the guest and the protocol.

In `docs/format.md`, replace:
```markdown
This document is normative. It defines the **kiln image** (schema version 1, provisional until M3 adds kernel and init layers), the **erofs profile** (format version 1), the **guest** that `kiln-init` sets up (devices, init layer, scratch disk, boot sequence) and the **control protocol** (version 1) between `kiln-init` and the host.

## kiln image, schema version 1
```
with:
```markdown
This document is normative. It defines the **kiln image** (schema versions 1 and 2, provisional until M3 adds kernel and init layers), the **erofs profile** (format version 1), the **guest** that `kiln-init` sets up (devices, init layer, scratch disk, boot sequence) and the **control protocol** (versions 1 and 2) between `kiln-init` and the host.

## kiln image, schema versions 1 and 2
```

In `docs/format.md`, replace:
```markdown
- `schemaVersion`: 1. Readers reject other values.
```
with:
```markdown
- `schemaVersion`: 2 when `services` is present, otherwise 1. Readers reject other values, and a config whose version does not match its `services` (version 1 with services, version 2 without).
```

In `docs/format.md`, replace:
```markdown
- There is no kernel command line field and no kiln version field.
```
with:
````markdown
- `services` (from M3d, omitted when empty): processes `kiln-init` starts before the main process (below). `kiln convert` takes them from the source OCI config's `dev.kiln.services` label.
- There is no kernel command line field and no kiln version field.

The config reader ignores fields it does not know, but a kiln from before M3d accepts only schema version 1: it refuses an image with services instead of running it without them.

### Services

The `dev.kiln.services` label of an OCI image config holds a JSON array; `kiln convert` parses it, checks it and stores the result as the kiln config's `services`, so the image digest covers it. The label is otherwise informational. A label that does not parse, has an unknown field anywhere, or breaks a rule below fails the conversion before any layer is converted. Each entry, and each entry of `services`:

```json
{ "name": "php-fpm", "process": { "cmd": ["-F"], "entrypoint": ["php-fpm"], "env": ["A=1"], "user": "root", "workingDir": "/app" }, "ready": "unix:/run/php-fpm.sock", "stopSignal": "SIGQUIT" }
```
- `name`: 1 to 32 of `[a-z0-9-]`, unique in the image. At most 16 services.
- `process`: as the main process's `process` without `stopSignal`: `entrypoint` and `cmd` (together a non-empty argv with a non-empty argv[0]), `env`, `workingDir` (absolute), `user`. Empty lists and unset strings are omitted. The service does not inherit the main process's environment.
- `ready`: `unix:<path>` (an absolute path of at most 107 bytes), `tcp:<port>` (1–65535, digits only) or `none`, the default (omitted).
- `stopSignal`: a signal name or number as for `STOPSIGNAL` (1–31; real-time signals are refused), default SIGTERM (omitted).
````

In `docs/format.md`, replace:
```markdown
| 6 | process | Starts the main process (below) and sends `Running`. |
| 7 | supervise | Reaps every process and handles host messages and signals until the main process exits (below). |
```
with:
```markdown
| 6 | process | Starts the services, each ready before the next, then the main process (below), and sends `Running`. |
| 7 | supervise | Reaps every process and handles host messages and signals until the run ends (below). |
```

In `docs/format.md`, replace:
```markdown
**Supervision** (stage 7):
- `Signal { sig }` is sent to the main process. `WindowSize` resizes the pseudo-terminal (ignored without `tty`).
- `Shutdown { graceSecs }`, or SIGINT or SIGTERM to init (with `Config.shutdownGraceSecs`): `stopSignal` is sent to the main process (once), and when the grace period ends every process but init gets SIGKILL. A later request can shorten the deadline, never extend it.
- When the main process exits, every other process gets SIGKILL and is reaped; then stdout and stderr (or the terminal) are relayed to EOF; then `Exited` is sent. The scratch filesystem is then synced and, on a best-effort basis, made read-only (EBUSY is ignored), and init ends the VM with `Config.exitMethod`.

**Failure:** a failing stage prints `kiln-init: <stage name>: <error>` on the console, sends `InitFailed` when the control connection is up, sends SIGKILL to every other process, and ends the VM with `Config.exitMethod`, or with a reboot before `Config` arrived. A panic does the same. A protocol error from the host, a second `Config`, or the host closing the control connection, is a failure too: it is detected by a reader thread once `Config` has arrived and acted on when init enters its next stage (and at the end of stage 6, before `Running`), or at once during stage 7; the failure names it rather than a send that failed because of it.

## Control protocol, version 1
```
with:
```markdown
**Services** (stage 6, protocol version 2): before the main process, each of `Config.services` is started in order:
- Its user, groups, environment and working directory follow the main process's rules above, from its own `process` (without `TERM`). It runs in a new session, with stdin `/dev/null`.
- It shares the main process's stdout and stderr pipes. With `tty`, its stdout and stderr are the pseudo-terminal, which is not its controlling terminal. The pipes are owned by the main process's user; when a service runs as another uid, they are made writable by everyone (mode 0622), so that it too can reopen `/dev/stdout`.
- It is ready when a connection to its Unix stream socket, or to `127.0.0.1:<port>`, is accepted (init connects and closes at once, retrying every 5 to 50 ms), or at once with `none`. The next service, or the main process, starts only then. A `tcp:` service must listen on the loopback address or the wildcard address (`0.0.0.0`); one listening only on `::1` or on the network's address is never ready. The probes are real connections, closed without a byte sent, and may show up in the service's log.
- `Config.readyTimeoutMs`, counted from `Config`'s arrival, bounds every service's readiness. A service not ready by then, a service exiting before the main process starts, or one that cannot be started fails stage 6 with a message naming it; an exec failure keeps its errno, as the main process's does.

**Supervision** (stage 7):
- `Signal { sig }` is sent to the main process. `WindowSize` resizes the pseudo-terminal (ignored without `tty`).
- The run ends with the first of its processes (services and the main process) to exit, or with `Shutdown { graceSecs }`, or SIGINT or SIGTERM to init (with `Config.shutdownGraceSecs`). The processes still running are then stopped one at a time in reverse start order, so the main process first: each gets its stop signal (the main process `stopSignal`, a service its own). The grace period (`graceSecs`, or `Config.shutdownGraceSecs` when a process's exit ended the run) is split evenly between the processes still to stop: when one is sent its stop signal, its share is what remains of the grace period divided by the number of processes still running that have not yet been sent theirs, itself included. The next one gets its stop signal once that one has exited, or, if it has not exited when its share ends, once it has been sent SIGKILL. So a process that ignores its stop signal costs the ones after it only its share, never their stop signals. When the grace period ends, every process but init gets SIGKILL. A later request can shorten the deadline, never extend it (the current share then ends no later than its share of what remains). Without services this is: `stopSignal` to the main process once, and SIGKILL to everything at the deadline.
- When every one of them has exited, every other process gets SIGKILL and is reaped; then stdout and stderr (or the terminal) are relayed to EOF; then `Exited` is sent, with the status of the process whose exit ended the run (and its name in `service` when it was a service), or after a shutdown request the main process's. The scratch filesystem is then synced and, on a best-effort basis, made read-only (EBUSY is ignored), and init ends the VM with `Config.exitMethod`.

**Failure:** a failing stage prints `kiln-init: <stage name>: <error>` on the console, sends `InitFailed` when the control connection is up, sends SIGKILL to every other process, and ends the VM with `Config.exitMethod`, or with a reboot before `Config` arrived. A panic does the same. A protocol error from the host, a second `Config`, or the host closing the control connection, is a failure too: it is detected by a reader thread once `Config` has arrived and acted on when init enters its next stage (and at the end of stage 6, before `Running`), or at once during stage 7; the failure names it rather than a send that failed because of it.

## Control protocol

This section defines versions 1 and 2. Version 2 adds `Config.services`, `Config.readyTimeoutMs` and `Exited.service` and nothing else; a version adds message types, optional fields and values, and never changes what an earlier version defines (spec §9.8).
```

In `docs/format.md`, replace:
```markdown
| 1 | `Hello` | guest → host | `{"protocol": 1}`: the protocol version, at least 1. |
| 2 | `Config` | host → guest | The run's configuration (below). |
| 3 | `Stage` | guest → host | `{"n": 3}`: init entered stage `n` (1–7). Sent for stages 3 to 7; stages 1 and 2 precede it. |
| 4 | `Running` | guest → host | `{}`: the main process started. |
| 5 | `Exited` | guest → host | `{"signaled": false, "code": 0}`: the main process ended, and its output reached EOF. `code` is the exit status (0–255), or with `signaled` the signal that killed it (1–64). |
| 6 | `InitFailed` | guest → host | `{"stage": 6, "errno": 2, "message": "…"}`: init failed at `stage` (1–7). `errno` (1–4095) is present at stage 6 only when executing the entrypoint failed; it may be present at other stages. `message` has at most 4096 bytes and is untrusted text. |
```
with:
```markdown
| 1 | `Hello` | guest → host | `{"protocol": 2}`: the highest protocol version the guest implements, at least 1. |
| 2 | `Config` | host → guest | The run's configuration (below). |
| 3 | `Stage` | guest → host | `{"n": 3}`: init entered stage `n` (1–7). Sent for stages 3 to 7; stages 1 and 2 precede it. |
| 4 | `Running` | guest → host | `{}`: the main process started (after every service was ready). |
| 5 | `Exited` | guest → host | `{"signaled": false, "code": 0}`: the run ended, and its output reached EOF. `code` is the exit status (0–255), or with `signaled` the signal that killed it (1–64), of the main process, or (version 2) of the service named in the optional `service` (`[a-z0-9-]{1,32}`), whose exit ended the run. |
| 6 | `InitFailed` | guest → host | `{"stage": 6, "errno": 2, "message": "…"}`: init failed at `stage` (1–7). `errno` (1–4095) is present at stage 6 only when executing the entrypoint (the main process's, or a service's) failed; it may be present at other stages. `message` has at most 4096 bytes and is untrusted text. |
```

In `docs/format.md`, replace:
```markdown
| `shutdownGraceSecs` | integer | The grace period for SIGINT or SIGTERM to init. |
```
with:
```markdown
| `shutdownGraceSecs` | integer | The grace period for SIGINT or SIGTERM to init, and (version 2) for stopping the other processes when one exits. |
| `services` | list, optional (version 2) | The services, as in the kiln config (above) with these differences: `process.env` is the final environment (the image's, then the run's overrides, which the host adds to every service too); `ready` is always present; `stopSignal` is an integer 1–31. At most 16, unique names. Omitted when empty, so a `Config` without services is a version 1 `Config`. |
| `readyTimeoutMs` | integer, optional (version 2) | 1–4294967295; present exactly when `services` is. How long, from `Config`'s arrival, the guest allows for every service to be ready. The host sets it when it sends `Config`, to what remains of its boot timeout less 2 seconds, so that the guest reports a service that is never ready (`InitFailed`) before the host's boot timeout ends the VM. |
```

In `docs/format.md`, replace:
```markdown
1. The guest connects port 1024 and sends `Hello`. The host answers the first `Hello` with `Config`, once per VM.
2. The guest sends `Stage` for stages 3 to 7, `Running` once the main process has started, then `Exited`, or `InitFailed` at any point. The host sends `Shutdown`, `Signal` and `WindowSize` at any time after `Config`.
```
with:
```markdown
1. The guest connects port 1024 and sends `Hello`. The host answers the first `Hello` with `Config`, once per VM.
   - **Versions.** A host supports every version from 1 to its own (2). A `Hello` naming a higher version is a violation, so a version 1 host refuses a version 2 `kiln-init`; that is by design, since `kiln run` boots the `kiln-init` pinned to it (spec §8.1) and the two ship together: only a `kiln-init` named with `--init` can be of another release. The host sends only what the guest's version defines: when the run needs more (services need version 2), the host sends no `Config`, kills the VM and reports 125 with an error naming the version required. A guest may use only what its version defines: `Exited.service` from a version 1 guest, or naming a service the run does not have, is a violation. The guest decodes strictly, so a version 1 `kiln-init` refuses a version 2 field (`InitFailed` at stage 2).
2. The guest sends `Stage` for stages 3 to 7, `Running` once the main process has started, then `Exited`, or `InitFailed` at any point. The host sends `Shutdown`, `Signal` and `WindowSize` at any time after `Config`.
```

In `docs/format.md`, replace:
```markdown
- after `Exited`: `code`, or `128 + code` when `signaled`;
- after `InitFailed` at stage 6: 127 when `errno` is 2 (ENOENT: the entrypoint was not found), 126 when `errno` is 13 or 21 (EACCES, EISDIR: found but not invokable);
```
with:
```markdown
- after `Exited`: `code`, or `128 + code` when `signaled` (a service's when `service` is set; `kiln run` then says which service ended the run);
- after `InitFailed` at stage 6: 127 when `errno` is 2 (ENOENT: the entrypoint, the main process's or a service's, was not found), 126 when `errno` is 13 or 21 (EACCES, EISDIR: found but not invokable);
```

- [ ] **Step 2: Format, lint and run the whole workspace**

Run:
```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test --all -q
```
Expected: no clippy warnings; every test passes (410 tests).

- [ ] **Step 3: Lint and test on Linux**

In the Lima VM (`limactl shell vmkit`, then `cd` to the repository and `export CARGO_TARGET_DIR=$HOME/target-m3d`):

Run (Linux):
```bash
cargo clippy --all-targets -- -D warnings && cargo test --all -q
```
Expected: no clippy warnings; every test passes (528 tests; the boot and `kiln run` cases skip themselves without their environment).

- [ ] **Step 4: Build the guests for both architectures, as the `musl` job does**

Run (Linux):
```bash
for t in aarch64-unknown-linux-musl x86_64-unknown-linux-musl; do
  cargo clippy --release --target $t -p kiln-init -p kiln-testguest -- -D warnings
  CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld cargo build --release --target $t -p kiln-init -p kiln-testguest
  file "$CARGO_TARGET_DIR/$t/release/kiln-init" "$CARGO_TARGET_DIR/$t/release/hostile"
done
```
Expected: no clippy warnings; aarch64 `statically linked`, x86_64 `static-pie linked`.

- [ ] **Step 5: Run the boot suite and `kiln run` end to end on Firecracker**

In the Lima VM, in an `sg kvm` shell, with Firecracker and Cloud Hypervisor on `PATH`:

Run (Linux, KVM):
```bash
kernel=/Users/alfonso/Github/Personal/playground/vmkit/out/vmlinux-6.18.54-aarch64
KILN_TEST_NET=1 scripts/boot-tests.sh $kernel firecracker:: -- --test-threads=4
KILN_TEST_NET=1 KILN_TEST_VMM=firecracker scripts/boot-tests.sh $kernel -- --skip firecracker:: --skip cloud_hypervisor:: --test-threads=4
```
With `KILN_TEST_INIT_V1` still exported (Task 7). Expected: the boot suite 30 passed (M3b-1's 23 and the 7 service cases; the 30 Cloud Hypervisor cases filtered out); `kiln run` end to end 12 passed (M3b-1's 11 and `services_from_the_image_label`). About 70 s and 55 s in the Lima VM.

- [ ] **Step 6: Run them on Cloud Hypervisor**

Run (Linux, KVM):
```bash
KILN_TEST_NET=1 scripts/boot-tests.sh $kernel cloud_hypervisor:: -- --test-threads=4
KILN_TEST_NET=1 KILN_TEST_VMM=cloud-hypervisor scripts/boot-tests.sh $kernel -- --skip firecracker:: --skip cloud_hypervisor:: --test-threads=4
```
Expected: Cloud Hypervisor 53 under nested virtualization stalls guests, so it is judged by pass rate: rerun what fails serially (`scripts/boot-tests.sh $kernel <full test name> -- --exact --test-threads=1`, with `KILN_TEST_VMM=cloud-hypervisor` for the `kiln run` cases) and accept failures that are stalls (a boot timeout, a VM that does not end after its final message, output that never comes), never a wrong result. Validation got 10/30 boot cases and 10/12 `kiln run` cases in the full run, every failure a stall (Global Constraints, Validation).

- [ ] **Step 7: Commit**

```bash
git add -A docs/format.md
git commit -m 'docs(format): services, config schema version 2 and control protocol version 2'
```

---

## Spec coverage (M3d)

| Spec item | Task |
|---|---|
| §9.8 `services`: `Config.services: [Service{name, process, ready, stopSignal}]`, `ready` = `unix:<path>` \| `tcp:<port>` \| `none`; the shape of a service's `process` (designed here: the main process's without `stopSignal`, unknown fields refused) | 1 |
| §9.8 versioning rule 1: version 2 only adds (`services`, `readyTimeoutMs`, `Exited.service`); a `Config` without services is a version 1 `Config` | 1 |
| §9.8 rules 2 and 3: the host supports versions 1 to 2, refuses a higher one, and refuses before `Config` a run that needs version 2 from a version 1 guest (125, naming the version); a guest uses only what its version defines (`Exited.service`) | 2 |
| §9.8 readiness timeouts (designed here: `readyTimeoutMs` from the host's remaining boot timeout less 2 s) | 1, 2 |
| §5.1 the config's `services`, from the `dev.kiln.services` label; `schemaVersion` 2 with services (ruling 1); potter's route through `convert_resolved` | 3, 4 |
| §9.8 whether `kiln run` exposes services (designed here: from the image only; `kiln inspect` lists them, `--env` reaches them, `kiln run` names a service that ended the run) | 4, 5 |
| §9.6 stage 6: services start in order, each ready before the next, with their own user, groups, environment and working directory, sharing stdout and stderr; the main process last | 6 |
| §9.6 stage 7 and §9.8: the first exit ends the run; the rest stop in reverse order with their stop signals within the grace period, split per process (ruling 3); host signals to the main process | 6 |
| §9.8 how `Exited` reports a service that exited first (designed here: `Exited.service`, the status the service's) | 1, 6 |
| §9.8 rule 4: a version 1 `kiln-init` refuses a version 2 `Config` (strict decoding) | 7 |
| §11 boot suite on both VMMs: services, potter's PHP shape, a stuck main process, readiness failures, `-t`, mixed versions | 7 |
| `kiln run` end to end with services from the label | 8 |
| §9.5 and `docs/format.md`: protocol version 2, the config's `services` and `schemaVersion` 2, stages 6 and 7, rulings 2, 4 and 5 | 9 |

**Left to later milestones:** build mode (M3e, protocol version 3) and the shim's exec, stats and file updates (M4, §9.8). potter P2 can start on Tasks 3 and 4: it sets `dev.kiln.services` on the config of the `ResolvedImage` it builds and calls `kiln_image::convert_resolved` unchanged.
