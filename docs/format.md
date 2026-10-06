# kiln formats

This document is normative. It defines the **kiln image** (schema version 1, provisional until M3 adds kernel and init layers), the **erofs profile** (format version 1), the **guest** that `kiln-init` sets up (devices, init layer, scratch disk, boot sequence) and the **control protocol** (version 1) between `kiln-init` and the host.

## kiln image, schema version 1

A kiln image is an OCI 1.1 artifact. All JSON kiln writes is serialised with keys sorted and no insignificant whitespace, so identical inputs give identical digests.

### Index (multi-platform images)

An OCI image index (`mediaType` `application/vnd.oci.image.index.v1+json`, `schemaVersion` 2) with `artifactType` `application/vnd.kiln.image.v1`. Each entry in `manifests` is a kiln manifest descriptor with `artifactType` `application/vnd.kiln.image.v1` and a `platform` (`os`, `architecture`, optional `variant`). Entries are sorted by platform (architecture, then os, then variant). A single-platform image is a bare manifest with no index.

### Manifest

An OCI image manifest (`mediaType` `application/vnd.oci.image.manifest.v1+json`, `schemaVersion` 2) with:
- `artifactType`: `application/vnd.kiln.image.v1`. Readers reject manifests without it.
- `config`: a descriptor with `mediaType` `application/vnd.kiln.image.config.v1+json`.
- `layers`, in boot order:
  1. Kernel, `application/vnd.kiln.kernel.v1` (from M3; absent in schema-1 images built before M3).
  2. Init layer, `application/vnd.kiln.init.v1.erofs` (from M3; absent likewise).
  3. App layers, lowest first, `application/vnd.kiln.layer.v1.erofs`: erofs profile images (below), applied as overlayfs lower layers.
- App layer annotations, **informational only** (never used for caching or verification):
  - `dev.kiln.source.digests`: the comma-separated digests of the OCI layers the erofs layer was built from, in order. A squashed bottom layer lists all of them.
  - `dev.kiln.inherits`: `"true"` when the layer has implicit directories whose attributes came from the layers below it. Such a layer is only valid on top of exactly those layers.

### Config

```json
{
  "architecture": "arm64",
  "erofsFormatVersion": 1,
  "process": { "cmd": ["php", "-a"], "entrypoint": ["docker-php-entrypoint"], "env": ["PATH=/usr/local/bin:/usr/bin:/bin"], "stopSignal": "SIGQUIT", "user": "www-data", "workingDir": "/app" },
  "schemaVersion": 1,
  "source": { "manifestDigest": "sha256:…", "reference": "docker.io/library/php:8.4-cli" }
}
```
- `schemaVersion`: 1. Readers reject other values.
- `architecture`: `amd64` or `arm64`.
- `process`: copied from the source OCI config. Empty lists and strings are omitted.
- `kernel` (`{ "profile", "version" }`) and `init` (`{ "version" }`): set from M3, omitted before.
- `source.manifestDigest`: the source OCI manifest. `source.reference` is present only for registry inputs. This is unverified provenance.
- `erofsFormatVersion`: the erofs profile version of every app layer.
- There is no kernel command line field and no kiln version field.

### Local store (informative)

`$KILN_HOME` (default `~/.local/share/kiln`) holds `blobs/sha256/<hex>`, `refs.json` (`{"refs": {"<name>": "<digest>"}}`), and four caches of small text files. Each entry is written only after the layer it describes has been verified and committed:
- `cache/layers/<source layer digest>@<hex diff_id>@<format version>`: `erofs <digest>` for a layer that depends only on its tar, or `parents <JSON list of hex-encoded implicit paths>`.
- `cache/layers-ctx/<source layer digest>@<hex diff_id>@<format version>@<ctx>`: the erofs digest for a layer with implicit parents. `ctx` is the hex SHA-256 of the JSON list, in implicit-path order, of `[hex(path), null]` (absent in the lowers, or not a directory) or `[hex(path), [mode, uid, gid, mtime_sec, mtime_nsec, [[xattr_index, hex(name), hex(value)], …]]]`.
- `cache/squash/<hex SHA-256 of the newline-joined erofs digests>@<format version>`: a squashed bottom layer.
- `cache/warnings/<layer cache key>`: the JSON list of warnings converting that layer produced, so a cache hit reports them too. Removed by GC together with the `cache/layers/` entry of the same name.

