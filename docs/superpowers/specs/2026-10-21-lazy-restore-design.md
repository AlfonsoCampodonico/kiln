# Snapshot, restore and lazy restore — Design

**Date:** 2026-10-21
**Status:** Draft rev 1
**Scope:** `vmkit` snapshot/restore on both VMMs (the `Vmm::restore` and `Vm::snapshot` the kiln spec §4.1 fixed), the kiln snapshot artifact (roadmap codename **sherd**), and lazy restore below the VMM (codename **raku**).
**Related:** `2026-09-30-kiln-design.md` (§3 threat model, §4.1 `vmkit`, §9 runtime), `../../../../ROADMAP.md` ("Carried-forward decisions for sagger, sherd and raku"), plans `../plans/2026-10-21-p1-snapshot-restore.md`, `…-p2-snapshot-artifact.md`, `…-p3-lazy-restore.md`.

---

## 1. Goals and scope

1. **Snapshot and restore** a running VM on Firecracker and Cloud Hypervisor, on aarch64 and x86_64. A snapshot covers the VM state, guest memory and the writable disks.
2. **A snapshot artifact.** A snapshot becomes an OCI artifact made of fixed-size, content-addressed chunks. It is pushed and pulled through `kiln-store` and `kiln-registry`, and documented in `docs/format.md`.
3. **Lazy restore.** A restored VM starts before its memory and disks are local. One on-demand chunk fetcher, below the VMM, serves guest memory and disks to **both** VMMs. It keeps a local chunk cache and verifies every chunk.

### Decisions taken with the user (2026-10-21)

- **A. Stop before sagger.** Sagger is the roadmap's agent-sandbox core: snapshot, restore and fork of guests started by `kiln run`. This cut builds the layers underneath it and tests them with vmkit's busybox test guest.
  - A `kiln run` guest cannot be snapshotted yet. `kiln-init` exits when its control connection closes, and every restore closes it (§4.6). Teaching `kiln-init` and `kiln-proto` to reconnect is sagger's job, under its own spec.
- **B. Code goes where the roadmap puts it.** Snapshot/restore and raku go in `vmkit`, on branch `snapshot-restore`. The artifact and the registry-backed chunk source go in `kiln`. The user approved pushing the vmkit feature branch so kiln can pin it. PRs and merges still need the user's OK.

### What "both" means

The roadmap item reads: *"Snapshots as OCI artifacts, fetched in chunks on demand from below the VMM, so one mechanism works for both."*
- `ROADMAP.md` resolves "both": *"A lazily fetched, chunk-backed block/file layer (FUSE or ublk) serves the guest memory file, and each VMM maps it normally. That gives one mechanism for both backends."*
- So "both" means both VMMs, and also both kinds of data: memory and disk go through the same layer.
- §3 records the comparison the roadmap asked for, and the result: **FUSE, not userfaultfd.** The brief that started this work suggested a userfaultfd page-fault handler; §3 explains why that cannot serve Cloud Hypervisor.

### Non-goals (this cut)

- Snapshotting `kiln run` guests, fork, and `kiln restore` (sagger).
- Diff and incremental snapshots. Firecracker supports them (`supports_diff_snapshot`). The chunk format dedups unchanged chunks across full snapshots anyway (§5.3).
- Snapshots portable across VMMs, VMM versions, architectures or CPU models. Restore refuses a bundle whose VMM, version or arch differs (§4.3).
- Range-fetching chunks out of packed blobs (estargz, SOCI or nydus style). One chunk is one blob (§5.3).
- Background prefetch and access-pattern recording. The format leaves room for both (§5.6).
- Referencing image layers from a snapshot instead of capturing read-only disks. It is not needed until sagger snapshots kiln guests (§4.2).

---

## 2. Background: what the VMMs do

Firecracker v1.17.0 and Cloud Hypervisor v53.0, read from source.

