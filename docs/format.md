# kiln formats

This document is normative. It currently defines the **erofs profile** (format version 1). The image manifest (§5.1 of the design spec) and the control protocol (§9.5) are added by milestones M1b and M3.

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

kiln is checked against containerd's overlayfs snapshotter (kiln-erofs Task 14). It deliberately differs in three cases that image builders do not produce:
1. containerd sets directory mtimes in a final pass. If a later entry in the same layer replaced a directory, or one of its parents, with a non-directory, that pass fails or re-times the replacement. kiln keeps each entry's own attributes.
2. containerd resolves an implicit directory's attributes by searching each lower layer on its own. It skips non-directories, ignores whiteouts and opaque directories, and fails on a file in the middle of the path. kiln uses the overlay view.
3. containerd follows symlinks in lower layers while resolving parents. kiln does not.

### Determinism

Output depends only on the tar content, the resolved inherited attributes and the format version. Any change to output bytes requires a new format version; golden digests for each version are immutable.
