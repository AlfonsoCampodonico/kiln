# kiln-shim: exec and volumes — Implementation Design

**Date:** 2026-10-30
**Status:** Draft rev 1.
**Implements:** the first two rows of the [kiln-shim v1 design](2026-10-21-kiln-shim-v1-design.md)'s out-of-scope table (§12): exec (kiln-shim spec §4.1, milestone S1) and projected, Secret, ConfigMap, downward-API and `emptyDir` volumes (kiln-shim spec §4.5). Where this narrows or changes those, §8 says so.

---

## 1. What this delivers

- **Exec.** `kubectl exec` (with and without a TTY) and exec probes work on a kiln pod. They map to the shim's `Exec`, `Start`, `Kill`, `Wait`, `State`, `Delete`, `ResizePty` and `CloseIO` calls with an `exec_id`. The exit status reaches containerd as `TaskExit`.
- **Volumes.** These are delivered into the guest, replacing v1's "ignored with a warning" (v1 decision V7):
  - Secrets, ConfigMaps, projected volumes (including the service-account token) and the downward API arrive as files. Later changes arrive the same way kubelet makes them, with an atomic `..data` swap.
  - `emptyDir` on disk lives on a per-pod disk, so what an init container writes reaches the app.
  - `emptyDir` with `medium: Memory`, and OCI `tmpfs` mounts, become guest tmpfs.
  - A `subPath` that names one file of a file volume is delivered as that file.
- **Still refused at `Create`:** hostPath, CSI, NFS, and `subPath` directories.

### Success criteria

1. `crictl exec` of a command prints its output and returns its exit code. `crictl exec -s` (ExecSync, what probes use) fails, then passes once the command's condition holds. That is an exec readiness probe.
2. A tty exec (`crictl exec -t`) runs on a pseudo-terminal in the guest.
3. A pod gets the following inside its guest, at their mount paths:
   - a Secret, as kubelet's atomic-writer layout;
   - a ConfigMap, the same way;
   - an `emptyDir` that its init container writes and its app container reads.
4. An atomic update of the ConfigMap on the host (a new `..data`) is seen by the running app within a few seconds.
5. `cargo test` and `cargo clippy -D warnings` pass for kiln-proto, kiln-init, kiln-run and kiln-shim. `scripts/shim-e2e.sh` passes on Firecracker in the Lima VM.

---

## 2. kiln-proto

`PROTOCOL_VERSION` stays 1. The additions follow v1 decision V8's rule: each new `Config` field is omitted when empty, so a `Config` without volumes is byte-identical to today's. The new message types are sent only to a guest that is running exec or has volumes. kiln pins no kiln-init yet, so host and guest always come from one build. A version bump can wait until one is released.

### 2.1 Messages

| Type | Dir | Message | Fields |
|---|---|---|---|
| 10 | host→guest | `ExecStart` | `id: u32` (host-chosen, ≥ 1), `process: Process` (the same struct as `Config.process`), `tty: Option<WindowSize>`, `stdin: bool` |
| 11 | host→guest | `ExecSignal` | `id`, `sig` (1–31) |
| 12 | host→guest | `ExecWindowSize` | `id`, `rows`, `cols` |
| 13 | host→guest | `FileUpdate` | `volume: u32` (index into `Config.volumes`), `files: [{path, mode, uid, gid, data (base64), append}]`, `commit: bool` |
| 14 | guest→host | `ExecStarted` | `id` |
| 15 | guest→host | `ExecExited` | `id`, `signaled`, `code` (as `Exited`) |
| 16 | guest→host | `ExecFailed` | `id`, `errno?`, `message` (≤ 4 KiB, sanitised before printing, as `InitFailed`) |

**Per-message validation**, on both sides:
- a `path` is relative, its components are non-empty and not `.` or `..`, and its first component does not start with `..` (kubelet reserves those names);
- `mode` ≤ `0o7777`;
- the `Process` rules of `Config.process`;
- `id` ≥ 1.

### 2.2 `Config` additions

- **`volumes: [Volume]`**, at most 64. Each has an absolute guest `path` and a `kind`:
  - `files {file: bool}`: content from `FileUpdate`, mounted read-only. With `file`, the mount is a single file: the volume's one entry, bound onto `path`.
  - `emptyDir {name}`: the directory `name` on the pod disk.
  - `tmpfs {sizeBytes?}`: a fresh tmpfs.
- **`podDisk: Option<Scratch>`**: the pod disk, the virtio disk after the layers (`vd<2 + layers>`). The guest grows its ext4 like the scratch disk's.