| | Firecracker 1.17 | Cloud Hypervisor 53 |
|---|---|---|
| Snapshot API | `PATCH /vm Paused`, `PUT /snapshot/create {snapshot_type: Full, snapshot_path, mem_file_path}` | `vm.pause`, `PUT vm.snapshot {destination_url: file:///dir}` |
| Files | vmstate (bitcode plus CRC64), memory (raw dump) | `config.json` (VmConfig), `state.json`, `memory-ranges` (raw) |
| Disks | Not captured: drained and fsync'd, then up to the caller | Not captured |
| Restore | Fresh process, `PUT /snapshot/load {snapshot_path, mem_backend {File, path}, resume_vm, network_overrides, vsock_override}` | Fresh process (no `vm.create`), `PUT vm.restore {source_url, prefault, memory_restore_mode Copy\|OnDemand, resume}` |
| Memory on restore | File backend: `mmap(MAP_PRIVATE\|MAP_NORESERVE)` of the file, demand-paged through the page cache. Needs `st_size ≥` guest memory | `Copy`: reads all data extents eagerly (holes skipped with `SEEK_DATA`). `OnDemand`: an in-process userfaultfd thread serves faults, then prefaults the rest in the background |
| Disk paths on restore | Reopened at the `path_on_host` recorded in vmstate; must exist | Taken from `config.json` only |
| Vsock | Not persisted. The guest gets `TRANSPORT_RESET` when the snapshot is **created**; connections close, listeners survive | Not persisted, no reset. The guest's connections go stale. On restore the muxer `bind`s the UDS, which must not exist |
| Kernel on restore | Not needed | Not loaded, but with `--landlock` the payload paths get rules, so the files must exist |
| Compatibility | Same snapshot data version (in practice the same FC version), same CPU model and GIC | Same version and host |

Sandbox paths are fixed (`/vm/disk/<n>`, `/vm/sock/…`; kiln spec §9.2), so recorded paths resolve in any new sandbox. That is why a snapshot restores from any host location.

---

## 3. Lazy loading below the VMM: the comparison

The roadmap asked for FUSE vs ublk vs UFFD at design time.

| | FUSE file | ublk block device | userfaultfd |
|---|---|---|---|
| Unprivileged | Yes. `fuse` is `FS_USERNS_MOUNT` (4.18+), and `/dev/fuse` is normally 0666. Mounted inside the sandbox's userns-owned mount namespace | No. Root must open up `/dev/ublk-control` and add udev rules; nodes appear on the host and can't be made inside a userns | Only `UFFD_USER_MODE_ONLY`, which misses KVM's and the VMM's kernel-mode faults. Real use needs `/dev/userfaultfd` access (0600 root) or `vm.unprivileged_userfaultfd=1` |
| FC memory | `File` backend maps it `MAP_PRIVATE`: lazy, no uffd | FC's File backend rejects block devices (`st_size` 0) | FC's own UFFD handler protocol, FC-only |
| CH memory | `Copy` reads all data (holes are free); `OnDemand` is lazy but needs userfaultfd | Same as FUSE but needs root | CH has no external handler; its uffd is in-process |
| Disks, both VMMs | A FUSE file is a raw disk image | Yes | No |
| One mechanism for both | **Yes** | Yes, but needs root | **No** |

**Decision: a FUSE file layer.**
- One FUSE server in the vmkit caller's process exposes the memory file and the disk images as regular files inside the VMM's sandbox.
- Firecracker maps the memory file lazily. Both VMMs read and write the disks lazily.
- **Cloud Hypervisor memory caveat.** CH only restores memory lazily when the host lets the caller use userfaultfd:
  - **With access** (`/dev/userfaultfd` readable and writable by the caller, e.g. through an ACL, or `vm.unprivileged_userfaultfd=1`), vmkit restores with `OnDemand`. When `/dev/userfaultfd` is the route, vmkit binds it into the sandbox. The VM resumes before its memory is local, though CH's background prefault eventually reads the whole file.
  - **Without access**, vmkit uses `Copy`. Memory is fetched through the same verified layer before the VM resumes. All-zero chunks cost nothing, because the FUSE server reports them as holes (`FUSE_LSEEK`) and CH skips holes.
  - `Capabilities::lazy_memory` reports which case applies. Disks are lazy on both VMMs either way.
- Rejected:
  - **UFFD:** it cannot serve CH or disks.
  - **ublk:** it needs root, and FC can't map a block device as memory.
  - **FUSE passthrough:** it needs `CAP_SYS_ADMIN` in the initial namespace.

---

## 4. Phase 1: snapshot and restore in `vmkit`

### 4.1 API

The trait methods keep the shape the kiln spec fixed. The structs gain fields.