Keying by the verified `diff_id` as well as the compressed digest ties a cache hit to the decompressed content the image config claims, so a blob listed with a different compression or a wrong `diff_id` misses instead of being served another image's filesystem.

`:` in a cache key is written as `_` in its file name.

## erofs profile, version 1

A kiln layer image is a Linux erofs filesystem restricted as follows. Readers reject images that violate the layout rules below (feature flags, data layouts, block size, bounds, ordering).

### Superblock (byte 1024)

| Field | Value |
|---|---|
| `magic` | `0xE0F5E1E2` |
| `checksum`, `feature_compat`, `feature_incompat` | 0 |
| `blkszbits` | 12 (4096-byte blocks) |
| `sb_extslots`, `dirblkbits`, `extra_devices`, compression fields | 0 |
| `root_nid` | 1 |
| `inos` | number of inodes |
| `epoch`, `fixed_nsec` | base time (below) |
| `blocks` | image size / 4096 |
| `meta_blkaddr` | first block of the metadata area |
| `xattr_blkaddr` | first block of the shared xattr table, or 0 if there is none |
| `uuid`, `volume_name` | all zeros |

### Layout

```
block 0               zeros, with the superblock at byte 1024
streamed file data    in tar order
external file data    squash only, in inode order
relocated file data   in inode order (blocks copied to new contiguous FLAT_PLAIN;
                      tail zero-padded; original blocks stay as unused bytes)
directory/symlink     bodies not fully inline, in inode order
xattr table           shared xattr entries, each 4-byte aligned (optional)
metadata area         32-byte slots; slot 0 is zero; inodes from nid 1, in inode order
```

The image ends at a 4096-byte boundary.

### Inodes

- **Numbering:** inodes are numbered breadth-first from the root, visiting directory entries in byte order of their names. A hardlinked inode takes its number at its first occurrence in the breadth-first walk.
- **Data layouts:** only `FLAT_PLAIN` (0) and `FLAT_INLINE` (2). `i_format` bit 4 is never set.
- **Inline tails:** a tail is inline only when it fits after the inode and its xattrs within one block.
  - For regular files, that is decided while streaming, assuming the worst case (a 64-byte inode and all xattrs inline): `tail + 64 + X ≤ 4096`, where X = 0 for a file without xattrs, and otherwise X = `12 + Σ entry sizes` with entry size = round_up(4 + name_len + value_len, 4). Thus a file with no xattrs can inline a tail of up to 4032 bytes.
  - For directories and symlinks, it is decided exactly.
- **Compact vs extended:** a 32-byte compact inode is used only when all of these hold. Otherwise the inode is 64-byte extended.
  - uid ≤ 65535 and gid ≤ 65535
  - nlink ≤ 65535
  - size < 2³²
  - mtime equals the base time, including nanoseconds

  A compact inode's `i_mtime` is 0.
- **Base time:** the minimum `(sec, nsec)` mtime of the layer's explicit directory, file, symlink, device, FIFO, whiteout and hardlink entries (opaque markers excluded). It is 0 for an empty layer. For squashed images, it is the minimum over all inodes.
- **Inode records:** a record never crosses a block boundary. When it would, it moves to the next block.
- **`i_ino`:** the 1-based inode number.

### Directories

- Every directory contains `.` and `..`. The root's `..` is the root.
- Entries are sorted in strict byte order, including `.` and `..`, and packed greedily into 4096-byte blocks of dirents followed by names. Non-last blocks are zero-padded. Readers reject entries that are not in strict byte order.
- `nlink` is 2 plus the number of subdirectories.

### Xattrs

- **Name indexes:** `user.` 1, `system.posix_acl_access` 2, `system.posix_acl_default` 3, `trusted.` 4, `security.` 6. Other namespaces are dropped with a warning, and so are `trusted.overlay.*` xattrs supplied by the tar.
- **Shared table:** an `(index, name, value)` triple goes into the shared table when two or more inodes carry it, or when an inode's all-inline body would exceed 4032 bytes (in which case all of that inode's xattrs are shared).
  - Shared entries are ordered by first use in inode order.
  - Within an inode, shared ids come first, then inline entries, each sorted by `(index, name)`.