### 2.3 Exec stdio: vsock port 1029

Each exec stream is a guest-initiated connection to port **1029** (`port::EXEC`). It starts with a 5-byte header: the exec `id` (u32 LE), then the stream (`0` stdin, `1` stdout, `2` stderr, `3` tty). After that it carries raw bytes, as ports 1025–1028 do, with EOF as a half-close.

The host accepts a connection only for an id it started and a stream that exec expects, at most once each. Anything else is a protocol violation (S5).
- The header has 5 s to arrive, in all.
- At any moment, the connections that have not yet sent their header may number at most the streams the host's execs still expect. Each such connection holds a thread, so a guest that opens more is a violation.

### 2.4 Limits

| Limit | Value | Enforced by |
|---|---|---|
| Concurrent execs per VM | 32 | the shim (refuses `Exec`); kiln-run (refuses the 33rd); kiln-init (`ExecFailed`) |
| Bytes in one files volume's generation | 8 MiB | the shim (refuses `Create`, logs and skips an update); kiln-init |
| Files in one generation | 1024 | same |
| Volumes per `Config` | 64 | kiln-proto |

---

## 3. kiln-init

### 3.1 Volumes

- **At `Config` (stage 2).**
  - When `Config.volumes` has files volumes, the host follows `Config` with their initial `FileUpdate`s, before any other message.
  - `control::connect` reads them synchronously, until every files volume has had a `commit`.
  - Each files volume is a detached tmpfs (`fsopen`/`fsmount`), filled in kubelet's atomic-writer layout:
    - a generation directory `..kiln.<n>`;
    - `..data` → `..kiln.<n>`;
    - for each top-level name `x`, `x` → `..data/x`.
  - Directories are `0755` root. Files get the mode, uid and gid the host sent.
- **Stage 3.** The pod disk is mounted detached as ext4, then grown online.
- **Stage 4, after `pivot_root`.** Every volume is attached at its path inside the new root:
  - `mkdir -p`; for a single file, create an empty file;
  - a clone of the detached mount (`open_tree`), then `move_mount`. The kernel clones only a detached mount's root, so the part of a mount that a volume needs (an `emptyDir`'s directory on the pod disk, or a single file) is bound from that mount after it has been attached at a staging directory, `/.kiln-volumes`. The staging mounts are then detached and the directory removed, before the process starts;
  - files volumes are then remounted read-only.

  Doing it after the pivot means a symlink in the image resolves inside the container's root, never into init's.
- **Updates (stage 7).** `FileUpdate`s write a new generation as they arrive. On `commit`, kiln-init does what kubelet's atomic writer does:
  1. points `..data_tmp` at the new generation;
  2. `rename`s it over `..data`;
  3. adds the new top-level links and removes stale ones;
  4. deletes the old generation.

  The bind is read-only, but kiln-init writes through its own detached mount.
- **Shutdown.** The pod disk is synced and made read-only before `Exited`, like the scratch disk.

### 3.2 Exec

- `ExecStart` is handled by the supervisor in stage 7. A message that arrives earlier waits in its queue.
- **Starting it.** kiln-init starts the process as it starts the main one:
  - identity from the image's passwd and group files;
  - `env::build` defaults;
  - the working directory created if missing;
  - `setsid`, plus a controlling pty with `tty`.

  It connects each stream to port 1029 with its header, then sends `ExecStarted`. A failed spawn sends `ExecFailed`, carrying the errno of a failed exec(2).
- **Signals and resizing.** `ExecSignal` and `ExecWindowSize` go to that exec's process and pty.
- **Reaping.** The supervisor reaps every child. When an exec's process ends, a thread joins its output relays and then sends `ExecExited`, so "exited" means its output is complete, as for `Exited`.
- **When the main process exits,** every exec is killed with the rest. After `Exited` no `ExecExited` is sent: one lock and a "finished" flag order them. The host reports unfinished execs as killed (137), as runc does when a container's init dies.

---

## 4. kiln-run (host session)

- `Session::start` listens on 1029 too when `Config` allows exec (always, under the shim; `SessionOptions.exec`).
- **Exec registry.** A registry shared by the listener and the control reader holds each exec's state (`sent → started → exited`, or `failed`) and its streams.
  - The control reader validates `ExecStarted`, `ExecExited` and `ExecFailed` against the registry. Each is allowed once, in order, only while the guest is running and before its final message. Anything else is a violation, which kills the VM (T9).
- **`Handle::exec(id, ExecStart, ExecStreams, Sender<ExecEvent>)`.**
  - Events: `Started`, `Failed(message)`, then `Exited(code)`.
  - `Exited` is delivered once both the guest's `ExecExited` and EOF on its output streams have arrived.