```rust
pub trait Vmm {
    fn restore(&self, bundle: &SnapshotBundle, spec: &RestoreSpec) -> Result<Box<dyn Vm>>;
}
pub trait Vm {
    /// Pauses the VM (if running) and writes a snapshot into `dest`, an empty or missing
    /// directory. The VM stays paused: resume it, or kill it.
    fn snapshot(&mut self, dest: &Path) -> Result<SnapshotBundle>;
}

pub struct SnapshotBundle { pub dir: PathBuf }
impl SnapshotBundle {
    /// Reads and checks `<dir>/bundle.json`.
    pub fn info(&self) -> Result<BundleInfo>;
}

pub struct RestoreSpec {
    /// One per disk of the snapshot, in order.
    pub disks: Vec<RestoreDisk>,
    pub console_log: PathBuf,
    pub run_dir: PathBuf,
    /// Must be present exactly when the snapshot had a NIC.
    pub net: Option<NetSpec>,
    /// Serves the bundle's files that are not in `dir` (phase 3).
    pub chunks: Option<Arc<dyn ChunkSource>>,
}

pub enum RestoreDisk {
    /// The content the bundle captured.
    Captured,
    /// A host file whose content is what the disk held when the snapshot was taken.
    Host(PathBuf),
}
```

- `Vm::snapshot` leaves the VM paused. The caller decides whether the source VM continues.
  - On Firecracker the source guest's vsock connections were reset by the snapshot (§2).
- `Capabilities` gains `lazy_memory: bool` (phase 3).
- `Error` gains `Snapshot(String)` for invalid or incompatible bundles.

### 4.2 The bundle (`bundle.json`, vmkit bundle format 1)

```
<dir>/bundle.json
<dir>/<VMM files>          vmstate + memory (FC) | config.json + state.json + memory-ranges (CH)
<dir>/disk-<n>             one per captured disk, sparse
```

```json
{
  "arch": "aarch64",
  "disks": [{"captured": true, "read_only": false, "size": 67108864}],
  "files": [
    {"name": "vmstate", "role": "state"},
    {"name": "memory", "role": "memory"},
    {"name": "disk-0", "role": "disk"}
  ],
  "memory_mib": 256,
  "net": false,
  "vcpus": 1,
  "version": 1,
  "vmm": {"name": "firecracker", "version": "1.17.0"},
  "vsock": {"guest_cid": 3}
}
```

**Disks**
- Writable disks are always captured. The snapshot copies them while the VM is paused (sparse: `SEEK_DATA`/`SEEK_HOLE` plus `copy_file_range`).
- Read-only disks are not captured. Their size is recorded, and the caller supplies them on restore as `RestoreDisk::Host`.
- `Captured` for a read-only disk is an error.
- A host disk whose size differs from the recorded size is refused.

**Restore never modifies the bundle**, so one bundle restores any number of times.
- A captured writable disk is copied (sparse) into the run directory on a local restore. On a lazy restore it gets a copy-on-write file instead (§6).
- The memory file is mapped `MAP_PRIVATE` (FC) or read (CH).

**Files the VMM wrote are untrusted.** A compromised VMM controls `<run_dir>/sock`, where it writes its snapshot files.
- vmkit opens every bundle file with `O_NOFOLLOW` and requires a regular file.
- It moves the files out of the run directory with `rename`. If `dest` is on another filesystem, it copies them instead.
- So a symlink planted by the VMM can never make vmkit read a host file into a snapshot that might later be pushed.

### 4.3 Checks before restore

`SnapshotBundle::info` and `restore` check the following, and fail with `Error::Snapshot`:
- `version == 1`, and every key is known.
- The VMM name and version and the arch equal the restoring backend's.
- `files` holds exactly the VMM's file set, plus `disk-<n>` for each captured disk.
  - Names are fixed, so a bundle cannot name an arbitrary path.
  - Each file is regular and not a symlink.
- Sizes:
  - The memory file is exactly `memory_mib` MiB.
  - Each captured disk equals its recorded size.
  - `memory_mib` and `vcpus` are within `VmSpec::check`'s limits.
- The NIC and vsock presence match the `RestoreSpec`.

The VMM's cgroup is sized from `memory_mib`, as for a created VM. A vmstate that lies about memory is bounded by the cgroup, and the VMM is sandboxed (T6). Callers apply their own policy to `info()` (for example a memory ceiling) before restoring.