- `h_name_filter` is 0.

### Overlay markers

- A whiteout is a character device 0:0, mode 0, uid and gid 0.
- An opaque directory carries `trusted.overlay.opaque = "y"`. Only `.wh.` entries produce markers: `trusted.overlay.*` xattrs supplied by the tar are dropped.
- When merging (parent inheritance and squash), a directory is opaque only when `trusted.overlay.opaque` is exactly `"y"`, and a root-level opaque marker has no effect on the merged view, as in overlayfs.
- Squashed images contain no markers.

### Layer semantics (applying one tar)

- **Directory over directory:** merge. The header's mode, uid, gid and mtime replace the old ones; header xattrs are added over the existing ones; children are kept.
- **Other replacements:** anything else at an existing name replaces it, along with its subtree.
- **Symlinks:** permission bits are always 0777.
- **Hardlinks:**
  - A hardlink names the inode at its target path when the link entry is read.
  - The link header's mode (except for symlinks), uid, gid, mtime and xattrs are applied to that inode.
  - It is an error if the target is missing, is a directory, is a whiteout, is the link itself, or lies inside the entry the link replaces.
- **Whiteouts:**
  - `.wh.<name>` hides `<name>` in lower layers.
  - It is an error if `<name>` already exists in the same layer. A later entry with that name replaces the whiteout.
  - `.wh..wh..opq` marks its directory opaque. On the layer root, the marker is recorded but has no effect on the merged view (overlayfs ignores it).
- **PAX records:** an empty `path`, `linkpath`, `uid`, `gid`, `size` or `mtime` value means the record is absent; the ustar header value is kept.
- **Unsupported entries** are errors:
  - Sparse entries.
  - Tar entry types other than regular file, directory, symlink, hardlink, character and block device and FIFO.
  - A PAX size that differs from the header size.
  - Devices with major > 4095 or minor > 1048575.
  - A uid or gid above 32 bits.
  - An empty symlink target.
  - Header-only entries (hardlink, symlink, directory, device, FIFO, V7 trailing-slash directory) whose tar header size is nonzero.
  - A global PAX (`g`) header carrying any key kiln acts on (path, linkpath, uid, gid, size, mtime, `SCHILY.xattr.*`, `GNU.sparse.*`, `LIBARCHIVE.xattr.*`).
  - More than one pending header of the same type (x, L or K) before one entry.
  - An entry with both a GNU long name (L) and a PAX `path`, or both a GNU long link (K) and a PAX `linkpath`.
  - An xattr name longer than 255 bytes (after its namespace prefix) or a value larger than 65535 bytes.
  - An inode that needs more than 255 shared xattrs.
  - More entries than the entries-per-layer limit, counting both tar headers and every entry the layer creates, implicit directories included.
- **Warnings** (conversion continues):
  - Xattrs outside the five namespaces above are dropped with a warning.
  - `trusted.overlay.*` xattrs supplied by the tar are dropped with a warning.
  - `LIBARCHIVE.xattr.*` records are dropped with a warning.
  - Conversion warnings are capped at 100, followed by one "N more warnings suppressed" line.

### Implicit directories

A directory created because a descendant, whiteout or opaque marker needed it inherits from the same path in the overlay of the lower layers, with `trusted.overlay.*` keys stripped. That means the topmost lower layer providing the path, where whiteouts, opaque directories and non-directories in nearer layers hide farther ones. A directory is opaque only when `trusted.overlay.opaque` is exactly `"y"`, and a root-level opaque marker hides nothing (as in overlayfs).
- **Not described in this layer:** it takes mode, uid, gid, mtime and xattrs from that lower directory. If the path is absent there, or is not a directory, it takes mode 0755, uid 0, gid 0 and mtime = base time.
- **Described later by its own header:** it keeps the header's mode, uid, gid and mtime, and the inherited xattrs lie under its own.
- **Root:** an implicit root always takes the defaults.

### Known divergences from containerd's applier

