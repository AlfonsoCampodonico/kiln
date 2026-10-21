# P1: Snapshot and restore in vmkit — Implementation Plan

**Goal:** `Vm::snapshot` and `Vmm::restore` work on Firecracker 1.17 and Cloud Hypervisor 53, with the bundle format, checks and tests of the design's §4.
**Design:** `../specs/2026-10-21-lazy-restore-design.md` §2, §4.
**Repo:** vmkit, branch `snapshot-restore` (worktree `vmkit/.claude/worktrees/lazy-restore`, from `origin/main` 51d66ba).
**Tests:** Unit tests run anywhere (`cargo test`). KVM tests run on the Lima template (aarch64) with the usual `VMKIT_TEST_KERNEL`/`VMKIT_TEST_INITRAMFS`, and in CI `contract-x86_64` once the branch is pushed.
**Rules:** TDD (failing test first), `cargo fmt`, `cargo clippy --all-targets -D warnings`, `#![forbid(unsafe_code)]` stays, and no attribution trailers in commits.

---

### Task 1: Bundle types and checks (`src/bundle.rs`)

**Files:** `src/bundle.rs` (new), `src/spec.rs`, `src/error.rs`, `src/lib.rs`.

**API**
- `BundleInfo` holds the version, `VmmId { name, version }`, arch, vcpus, `memory_mib`, net, vsock, disks (`DiskInfo { read_only, size, captured }`) and files (`FileInfo { name, role }`).
  - `Role` is `State`, `Memory` or `Disk`.
  - serde uses `deny_unknown_fields` and serialises with sorted keys.
- `SnapshotBundle::info()` reads `bundle.json`: at most 64 KiB, opened with `O_NOFOLLOW`, regular file only.
- `BundleInfo::check(&self, expect: &Expect)`:
  - `Expect` holds the backend's VMM name, version, arch and expected file names.
  - It checks the file set, the sizes (memory exactly `memory_mib` MiB, captured disks equal to their size) and the regular-file and no-symlink rule.
- `RestoreSpec` gets `disks: Vec<RestoreDisk>`, `net: Option<NetSpec>` and `chunks: Option<Arc<dyn ChunkSource>>`.
  - The `ChunkSource` trait is declared here: `fetch(&self, digest: &str, size: u64) -> io::Result<Vec<u8>>`. It stays unused until P3.
- `RestoreDisk` is `Captured` or `Host(PathBuf)`.
- `Error::Snapshot(String)`.
- `Capabilities` gains `lazy_memory: bool`, false for now.

**Tests first**
- `bundle_json_round_trips_with_sorted_keys`
- `unknown_keys_and_versions_are_refused`
- `the_file_set_must_match_the_vmm`
- `memory_and_disk_sizes_are_checked`
- `symlinked_and_non_regular_files_are_refused`
- `captured_read_only_disks_are_refused`
- `host_disks_must_match_the_recorded_size`

**Commit:** `snapshot: the bundle format and its checks`

### Task 2: Moving VMM output and capturing disks (`src/bundle.rs`)

**Functions**
- `take_file(src, dest_dir, name)`:
  - opens `src` with `O_NOFOLLOW`, regular file only;
  - `rename`s it, or on `EXDEV` copies it sparsely and then removes the source;
  - fsyncs the destination.
- `copy_sparse(src, dst)`: a `SEEK_DATA`/`SEEK_HOLE` loop with `copy_file_range` (falling back to read and write), then `set_len`.
- `prepare_dest(dest)`: creates the directory if missing; refuses one that exists and is not empty.

**Tests first**
- `take_file_refuses_a_symlink_planted_by_the_vmm`
- `copy_sparse_keeps_holes_and_content` (checks `st_blocks`)
- `dest_must_be_empty_or_missing`

**Commit:** `snapshot: moving the VMM's files out and copying disks sparsely`

### Task 3: Guest action `count` and `vsock-after` (`testguest/init`)

**`count`**
1. If `/dev/vda` exists, write `VMKIT-MARK-<n>` to its sector 0. `<n>` comes from `vmkit.mark=`, default `1`.
2. Loop: `i=$((i+1))`, then `echo "VMKIT-COUNT $i $(dd if=/dev/vda bs=512 count=1 iflag=direct 2>/dev/null | head -c 16)"`, then `sleep 0.2`.