### 4.4 Firecracker

**Snapshot**
1. `PATCH /vm Paused`.
2. `PUT /snapshot/create {Full, /vm/sock/snapshot/vmstate, /vm/sock/snapshot/memory}`.
3. Move both files into `dest` and capture the writable disks.

**Restore**
1. Start a fresh sandboxed process with the disks bound at `/vm/disk/<n>`.
2. `PUT /snapshot/load {snapshot_path: /vm/snapshot/vmstate, mem_backend: {File, /vm/snapshot/memory}, resume_vm: false}`.
3. Leave `vsock_override` and `network_overrides` unset. The recorded `/vm/sock/vsock.sock` and `tap0` are right in every sandbox. The stale UDS is unlinked first.
4. The VM comes back paused; `Vm::resume` starts it.

Bundle files are bound read-only at `/vm/snapshot/<name>`.

### 4.5 Cloud Hypervisor

**Snapshot:** `vm.pause`, then `vm.snapshot {file:///vm/sock/snapshot}`, then move the files and capture the disks.

**Restore**
1. Start a fresh process with the same arguments as `create` (API socket, event monitor and its reset backstop, `--seccomp true`).
2. `PUT vm.restore {source_url: file:///vm/snapshot, memory_restore_mode, resume: false, prefault: false}`.

**`config.json` is rewritten, never trusted.** It is the only place CH takes paths from, and a hostile bundle could name any device or path.
- vmkit parses it and keeps only the fields CH needs to match `state.json`:
  - `cpus`, `memory`, the payload's cmdline, `disks` (count, `readonly`, `id`), `vsock.cid`, `net` (count, `mac`, `id`), `serial` and `console` modes, `platform`, `iommu` flags.
- It refuses anything outside an allowlist: `fs`, `pmem`, `devices` (VFIO), `user_devices`, `vdpa`, vhost-user, `numa`, `sgx_epc`, `tpm`, landlock overrides, and so on.
- It then writes the fixed values itself:
  - `payload.kernel` and `initramfs` point at `/vm/kernel` and `/vm/initramfs`. These are empty placeholder files, because `--landlock` needs the paths to exist (§2).
  - Disk paths become `/vm/disk/<n>` with `image_type: Raw`.
  - The vsock socket becomes `/vm/sock/vsock.sock`.
  - The tap becomes `tap0`.
  - The serial goes to `Tty` and the console is `Off`.
  - `landlock_enable` is `true`, with the same rules `create` uses.
- The rewritten file lives in the run directory and is bound at `/vm/snapshot/config.json`.

**Memory restore mode:** `OnDemand` when `lazy_memory` holds (§3), else `Copy`.

### 4.6 Vsock and the guest

- **Firecracker** resets the guest's vsock connections when the snapshot is taken.
- **Cloud Hypervisor** leaves them stale until the guest's next packet.
- In both cases, connections the guest makes **after** restore work: the host side listens on the new run directory's `sock/vsock.sock_<port>`.
- The contract test checks exactly that. Reconnecting `kiln-init` builds on it (sagger).

### 4.7 Tests (TDD, the KVM contract suite, both backends)

New guest action `vmkit.test=count`:
- Writes a marker to sector 0 of `vda` if a disk is attached.
- Then loops: increment a shell counter (guest memory), print `VMKIT-COUNT <n>` and the marker read back from `vda` with `iflag=direct`, sleep 0.2 s.

| Test | Checks |
|---|---|
| `snapshot_and_restore_continue_the_guest` | Snapshot at count ≥ 3 and kill the source. Restore into a **different** run dir. Counting continues from the snapshot's value (not 1), there is no second `VMKIT-GUEST-UP`, and the disk marker reads back. |
| `a_bundle_restores_twice` | Two restores from one bundle both continue from the same count. The bundle's files are byte-identical before and after. |
| `restore_refuses_a_foreign_or_tampered_bundle` | Wrong VMM name or version, wrong arch, a memory file one page short, a symlinked file, an unknown key, a host disk of the wrong size. |
| `vsock_works_after_restore` | The guest connects to a host port after restore (new action `vsock-after`). |
| CH only: `a_hostile_config_is_refused` | A `config.json` with `fs` or `devices` entries. |

Unit tests (no KVM) cover the bundle JSON, the checks and the CH config rewrite.

