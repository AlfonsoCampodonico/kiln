# kiln-shim exec and volumes — Plan

**Design:** [2026-10-30-kiln-shim-exec-volumes-design.md](../specs/2026-10-30-kiln-shim-exec-volumes-design.md)
**Branch:** `feature/kiln-shim-exec-volumes`, from `main` at `bf2c456`. vmkit is unchanged: the pod disk is one more `VmSpec.disks` entry.

Each task is test-first. Its unit tests fail before the change and pass after it. `cargo test` and `cargo clippy --all-targets -- -D warnings` run on macOS. The Linux-only code is checked with `cargo clippy --target x86_64-unknown-linux-musl` and tested in the Lima `vmkit` VM. Commit after each task.

## Tasks

1. **kiln-proto: messages and `Config` fields** (design §2).
   - `Process::validate`, extracted from `Config::validate`.
   - `ExecStart`, `ExecSignal`, `ExecWindowSize`, `FileUpdate` (base64 data), `ExecStarted`, `ExecExited`, `ExecFailed`, with type bytes 10–16.
   - `Volume`/`VolumeKind`, `Config.volumes` and `Config.pod_disk`, both omitted when empty.
   - `port::EXEC` and `ExecStream` with its 5-byte header encode and decode.
   - The limits.
   - **Tests:** a round trip per message; every rejection (bad path, `..data`, mode, id 0, too many volumes); a `Config` with no volumes serialises unchanged.

2. **kiln-run: exec, files and the extra disk** (design §4).
   - The `Execs` registry and its state machine, as a pure module with tests.
   - Session: the port 1029 listener with header dispatch, the control reader's exec validation, the main loop's handling of the final message, and 137 at the end.
   - `Handle::exec`, `exec_signal`, `exec_window_size` and `update_files`, with the half-queue budget.
   - `SessionOptions.after_config`, and `check_devices(.., extra)`.

3. **kiln-init: volumes** (design §3.1).
   - Atomic-writer generation logic over a directory fd: pure enough to test on a Linux tempdir.
   - Detached tmpfs per files volume, the pod disk mount and grow, attachment after the pivot, and the read-only remount.
   - Initial `FileUpdate`s read in `connect`; updates applied in supervise.

4. **kiln-init: exec** (design §3.2).
   - Spawn with stream headers, reap every child, report per exec after its relays.
   - The finished flag against `Exited`.

5. **kiln-shim: classification and the volume reader** (design §5.1–5.2).
   - `mounts.rs` classes.
   - A new `volumes.rs`: walk, limits, subPath lookup, tmpfs detection (statfs on Linux).
   - `settings.pod_disk`.

6. **kiln-shim: wire, plan and worker** (design §5.3–5.5).
   - `ToWorker`/`FromWorker` additions, and the plan's volumes, initial files and pod disk.
   - The fd-passing socketpair.
   - The worker: the pod disk, the `Config` volumes, `after_config`, exec and files handling.
   - The poll thread in the backend.
   - The `cleanup` mode for the pod disk.

7. **kiln-shim: exec in `Tasks` and the service** (design §5.5).
   - The `ExecProcess` trait, `Tasks` exec entries and events, and the fake backend.
   - The service's calls with `exec_id`, and the `TaskExecAdded`/`TaskExecStarted` events.

8. **e2e** (design §7).
   - `scripts/shim-e2e.sh`: an exec case, an ExecSync readiness probe, a tty exec, and the volumes pod with a live ConfigMap update.
   - Run on Firecracker in Lima.

9. **Docs.**
   - `docs/kubernetes.md`: what now works.
   - The v1 design's §12 rows link here, and V7 is marked superseded by this design.

10. **Review.** Independent reviewers; fix what they confirm.

11. **PR.** No vmkit PR. The kiln PR follows the user's PR style.