kiln is checked against containerd's overlayfs snapshotter (kiln-erofs Task 14). It deliberately differs in these cases, which image builders do not produce:
1. containerd sets directory mtimes in a final pass. If a later entry in the same layer replaced a directory, or one of its parents, with a non-directory, that pass fails or re-times the replacement. kiln keeps each entry's own attributes.
2. containerd resolves an implicit directory's attributes by searching each lower layer on its own. It skips non-directories, ignores whiteouts and opaque directories, and fails on a file in the middle of the path. kiln uses the overlay view.
3. containerd follows symlinks in lower layers while resolving parents. kiln does not.
4. A tar may carry `trusted.overlay.*` xattrs in PAX records. containerd writes them to disk, where they can forge whiteouts, opaque directories or redirects. kiln drops them with a warning; overlay markers come only from `.wh.` entries.
5. containerd ignores everything after a tar's end-of-archive marker. kiln requires the decompressed remainder to be zero padding and rejects the layer otherwise, so the bytes covered by `diff_id` mean one thing.
6. A tar header whose size field holds only NULs and spaces: Go's `archive/tar` reads it as 0; kiln rejects the layer (the `tar` crate parses sizes itself).

Other header numeric fields (mode, uid, gid, mtime, device numbers) that hold only NULs and spaces read as 0, as in Go's `archive/tar`.

### Determinism

Output depends only on the tar content, the resolved inherited attributes and the format version. Any change to output bytes requires a new format version; golden digests for each version are immutable.

## Guest

### Devices

The VM's block devices, in this order (Linux names them `vda`, `vdb`, …):
1. `vda`, read-only: the init layer.
2. `vdb`, read-write: the scratch disk.
3. `vdc` onward, read-only: the app layers, lowest first. `vdz` is followed by `vdaa`.

vsock gives the guest CID 3. The kernel command line is `root=/dev/vda ro rootfstype=erofs init=/kiln-init panic=-1 quiet loglevel=3`, followed by the VMM's console and backend parameters.

### Init layer

An erofs profile image of exactly these entries, each owned by 0:0 with mtime 0: the directories `/`, `/dev`, `/kiln`, `/proc` and `/sys` (mode 0755) and the static `kiln-init` binary at `/kiln-init` (mode 0755). The same binary always gives the same bytes.

### Scratch disk

The scratch disk starts as the ext4 template `crates/kiln-image/assets/ext4-template.img.zst` (zstd; SHA-256 `29a11ac3061b3adc119b95917100e702130eb6cc1f686981bb84f70da1370e1d`), which `assets/make-ext4-template.sh` regenerates byte for byte:
- 64 MiB, made with `mke2fs -t ext4 -b 4096 -I 256 -i 65536 -O meta_bg,^resize_inode -U 6b696c6e-7363-7261-7463-680000000001 -E hash_seed=6b696c6e-6861-7368-7365-656400000001,lazy_itable_init=1,lazy_journal_init=0,nodiscard,root_owner=0:0` from e2fsprogs 1.47.2-3+b12, with `E2FSPROGS_FAKE_TIME=1`;
- the superblock's `s_flags` set to 2 (unsigned directory hashes), which `mke2fs` would otherwise take from the build machine's `char` signedness.

The host decompresses the template into a sparse file (blocks of zeros stay holes) and extends it to the run's size, a multiple of 4096 of at least 64 MiB. The guest grows the filesystem to exactly that size.

Growing online to 64 GiB works on Firecracker. On Cloud Hypervisor 53 the resize hangs beyond roughly 8 GiB: Cloud Hypervisor offers WRITE_ZEROES on the disk, the kernel then zeroes the new inode tables through it, and that never completes. 8 GiB and kiln's default of 4 GiB are expected to work without nesting; this is unverified on bare metal. Under nested virtualization (Lima on Apple Silicon) Cloud Hypervisor 53 growth is unreliable at smaller sizes too. Cloud Hypervisor 53 guests under nested virtualization also sometimes stall, more often in vsock-heavy cases (stdio streaming, protocol abuse). Firecracker is the reference VMM.

### Differences from Docker