**`vsock-after`**
- Loop until a file flag appears; the host signals it by writing to the disk.
- Simpler alternative: loop `vsock-hello <port>` every second until it succeeds, printing `VMKIT-VSOCK-OK` once.

**Commit:** `testguest: count and vsock-after actions for snapshot tests`

### Task 4: Sandbox support for restore (`src/sandbox.rs`)

- `vmm_spec` gets a variant for restore that adds read-only file binds at `/vm/snapshot/<name>`.
- It also binds empty placeholder files at `/vm/kernel` and `/vm/initramfs` when the backend needs them (CH landlock).
- **Test:** `restore_binds_the_snapshot_files_read_only` (plan JSON).

**Commit:** `sandbox: snapshot file binds for restore`

### Task 5: Firecracker snapshot and restore (`src/firecracker.rs`)

**Snapshot**
1. Pause.
2. `mkdir <run_dir>/sock/snapshot`.
3. `PUT /snapshot/create` (Full).
4. `take_file` both files.
5. Copy writable disks into `disk-<n>`.
6. Write `bundle.json` last, via a temp file and rename.

The driver remembers its spec (disks, vcpus, memory, vsock, net) and the discovered VMM version, so `binary::check_version` has to return the version.

**Restore**
1. `info()` and `check`.
2. Resolve the disks:
   - `Captured` writable disks are copied sparsely into `<run_dir>/disk-<n>`;
   - `Captured` read-only disks are an error;
   - `Host` disks are size-checked.
3. Spawn the sandbox with the disk binds and the snapshot binds, and clear the vsock UDS.
4. `PUT /snapshot/load` with `resume_vm: false`.

**Unit tests**
- `load_body_points_at_the_sandbox_paths`
- `snapshot_body_is_full_into_the_vmms_dir`

**Commit:** `firecracker: snapshot and restore`

### Task 6: Cloud Hypervisor snapshot and restore (`src/cloud_hypervisor.rs`, `src/ch_config.rs`)

**`ch_config::rewrite(snapshot_config: &Value, spec) -> Result<Value>`**
- An allowlist of top-level keys. Dangerous device classes must be absent or null or empty.
- Keep `cpus`, `memory`, `payload.cmdline`, disk `id`/`readonly` (count must match the bundle), `vsock.cid`, net `id`/`mac`, `rng`, `platform`, `iommu`, `pvpanic`, `watchdog`.
- Set the fixed paths, serial and console, and the landlock rules (design §4.5).
- Each key is checked against a real CH 53 `config.json` captured on Lima. A copy goes into `tests/data/ch53-config.json` as a fixture.

**Snapshot**
1. `vm.pause`.
2. `vm.snapshot` to `/vm/sock/snapshot`.
3. `take_file` the three files and the disks, then write `bundle.json`.

**Restore**
1. `info()` and `check`, then the disks.
2. Rewrite `config.json` into `<run_dir>/snapshot-config.json`.
3. Spawn with the same arguments as `create` (event monitor and backstop) and the snapshot binds.
4. `vm.restore {source_url: file:///vm/snapshot, memory_restore_mode: Copy, resume: false, prefault: false}`.

**Unit tests**
- `rewrite_keeps_only_allowed_fields_and_fixes_paths` (fixture)
- `rewrite_refuses_fs_devices_vdpa_pmem_user_devices`
- `rewrite_refuses_a_disk_count_mismatch`

**Commit:** `cloud-hypervisor: snapshot and restore with a rewritten config`

### Task 7: The KVM contract tests (`tests/snapshot.rs`)

Implement design §4.7: both backends, each test spawning one VM at a time. CH runs with `--test-threads=1` like `pause.rs`. Add the CI step `cargo test --test snapshot -- --test-threads=1`.

**Commit:** `tests: snapshot and restore contract suite`

### Task 8: Docs

- README: API section and status.
- The `Vmm` doc comments: drop "implemented by project #2".

**Commit:** `docs: snapshot and restore`

**Done when:** `cargo fmt --check`, `cargo clippy --all-targets -D warnings` and `cargo test` pass on macOS and in Lima, and `tests/snapshot.rs` passes for Firecracker and (with reruns) Cloud Hypervisor on aarch64 Lima.