- **Other handle calls.**
  - `exec_signal` and `exec_window_size` send their messages.
  - CloseIO is the stdin reader returning EOF (a half-close), as for the main process.
- **Initial files.** `SessionOptions.after_config` holds the initial `FileUpdate`s. The control reader writes them right after `Config`, before handing the socket to the writer.
- **`Handle::update_files(Vec<HostMessage>)`.** It blocks while its frames hold half of the 64-slot queue. A large update can then never fill the queue under a `Signal`, which would count as a violation.
- **At the session's end,** every exec that has not exited gets `Exited(137)`.
- **`check_devices`** takes the extra disk count (the pod disk).

---

## 5. kiln-shim

### 5.1 Mount classification (pure)

The v1 table changes as follows:

| Source | Class | Delivered as |
|---|---|---|
| `…/volumes/kubernetes.io~{secret,configmap,projected,downward-api}/<name>` | `Files` | `files` |
| `…/volumes/kubernetes.io~empty-dir/<name>` on disk | `EmptyDir` | `emptyDir {name}` on the pod disk |
| the same on a tmpfs (kubelet's `medium: Memory`) | `Memory` | `tmpfs {sizeBytes}` = the host tmpfs's size |
| `…/volume-subpaths/<vol>/…`, a regular file of a files volume | `FilesFile` | `files {file: true}`, static |
| `…/volume-subpaths/…` that is anything else | refused | — |
| an OCI `tmpfs` mount (not `/dev/*`) | `Memory` | `tmpfs` with its `size=` |
| `/dev/termination-log` | ignored, without a warning | — |
| hostPath, CSI, NFS, other host paths | refused (as v1) | — |

### 5.2 Reading a files volume (root, in the shim)

- If `..data` exists:
  - its link target must be one `..`-prefixed name, with no `/`;
  - the shim walks that directory with `O_NOFOLLOW`, taking only regular files and directories;
  - the generation is the target name.
- Otherwise the shim walks the directory itself (generation `""`).
- The bounds are those of §2.4. A file over 8 MiB, or a symlink, fifo or device inside, refuses `Create`.
- The shim reads, not the worker, because the files are often `0400` root with fsGroup ownership.

### 5.3 Updates

- **The poll.** A thread per container worker polls each `..data`-style volume's link every 2 s from `Create` on. kubelet's own sync period is a minute.
- **On a change,** it reads the volume and sends the worker `ToWorker::Files {volume, files}`. That line may be up to 32 MiB; lines from the shim are trusted.
- **The worker** chunks the content into `FileUpdate` frames of at most 48 KiB of data. A file larger than that spans several entries (`append`). The last frame sets `commit`.
- **A failed read** (kubelet mid-swap, or over a limit) is logged and retried at the next poll.
- **Initial content** goes in the worker's plan (its cap rises to 32 MiB), and from there into `after_config`.

### 5.4 The pod disk

- **Configuration.** The setting `pod_disk = "1G"` sets its size (default 1 GiB, sparse).
- **Location.** `<store>/shim-scratch/<ns>-pod-<sandbox id>/pod.img`.
- **Creation.** The first container worker of the pod that has a disk `emptyDir` creates it, as the kiln user (`create_scratch`, the ext4 template). Later workers attach the same file. One-at-a-time (v1 §4) means two VMs never have it at once.
- **Removal.** When the pod's pause process ends (`StopPodSandbox`), the shim runs `containerd-shim-kiln-v1 cleanup <dir>`. That child drops to the kiln user and removes the directory: root deletes nothing in that user's store (v1 §8).
- **`sizeLimit`** is not enforced. kubelet enforces it by eviction, which never sees inside the VM.

### 5.5 Exec

- **`Tasks`.** Each container entry gains `execs: exec_id → ExecEntry`, with the same `Created`/`Running`/`Stopped` states.
  - `Exec` is refused unless the container is running, and beyond 32 execs. A duplicate `exec_id` is refused too.
  - Events: `TaskExecAdded` at `Exec`, `TaskExecStarted` at `Start`, `TaskExit {id: exec_id}` at its end. `Delete` of an exec publishes nothing, as runc's shim does.
  - `Wait`, `State`, `Kill`, `ResizePty` and `CloseIO` with an `exec_id` act on the exec.
- **The `pid` of an exec** is the worker's pid. A guest pid means nothing on the host, and could name an unrelated host process.
- **The spec.** `ExecProcessRequest.spec` is an OCI `Process` (JSON in an `Any`). It is translated like the main one: `args`, `env`, `cwd`, `user.uid:gid`, `terminal`.
- **Backend.** `Process::exec(&ExecLaunch) -> Arc<dyn ExecProcess>`, where `ExecProcess` has `start`, `signal`, `resize`, `close_stdin` and `wait`.
  - **Linux.** The shim opens the exec's FIFOs (as root, as for the task). It passes them to the worker with `SCM_RIGHTS` over a second socketpair (`SOCK_SEQPACKET`). Each datagram holds a 5-byte header (id, which fds) and the fds. The JSON line `ToWorker::Exec {id, process, tty, stdin}` follows.
  - The worker answers `ExecStarted`, `ExecFailed {message}` and `ExecExited {code}`. `ExecSignal`, `ExecResize` and `ExecCloseStdin` go the other way.
  - A worker that ends with execs pending reports them as 137.
- **Ids.** The shim maps containerd's `exec_id` string to a host-chosen u32 per container (S5).

---

## 6. Threat model

| # | Property | How |
|---|---|---|
| S5 | The guest cannot start an exec, forge an exec id, or take another exec's stdio | Ids are host-chosen. Every exec message and stream header is checked against the registry; a mismatch kills the VM |
| V-1 | A pod's volume cannot make the shim (root) read a host file outside kubelet's volume | Only the four atomic-writer plugins' directories under `/var/lib/kubelet/pods/<uid>/volumes/` are read. The walk uses `O_NOFOLLOW` and takes no symlinks, fifos or devices. A `subPath` is read only when its volume name is a files volume of the same pod |
| V-2 | Volume content cannot exhaust the shim or the guest | 8 MiB and 1024 files per generation, 64 volumes. Each generation is read and sent whole only on a change |
| V-3 | A symlink in the image cannot point a volume mount into init's root | Volumes are attached after `pivot_root` |

---

## 7. Testing

- **Unit tests (macOS and Linux):**
  - kiln-proto: the new messages' validation, payloads and rejection, and the stream header;
  - kiln-run: the exec registry's state machine;
  - kiln-shim:
    - classification, the volume reader (tempdir trees with a `..data` swap, symlinks, fifos, size limits) and the subPath lookup;
    - the wire messages and the plan;
    - `Tasks` exec lifecycle: events, refusals, exits on the container's end, and the limit;
    - the service's exec calls with the fake backend.
- **kiln-init (Linux):**
  - the atomic-writer layout and its swap, on a tempdir;
  - path validation.
- **kiln-run session (Linux, testguest):** the testguest gains an exec mode that answers `ExecStart` with stream connections and a forged one. That covers S5 in-process, without a VM.
- **End to end, `scripts/shim-e2e.sh`:**
  - **exec:** output and exit code, an ExecSync readiness probe that fails and then passes, and a tty exec;
  - **volumes:** a pod with a Secret, a ConfigMap and an `emptyDir`, laid out on the host as kubelet lays them out (the script plays kubelet, since crictl has none). An init container writes to the `emptyDir`; the app reads it, the Secret and the ConfigMap. Then the script swaps the ConfigMap's `..data` and waits for the app to print the new value.

---

## 8. Decisions and deviations

| # | Decision | By |
|---|---|---|
| E1 | Disk `emptyDir` is a directory on one per-pod ext4 disk, attached to each container VM in turn, so init containers' writes reach the app. Memory `emptyDir` is per-VM tmpfs. | The user, 2026-10-30 |
| E2 | File content travels in `FileUpdate` frames after `Config` (initially) and later. It does not travel inside `Config`: `Config` is one 64 KiB frame, and a Secret may be 1 MiB. | My call |
| E3 | Exec stdio uses one new vsock port (1029) with a per-connection header. It does not add a `proc` field to stdio messages: kiln's stdio is raw streams, not messages, so the kiln-shim spec's §4.1 multiplexing does not apply. | My call |
| E4 | `PROTOCOL_VERSION` stays 1. The additions are omitted when unused, and kiln pins no kiln-init yet. | My call |
| E5 | `subPath` is supported for single files of files volumes, statically, as kubelet does. `subPath` directories are refused. | My call |
| E6 | `/dev/termination-log` is ignored without a warning. Copying it back to the host is a follow-up. | My call |
| E7 | An exec's pid is the worker's pid. | My call |
| E8 | A guest's terminal connection is closed both ways when its output ends. The VMMs pass a half-close on to the host only once the connection is closed, and a terminal's input relay holds a clone of it. | Found by the e2e |