---

## 5. Phase 2: the snapshot artifact in `kiln` (`kiln-snapshot`)

### 5.1 Crate and commands

- New crate `kiln-snapshot`, depending on `kiln-store`, `kiln-registry`, serde and sha2. It builds and runs on macOS, like `convert`. It does not depend on `vmkit`; it reads a bundle directory as files plus `bundle.json`.
- CLI:
  - `kiln snapshot push <bundle-dir> <ref>`: chunk the bundle into the store, write the config and manifest, tag it locally, push.
  - `kiln snapshot pull <ref> <dir> [--lazy]`: without `--lazy`, fetch every chunk (verified) and write a complete bundle. With `--lazy`, write only `bundle.json` and the chunk index `lazy.json` (§6.2).
  - `kiln snapshot inspect <ref>`: VMM, arch, memory, files, logical size, stored size and chunk counts.
- Refs share `refs.json` with images. `kiln ls` shows a snapshot ref with type `snapshot`. `kiln run` refuses a snapshot ref with a clear error. `gc` already walks `config` and `layers`, so a tagged snapshot keeps its chunks alive.

### 5.2 Format (normative in `docs/format.md`)

**Manifest:** an OCI image manifest with:
- `artifactType: application/vnd.kiln.snapshot.v1`
- `config`: `application/vnd.kiln.snapshot.config.v1+json`
- `layers`: one descriptor per **distinct** non-zero chunk, `application/vnd.kiln.snapshot.chunk.v1`, in order of first use (files in config order, chunks in offset order).

There are no annotations, and JSON is serialised canonically (sorted keys), so the same bundle always gives the same digest.

**Config:**
```json
{
  "bundle": { "...": "bundle.json, verbatim as a JSON value" },
  "chunkSize": 1048576,
  "files": [
    {"name": "memory", "size": 268435456, "chunks": ["sha256:…", null, "sha256:…"]}
  ],
  "schemaVersion": 1
}
```
- `chunks[i]` covers bytes `[i·chunkSize, min((i+1)·chunkSize, size))`.
- `null` means all zero; zero chunks are never stored.
- Every chunk is its own blob. Its size is the covered length, so only a file's last chunk may be short.
- `files` lists every file in the bundle except `bundle.json`, which is in `bundle`.

### 5.3 Why one blob per chunk

- Registry GC keeps only blobs that manifests reference. Chunks referenced only from a config blob would be collected, so they are `layers`.
- Identical chunks dedupe within a snapshot, across snapshots in the store, and in the registry. Push skips blobs the registry has (`HEAD`).
- So two snapshots of the same VM a minute apart share most of their chunks. That is what makes diff snapshots unnecessary for distribution.
- **Cost:** one HTTP request per chunk. At 1 MiB, a manifest of the 4 MiB metadata limit lists about 25,000 descriptors, which is about 25 GiB of non-zero data, plenty for agent sandboxes. Packing chunks into larger blobs fetched by range is a later optimisation (§1 non-goals). It would need range GETs in `kiln-registry`.

**Chunk size:** 1 MiB by default (`--chunk-size`). Valid values are powers of two from 64 KiB to 16 MiB.
- Smaller chunks fetch less per fault.
- Larger chunks mean fewer round trips and a smaller manifest.
- 1 MiB is 256 pages, close to the kernel's readahead window.

### 5.4 Untrusted input (the snapshot threat model)

A pulled snapshot is hostile input like a pulled image (kiln spec §3: "hostile registry or image content"). Reading it enforces these rules:
- **T1 integrity:** every chunk enters the store only through `put_verified` against its digest and expected size. The config and manifest are verified the same way. A snapshot is tagged only after its config parses and passes every check below.
- **T3 bounds** (configurable; typed errors name the limit):
  - config ≤ 4 MiB;
  - `chunkSize` in range;
  - at most 64 files;
  - file size ≤ 1 TiB;
  - total logical size ≤ 4 TiB;
  - `chunks.len() == ceil(size / chunkSize)`;
  - every referenced digest is in `layers` with the right size;
  - no layer is unused.