`kiln-init` sets the guest up much as Docker sets up a container, with these deliberate differences:
- **PID 1:** init is PID 1, as with `docker run --init`; the main process is not. An app without a SIGTERM handler therefore exits on `Shutdown` at once, with 143, where under Docker it would be ignored as PID 1 and killed after the grace period with 137.
- **`/sys` and cgroup2** are mounted read-write (`nosuid,nodev,noexec`); Docker mounts them read-only.
- **`/dev/shm`** is a tmpfs with `nosuid,nodev` and mode 1777, without `noexec` and without Docker's `size=64m`, so its size is the tmpfs default of half the guest's RAM.
- **`/dev`** is the kernel's full devtmpfs, not Docker's minimal tmpfs, so every device the guest kernel has is visible.
- **`/dev/console`** is the VM's serial console; what is written to it lands in the VM's `console.log`.

### Boot sequence

`kiln-init` runs as PID 1 through seven stages:

| Stage | Name | What it does |
|---|---|---|
| 1 | early | Mounts `proc` and `sysfs` (`nosuid,nodev,noexec`), and `devtmpfs` unless the kernel mounted it; `reboot(CAD_OFF)`, so Ctrl-Alt-Del becomes SIGINT to init. |
| 2 | control | Connects vsock port 1024, sends `Hello`, receives `Config`. |
| 3 | storage | A tmpfs at `/kiln`; app layer `n` mounted read-only (erofs) at `/kiln/layers/<n>`; the scratch disk mounted at `/kiln/rw` with `noinit_itable` and grown online with `EXT4_IOC_RESIZE_FS` (it fails if `vdb` is smaller than `Config.scratch.sizeBytes`); `upper/` and `work/` created on it; the overlay mounted at `/kiln/root` with one mount(2) call and the data `lowerdir=<top…bottom>,upperdir=/kiln/rw/upper,workdir=/kiln/rw/work,xino=on,redirect_dir=off,index=off,metacopy=off`. Without layers, the only lower directory is the empty `/kiln/empty`. |
| 4 | root | Refuses an image whose `/proc`, `/sys`, `/dev` or `/etc` exists as a symlink or another non-directory, as runc does; creates missing `/etc`, `/proc`, `/sys`, `/dev` in the overlay (they land in the upper layer); mounts `proc`, `sysfs`, `cgroup2` on `/sys/fs/cgroup` and `mqueue` on `/dev/mqueue` (each `nosuid,nodev,noexec`), `devtmpfs` (`nosuid`), `devpts` (`nosuid,noexec`, `newinstance,ptmxmode=0666,mode=0620,gid=5`) and a tmpfs on `/dev/shm` (`nosuid,nodev`, mode 1777); replaces `/dev/fd`, `/dev/stdin`, `/dev/stdout`, `/dev/stderr` and `/dev/ptmx` with symlinks to `/proc/self/fd`, `/proc/self/fd/0`, `/proc/self/fd/1`, `/proc/self/fd/2` and `pts/ptmx`; `pivot_root(".", ".")` and detaches the old root. Nothing is mounted on `/run` or `/tmp`. |
| 5 | identity | Sets the hostname; writes `/etc/hostname`, `/etc/hosts` and `/etc/resolv.conf` as new regular files (mode 0644, after unlinking whatever was there, so a symlink is replaced, never followed); brings up `lo`, and with `Config.network` brings up `eth0`, adds its address and a default route over rtnetlink. |
| 6 | process | Starts the main process (below) and sends `Running`. |
| 7 | supervise | Reaps every process and handles host messages and signals until the main process exits (below). |

`/etc/hosts` has Docker's layout: `127.0.0.1 localhost`, the IPv6 loopback and multicast names, then the guest's address (or `127.0.1.1` without a network) with the hostname. `/etc/resolv.conf` lists `Config.network.dns` as `nameserver` lines; without a network it holds only the comment `# kiln: this VM has no network`, and with a network but no DNS servers only `# kiln: no DNS servers configured`.

