# P3: Lazy restore below the VMM (raku) — Implementation Plan

**Goal:** a VM restores before its memory and disks are local. One FUSE layer fetches verified chunks on demand for both VMMs (design §3, §6).
**Design:** `../specs/2026-10-21-lazy-restore-design.md` §3, §6.
**Needs:** P1 (vmkit restore) and P2 (kiln artifact, `lazy.json`).
**Repos:**
- vmkit `snapshot-restore`: Tasks 1–6.
- kiln `feature/kiln-lazy-restore-1b2b15`: Tasks 7–8, after the vmkit branch is pushed (approved) and pinned.

**Rules:** TDD, `#![forbid(unsafe_code)]` in the vmkit library (the helper already allows targeted `unsafe` for `mount_setattr`; the FUSE mount itself uses `rustix::mount::mount`), `cargo fmt`, clippy `-D warnings`, and no attribution trailers.

---

### Task 1: `lazy.json` and the chunk index (`src/lazy/index.rs`)

**Types**
- `LazyIndex { version, chunk_size, files: BTreeMap<String, IndexedFile { size, chunks: Vec<Option<String>> }> }`. serde uses `deny_unknown_fields`.
- `check()` mirrors the kiln limits: chunk size range, counts, digest syntax `sha256:<64 hex>`, file names from the bundle's set.

**Tests:** `digests_must_be_sha256`, `chunk_counts_must_cover_the_size`, `unknown_files_are_refused`.

### Task 2: `LazyFile` (`src/lazy/file.rs`)

**Struct**
- A sparse backing `File` and a chunk-state vector (`Absent`, `Fetching`, `Present`) under a `Mutex` with a `Condvar`.
- `read_at`, `write_at` (writable files only), `ensure(range)`, `is_hole(chunk)`.

**Fetching**
- Fetches run outside the lock.
- Bytes are verified with SHA-256 and their length.
- A failure is retried up to 3 times, then `EIO`. Each failure is logged to `lazy.log`.
- Zero chunks are present without a fetch.

**Tests** (in-memory sources, any OS)
- `only_touched_chunks_are_fetched`
- `concurrent_readers_of_one_chunk_fetch_it_once`
- `a_corrupt_chunk_is_an_io_error_and_is_logged`
- `writes_after_fetch_persist_and_never_reach_the_source`
- `zero_chunks_read_as_zero_without_a_fetch`

### Task 3: `DirCache` (`src/lazy/cache.rs`)

A `ChunkSource` wrapper that stores verified chunks in `<dir>/<hex>`, written atomically.

**Tests:** `a_cached_chunk_is_not_fetched_again`, `a_corrupt_cache_entry_is_refetched`.

### Task 4: The FUSE protocol and server (`src/lazy/fuse.rs`, Linux for the device loop)

**Protocol**
- `decode(&[u8]) -> Request` and `Reply::encode() -> Vec<u8>` for the opcodes in design §6.1. Constants and struct layouts come from `include/uapi/linux/fuse.h` 7.31 or later.
- `INIT` negotiates max write 128 KiB and sets no `writeback_cache`.

**`Fs` (pure)**
- Maps requests to a flat directory of `LazyFile`s.
- Inode 1 is the root; files are 2…
- Reports full sizes, mode 0444 for memory, and refuses write-opens on read-only files.
- `LSEEK` returns holes for zero chunks.

**Device loop**
- `serve(fd: OwnedFd, fs: Arc<Fs>, threads: 4)`.
- Each worker reads a request (up to 1 MiB + 4 KiB buffer), handles it, and writes the reply.
- `ENODEV` on read means unmounted: the workers stop.

**Tests (any OS)**
- golden vectors per opcode;
- `lookup_getattr_read_through_fs`;
- `write_to_read_only_file_is_eacces`;
- `lseek_data_skips_zero_chunks`;
- `unknown_opcode_is_enosys`.

### Task 5: Sandbox `Mount::Fuse` and `Mount::Rebind` (`src/sandbox.rs`, `src/bin/vmkit-sandbox.rs`)

**Spec**
- `Mount::Fuse { target, socket }` and `Mount::Rebind { source, target, writable }`.
- `Spec::check`:
  - socket path absolute;
  - Fuse at most once;
  - Rebind sources must lie under a Fuse target.