- **Names:** `[a-z0-9][a-z0-9._-]{0,63}`, not `bundle.json` or `lazy.json`, no duplicates. The bundle cannot write outside its directory.
- **Version gate:** `schemaVersion` must be 1, and `bundle.version` must be 1. vmkit checks the bundle fully again at restore (§4.3).
- **Restore safety lives in vmkit, not in the artifact.** The VMM state files are untrusted input to the VMM. That is safe only because the VMM runs in vmkit's sandbox (T6), and CH's `config.json` is rewritten (§4.5).

The kiln spec's §3 gains a property **T10**: *a snapshot's chunks are verified before use, and its VMM state only ever reaches a sandboxed VMM.*

### 5.5 Push and pull

- **Push** reads the bundle files with `O_NOFOLLOW` and requires regular files. It hashes chunk by chunk and skips zero chunks. Each distinct chunk is stored with `put_bytes`, and blobs are pushed with `push_blob` (skipping existing ones). Then the config and the manifest are pushed by tag.
- **Pull** resolves the manifest (`resolve_manifest`, verified) and the config. It checks §5.4, then:
  - **eager:** `fetch_blob` each chunk into the store and assemble the files (holes for zero chunks), then write `bundle.json`;
  - **lazy:** write `bundle.json` and `lazy.json` only.
- The snapshot is tagged in `refs.json` either way.

### 5.6 Room to grow

The config may later carry an access-order hint (`prefetch`: chunk indices) and per-disk references to image layers (`layer: <digest>` instead of `chunks`). Both are additive.

---

## 6. Phase 3: lazy restore (raku)

### 6.1 Pieces

**In vmkit (`vmkit::lazy`, Linux):**
- `ChunkSource`:
  ```rust
  pub trait ChunkSource: Send + Sync {
      /// The bytes of the chunk whose SHA-256 is `digest` (`sha256:<hex>`), `size` bytes long.
      fn fetch(&self, digest: &str, size: u64) -> std::io::Result<Vec<u8>>;
  }
  ```
  Implementations may cache. vmkit verifies every returned chunk (SHA-256 and length) whatever the source; a mismatch is an integrity error, never retried with the same bytes.
- `DirCache`: a `ChunkSource` wrapper caching verified chunks in a directory, keyed by digest. It is for callers without a store; kiln uses its store instead.
- `LazyFile`: one file of the bundle. It has:
  - a sparse local backing file in the run directory;
  - a per-chunk state of *absent*, *fetching* or *present*;
  - **fetch-on-first-access:** the chunk is fetched, verified and written into the backing file, then read and written there. Zero chunks are filled with zeros without a fetch.
  - Concurrent accesses to a chunk being fetched wait on it, and other chunks proceed.
  - Writes (writable disks only) go to the backing file once the chunk is present. **Copy-on-access** keeps the logic small: a written chunk is always a fetched chunk.
- **The FUSE server:**
  - Own implementation of the protocol subset vmkit needs (`INIT`, `LOOKUP`, `FORGET`, `GETATTR`, `OPEN`, `READ`, `WRITE`, `FLUSH`, `FSYNC`, `RELEASE`, `STATFS`, `OPENDIR`, `READDIR`, `RELEASEDIR`, `LSEEK`, `FALLOCATE`, `INTERRUPT`, `DESTROY`; the rest get `ENOSYS`). This matches kiln's practice of owning the internals (native erofs, own registry client).
  - The request parsing is pure (byte slices in, replies out), so it is unit-tested without a kernel. vmkit keeps `#![forbid(unsafe_code)]`: the protocol is decoded with `from_le_bytes`, and `/dev/fuse` I/O goes through `rustix`.
  - It serves a flat, read-only directory of `LazyFile`s:
    - memory files are 0444, and writes are refused;
    - disks are 0644 or 0444 as captured.
  - `FOPEN_KEEP_CACHE` is set, and `GETATTR` reports full sizes (FC checks `st_size`).
  - `LSEEK` reports *absent zero* chunks as holes, so CH `Copy` skips them.
  - Four worker threads read the same `/dev/fuse` descriptor.
  - A fetch that fails three times returns `EIO`. The VMM then faults: Firecracker gets SIGBUS, and CH exits. The run directory's `lazy.log` says which chunk failed and why.