**The main process** (stage 6):
- **User:** `Config.process.user` is `user`, `uid`, `user:group` or `uid:gid`, resolved with runc's rules against the image's `/etc/passwd` and `/etc/group` (missing files read as empty; lines are trimmed; trailing fields may be missing, so `app:x:1000` is a group without members and a passwd line needs only its name, password, uid and gid; lines without those are skipped). A name must exist; a number need not. A matched user gets its passwd gid and home. An explicit group (a name that must exist, or any number) replaces the gid; without one, a user matched by passwd also gets every group that lists it as a member. The process's groups are the gid followed by those, without duplicates. No user means root.
- **Environment:** `Config.process.env` as given, later entries replacing earlier ones with the same key, then `PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin`, `HOSTNAME=<hostname>`, `HOME=<passwd home, else />` and, in tty mode, `TERM=xterm`, each only when not already set.
- **Working directory:** `Config.process.workingDir`, default `/`, created (mode 0755, owned by root) when missing.
- **Stdio:** without `tty`, pipes relayed to vsock ports 1025–1027 (stdin is `/dev/null` and port 1025 is not connected unless `interactive`). With `tty`, a pseudo-terminal from the guest's devpts, set to the given size and relayed to port 1028; the process gets it as its controlling terminal.
- The process runs in a new session. Then groups, gid and uid are set (all three real, effective and saved IDs), and argv (`entrypoint` followed by `cmd`) is executed, searching the environment's `PATH` when argv[0] has no `/`.

**Supervision** (stage 7):
- `Signal { sig }` is sent to the main process. `WindowSize` resizes the pseudo-terminal (ignored without `tty`).
- `Shutdown { graceSecs }`, or SIGINT or SIGTERM to init (with `Config.shutdownGraceSecs`): `stopSignal` is sent to the main process (once), and when the grace period ends every process but init gets SIGKILL. A later request can shorten the deadline, never extend it.
- When the main process exits, every other process gets SIGKILL and is reaped; then stdout and stderr (or the terminal) are relayed to EOF; then `Exited` is sent. The scratch filesystem is then synced and, on a best-effort basis, made read-only (EBUSY is ignored), and init ends the VM with `Config.exitMethod`.

**Failure:** a failing stage prints `kiln-init: <stage name>: <error>` on the console, sends `InitFailed` when the control connection is up, sends SIGKILL to every other process, and ends the VM with `Config.exitMethod`, or with a reboot before `Config` arrived. A panic does the same. A protocol error from the host, or the host closing the control connection, is a failure too: it is detected by a reader thread once `Config` has arrived and acted on when init enters its next stage, or at once during stage 7.

## Control protocol, version 1

### Transport

The guest connects to the host (vsock CID 2) on these ports, and only the guest connects. With vmkit, the host listens on the Unix socket `<vm.vsock_socket()>_<port>` for each port it serves, bound before the VM starts.

| Port | Stream | Direction | Connected |
|---|---|---|---|
| 1024 | control (framed, below) | both | stage 2, once |
| 1025 | stdin, raw bytes | host → guest | stage 6, only with `interactive` and without `tty` |
| 1026 | stdout, raw bytes | guest → host | stage 6, without `tty` |
| 1027 | stderr, raw bytes | guest → host | stage 6, without `tty` |
| 1028 | terminal, raw bytes | both | stage 6, only with `tty` |

On the raw streams EOF is a half-close: the host shuts down its writing side of 1025 when stdin ends, and the guest shuts down its writing side of 1026, 1027 or 1028 when the process's output ends. The bytes are not interpreted.

### Framing

A frame is a `u32` little-endian length, then a `u8` message type, then the payload. The length counts the type byte and the payload: at least 1 and at most 65536. The payload is exactly one JSON object: its first byte is `{` and its last `}`, with no whitespace or data around it. Unknown fields, duplicate keys, wrong types and out-of-range values are errors, as are an unknown type, a type sent in the wrong direction, a length of 0 or over 65536 (rejected before the rest is read) and EOF inside a frame. EOF before a frame's first byte closes the connection cleanly. Both sides validate a message before sending it.

### Messages