**Helper**
- After the other mounts:
  1. open `/dev/fuse` (O_RDWR|O_CLOEXEC);
  2. `mount("fuse", target, "fuse", NOSUID|NODEV, "fd=N,rootmode=40000,user_id=U,group_id=G")`;
  3. connect `socket` and send `N` with `SCM_RIGHTS` (rustix `sendmsg`);
  4. close it.
- Then the rebinds (`open_tree` inside the new root plus `move_mount`, with the read-only remount as for binds).

**Library**
- `lazy::Mounted::listen(run_dir) -> (socket path, receiver thread)`.
- The receiver checks the received fd is a char device 10:229 and then runs `serve`.

**Tests (Linux, sandbox helper, no KVM)**
- `a_sandboxed_reader_sees_lazy_files`
- `only_read_chunks_are_fetched`
- `a_tampered_chunk_reads_as_eio`

### Task 6: Lazy restore in both drivers, `lazy_memory`

**Restore**
- When `RestoreSpec::chunks` is set and `<dir>/lazy.json` exists:
  - fetch state files in full into the run dir;
  - build `LazyFile`s for memory and captured disks (writable disks get CoW backings in the run dir);
  - mount via `Mount::Fuse` at `/vm/lazy`;
  - rebind `/vm/disk/<n>` (and CH's `/vm/snapshot/memory-ranges`).
- FC loads `/vm/lazy/memory`. CH restores with `OnDemand` when `lazy_memory`, else `Copy`.

**`lazy_memory`**
- FC: true.
- CH: true when `/dev/userfaultfd` is readable and writable by the caller, or `/proc/sys/vm/unprivileged_userfaultfd` is `1`.
- In the `/dev/userfaultfd` case, `vmm_spec` binds it into the sandbox.

**Contract tests (`tests/snapshot.rs`):** `lazy_restore_continues_the_guest_and_fetches_only_what_it_touches`, with a counting `ChunkSource` built from a local bundle via `lazy::split` (the test helper that chunks a file).

**Commit per task in vmkit:**
- `lazy: chunk index`
- `lazy: lazily fetched files`
- `lazy: directory chunk cache`
- `lazy: FUSE server`
- `sandbox: FUSE and rebind mounts`
- `lazy restore on both backends`

Then push the branch (approved) and let CI run `contract-x86_64`.

### Task 7: kiln `StoreSource` and `pull --lazy` restore (kiln)

**Pin bump**
- Move the vmkit dependency to `[workspace.dependencies]`, pinned to the pushed `snapshot-restore` head.
- `kiln-run` and `kiln-snapshot` use `vmkit.workspace = true` (Linux only).

**`kiln_snapshot::StoreSource { store, client: Option<Client>, repo }`** implements `vmkit::lazy::ChunkSource`:
- read from the store if present;
- else `fetch_blob` verified into the store, then read;
- 2 retries with backoff on registry errors.

**Tests:** `store_source_prefers_the_store`, `store_source_fetches_and_caches`, `a_bad_registry_chunk_never_enters_the_store`.

### Task 8: kiln end-to-end (Linux, KVM, local registry)

**Test:** `crates/kiln-snapshot/tests/lazy.rs`, gated on `VMKIT_TEST_KERNEL`/`VMKIT_TEST_INITRAMFS`/KVM like vmkit's suite:
1. Boot the vmkit test guest (`count`) through vmkit and snapshot it.
2. `push` to the test registry.
3. Use a fresh store: `pull --lazy`.
4. `restore` with `StoreSource`. Counting continues.
5. The store holds fewer chunks than the manifest lists (Firecracker).

**Commits**
- `feat(kiln-snapshot): a store and registry chunk source for lazy restore`
- `test(kiln-snapshot): lazy restore end to end`
- `docs(spec): snapshot, restore and lazy restore implemented; non-goals updated` (kiln spec §1 non-goals, §3 T10, §4.1, §12, plus the vmkit pin note).

**Done when:**
- `cargo test` and clippy pass for vmkit and the touched kiln crates.
- The lazy contract test passes on Firecracker, and on Cloud Hypervisor with reruns, on aarch64 Lima.
- vmkit CI `contract-x86_64` is green on the pushed branch.