- **`Mount::Fuse { target, socket }` in the sandbox helper.** Inside the sandbox's mount namespace, the helper:
  1. opens `/dev/fuse`;
  2. mounts `fuse` on `target` with `fd=N,rootmode=40000,user_id,group_id` (`nosuid,nodev`), with no `allow_other`, so only the caller's uid inside;
  3. sends the descriptor over the Unix socket `socket` with `SCM_RIGHTS`;
  4. continues building the root.

  The socket lives in the run directory, outside `<run_dir>/sock`, so the VMM can never reach it. vmkit checks that the received descriptor is `/dev/fuse` (char 10:229) before serving it.
- **`Mount::Rebind { source, target, writable }`:** a bind of a path already inside the new root, used for `/vm/disk/<n>` ← `/vm/lazy/disk-<n>`. Rebinds run after the FUSE mount, by which point the server is answering.

**In kiln (`kiln-snapshot`, Linux):**
- `StoreSource`, a `vmkit::lazy::ChunkSource`.
- It serves a chunk from the store when present, and otherwise runs `Client::fetch_blob` (verified, `put_verified`) into the store first. The store is the local chunk cache, and GC keeps a tagged snapshot's chunks.
- Retries a transient registry error twice with backoff.

### 6.2 `lazy.json` (vmkit lazy index format 1)

```json
{"chunk_size": 1048576, "files": {"memory": {"chunks": ["sha256:…", null], "size": 268435456}}, "version": 1}
```

A bundle directory may hold any of its files in full; the rest must be in `lazy.json`. A file in neither is an error. `restore` with `RestoreSpec::chunks` set serves the `lazy.json` files:
- **State files** (`vmstate`, `config.json`, `state.json`): fetched in full into the run directory before the VMM starts. They are small, and the VMM needs them whole.
- **The memory file and captured disks:** served by the FUSE mount at `/vm/lazy`.
  - FC maps `/vm/lazy/memory`.
  - CH reads `/vm/snapshot/memory-ranges`, rebound from `/vm/lazy/memory-ranges`.
  - Disks are rebound to `/vm/disk/<n>`.

The bounds of §5.4 apply to `lazy.json` too, since kiln wrote it from untrusted input.

### 6.3 Tests

- **Unit (any OS):**
  - FUSE request decoding and replies, golden byte vectors for every opcode served;
  - `LazyFile` chunk state machine, concurrency, zero chunks, the write path;
  - verification failures and retries;
  - `LSEEK` holes.
- **Sandbox (Linux, no KVM):** a sandboxed `dd`/`cmp` reads a lazily served file through `Mount::Fuse` and `Mount::Rebind`. A counting `ChunkSource` shows only the touched chunks were fetched. A tampered chunk gives `EIO` and a `lazy.log` line.
- **Contract (KVM, both backends):**
  - snapshot, then restore from a `lazy.json` bundle served by a counting source. The guest continues counting with its disk marker.
  - On FC, the bytes fetched before the guest's first post-restore tick are a fraction of the memory size.
  - On CH, the same holds with `lazy_memory`, and otherwise all data chunks are fetched and the zero chunks are not.
- **kiln (Linux, KVM, local registry):** `kiln snapshot push` a vmkit bundle to the test registry, `pull --lazy` into an empty store, restore through vmkit with `StoreSource`, and the guest continues. The store then holds only the fetched chunks.

---

## 7. Phases, branches and order

| Phase | Repo / branch | Lands | Needs KVM |
|---|---|---|---|
| 1. Snapshot/restore | vmkit `snapshot-restore` | API, bundle, both drivers, contract tests | yes |
| 2. Snapshot artifact | kiln `feature/kiln-lazy-restore-1b2b15` | `kiln-snapshot` (format, push/pull, limits), CLI, `docs/format.md` | no |
| 3. Lazy restore | both | `vmkit::lazy`, `Mount::Fuse`/`Rebind`, lazy restore; kiln `StoreSource`, `pull --lazy`, the vmkit pin bump, end-to-end test | yes |

- Phases 1 and 2 are independent and run in parallel.
- Phase 3 needs both.
- aarch64 is tested on the Lima template (nested KVM).
- x86_64 is tested by vmkit's CI `contract-x86_64` job once the branch is pushed. kiln's KVM tests on x86_64 follow the repo's existing practice (§11.7).

**Known environment limit:** Cloud Hypervisor 53 under aarch64 nested virtualization freezes guests at random and after resumes (HANDOFF, `tests/pause.rs`). Firecracker is the reference VMM. CH snapshot tests run alone (like `pause.rs`) and are judged on pass rates.