| Type | Name | Direction | Payload |
|---|---|---|---|
| 1 | `Hello` | guest → host | `{"protocol": 1}`: the protocol version, at least 1. |
| 2 | `Config` | host → guest | The run's configuration (below). |
| 3 | `Stage` | guest → host | `{"n": 3}`: init entered stage `n` (1–7). Sent for stages 3 to 7; stages 1 and 2 precede it. |
| 4 | `Running` | guest → host | `{}`: the main process started. |
| 5 | `Exited` | guest → host | `{"signaled": false, "code": 0}`: the main process ended, and its output reached EOF. `code` is the exit status (0–255), or with `signaled` the signal that killed it (1–64). |
| 6 | `InitFailed` | guest → host | `{"stage": 6, "errno": 2, "message": "…"}`: init failed at `stage` (1–7). `errno` (1–4095) is present at stage 6 only when executing the entrypoint failed; it may be present at other stages. `message` has at most 4096 bytes and is untrusted text. |
| 7 | `Shutdown` | host → guest | `{"graceSecs": 10}`. |
| 8 | `Signal` | host → guest | `{"sig": 1}`: a signal number from 1 to 31. |
| 9 | `WindowSize` | host → guest | `{"rows": 24, "cols": 80}` (each 0–65535). |

`Config` (camelCase keys; optional fields are omitted when unset, and a reader also accepts `null` for them):

| Field | Type | Rules |
|---|---|---|
| `process.entrypoint`, `process.cmd` | lists of strings (default empty) | Together the argv; it must be non-empty, with a non-empty argv[0]. |
| `process.env` | list of `KEY=value` (default empty) | `KEY` (the text before the first `=`) non-empty and without control characters; the value may be empty. The final environment: the host has already merged the image's env and overrides. |
| `process.workingDir` | string, optional | Absolute. |
| `process.user` | string, optional | `user` or `user:group`, both parts non-empty, without control characters (newline included). |
| `stopSignal` | integer | 1–31. |
| `tty` | `{rows, cols}`, optional | Run on a pseudo-terminal of this initial size. |
| `interactive` | boolean | Relay stdin. |
| `hostname` | string | 1–64 characters of `[A-Za-z0-9.-]`, the first alphanumeric. |
| `network` | object, optional | `eth0`: `address` and `gateway` (dotted IPv4) in the same `/prefixLen` (8–30), the address neither the gateway nor the subnet's first or last address; `dns`: at most 3 IPv4 addresses. |
| `layers` | integer | The number of app layers, 0–128. |
| `scratch.sizeBytes` | integer | A multiple of 4096, at least 67108864. |
| `exitMethod` | `"reboot"` or `"poweroff"` | How the guest ends the VM (vmkit's `guest_exit` capability). |
| `shutdownGraceSecs` | integer | The grace period for SIGINT or SIGTERM to init. |

No string may contain a NUL byte.

The whole `Config` is one frame, and a frame is at most 65536 bytes including the type byte, so the JSON payload (argv, environment, hostname and every other field, with JSON escaping) must fit in 65535 bytes. Docker allows about 2 MiB for argv and environment (ARG_MAX). A larger `Config` fails with an oversize error when the host sends it, after the VM has booted.

### Session

1. The guest connects port 1024 and sends `Hello`. The host answers the first `Hello` with `Config`, once per VM.
2. The guest sends `Stage` for stages 3 to 7, `Running` once the main process has started, then `Exited`, or `InitFailed` at any point. The host sends `Shutdown`, `Signal` and `WindowSize` at any time after `Config`.

The host kills the VM, and reports 125, on any violation: a second connection to port 1024, a second `Hello` (a reset guest starting over), any message before `Hello`, a protocol version it does not support, or any framing or payload error. The guest treats a framing or payload error, a second `Config`, or the host closing port 1024 as a failure (InitFailed, then the VM ends).

### Exit codes

The host reports, as Docker does:
- after `Exited`: `code`, or `128 + code` when `signaled`;
- after `InitFailed` at stage 6: 127 when `errno` is 2 (ENOENT: the entrypoint was not found), 126 when `errno` is 13 or 21 (EACCES, EISDIR: found but not invokable);
- after any other `InitFailed` (an unknown user or group, setgroups, ENOEXEC, ENOTDIR, any earlier stage), a protocol violation, a boot timeout, or a VM that ended without either message: 125.

Signals are numbered as on Linux aarch64 and x86_64 (1 SIGHUP to 31 SIGSYS). Real-time signals are not supported: an image whose `STOPSIGNAL` is one (`SIGRTMIN+3`, or 32–64) is refused with an error that says so.
