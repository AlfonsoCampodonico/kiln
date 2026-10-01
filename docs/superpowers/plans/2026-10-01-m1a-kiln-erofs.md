# M1a: `kiln-erofs` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `kiln-erofs`, a pure-Rust library that converts one OCI layer tar into one deterministic erofs image, reads such images back, resolves inherited parent directories, and squashes layer stacks. It is the foundation of `kiln convert`.

**Architecture:**
- **Writer.** A single pass streams tar entries:
  - Regular-file data goes straight into block-aligned extents of the output.
  - Small tails go to a spill file.
  - An in-memory metadata tree applies the layer semantics.
- **`finish`.** Lays out directories, shared xattrs and inodes after the data, then writes the superblock last.
- **Reader.** The same on-disk structures, read back. It powers squash (erofs in, erofs out) and parent inheritance (a merged lookup across lower layers).
- **Verification.** Correctness is pinned three ways:
  - property tests against an independent model;
  - golden digests;
  - on Linux CI: `fsck.erofs`, kernel mounts, and a containerd-applied overlayfs oracle.

**Tech Stack:**
- Rust 2024 edition, Cargo workspace.
- Runtime dependencies: `tar` (raw header parsing only), `thiserror`, `tempfile`.
- Dev dependencies: `proptest`, `sha2`, `xattr` (Linux only).
- Oracle helper: Go with `github.com/containerd/containerd/v2/pkg/archive`.

**Spec:** `docs/superpowers/specs/2026-09-30-kiln-design.md` (rev 2). This plan implements §7 (the erofs profile), §6.3 (implicit parents), §6.4 (squash), §6.6 (determinism) and §11.1–§11.2 (tests). It also covers the `kiln-erofs` part of §12 M1. Plan **M1b** (separate) builds `kiln-store`, `kiln-registry`, `kiln-oci`, the verified pipeline and the CLI on top of this crate.

## Global Constraints

- **Repository and license:** the repo is `/Users/alfonso/Github/Personal/playground/kiln`, licensed Apache-2.0.
- **Format:**
  - Block size 4096. Uncompressed. Data layouts `FLAT_PLAIN` (0) and `FLAT_INLINE` (2) only.
  - Superblock `feature_compat` = 0 and `feature_incompat` = 0. UUID is all zeros, volume name is empty, no checksum.
  - `FORMAT_VERSION` = 1. Any change to output bytes must bump it, and CI enforces this through the golden digests.
- **Inodes:** a compact inode is used only when all of the following hold. Otherwise it is extended.
  - uid ≤ 65535 and gid ≤ 65535
  - nlink ≤ 65535
  - size < 2³²
  - mtime equals the superblock base time, including nanoseconds
- **Base time:** the minimum `(sec, nsec)` mtime of the layer's explicit inode-creating entries, or 0 for an empty layer.
- **Determinism:**
  - Inodes are numbered breadth-first in byte order of names.
  - The root is the first inode at nid 1. nid 0 is never used.
  - Hardlink groups are numbered by first appearance, and their canonical path is the first one in tar order.
  - Inline xattrs are sorted by (index, name).
  - An xattr goes into the shared table when it appears on two or more inodes. Shared xattrs are ordered by first use.
  - All padding is zeros. The image is padded to a 4096 multiple.
- **Default limits:**

  | Limit | Default |
  |---|---|
  | Uncompressed bytes per layer | 16 GiB |
  | Entries per layer | 2,000,000 |
  | PAX record, long name or link, single xattr value | 1 MiB |
  | Path length | 4096 bytes |
  | Path depth | 256 components |

  The per-image limit (64 GiB) and the expansion ratio (200) belong to M1b, because they need compressed sizes.
- **Traversals** are iterative, never recursive.
- **Portability:** the library must build and pass its non-Linux tests natively on macOS. Linux-only checks are gated with `#[cfg(target_os = "linux")]` and environment variables.
- **Safety:** the crate uses `#![forbid(unsafe_code)]`.
- **Formatting:** `rustfmt.toml` sets `max_width = 120`. Run `cargo fmt --all` before every commit; CI runs `cargo fmt --all --check`. The plan's code is correct but not pre-formatted.
- **Commits:** end each commit message with these trailer lines:
  ```
  Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01P7rdQAESQYNEo2qFobUNxt
  ```

## Review Focus

These are inputs that no task's main tests target but that real users will hit. Each has a pinned test in the task named.

1. **Truncated tar stream mid-file** (a network cut): must return `Error::MalformedTar`, never panic or produce an image. Pinned in Task 8 (`truncated_tar_is_malformed`).
2. **Non-UTF-8 file names** (Latin-1 bytes from old images): must round-trip byte-exact. Pinned in Task 9 (`non_utf8_names_round_trip`).
3. **Names that sort before `.`** (`!`, `#`, `+`, `-`): directory blocks must stay strictly sorted with `.` and `..` placed by byte order, so kernel binary search works. Pinned in Task 9 (`names_sorting_before_dot`).
4. **Pre-1970 or zero mtimes:** negative PAX mtimes must round-trip and force extended inodes. Pinned in Task 9 (`negative_mtime_round_trips`).
5. **Bytes after the end-of-archive marker:** `append_tar` must stop at the marker and leave the rest unread, because M1b drains and digests the remainder. Pinned in Task 8 (`stops_at_end_of_archive`).

## File Structure

```
kiln/
  Cargo.toml                         # workspace
  LICENSE                            # Apache-2.0 text
  .gitignore
  .github/workflows/ci.yml           # macOS + Linux; Linux adds erofs-utils, kernel and oracle jobs
  scripts/run-root-test.sh           # builds a test binary as the user, runs it under sudo
  docs/format.md                     # normative erofs profile (Task 12)
  tools/oracle/{go.mod,main.go}      # containerd archive.Apply wrapper (Task 15)
  crates/kiln-erofs/
    Cargo.toml
    src/lib.rs                       # module wiring and public re-exports
    src/error.rs                     # Error, Result
    src/limits.rs                    # Limits
    src/ondisk.rs                    # on-disk constants and encode/decode
    src/path.rs                      # tar path normalization
    src/pax.rs                       # PAX records and overrides
    src/tree.rs                      # in-memory tree: Node, Kind, Meta, Timestamp, XattrKey
    src/apply.rs                     # §7.4 layer semantics: LayerBuilder
    src/tarstream.rs                 # raw tar → Entry events, with limits
    src/layout.rs                    # dir packing, xattr planning, nid assignment, inline rules
    src/writer.rs                    # DataStore, LayerWriter, emit()
    src/reader.rs                    # Image, InodeInfo, DirEntry, DataReader
    src/merge.rs                     # resolve_inherited, squash
    src/testtar.rs                   # byte-exact tar builder (doc-hidden; shared with M1b fixtures)
    tests/common/mod.rs              # convert(), walk(), stacks, fixtures, Linux mount helpers
    tests/writer.rs                  # image structure, truncation, end-of-archive contract
    tests/roundtrip.rs               # writer → reader
    tests/stack.rs                   # inheritance and squash
    tests/model/mod.rs               # independent model of §7.4 and overlay semantics
    tests/model_props.rs             # proptest: random stacks vs model
    tests/golden.rs                  # determinism goldens
    tests/golden/digests-v1.txt      # immutable once merged
    tests/kernel.rs                  # Linux: fsck.erofs, loop mount, overlay
    tests/oracle.rs                  # Linux: containerd oracle
```

---

### Task 1: Workspace, crate skeleton, errors, limits, CI

**Files:**
- Create: `Cargo.toml`, `.gitignore`, `rustfmt.toml`, `LICENSE`, `.github/workflows/ci.yml`
- Create: `crates/kiln-erofs/Cargo.toml`, `crates/kiln-erofs/src/lib.rs`, `crates/kiln-erofs/src/error.rs`, `crates/kiln-erofs/src/limits.rs`

**Interfaces:**
- Produces: `kiln_erofs::{Error, Result, Limits}`.
  - `Error` variants: `Io`, `MalformedTar(String)`, `PathEscapesRoot { path }`, `InvalidPath { path, reason }`, `UnsupportedEntry { path, kind }`, `ParentNotDirectory { path }`, `InvalidHardlink { path, target, reason }`, `LimitExceeded { limit, max, path }`, `XattrUnencodable { path, name, reason }`, `TooManyXattrs`, `ProfileViolation(String)`, `Corrupt(String)`.
  - `Limits { max_layer_bytes: u64, max_entries: u64, max_header_record: u64, max_path_len: usize, max_path_depth: usize }`, with `Default` set to the spec values.
  - `pub(crate) fn lossy(&[u8]) -> String`.

- [ ] **Step 1: Create the workspace files**

`Cargo.toml`:
```toml
[workspace]
resolver = "3"
members = ["crates/*"]

[workspace.package]
edition = "2024"
license = "Apache-2.0"
```

`.gitignore`:
```
/target
```

`rustfmt.toml`:
```
max_width = 120
```

Fetch the license text:
```bash
cd /Users/alfonso/Github/Personal/playground/kiln
curl -sfL https://www.apache.org/licenses/LICENSE-2.0.txt -o LICENSE && head -3 LICENSE
```
Expected: the first lines include `Apache License` and `Version 2.0, January 2004`.

`crates/kiln-erofs/Cargo.toml`:
```toml
[package]
name = "kiln-erofs"
version = "0.1.0"
edition.workspace = true
license.workspace = true
description = "Deterministic erofs writer and reader for kiln microVM images"

[dependencies]

[dev-dependencies]
```

Then add dependencies (Cargo picks the current versions):
```bash
cargo add -p kiln-erofs thiserror tar tempfile
cargo add -p kiln-erofs --dev proptest sha2
```

- [ ] **Step 2: Write the failing test for `Limits::default`**

`crates/kiln-erofs/src/limits.rs`:
```rust
/// Resource limits for converting one untrusted layer (spec §7.6, threat T3).
///
/// Per-image byte limits and the expansion-ratio limit need the compressed size
/// and are enforced by the pipeline (M1b), not here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// Maximum uncompressed tar bytes consumed for one layer.
    pub max_layer_bytes: u64,
    /// Maximum filesystem entries in one layer.
    pub max_entries: u64,
    /// Maximum size of one PAX header, GNU long name or link, or xattr value.
    pub max_header_record: u64,
    /// Maximum path length in bytes.
    pub max_path_len: usize,
    /// Maximum number of path components.
    pub max_path_depth: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_spec() {
        let l = Limits::default();
        assert_eq!(l.max_layer_bytes, 16 << 30);
        assert_eq!(l.max_entries, 2_000_000);
        assert_eq!(l.max_header_record, 1 << 20);
        assert_eq!(l.max_path_len, 4096);
        assert_eq!(l.max_path_depth, 256);
    }
}
```

`crates/kiln-erofs/src/lib.rs`:
```rust
//! Deterministic erofs writer and reader implementing kiln's erofs profile
//! (spec §7). One OCI layer tar becomes one erofs image.
#![forbid(unsafe_code)]
// Modules are wired together incrementally; Task 10 removes this allowance.
#![allow(dead_code)]

mod error;
mod limits;

pub use error::{Error, Result};
pub use limits::Limits;
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p kiln-erofs defaults_match_spec`
Expected: compile error, either `no function or associated item named 'default'` or a missing `error` module.

- [ ] **Step 4: Implement `Error` and `Limits::default`**

`crates/kiln-erofs/src/error.rs`:
```rust
use thiserror::Error;

/// Errors from converting or reading erofs layers.
#[derive(Debug, Error)]
pub enum Error {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("malformed tar: {0}")]
    MalformedTar(String),
    #[error("path {path:?} escapes the layer root")]
    PathEscapesRoot { path: String },
    #[error("invalid path {path:?}: {reason}")]
    InvalidPath { path: String, reason: &'static str },
    #[error("unsupported tar entry at {path:?}: {kind}")]
    UnsupportedEntry { path: String, kind: String },
    #[error("parent of {path:?} is not a directory")]
    ParentNotDirectory { path: String },
    #[error("invalid hardlink {path:?} -> {target:?}: {reason}")]
    InvalidHardlink { path: String, target: String, reason: &'static str },
    #[error("limit exceeded: {limit} (max {max}) at {path:?}")]
    LimitExceeded { limit: &'static str, max: u64, path: String },
    #[error("xattr {name:?} on {path:?} cannot be encoded: {reason}")]
    XattrUnencodable { path: String, name: String, reason: &'static str },
    #[error("an inode needs more than 255 shared xattrs")]
    TooManyXattrs,
    #[error("not a kiln-profile erofs image: {0}")]
    ProfileViolation(String),
    #[error("corrupt erofs image: {0}")]
    Corrupt(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Renders raw path bytes for error messages.
pub(crate) fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}
```

Add to `limits.rs`, above the tests module:
```rust
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_layer_bytes: 16 << 30,
            max_entries: 2_000_000,
            max_header_record: 1 << 20,
            max_path_len: 4096,
            max_path_depth: 256,
        }
    }
}
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p kiln-erofs defaults_match_spec`
Expected: `test limits::tests::defaults_match_spec ... ok`

- [ ] **Step 6: Add CI**

`.github/workflows/ci.yml`:
```yaml
name: ci
on: [push, pull_request]
jobs:
  test:
    strategy:
      matrix:
        os: [ubuntu-24.04, macos-15]
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: clippy, rustfmt
      - run: cargo fmt --all --check
      - run: cargo clippy --all-targets -- -D warnings
      - run: cargo test --all
```

Run locally: `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test --all`
Expected: all three succeed.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock .gitignore rustfmt.toml LICENSE .github crates/kiln-erofs
git commit -m "feat(erofs): workspace, crate skeleton, errors and limits

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P7rdQAESQYNEo2qFobUNxt"
```

---

### Task 2: On-disk structures

**Files:**
- Create: `crates/kiln-erofs/src/ondisk.rs`
- Modify: `crates/kiln-erofs/src/lib.rs` (add `pub mod ondisk;`)

**Interfaces:**
- Produces (all `pub` in `kiln_erofs::ondisk`):
  - **Constants:** `BLOCK_SIZE: u64 = 4096`, `BLKSZ_BITS: u8 = 12`, `SUPER_OFFSET: u64 = 1024`, `SUPER_MAGIC: u32 = 0xE0F5_E1E2`, `SUPER_LEN: usize = 128`, `SLOT_SIZE: u64 = 32`, `COMPACT_INODE_LEN = 32`, `EXTENDED_INODE_LEN = 64`, `DIRENT_LEN = 12`, `XATTR_IBODY_HEADER_LEN = 12`, `XATTR_ENTRY_HEADER_LEN = 4`, `NAME_LEN_MAX = 255`, `MAX_SHARED_XATTRS = 255`, `LAYOUT_FLAT_PLAIN: u16 = 0`, `LAYOUT_FLAT_INLINE: u16 = 2`.
  - **File types:** `FT_REG_FILE=1`, `FT_DIR=2`, `FT_CHRDEV=3`, `FT_BLKDEV=4`, `FT_FIFO=5`, `FT_SYMLINK=7`.
  - **Mode bits:** `S_IFMT`, `S_IFDIR`, `S_IFREG`, `S_IFLNK`, `S_IFCHR`, `S_IFBLK`, `S_IFIFO`.
  - **Xattr indexes:** `XATTR_INDEX_USER=1`, `XATTR_INDEX_POSIX_ACL_ACCESS=2`, `XATTR_INDEX_POSIX_ACL_DEFAULT=3`, `XATTR_INDEX_TRUSTED=4`, `XATTR_INDEX_SECURITY=6`.
  - **Superblock:** `struct SuperBlock { root_nid: u16, inos: u64, epoch: u64, fixed_nsec: u32, blocks: u32, meta_blkaddr: u32, xattr_blkaddr: u32 }` with `encode() -> [u8; 128]` and `decode(&[u8]) -> Result<SuperBlock>`.
  - **Inode:** `struct DiskInode { extended: bool, layout: u16, xattr_icount: u16, mode: u16, nlink: u32, size: u64, mtime: u64, mtime_nsec: u32, i_u: u32, ino: u32, uid: u32, gid: u32 }` with `encoded_len()`, `encode() -> Vec<u8>` and `decode(&[u8]) -> Result<DiskInode>`.
  - **Helpers:** `encode_dirent(nid, nameoff, file_type, &mut Vec<u8>)`, `decode_dirent(&[u8]) -> (u64, u16, u8)`, `xattr_entry_len(name_len, value_len) -> usize`, `encode_xattr_entry(index, name, value, &mut Vec<u8>)`, `encode_xattr_ibody_header(shared_count, &mut Vec<u8>)`, `xattr_ibody_len(icount) -> usize`, `xattr_icount_for(ibody_len) -> u16`, `encode_rdev(major, minor) -> u32`, `decode_rdev(u32) -> (u32, u32)`, `round_up(u64, u64) -> u64`.

The offsets below come from Linux `fs/erofs/erofs_fs.h`:
- **Superblock (offsets from byte 1024):** magic 0, checksum 4, feature_compat 8, blkszbits 12, sb_extslots 13, rootnid_2b 14, inos 16, epoch 24, fixed_nsec 32, blocks_lo 36, meta_blkaddr 40, xattr_blkaddr 44, uuid 48, volume_name 64, feature_incompat 80, available_compr_algs 84, extra_devices 86, devt_slotoff 88, dirblkbits 90.
- **Compact inode:** i_format 0, i_xattr_icount 2, i_mode 4, i_nb/nlink 6, i_size(u32) 8, i_mtime(u32) 12, i_u 16, i_ino 20, i_uid(u16) 24, i_gid(u16) 26, reserved 28.
- **Extended inode:** i_format 0, i_xattr_icount 2, i_mode 4, i_nb 6, i_size(u64) 8, i_u 16, i_ino 20, i_uid 24, i_gid 28, i_mtime(u64) 32, i_mtime_nsec 40, i_nlink 44, reserved 48..64.
- **i_format bits:** bit 0 = extended, bits 1–3 = data layout. Bit 4 (`NLINK_1`/`DOT_OMITTED`) is outside kiln's profile.

- [ ] **Step 1: Write the failing tests**

Create `crates/kiln-erofs/src/ondisk.rs` containing only the tests module for now:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn superblock_round_trips_and_has_kernel_offsets() {
        let sb = SuperBlock {
            root_nid: 1,
            inos: 7,
            epoch: 1_700_000_000,
            fixed_nsec: 5,
            blocks: 9,
            meta_blkaddr: 3,
            xattr_blkaddr: 2,
        };
        let b = sb.encode();
        assert_eq!(&b[0..4], &[0xE2, 0xE1, 0xF5, 0xE0]);
        assert_eq!(b[12], 12);
        assert_eq!(u16::from_le_bytes([b[14], b[15]]), 1);
        assert_eq!(u32::from_le_bytes(b[36..40].try_into().unwrap()), 9);
        assert_eq!(&b[48..80], &[0u8; 32], "uuid and volume name must be zero");
        assert_eq!(SuperBlock::decode(&b).unwrap(), sb);
    }

    #[test]
    fn superblock_rejects_features_outside_profile() {
        let mut b = SuperBlock::default().encode();
        b[80] = 1; // feature_incompat
        assert!(matches!(SuperBlock::decode(&b), Err(Error::ProfileViolation(_))));
        let mut b = SuperBlock::default().encode();
        b[8] = 1; // feature_compat (checksum)
        assert!(matches!(SuperBlock::decode(&b), Err(Error::ProfileViolation(_))));
        let mut b = SuperBlock::default().encode();
        b[0] = 0;
        assert!(matches!(SuperBlock::decode(&b), Err(Error::ProfileViolation(_))));
    }

    fn sample(extended: bool) -> DiskInode {
        DiskInode {
            extended,
            layout: LAYOUT_FLAT_INLINE,
            xattr_icount: 3,
            mode: (S_IFREG | 0o644) as u16,
            nlink: 2,
            size: 10_000,
            mtime: if extended { 1_700_000_123 } else { 0 },
            mtime_nsec: if extended { 77 } else { 0 },
            i_u: 42,
            ino: 5,
            uid: 1000,
            gid: 1001,
        }
    }

    #[test]
    fn compact_inode_offsets() {
        let b = sample(false).encode();
        assert_eq!(b.len(), 32);
        assert_eq!(u16::from_le_bytes([b[0], b[1]]), LAYOUT_FLAT_INLINE << 1);
        assert_eq!(u16::from_le_bytes([b[6], b[7]]), 2, "nlink");
        assert_eq!(u32::from_le_bytes(b[8..12].try_into().unwrap()), 10_000);
        assert_eq!(u32::from_le_bytes(b[16..20].try_into().unwrap()), 42);
        assert_eq!(u16::from_le_bytes([b[24], b[25]]), 1000);
        assert_eq!(u16::from_le_bytes([b[26], b[27]]), 1001);
        assert_eq!(DiskInode::decode(&b).unwrap(), sample(false));
    }

    #[test]
    fn extended_inode_offsets() {
        let b = sample(true).encode();
        assert_eq!(b.len(), 64);
        assert_eq!(u16::from_le_bytes([b[0], b[1]]), 1 | (LAYOUT_FLAT_INLINE << 1));
        assert_eq!(u64::from_le_bytes(b[8..16].try_into().unwrap()), 10_000);
        assert_eq!(u64::from_le_bytes(b[32..40].try_into().unwrap()), 1_700_000_123);
        assert_eq!(u32::from_le_bytes(b[40..44].try_into().unwrap()), 77);
        assert_eq!(u32::from_le_bytes(b[44..48].try_into().unwrap()), 2, "nlink");
        assert_eq!(DiskInode::decode(&b).unwrap(), sample(true));
    }

    #[test]
    fn inode_rejects_bits_outside_profile() {
        let mut b = sample(false).encode();
        b[0] |= 1 << 4;
        assert!(matches!(DiskInode::decode(&b), Err(Error::ProfileViolation(_))));
        let mut b = sample(false).encode();
        b[0] = 1 << 1; // layout 1: compressed
        assert!(matches!(DiskInode::decode(&b), Err(Error::ProfileViolation(_))));
    }

    #[test]
    fn rdev_matches_linux_new_encode_dev() {
        assert_eq!(encode_rdev(8, 1), 0x801);
        assert_eq!(decode_rdev(encode_rdev(259, 65_536)), (259, 65_536));
        assert_eq!(decode_rdev(encode_rdev(0, 0)), (0, 0));
    }

    #[test]
    fn xattr_sizes() {
        assert_eq!(xattr_entry_len(0, 0), 4);
        assert_eq!(xattr_entry_len(3, 1), 8);
        assert_eq!(xattr_entry_len(5, 0), 12);
        assert_eq!(xattr_ibody_len(0), 0);
        assert_eq!(xattr_ibody_len(1), 12);
        assert_eq!(xattr_ibody_len(3), 20);
        for len in [0usize, 12, 16, 40] {
            assert_eq!(xattr_ibody_len(xattr_icount_for(len)), len);
        }
        let mut v = vec![];
        encode_xattr_entry(XATTR_INDEX_USER, b"abc", b"z", &mut v);
        assert_eq!(v, vec![3, 1, 1, 0, b'a', b'b', b'c', b'z']);
    }

    #[test]
    fn dirent_round_trips() {
        let mut v = vec![];
        encode_dirent(0x1122_3344_5566, 24, FT_DIR, &mut v);
        assert_eq!(v.len(), DIRENT_LEN);
        assert_eq!(decode_dirent(&v), (0x1122_3344_5566, 24, FT_DIR));
    }
}
```

Add `pub mod ondisk;` to `lib.rs` after `mod limits;`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p kiln-erofs ondisk`
Expected: compile errors such as `cannot find type SuperBlock`.

- [ ] **Step 3: Implement**

Prepend to `ondisk.rs`:
```rust
//! erofs on-disk structures (Linux `fs/erofs/erofs_fs.h`), restricted to kiln's profile.

use crate::error::{Error, Result};

pub const BLOCK_SIZE: u64 = 4096;
pub const BLKSZ_BITS: u8 = 12;
pub const SUPER_OFFSET: u64 = 1024;
pub const SUPER_MAGIC: u32 = 0xE0F5_E1E2;
pub const SUPER_LEN: usize = 128;
pub const SLOT_SIZE: u64 = 32;
pub const COMPACT_INODE_LEN: usize = 32;
pub const EXTENDED_INODE_LEN: usize = 64;
pub const DIRENT_LEN: usize = 12;
pub const XATTR_IBODY_HEADER_LEN: usize = 12;
pub const XATTR_ENTRY_HEADER_LEN: usize = 4;
pub const NAME_LEN_MAX: usize = 255;
pub const MAX_SHARED_XATTRS: usize = 255;

pub const LAYOUT_FLAT_PLAIN: u16 = 0;
pub const LAYOUT_FLAT_INLINE: u16 = 2;

pub const FT_REG_FILE: u8 = 1;
pub const FT_DIR: u8 = 2;
pub const FT_CHRDEV: u8 = 3;
pub const FT_BLKDEV: u8 = 4;
pub const FT_FIFO: u8 = 5;
pub const FT_SYMLINK: u8 = 7;

pub const S_IFMT: u32 = 0o170000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFREG: u32 = 0o100000;
pub const S_IFLNK: u32 = 0o120000;
pub const S_IFCHR: u32 = 0o020000;
pub const S_IFBLK: u32 = 0o060000;
pub const S_IFIFO: u32 = 0o010000;

pub const XATTR_INDEX_USER: u8 = 1;
pub const XATTR_INDEX_POSIX_ACL_ACCESS: u8 = 2;
pub const XATTR_INDEX_POSIX_ACL_DEFAULT: u8 = 3;
pub const XATTR_INDEX_TRUSTED: u8 = 4;
pub const XATTR_INDEX_SECURITY: u8 = 6;

fn put_u16(b: &mut [u8], off: usize, v: u16) {
    b[off..off + 2].copy_from_slice(&v.to_le_bytes());
}
fn put_u32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
fn put_u64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}
fn get_u16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(b[off..off + 2].try_into().expect("2 bytes"))
}
fn get_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().expect("4 bytes"))
}
fn get_u64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().expect("8 bytes"))
}

pub fn round_up(v: u64, align: u64) -> u64 {
    v.div_ceil(align) * align
}

/// The fields of `struct erofs_super_block` that kiln writes; all others are zero.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SuperBlock {
    pub root_nid: u16,
    pub inos: u64,
    pub epoch: u64,
    pub fixed_nsec: u32,
    pub blocks: u32,
    pub meta_blkaddr: u32,
    pub xattr_blkaddr: u32,
}

impl SuperBlock {
    pub fn encode(&self) -> [u8; SUPER_LEN] {
        let mut b = [0u8; SUPER_LEN];
        put_u32(&mut b, 0, SUPER_MAGIC);
        b[12] = BLKSZ_BITS;
        put_u16(&mut b, 14, self.root_nid);
        put_u64(&mut b, 16, self.inos);
        put_u64(&mut b, 24, self.epoch);
        put_u32(&mut b, 32, self.fixed_nsec);
        put_u32(&mut b, 36, self.blocks);
        put_u32(&mut b, 40, self.meta_blkaddr);
        put_u32(&mut b, 44, self.xattr_blkaddr);
        b
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        if b.len() < SUPER_LEN {
            return Err(Error::Corrupt("superblock truncated".into()));
        }
        let violation = |what: String| Err(Error::ProfileViolation(what));
        if get_u32(b, 0) != SUPER_MAGIC {
            return violation("bad superblock magic".into());
        }
        if get_u32(b, 8) != 0 {
            return violation(format!("feature_compat {:#x}", get_u32(b, 8)));
        }
        if b[12] != BLKSZ_BITS {
            return violation(format!("block size 2^{}", b[12]));
        }
        if get_u32(b, 80) != 0 {
            return violation(format!("feature_incompat {:#x}", get_u32(b, 80)));
        }
        if get_u16(b, 84) != 0 || get_u16(b, 86) != 0 || b[90] != 0 {
            return violation("compression, extra devices or dirblkbits".into());
        }
        Ok(Self {
            root_nid: get_u16(b, 14),
            inos: get_u64(b, 16),
            epoch: get_u64(b, 24),
            fixed_nsec: get_u32(b, 32),
            blocks: get_u32(b, 36),
            meta_blkaddr: get_u32(b, 40),
            xattr_blkaddr: get_u32(b, 44),
        })
    }
}

/// A compact (32-byte) or extended (64-byte) on-disk inode.
///
/// For compact inodes `mtime` is the offset from the superblock epoch (kiln always
/// writes 0) and `mtime_nsec` is unused; for extended inodes both are absolute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskInode {
    pub extended: bool,
    pub layout: u16,
    pub xattr_icount: u16,
    pub mode: u16,
    pub nlink: u32,
    pub size: u64,
    pub mtime: u64,
    pub mtime_nsec: u32,
    pub i_u: u32,
    pub ino: u32,
    pub uid: u32,
    pub gid: u32,
}

impl DiskInode {
    pub fn encoded_len(&self) -> usize {
        if self.extended { EXTENDED_INODE_LEN } else { COMPACT_INODE_LEN }
    }

    pub fn encode(&self) -> Vec<u8> {
        let format = u16::from(self.extended) | (self.layout << 1);
        let mut b = vec![0u8; self.encoded_len()];
        put_u16(&mut b, 0, format);
        put_u16(&mut b, 2, self.xattr_icount);
        put_u16(&mut b, 4, self.mode);
        if self.extended {
            put_u64(&mut b, 8, self.size);
            put_u32(&mut b, 16, self.i_u);
            put_u32(&mut b, 20, self.ino);
            put_u32(&mut b, 24, self.uid);
            put_u32(&mut b, 28, self.gid);
            put_u64(&mut b, 32, self.mtime);
            put_u32(&mut b, 40, self.mtime_nsec);
            put_u32(&mut b, 44, self.nlink);
        } else {
            put_u16(&mut b, 6, self.nlink as u16);
            put_u32(&mut b, 8, self.size as u32);
            put_u32(&mut b, 12, self.mtime as u32);
            put_u32(&mut b, 16, self.i_u);
            put_u32(&mut b, 20, self.ino);
            put_u16(&mut b, 24, self.uid as u16);
            put_u16(&mut b, 26, self.gid as u16);
        }
        b
    }

    /// Decodes an inode; `b` must hold 32 bytes, or 64 for an extended inode.
    pub fn decode(b: &[u8]) -> Result<Self> {
        if b.len() < COMPACT_INODE_LEN {
            return Err(Error::Corrupt("inode truncated".into()));
        }
        let format = get_u16(b, 0);
        if format & !0x0F != 0 {
            return Err(Error::ProfileViolation(format!("i_format {format:#x}")));
        }
        let extended = format & 1 == 1;
        let layout = (format >> 1) & 0x7;
        if layout != LAYOUT_FLAT_PLAIN && layout != LAYOUT_FLAT_INLINE {
            return Err(Error::ProfileViolation(format!("data layout {layout}")));
        }
        let common = (get_u16(b, 2), get_u16(b, 4));
        if extended {
            if b.len() < EXTENDED_INODE_LEN {
                return Err(Error::Corrupt("extended inode truncated".into()));
            }
            Ok(Self {
                extended,
                layout,
                xattr_icount: common.0,
                mode: common.1,
                nlink: get_u32(b, 44),
                size: get_u64(b, 8),
                mtime: get_u64(b, 32),
                mtime_nsec: get_u32(b, 40),
                i_u: get_u32(b, 16),
                ino: get_u32(b, 20),
                uid: get_u32(b, 24),
                gid: get_u32(b, 28),
            })
        } else {
            Ok(Self {
                extended,
                layout,
                xattr_icount: common.0,
                mode: common.1,
                nlink: u32::from(get_u16(b, 6)),
                size: u64::from(get_u32(b, 8)),
                mtime: u64::from(get_u32(b, 12)),
                mtime_nsec: 0,
                i_u: get_u32(b, 16),
                ino: get_u32(b, 20),
                uid: u32::from(get_u16(b, 24)),
                gid: u32::from(get_u16(b, 26)),
            })
        }
    }
}

pub fn encode_dirent(nid: u64, nameoff: u16, file_type: u8, out: &mut Vec<u8>) {
    out.extend_from_slice(&nid.to_le_bytes());
    out.extend_from_slice(&nameoff.to_le_bytes());
    out.push(file_type);
    out.push(0);
}

/// Returns `(nid, nameoff, file_type)`; `b` must hold at least 12 bytes.
pub fn decode_dirent(b: &[u8]) -> (u64, u16, u8) {
    (get_u64(b, 0), get_u16(b, 8), b[10])
}

pub fn xattr_entry_len(name_len: usize, value_len: usize) -> usize {
    (XATTR_ENTRY_HEADER_LEN + name_len + value_len).div_ceil(4) * 4
}

/// Appends one xattr entry, zero-padded to a 4-byte boundary.
pub fn encode_xattr_entry(index: u8, name: &[u8], value: &[u8], out: &mut Vec<u8>) {
    let start = out.len();
    out.push(name.len() as u8);
    out.push(index);
    out.extend_from_slice(&(value.len() as u16).to_le_bytes());
    out.extend_from_slice(name);
    out.extend_from_slice(value);
    out.resize(start + xattr_entry_len(name.len(), value.len()), 0);
}

/// Appends `erofs_xattr_ibody_header` (name filter 0, as kiln sets no filter feature).
pub fn encode_xattr_ibody_header(shared_count: u8, out: &mut Vec<u8>) {
    out.extend_from_slice(&[0u8; 4]);
    out.push(shared_count);
    out.extend_from_slice(&[0u8; 7]);
}

pub fn xattr_ibody_len(icount: u16) -> usize {
    if icount == 0 { 0 } else { XATTR_IBODY_HEADER_LEN + 4 * (usize::from(icount) - 1) }
}

pub fn xattr_icount_for(ibody_len: usize) -> u16 {
    if ibody_len == 0 { 0 } else { ((ibody_len - XATTR_IBODY_HEADER_LEN) / 4 + 1) as u16 }
}

/// Linux `new_encode_dev`.
pub fn encode_rdev(major: u32, minor: u32) -> u32 {
    (minor & 0xff) | (major << 8) | ((minor & !0xff) << 12)
}

/// Linux `new_decode_dev`, returning `(major, minor)`.
pub fn decode_rdev(dev: u32) -> (u32, u32) {
    ((dev & 0xfff00) >> 8, (dev & 0xff) | ((dev >> 12) & 0xfff00))
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p kiln-erofs ondisk`
Expected: 8 tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/kiln-erofs/src/ondisk.rs crates/kiln-erofs/src/lib.rs
git commit -m "feat(erofs): on-disk superblock, inode, dirent and xattr encoding

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P7rdQAESQYNEo2qFobUNxt"
```

---

### Task 3: Path normalization

**Files:**
- Create: `crates/kiln-erofs/src/path.rs`
- Modify: `crates/kiln-erofs/src/lib.rs` (add `mod path;`)

**Interfaces:**
- Consumes: `Limits`, `Error`, `lossy`.
- Produces (`pub(crate)`):
  - `normalize(raw: &[u8], limits: &Limits) -> Result<Vec<u8>>` returns the canonical relative path. It has no leading `/` or `./`, no `.` or empty components, and no trailing `/`. The root is `b""`.
  - `components(path: &[u8]) -> impl Iterator<Item = &[u8]>`.
  - `split_parent(path: &[u8]) -> (&[u8], &[u8])` returns `(parent, name)`. For example, `b"a"` gives `(b"", b"a")`.

- [ ] **Step 1: Write the failing tests**

`crates/kiln-erofs/src/path.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Error, Limits};

    fn n(p: &str) -> Vec<u8> {
        normalize(p.as_bytes(), &Limits::default()).unwrap()
    }

    #[test]
    fn canonical_forms() {
        assert_eq!(n("./usr/bin/"), b"usr/bin");
        assert_eq!(n("/usr//bin"), b"usr/bin");
        assert_eq!(n("usr/./bin"), b"usr/bin");
        assert_eq!(n("usr/lib/../bin"), b"usr/bin");
        assert_eq!(n("./"), b"");
        assert_eq!(n("a/.."), b"");
    }

    #[test]
    fn rejects_escape_nul_and_long_components() {
        let l = Limits::default();
        assert!(matches!(normalize(b"../etc", &l), Err(Error::PathEscapesRoot { .. })));
        assert!(matches!(normalize(b"a/../../b", &l), Err(Error::PathEscapesRoot { .. })));
        assert!(matches!(normalize(b"a\0b", &l), Err(Error::InvalidPath { .. })));
        let long = vec![b'x'; 256];
        assert!(matches!(normalize(&long, &l), Err(Error::InvalidPath { .. })));
    }

    #[test]
    fn enforces_length_and_depth_limits() {
        let l = Limits { max_path_len: 10, max_path_depth: 3, ..Limits::default() };
        assert!(matches!(normalize(b"aaaaaaaaaaa", &l), Err(Error::LimitExceeded { .. })));
        assert!(matches!(normalize(b"a/b/c/d", &l), Err(Error::LimitExceeded { .. })));
        assert_eq!(normalize(b"a/b/c", &l).unwrap(), b"a/b/c");
    }

    #[test]
    fn split_and_components() {
        assert_eq!(split_parent(b"a/b/c"), (&b"a/b"[..], &b"c"[..]));
        assert_eq!(split_parent(b"a"), (&b""[..], &b"a"[..]));
        assert_eq!(components(b"a/b").collect::<Vec<_>>(), vec![&b"a"[..], &b"b"[..]]);
        assert_eq!(components(b"").count(), 0);
    }
}
```
Add `mod path;` to `lib.rs`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p kiln-erofs path::`
Expected: compile error `cannot find function normalize`.

- [ ] **Step 3: Implement**

Prepend to `path.rs`:
```rust
//! Tar path normalization (spec §7.4 "Paths").

use crate::error::{lossy, Error, Result};
use crate::limits::Limits;
use crate::ondisk::NAME_LEN_MAX;

/// Normalizes a raw tar path to kiln's canonical relative form.
pub(crate) fn normalize(raw: &[u8], limits: &Limits) -> Result<Vec<u8>> {
    if raw.contains(&0) {
        return Err(Error::InvalidPath { path: lossy(raw), reason: "contains NUL" });
    }
    if raw.len() > limits.max_path_len {
        return Err(Error::LimitExceeded {
            limit: "path length",
            max: limits.max_path_len as u64,
            path: lossy(&raw[..64.min(raw.len())]),
        });
    }
    let mut parts: Vec<&[u8]> = Vec::new();
    for comp in raw.split(|&b| b == b'/') {
        match comp {
            b"" | b"." => {}
            b".." => {
                if parts.pop().is_none() {
                    return Err(Error::PathEscapesRoot { path: lossy(raw) });
                }
            }
            c if c.len() > NAME_LEN_MAX => {
                return Err(Error::InvalidPath {
                    path: lossy(raw),
                    reason: "component longer than 255 bytes",
                });
            }
            c => parts.push(c),
        }
    }
    if parts.len() > limits.max_path_depth {
        return Err(Error::LimitExceeded {
            limit: "path depth",
            max: limits.max_path_depth as u64,
            path: lossy(raw),
        });
    }
    Ok(parts.join(&b'/'))
}

pub(crate) fn components(path: &[u8]) -> impl Iterator<Item = &[u8]> {
    path.split(|&b| b == b'/').filter(|c| !c.is_empty())
}

pub(crate) fn split_parent(path: &[u8]) -> (&[u8], &[u8]) {
    match path.iter().rposition(|&b| b == b'/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => (&path[..0], path),
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p kiln-erofs path::`
Expected: 4 tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/kiln-erofs/src/path.rs crates/kiln-erofs/src/lib.rs
git commit -m "feat(erofs): tar path normalization with escape and limit checks

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P7rdQAESQYNEo2qFobUNxt"
```

---

### Task 4: PAX records and the test tar builder

**Files:**
- Create: `crates/kiln-erofs/src/pax.rs`, `crates/kiln-erofs/src/tree.rs` (only `Timestamp` for now), `crates/kiln-erofs/src/testtar.rs`
- Modify: `crates/kiln-erofs/src/lib.rs`

**Interfaces:**
- **Produces in `tree.rs`:** `pub struct Timestamp { pub sec: i64, pub nsec: u32 }`, deriving `Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default`. It is re-exported as `kiln_erofs::Timestamp`.
- **Produces in `pax.rs` (`pub(crate)`):**
  - `parse_records(&[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>>`.
  - `struct PaxState { path, linkpath: Option<Vec<u8>>, uid, gid, size: Option<u64>, mtime: Option<Timestamp>, xattrs: Vec<(Vec<u8>, Vec<u8>)>, sparse: bool, dropped: Vec<Vec<u8>> }`, with `apply(&mut self, records) -> Result<()>` and `PaxState::overlay(&global, local) -> PaxState`.
  - `parse_timestamp(&[u8]) -> Result<Timestamp>`.
- **Produces in `testtar.rs`** (`#[doc(hidden)] pub mod testtar`):
  - `Opts`, with a builder: `mode()`, `uid()`, `gid()`, `mtime()`, `pax(key, value)`, `xattr(name, value)`.
  - `TarBuilder`, with: `new`, `entry(name, typeflag, data, link, (major, minor), &Opts)`, `dir`, `file`, `symlink`, `hardlink`, `chardev`, `fifo`, `whiteout`, `opaque`, `pax_header`, `bytes`, `finish`.
  - `pax_record(key, value) -> Vec<u8>`.
  - Names over 100 bytes automatically get a GNU `L` record. Links over 100 bytes get a GNU `K` record.

- [ ] **Step 1: Write the failing tests**

`crates/kiln-erofs/src/tree.rs` (initial content):
```rust
//! The in-memory filesystem tree built from one layer.

/// Seconds and nanoseconds since the Unix epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Timestamp {
    pub sec: i64,
    pub nsec: u32,
}
```

`crates/kiln-erofs/src/pax.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testtar::pax_record;

    #[test]
    fn parses_records_including_binary_values() {
        let mut data = pax_record(b"path", b"usr/bin/php");
        data.extend(pax_record(b"SCHILY.xattr.security.capability", &[1, 0, 0, 2, b'\n', 0]));
        data.extend([0u8; 7]); // trailing NUL padding is tolerated
        let recs = parse_records(&data).unwrap();
        assert_eq!(recs[0], (b"path".to_vec(), b"usr/bin/php".to_vec()));
        assert_eq!(recs[1].1, vec![1, 0, 0, 2, b'\n', 0]);
    }

    #[test]
    fn rejects_malformed_records() {
        assert!(parse_records(b"99 path=x\n").is_err());
        assert!(parse_records(b"x path=x\n").is_err());
        assert!(parse_records(b"9 pathx\n").is_err());
    }

    #[test]
    fn timestamps() {
        assert_eq!(parse_timestamp(b"1700000000").unwrap(), Timestamp { sec: 1_700_000_000, nsec: 0 });
        assert_eq!(parse_timestamp(b"1700000000.5").unwrap(), Timestamp { sec: 1_700_000_000, nsec: 500_000_000 });
        assert_eq!(parse_timestamp(b"1.1234567891").unwrap(), Timestamp { sec: 1, nsec: 123_456_789 });
        assert_eq!(parse_timestamp(b"-1.5").unwrap(), Timestamp { sec: -2, nsec: 500_000_000 });
        assert_eq!(parse_timestamp(b"-3").unwrap(), Timestamp { sec: -3, nsec: 0 });
        assert!(parse_timestamp(b"abc").is_err());
    }

    #[test]
    fn state_applies_known_keys_and_local_overrides_global() {
        let mut global = PaxState::default();
        global
            .apply(vec![
                (b"uid".to_vec(), b"5".to_vec()),
                (b"SCHILY.xattr.user.a".to_vec(), b"g".to_vec()),
            ])
            .unwrap();
        let mut local = PaxState::default();
        local
            .apply(vec![
                (b"uid".to_vec(), b"7".to_vec()),
                (b"GNU.sparse.major".to_vec(), b"1".to_vec()),
                (b"LIBARCHIVE.xattr.user.b".to_vec(), b"x".to_vec()),
                (b"SCHILY.xattr.user.a".to_vec(), b"l".to_vec()),
            ])
            .unwrap();
        let s = PaxState::overlay(&global, local);
        assert_eq!(s.uid, Some(7));
        assert!(s.sparse);
        assert_eq!(s.dropped, vec![b"user.b".to_vec()]);
        assert_eq!(s.xattrs.last().unwrap(), &(b"user.a".to_vec(), b"l".to_vec()));
    }
}
```

`crates/kiln-erofs/src/testtar.rs` (tests only for now):
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pax_record_length_is_self_consistent() {
        let cases: [&[u8]; 4] = [b"", b"x", &[b'y'; 95], &[b'z'; 995]];
        for v in cases {
            let rec = pax_record(b"path", v);
            let sp = rec.iter().position(|&b| b == b' ').unwrap();
            let len: usize = std::str::from_utf8(&rec[..sp]).unwrap().parse().unwrap();
            assert_eq!(len, rec.len());
        }
    }

    #[test]
    fn builds_readable_tar() {
        let tar = TarBuilder::new()
            .dir("etc", &Opts::default().mode(0o755))
            .file("etc/hostname", b"kiln\n", &Opts::default())
            .finish();
        assert_eq!(tar.len() % 512, 0);
        let mut ar = tar::Archive::new(&tar[..]);
        let names: Vec<String> = ar
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().display().to_string())
            .collect();
        assert_eq!(names, vec!["etc", "etc/hostname"]);
    }
}
```

Update `lib.rs` to:
```rust
//! Deterministic erofs writer and reader implementing kiln's erofs profile
//! (spec §7). One OCI layer tar becomes one erofs image.
#![forbid(unsafe_code)]
// Modules are wired together incrementally; Task 10 removes this allowance.
#![allow(dead_code)]

mod error;
mod limits;
pub mod ondisk;
mod path;
mod pax;
#[doc(hidden)]
pub mod testtar;
mod tree;

pub use error::{Error, Result};
pub use limits::Limits;
pub use tree::Timestamp;
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p kiln-erofs pax:: testtar::`
Expected: compile errors, such as `cannot find function parse_records` and `cannot find function pax_record`.

- [ ] **Step 3: Implement `pax.rs`**

Prepend to `pax.rs`:
```rust
//! PAX extended header parsing (POSIX.1-2001 `x`/`g` records).

use crate::error::{Error, Result};
use crate::tree::Timestamp;

fn malformed(what: &str) -> Error {
    Error::MalformedTar(format!("PAX header: {what}"))
}

/// Parses `"<len> <key>=<value>\n"` records. Values may be binary.
pub(crate) fn parse_records(data: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut out = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        if rest.iter().all(|&b| b == 0) {
            break;
        }
        let sp = rest.iter().position(|&b| b == b' ').ok_or_else(|| malformed("missing length"))?;
        let len: usize = std::str::from_utf8(&rest[..sp])
            .ok()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| malformed("bad length"))?;
        if len <= sp + 1 || len > rest.len() {
            return Err(malformed("record length out of range"));
        }
        let rec = rest[sp + 1..len]
            .strip_suffix(b"\n")
            .ok_or_else(|| malformed("record missing newline"))?;
        let eq = rec.iter().position(|&b| b == b'=').ok_or_else(|| malformed("record missing '='"))?;
        out.push((rec[..eq].to_vec(), rec[eq + 1..].to_vec()));
        rest = &rest[len..];
    }
    Ok(out)
}

fn parse_u64(v: &[u8], what: &str) -> Result<u64> {
    std::str::from_utf8(v).ok().and_then(|s| s.parse().ok()).ok_or_else(|| malformed(what))
}

/// Parses a PAX time such as `1700000000.123456789` or `-1.5`.
pub(crate) fn parse_timestamp(v: &[u8]) -> Result<Timestamp> {
    let s = std::str::from_utf8(v).map_err(|_| malformed("mtime"))?;
    let (neg, s) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    if int.is_empty() || !int.bytes().all(|b| b.is_ascii_digit()) || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return Err(malformed("mtime"));
    }
    let sec: i64 = int.parse().map_err(|_| malformed("mtime"))?;
    let mut digits: String = frac.chars().take(9).collect();
    while digits.len() < 9 {
        digits.push('0');
    }
    let nsec: u32 = digits.parse().map_err(|_| malformed("mtime"))?;
    Ok(match (neg, nsec) {
        (false, _) => Timestamp { sec, nsec },
        (true, 0) => Timestamp { sec: -sec, nsec: 0 },
        (true, n) => Timestamp { sec: -sec - 1, nsec: 1_000_000_000 - n },
    })
}

/// Accumulated PAX overrides for the next entry.
#[derive(Debug, Default, Clone)]
pub(crate) struct PaxState {
    pub path: Option<Vec<u8>>,
    pub linkpath: Option<Vec<u8>>,
    pub uid: Option<u64>,
    pub gid: Option<u64>,
    pub size: Option<u64>,
    pub mtime: Option<Timestamp>,
    /// Full xattr names (`user.foo`) with raw values, in record order.
    pub xattrs: Vec<(Vec<u8>, Vec<u8>)>,
    pub sparse: bool,
    /// Xattr names kiln cannot represent (`LIBARCHIVE.xattr.*`).
    pub dropped: Vec<Vec<u8>>,
}

impl PaxState {
    pub fn apply(&mut self, records: Vec<(Vec<u8>, Vec<u8>)>) -> Result<()> {
        for (k, v) in records {
            match k.as_slice() {
                b"path" => self.path = Some(v),
                b"linkpath" => self.linkpath = Some(v),
                b"uid" => self.uid = Some(parse_u64(&v, "uid")?),
                b"gid" => self.gid = Some(parse_u64(&v, "gid")?),
                b"size" => self.size = Some(parse_u64(&v, "size")?),
                b"mtime" => self.mtime = Some(parse_timestamp(&v)?),
                _ if k.starts_with(b"SCHILY.xattr.") => self.xattrs.push((k[13..].to_vec(), v)),
                _ if k.starts_with(b"GNU.sparse.") => self.sparse = true,
                _ if k.starts_with(b"LIBARCHIVE.xattr.") => self.dropped.push(k[17..].to_vec()),
                _ => {}
            }
        }
        Ok(())
    }

    /// Combines global (`g`) and local (`x`) state; local values win.
    pub fn overlay(global: &PaxState, local: PaxState) -> PaxState {
        let mut xattrs = global.xattrs.clone();
        xattrs.extend(local.xattrs);
        let mut dropped = global.dropped.clone();
        dropped.extend(local.dropped);
        PaxState {
            path: local.path.or_else(|| global.path.clone()),
            linkpath: local.linkpath.or_else(|| global.linkpath.clone()),
            uid: local.uid.or(global.uid),
            gid: local.gid.or(global.gid),
            size: local.size.or(global.size),
            mtime: local.mtime.or(global.mtime),
            xattrs,
            sparse: global.sparse || local.sparse,
            dropped,
        }
    }
}
```

- [ ] **Step 4: Implement `testtar.rs`**

Prepend to `testtar.rs`:
```rust
//! Byte-exact tar stream builder for tests and fixtures. Not for production use.

/// Header options for one entry.
#[derive(Debug, Clone)]
pub struct Opts {
    pub mode: u32,
    pub uid: u64,
    pub gid: u64,
    pub mtime: u64,
    pub pax: Vec<(Vec<u8>, Vec<u8>)>,
}

impl Default for Opts {
    fn default() -> Self {
        Opts { mode: 0o644, uid: 0, gid: 0, mtime: 1_700_000_000, pax: Vec::new() }
    }
}

impl Opts {
    pub fn mode(mut self, mode: u32) -> Self {
        self.mode = mode;
        self
    }
    pub fn uid(mut self, uid: u64) -> Self {
        self.uid = uid;
        self
    }
    pub fn gid(mut self, gid: u64) -> Self {
        self.gid = gid;
        self
    }
    pub fn mtime(mut self, mtime: u64) -> Self {
        self.mtime = mtime;
        self
    }
    pub fn pax(mut self, key: &str, value: &[u8]) -> Self {
        self.pax.push((key.as_bytes().to_vec(), value.to_vec()));
        self
    }
    pub fn xattr(self, name: &str, value: &[u8]) -> Self {
        self.pax(&format!("SCHILY.xattr.{name}"), value)
    }
}

/// Encodes one PAX record with a self-consistent length prefix.
pub fn pax_record(key: &[u8], value: &[u8]) -> Vec<u8> {
    let body = key.len() + value.len() + 3; // ' ', '=', '\n'
    let mut len = body + 1;
    while len.to_string().len() + body != len {
        len = body + len.to_string().len();
    }
    let mut rec = format!("{len} ").into_bytes();
    rec.extend_from_slice(key);
    rec.push(b'=');
    rec.extend_from_slice(value);
    rec.push(b'\n');
    rec
}

/// Appends raw tar entries to an in-memory buffer.
#[derive(Debug, Default)]
pub struct TarBuilder {
    buf: Vec<u8>,
}

impl TarBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    fn header(&mut self, name: &[u8], typeflag: u8, size: u64, link: &[u8], dev: (u32, u32), o: &Opts) {
        let mut h = tar::Header::new_ustar();
        h.set_mode(o.mode);
        h.set_uid(o.uid);
        h.set_gid(o.gid);
        h.set_mtime(o.mtime);
        h.set_size(size);
        if typeflag == b'3' || typeflag == b'4' {
            h.set_device_major(dev.0).expect("ustar device major");
            h.set_device_minor(dev.1).expect("ustar device minor");
        }
        let old = h.as_old_mut();
        let n = name.len().min(100);
        old.name[..n].copy_from_slice(&name[..n]);
        let l = link.len().min(100);
        old.linkname[..l].copy_from_slice(&link[..l]);
        old.linkflag = [typeflag];
        h.set_cksum();
        self.buf.extend_from_slice(h.as_bytes());
    }

    fn data(&mut self, d: &[u8]) {
        self.buf.extend_from_slice(d);
        let pad = (512 - d.len() % 512) % 512;
        self.buf.resize(self.buf.len() + pad, 0);
    }

    /// Appends a PAX local header (`x`) carrying `records`.
    pub fn pax_header(&mut self, records: &[(Vec<u8>, Vec<u8>)]) -> &mut Self {
        let mut payload = Vec::new();
        for (k, v) in records {
            payload.extend(pax_record(k, v));
        }
        self.header(b"././@PaxHeader", b'x', payload.len() as u64, b"", (0, 0), &Opts::default());
        self.data(&payload);
        self
    }

    /// Appends any entry. Long names and links get GNU `L`/`K` records first.
    pub fn entry(&mut self, name: &[u8], typeflag: u8, data: &[u8], link: &[u8], dev: (u32, u32), o: &Opts) -> &mut Self {
        if !o.pax.is_empty() {
            self.pax_header(&o.pax.clone());
        }
        if name.len() > 100 {
            let mut v = name.to_vec();
            v.push(0);
            self.header(b"././@LongLink", b'L', v.len() as u64, b"", (0, 0), &Opts::default());
            self.data(&v);
        }
        if link.len() > 100 {
            let mut v = link.to_vec();
            v.push(0);
            self.header(b"././@LongLink", b'K', v.len() as u64, b"", (0, 0), &Opts::default());
            self.data(&v);
        }
        self.header(name, typeflag, data.len() as u64, link, dev, o);
        self.data(data);
        self
    }

    pub fn dir(&mut self, path: &str, o: &Opts) -> &mut Self {
        self.entry(path.as_bytes(), b'5', b"", b"", (0, 0), o)
    }
    pub fn file(&mut self, path: &str, data: &[u8], o: &Opts) -> &mut Self {
        self.entry(path.as_bytes(), b'0', data, b"", (0, 0), o)
    }
    pub fn symlink(&mut self, path: &str, target: &str, o: &Opts) -> &mut Self {
        self.entry(path.as_bytes(), b'2', b"", target.as_bytes(), (0, 0), o)
    }
    pub fn hardlink(&mut self, path: &str, target: &str) -> &mut Self {
        self.entry(path.as_bytes(), b'1', b"", target.as_bytes(), (0, 0), &Opts::default())
    }
    pub fn chardev(&mut self, path: &str, major: u32, minor: u32, o: &Opts) -> &mut Self {
        self.entry(path.as_bytes(), b'3', b"", b"", (major, minor), o)
    }
    pub fn fifo(&mut self, path: &str, o: &Opts) -> &mut Self {
        self.entry(path.as_bytes(), b'6', b"", b"", (0, 0), o)
    }
    /// Whiteout hiding `path` (e.g. `etc/foo` → `etc/.wh.foo`).
    pub fn whiteout(&mut self, path: &str) -> &mut Self {
        let marker = match path.rsplit_once('/') {
            Some((dir, name)) => format!("{dir}/.wh.{name}"),
            None => format!(".wh.{path}"),
        };
        self.entry(marker.as_bytes(), b'0', b"", b"", (0, 0), &Opts::default())
    }
    /// Opaque marker for `dir` (`""` is the layer root).
    pub fn opaque(&mut self, dir: &str) -> &mut Self {
        let marker = if dir.is_empty() { ".wh..wh..opq".to_string() } else { format!("{dir}/.wh..wh..opq") };
        self.entry(marker.as_bytes(), b'0', b"", b"", (0, 0), &Opts::default())
    }
    /// The entries so far, without the end-of-archive marker.
    pub fn bytes(&self) -> Vec<u8> {
        self.buf.clone()
    }
    /// The entries plus the two zero blocks that end an archive.
    pub fn finish(&self) -> Vec<u8> {
        let mut v = self.buf.clone();
        v.resize(v.len() + 1024, 0);
        v
    }
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p kiln-erofs pax:: testtar::`
Expected: 6 tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/kiln-erofs/src
git commit -m "feat(erofs): PAX record parsing and byte-exact test tar builder

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P7rdQAESQYNEo2qFobUNxt"
```

---
### Task 5: Tree model and layer semantics (§7.4)

**Files:**
- Modify: `crates/kiln-erofs/src/tree.rs` (full content below)
- Create: `crates/kiln-erofs/src/apply.rs`
- Modify: `crates/kiln-erofs/src/lib.rs`

**Interfaces:**
- Consumes: `Timestamp` (Task 4), `path::{components, split_parent}` (Task 3), and the xattr index constants from `ondisk` (Task 2).
- Produces (`pub`, re-exported from `lib.rs`):
  - `Meta { mode: u32 /* 0o7777 bits */, uid: u32, gid: u32, mtime: Timestamp }` with `Meta::default_dir(mtime)`.
  - `XattrKey { index: u8, name: Vec<u8> }`, ordered by `(index, name)`, with `from_full_name(&[u8]) -> Option<XattrKey>`, `full_name() -> Vec<u8>`, `opaque()` and `is_overlay()`.
  - `type Xattrs = BTreeMap<XattrKey, Vec<u8>>`.
  - `DirAttrs { meta: Meta, xattrs: Xattrs }`.
- Produces (`pub(crate)`):
  - `TailRef { offset: u64, len: u32 }` and `FileData { start_blk: u32, blocks: u32, tail: Option<TailRef> }`.
  - `Data::{Written(FileData), External { layer: usize, nid: u64 }}`.
  - `Kind::{Dir { children: BTreeMap<Vec<u8>, NodeId>, implicit: bool }, File { size, data }, Symlink { target }, CharDev { major, minor }, BlockDev { major, minor }, Fifo}`.
  - `Node { kind, meta, xattrs }` with `dir()`, `whiteout()`, `is_dir()` and `is_whiteout()`.
  - `Tree { nodes: Vec<Node>, root: NodeId }` with `new`, `add`, `children`, `children_mut`, `child`, `lookup`, `dir_paths(keep: impl Fn(NodeId, &Node) -> bool) -> Vec<Vec<u8>>` and `min_mtime`.
  - In `apply.rs`:
    - `EntryKind::{Dir, File { size }, Symlink { target }, Hardlink { target }, CharDev { major, minor }, BlockDev { major, minor }, Fifo}`.
    - `Entry { path, kind, meta, xattrs }`.
    - `LayerBuilder` with `new()`, `apply(Entry, Option<Data>) -> Result<()>`, `base_time()`, `tree()` (test-only), `implicit_dirs() -> Vec<Vec<u8>>` and `finalize(self, &BTreeMap<Vec<u8>, DirAttrs>) -> (Tree, Timestamp, Vec<Vec<u8>>)`.

**Semantics:** these are the normative rules from spec §7.4. They were checked against containerd's overlay snapshotter (Task 14), and the tests below pin each one.
- **Directory over directory:** merge. Meta is replaced, header xattrs override same-named ones, other xattrs are kept (so a prior opaque marker survives), and children are kept.
- **Non-directory over anything, or directory over non-directory:** replace, dropping the subtree.
- **Symlinks:** permission bits are always `0777`, as Linux reports them, whatever the header says.
- **Hardlinks:**
  - A hardlink binds to the node at the target path **when the link entry is read**.
  - The link header's mode (unless the target is a symlink), uid, gid, mtime and xattrs are applied to that shared inode, as containerd does.
  - It is an error if the target is missing, is a directory, is a whiteout, is the link itself, or lies inside the entry the link replaces.
- **Whiteouts and opaque markers:**
  - `.wh.<name>` becomes a whiteout node (char 0:0) named `<name>`.
  - It is an error if `<name>` already exists in this layer: OCI says a whiteout cannot hide its own layer, and containerd rejects it. A later entry with that name replaces the whiteout.
  - `.wh.` and names that translate to `.` or `..` are errors.
  - `.wh..wh..opq` sets `trusted.overlay.opaque=y` on its directory.
- **Implicit directories:**
  - Missing parents are created as implicit directories, and the builder remembers them even if a later header describes them. A parent that is a non-directory is an error.
  - At `finalize`, a still-undescribed implicit directory takes the inherited attributes, or the defaults (`0755`, uid/gid 0, mtime = base) when nothing is inherited.
  - A described implicit directory keeps its header's meta, with the inherited xattrs underneath its own.
  - An opaque marker on an implicit directory is kept either way.
  - An implicit root always takes the defaults.
- **Base time:** the minimum mtime over directory, file, symlink, device, FIFO, whiteout and hardlink entries, including an explicit root. A hardlink counts because it sets its inode's mtime. Opaque markers do not count.

- [ ] **Step 1: Replace `tree.rs` with the full model**

```rust
//! The in-memory filesystem tree built from one layer (or a merged stack).

use std::collections::BTreeMap;

use crate::ondisk::{
    XATTR_INDEX_POSIX_ACL_ACCESS, XATTR_INDEX_POSIX_ACL_DEFAULT, XATTR_INDEX_SECURITY,
    XATTR_INDEX_TRUSTED, XATTR_INDEX_USER,
};
use crate::path::components;

/// Seconds and nanoseconds since the Unix epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Timestamp {
    pub sec: i64,
    pub nsec: u32,
}

/// An erofs xattr key: name index plus the name without its prefix.
/// Ordering by `(index, name)` is the on-disk inline order.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct XattrKey {
    pub index: u8,
    pub name: Vec<u8>,
}

pub type Xattrs = BTreeMap<XattrKey, Vec<u8>>;

const NAMED_PREFIXES: [(&[u8], u8); 3] = [
    (b"user.", XATTR_INDEX_USER),
    (b"trusted.", XATTR_INDEX_TRUSTED),
    (b"security.", XATTR_INDEX_SECURITY),
];
const ACL_ACCESS: &[u8] = b"system.posix_acl_access";
const ACL_DEFAULT: &[u8] = b"system.posix_acl_default";

impl XattrKey {
    /// Maps a full Linux xattr name to an erofs key; `None` if erofs cannot store it.
    pub fn from_full_name(full: &[u8]) -> Option<Self> {
        if full == ACL_ACCESS {
            return Some(Self { index: XATTR_INDEX_POSIX_ACL_ACCESS, name: Vec::new() });
        }
        if full == ACL_DEFAULT {
            return Some(Self { index: XATTR_INDEX_POSIX_ACL_DEFAULT, name: Vec::new() });
        }
        NAMED_PREFIXES.iter().find_map(|(prefix, index)| {
            full.strip_prefix(*prefix)
                .filter(|rest| !rest.is_empty())
                .map(|rest| Self { index: *index, name: rest.to_vec() })
        })
    }

    pub fn full_name(&self) -> Vec<u8> {
        match self.index {
            XATTR_INDEX_POSIX_ACL_ACCESS => ACL_ACCESS.to_vec(),
            XATTR_INDEX_POSIX_ACL_DEFAULT => ACL_DEFAULT.to_vec(),
            i => {
                let prefix = NAMED_PREFIXES.iter().find(|(_, x)| *x == i).map_or(&b""[..], |(p, _)| *p);
                [prefix, self.name.as_slice()].concat()
            }
        }
    }

    /// `trusted.overlay.opaque`.
    pub fn opaque() -> Self {
        Self { index: XATTR_INDEX_TRUSTED, name: b"overlay.opaque".to_vec() }
    }

    /// Any `trusted.overlay.*` key.
    pub fn is_overlay(&self) -> bool {
        self.index == XATTR_INDEX_TRUSTED && self.name.starts_with(b"overlay.")
    }
}

/// Inode attributes. `mode` holds permission bits only (`0o7777`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meta {
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime: Timestamp,
}

impl Meta {
    /// Attributes for a directory nothing else describes (spec §6.3).
    pub fn default_dir(mtime: Timestamp) -> Self {
        Self { mode: 0o755, uid: 0, gid: 0, mtime }
    }
}

/// Attributes inherited by an implicit directory from the layers below.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirAttrs {
    pub meta: Meta,
    pub xattrs: Xattrs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TailRef {
    pub offset: u64,
    pub len: u32,
}

/// Where a regular file's bytes were written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileData {
    /// First data block (0 when `blocks == 0`).
    pub start_blk: u32,
    /// Data-area blocks, including a zero-padded last block when the tail is not inline.
    pub blocks: u32,
    /// Tail bytes held in the spill file, to be stored inline.
    pub tail: Option<TailRef>,
}

#[derive(Debug, Clone)]
pub(crate) enum Data {
    Written(FileData),
    /// Still inside source image `layer` at inode `nid` (squash).
    External { layer: usize, nid: u64 },
}

pub(crate) type NodeId = usize;

#[derive(Debug, Clone)]
pub(crate) enum Kind {
    Dir { children: BTreeMap<Vec<u8>, NodeId>, implicit: bool },
    File { size: u64, data: Data },
    Symlink { target: Vec<u8> },
    CharDev { major: u32, minor: u32 },
    BlockDev { major: u32, minor: u32 },
    Fifo,
}

#[derive(Debug, Clone)]
pub(crate) struct Node {
    pub kind: Kind,
    pub meta: Meta,
    pub xattrs: Xattrs,
}

impl Node {
    pub fn dir(meta: Meta, implicit: bool) -> Self {
        Node { kind: Kind::Dir { children: BTreeMap::new(), implicit }, meta, xattrs: Xattrs::new() }
    }

    pub fn whiteout(mtime: Timestamp) -> Self {
        Node {
            kind: Kind::CharDev { major: 0, minor: 0 },
            meta: Meta { mode: 0, uid: 0, gid: 0, mtime },
            xattrs: Xattrs::new(),
        }
    }

    pub fn is_dir(&self) -> bool {
        matches!(self.kind, Kind::Dir { .. })
    }

    pub fn is_whiteout(&self) -> bool {
        matches!(self.kind, Kind::CharDev { major: 0, minor: 0 })
    }
}

/// An arena of nodes. Directories own name → node maps; hardlinks share a node.
#[derive(Debug, Clone)]
pub(crate) struct Tree {
    pub nodes: Vec<Node>,
    pub root: NodeId,
}

impl Tree {
    pub fn new(root: Node) -> Self {
        Tree { nodes: vec![root], root: 0 }
    }

    pub fn add(&mut self, node: Node) -> NodeId {
        self.nodes.push(node);
        self.nodes.len() - 1
    }

    pub fn children(&self, dir: NodeId) -> Option<&BTreeMap<Vec<u8>, NodeId>> {
        match &self.nodes[dir].kind {
            Kind::Dir { children, .. } => Some(children),
            _ => None,
        }
    }

    pub fn children_mut(&mut self, dir: NodeId) -> &mut BTreeMap<Vec<u8>, NodeId> {
        match &mut self.nodes[dir].kind {
            Kind::Dir { children, .. } => children,
            _ => unreachable!("children_mut on a non-directory"),
        }
    }

    pub fn child(&self, dir: NodeId, name: &[u8]) -> Option<NodeId> {
        self.children(dir)?.get(name).copied()
    }

    /// Looks up a normalized path; every component must resolve through directories.
    pub fn lookup(&self, path: &[u8]) -> Option<NodeId> {
        let mut cur = self.root;
        for c in components(path) {
            cur = self.child(cur, c)?;
        }
        Some(cur)
    }

    /// Sorted paths of reachable directories (excluding the root) for which `keep` holds.
    pub fn dir_paths(&self, keep: impl Fn(NodeId, &Node) -> bool) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut stack: Vec<(NodeId, Vec<u8>)> = vec![(self.root, Vec::new())];
        while let Some((id, path)) = stack.pop() {
            if let Kind::Dir { children, .. } = &self.nodes[id].kind {
                if !path.is_empty() && keep(id, &self.nodes[id]) {
                    out.push(path.clone());
                }
                for (name, &child) in children {
                    let mut p = path.clone();
                    if !p.is_empty() {
                        p.push(b'/');
                    }
                    p.extend_from_slice(name);
                    stack.push((child, p));
                }
            }
        }
        out.sort();
        out
    }

    /// Minimum mtime over reachable nodes (the root counts).
    pub fn min_mtime(&self) -> Timestamp {
        let mut min = self.nodes[self.root].meta.mtime;
        let mut stack = vec![self.root];
        while let Some(id) = stack.pop() {
            min = min.min(self.nodes[id].meta.mtime);
            if let Some(children) = self.children(id) {
                stack.extend(children.values().copied());
            }
        }
        min
    }
}
```

Note: hardlinked non-directories can be pushed onto the stack more than once. That is harmless here, and directories cannot be hardlinked.

- [ ] **Step 2: Write the failing tests for `apply.rs`**

`crates/kiln-erofs/src/apply.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;

    fn meta(mode: u32, sec: i64) -> Meta {
        Meta { mode, uid: 0, gid: 0, mtime: Timestamp { sec, nsec: 0 } }
    }
    fn entry(path: &str, kind: EntryKind, sec: i64) -> Entry {
        Entry { path: path.as_bytes().to_vec(), kind, meta: meta(0o644, sec), xattrs: Xattrs::new() }
    }
    fn data() -> Option<Data> {
        Some(Data::Written(FileData { start_blk: 0, blocks: 0, tail: None }))
    }
    fn file(b: &mut LayerBuilder, p: &str, sec: i64) {
        b.apply(entry(p, EntryKind::File { size: 0 }, sec), data()).unwrap();
    }
    fn dir(b: &mut LayerBuilder, p: &str, sec: i64) {
        b.apply(entry(p, EntryKind::Dir, sec), None).unwrap();
    }
    fn marker(b: &mut LayerBuilder, p: &str, sec: i64) -> crate::Result<()> {
        b.apply(entry(p, EntryKind::File { size: 0 }, sec), data())
    }
    fn user(name: &str) -> XattrKey {
        XattrKey { index: crate::ondisk::XATTR_INDEX_USER, name: name.as_bytes().to_vec() }
    }

    #[test]
    fn implicit_parents_are_reported_even_once_described() {
        let mut b = LayerBuilder::new();
        file(&mut b, "a/b/c", 10);
        assert_eq!(b.implicit_dirs(), vec![b"a".to_vec(), b"a/b".to_vec()]);
        dir(&mut b, "a", 10);
        assert_eq!(b.implicit_dirs(), vec![b"a".to_vec(), b"a/b".to_vec()]);
        file(&mut b, "a", 10);
        assert!(b.implicit_dirs().is_empty(), "replaced directories are gone");
    }

    #[test]
    fn described_implicit_dir_keeps_inherited_xattrs_under_its_own() {
        let mut b = LayerBuilder::new();
        file(&mut b, "a/f", 10);
        let mut d = entry("a", EntryKind::Dir, 11);
        d.meta.mode = 0o700;
        d.xattrs.insert(user("y"), b"own".to_vec());
        b.apply(d, None).unwrap();
        let mut xattrs = Xattrs::new();
        xattrs.insert(user("k"), b"lower".to_vec());
        xattrs.insert(user("y"), b"lower".to_vec());
        let inherited = BTreeMap::from([(b"a".to_vec(), DirAttrs { meta: meta(0o750, 1), xattrs })]);
        let (tree, _, _) = b.finalize(&inherited);
        let a = &tree.nodes[tree.lookup(b"a").unwrap()];
        assert_eq!(a.meta.mode, 0o700, "the header's meta wins");
        assert_eq!(a.xattrs[&user("k")], b"lower");
        assert_eq!(a.xattrs[&user("y")], b"own");
    }

    #[test]
    fn dir_over_dir_merges_attrs_children_and_opaque() {
        let mut b = LayerBuilder::new();
        let mut d = entry("d", EntryKind::Dir, 10);
        d.xattrs.insert(user("x"), b"1".to_vec());
        b.apply(d, None).unwrap();
        file(&mut b, "d/f", 10);
        marker(&mut b, "d/.wh..wh..opq", 10).unwrap();
        let mut d2 = entry("d", EntryKind::Dir, 11);
        d2.meta.mode = 0o700;
        d2.xattrs.insert(user("y"), b"2".to_vec());
        b.apply(d2, None).unwrap();
        let t = b.tree();
        let id = t.lookup(b"d").unwrap();
        assert_eq!(t.nodes[id].meta.mode, 0o700);
        let keys: Vec<_> = t.nodes[id].xattrs.keys().cloned().collect();
        assert_eq!(keys, vec![user("x"), user("y"), XattrKey::opaque()]);
        assert!(t.lookup(b"d/f").is_some());
    }

    #[test]
    fn non_dir_replaces_subtree_and_dir_replaces_non_dir() {
        let mut b = LayerBuilder::new();
        dir(&mut b, "d", 10);
        file(&mut b, "d/f", 10);
        file(&mut b, "d", 10);
        assert!(!b.tree().nodes[b.tree().lookup(b"d").unwrap()].is_dir());
        assert!(b.tree().lookup(b"d/f").is_none());
        dir(&mut b, "d", 10);
        let id = b.tree().lookup(b"d").unwrap();
        assert!(b.tree().children(id).unwrap().is_empty());
    }

    #[test]
    fn hardlink_binds_to_the_node_present_when_read() {
        let mut b = LayerBuilder::new();
        file(&mut b, "a", 1);
        let link = Entry { path: b"b".to_vec(), kind: EntryKind::Hardlink { target: b"a".to_vec() }, meta: meta(0o644, 1), xattrs: Xattrs::new() };
        b.apply(link, None).unwrap();
        file(&mut b, "a", 2);
        let t = b.tree();
        assert_eq!(t.nodes[t.lookup(b"b").unwrap()].meta.mtime.sec, 1);
        assert_eq!(t.nodes[t.lookup(b"a").unwrap()].meta.mtime.sec, 2);
        assert_ne!(t.lookup(b"a"), t.lookup(b"b"));
    }

    #[test]
    fn hardlink_header_metadata_applies_to_the_shared_inode() {
        let mut b = LayerBuilder::new();
        file(&mut b, "a", 1);
        let mut link = Entry { path: b"b".to_vec(), kind: EntryKind::Hardlink { target: b"a".to_vec() }, meta: Meta { mode: 0o600, uid: 7, gid: 8, mtime: Timestamp { sec: 9, nsec: 0 } }, xattrs: Xattrs::new() };
        link.xattrs.insert(user("k"), b"v".to_vec());
        b.apply(link, None).unwrap();
        let t = b.tree();
        let a = &t.nodes[t.lookup(b"a").unwrap()];
        assert_eq!(a.meta, Meta { mode: 0o600, uid: 7, gid: 8, mtime: Timestamp { sec: 9, nsec: 0 } });
        assert!(a.xattrs.contains_key(&user("k")));
        assert_eq!(t.lookup(b"a"), t.lookup(b"b"));
    }

    #[test]
    fn hardlink_errors() {
        let mut b = LayerBuilder::new();
        dir(&mut b, "d", 1);
        file(&mut b, "f", 1);
        marker(&mut b, ".wh.gone", 1).unwrap();
        file(&mut b, "d/inner", 1);
        for (path, target) in [("x", "missing"), ("x", "d"), ("f", "f"), ("x", "gone"), ("d", "d/inner")] {
            let e = Entry { path: path.into(), kind: EntryKind::Hardlink { target: target.into() }, meta: meta(0, 0), xattrs: Xattrs::new() };
            assert!(matches!(b.apply(e, None), Err(Error::InvalidHardlink { .. })), "{path} -> {target}");
        }
    }

    #[test]
    fn whiteouts_translate_and_collide_by_translated_name() {
        let mut b = LayerBuilder::new();
        marker(&mut b, "etc/.wh.foo", 1).unwrap();
        assert!(b.tree().nodes[b.tree().lookup(b"etc/foo").unwrap()].is_whiteout());
        file(&mut b, "etc/foo", 1);
        assert!(!b.tree().nodes[b.tree().lookup(b"etc/foo").unwrap()].is_whiteout(), "a later entry replaces the whiteout");
        assert!(matches!(marker(&mut b, "etc/.wh.foo", 1), Err(Error::InvalidPath { .. })), "a whiteout cannot hide its own layer");
        assert!(matches!(marker(&mut b, ".wh.etc", 1), Err(Error::InvalidPath { .. })), "nor an implicit directory");
        for bad in [".wh.", ".wh..", ".wh..."] {
            assert!(matches!(marker(&mut b, bad, 1), Err(Error::InvalidPath { .. })), "{bad}");
        }
    }

    #[test]
    fn parent_must_be_a_directory() {
        let mut b = LayerBuilder::new();
        file(&mut b, "f", 1);
        assert!(matches!(marker(&mut b, "f/x", 1), Err(Error::ParentNotDirectory { .. })));
        marker(&mut b, ".wh.w", 1).unwrap();
        assert!(matches!(marker(&mut b, "w/x", 1), Err(Error::ParentNotDirectory { .. })));
    }

    #[test]
    fn symlink_mode_is_always_0777() {
        let mut b = LayerBuilder::new();
        b.apply(entry("l", EntryKind::Symlink { target: b"x".to_vec() }, 1), None).unwrap();
        assert_eq!(b.tree().nodes[b.tree().lookup(b"l").unwrap()].meta.mode, 0o777);
    }

    #[test]
    fn root_entry() {
        let mut b = LayerBuilder::new();
        let mut root = entry("", EntryKind::Dir, 5);
        root.meta.mode = 0o700;
        b.apply(root, None).unwrap();
        assert_eq!(b.tree().nodes[b.tree().root].meta.mode, 0o700);
        assert!(matches!(marker(&mut b, "", 1), Err(Error::InvalidPath { .. })));
    }

    #[test]
    fn base_time_ignores_opaque_markers_only() {
        let mut b = LayerBuilder::new();
        assert_eq!(b.base_time(), Timestamp::default());
        file(&mut b, "f", 50);
        dir(&mut b, "d", 40);
        marker(&mut b, "d/.wh..wh..opq", 1).unwrap();
        assert_eq!(b.base_time(), Timestamp { sec: 40, nsec: 0 });
        marker(&mut b, ".wh.x", 30).unwrap();
        assert_eq!(b.base_time(), Timestamp { sec: 30, nsec: 0 });
        let link = Entry { path: b"l".to_vec(), kind: EntryKind::Hardlink { target: b"f".to_vec() }, meta: meta(0o644, 20), xattrs: Xattrs::new() };
        b.apply(link, None).unwrap();
        assert_eq!(b.base_time(), Timestamp { sec: 20, nsec: 0 }, "a hardlink header sets its inode's mtime");
    }

    #[test]
    fn finalize_inherits_or_defaults_and_keeps_opaque() {
        let mut b = LayerBuilder::new();
        file(&mut b, "a/b/f", 100);
        marker(&mut b, "a/.wh..wh..opq", 100).unwrap();
        let mut inherited = BTreeMap::new();
        let mut xattrs = Xattrs::new();
        xattrs.insert(user("k"), b"v".to_vec());
        inherited.insert(b"a".to_vec(), DirAttrs { meta: Meta { mode: 0o750, uid: 5, gid: 6, mtime: Timestamp { sec: 50, nsec: 0 } }, xattrs });
        let (tree, base, implicit) = b.finalize(&inherited);
        assert_eq!(base, Timestamp { sec: 100, nsec: 0 });
        assert_eq!(implicit, vec![b"a".to_vec(), b"a/b".to_vec()]);
        let a = &tree.nodes[tree.lookup(b"a").unwrap()];
        assert_eq!(a.meta.mode, 0o750);
        assert_eq!(a.meta.uid, 5);
        assert!(a.xattrs.contains_key(&user("k")));
        assert!(a.xattrs.contains_key(&XattrKey::opaque()));
        let ab = &tree.nodes[tree.lookup(b"a/b").unwrap()];
        assert_eq!(ab.meta, Meta::default_dir(base));
        assert_eq!(tree.nodes[tree.root].meta, Meta::default_dir(base));
        assert!(tree.dir_paths(|_, n| matches!(n.kind, Kind::Dir { implicit: true, .. })).is_empty());
    }
}
```

Update `lib.rs` (`apply` will be used by the writer in Task 8):
```rust
//! Deterministic erofs writer and reader implementing kiln's erofs profile
//! (spec §7). One OCI layer tar becomes one erofs image.
#![forbid(unsafe_code)]
// Modules are wired together incrementally; Task 10 removes this allowance.
#![allow(dead_code)]

mod apply;
mod error;
mod limits;
pub mod ondisk;
mod path;
mod pax;
#[doc(hidden)]
pub mod testtar;
mod tree;

pub use error::{Error, Result};
pub use limits::Limits;
pub use tree::{DirAttrs, Meta, Timestamp, XattrKey, Xattrs};
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p kiln-erofs apply::`
Expected: compile error `cannot find type LayerBuilder`.

- [ ] **Step 4: Implement `apply.rs`**

Prepend to `apply.rs`:
```rust
//! Layer semantics: applying one tar's entries to a tree (spec §7.4).

use std::collections::{BTreeMap, HashSet};

use crate::error::{lossy, Error, Result};
use crate::path::{components, split_parent};
use crate::tree::{Data, DirAttrs, Kind, Meta, Node, NodeId, Timestamp, Tree, XattrKey, Xattrs};

#[cfg(test)]
use crate::tree::FileData;

#[derive(Debug, Clone)]
pub(crate) enum EntryKind {
    Dir,
    File { size: u64 },
    Symlink { target: Vec<u8> },
    /// `target` is already normalized.
    Hardlink { target: Vec<u8> },
    CharDev { major: u32, minor: u32 },
    BlockDev { major: u32, minor: u32 },
    Fifo,
}

/// One filesystem entry decoded from a tar header. `path` is normalized.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub path: Vec<u8>,
    pub kind: EntryKind,
    pub meta: Meta,
    pub xattrs: Xattrs,
}

const WHITEOUT_PREFIX: &[u8] = b".wh.";
const OPAQUE_NAME: &[u8] = b".wh..wh..opq";

pub(crate) struct LayerBuilder {
    tree: Tree,
    base: Option<Timestamp>,
    /// Directories created because a descendant needed them (even if described later).
    created_implicitly: HashSet<NodeId>,
}

impl LayerBuilder {
    pub fn new() -> Self {
        Self {
            tree: Tree::new(Node::dir(Meta::default_dir(Timestamp::default()), true)),
            base: None,
            created_implicitly: HashSet::new(),
        }
    }

    #[cfg(test)]
    pub fn tree(&self) -> &Tree {
        &self.tree
    }

    /// Sorted paths of directories created implicitly (root excluded), including ones a
    /// later header described: they still inherit xattrs from the layers below.
    pub fn implicit_dirs(&self) -> Vec<Vec<u8>> {
        self.tree.dir_paths(|id, _| self.created_implicitly.contains(&id))
    }

    /// Minimum mtime of explicit inode-creating entries; 0 for an empty layer.
    pub fn base_time(&self) -> Timestamp {
        self.base.unwrap_or_default()
    }

    fn note_time(&mut self, t: Timestamp) {
        self.base = Some(self.base.map_or(t, |b| b.min(t)));
    }

    /// Applies one entry. `data` must be `Some` exactly for `EntryKind::File`.
    pub fn apply(&mut self, e: Entry, data: Option<Data>) -> Result<()> {
        if e.path.is_empty() {
            return match e.kind {
                EntryKind::Dir => {
                    self.note_time(e.meta.mtime);
                    let root = self.tree.root;
                    self.merge_dir(root, e.meta, e.xattrs);
                    Ok(())
                }
                _ => Err(Error::InvalidPath { path: "/".into(), reason: "layer root must be a directory" }),
            };
        }
        let (parent_path, name) = split_parent(&e.path);
        if name == OPAQUE_NAME {
            let dir = self.ensure_dir(parent_path, &e.path)?;
            self.tree.nodes[dir].xattrs.insert(XattrKey::opaque(), b"y".to_vec());
            return Ok(());
        }
        if let Some(hidden) = name.strip_prefix(WHITEOUT_PREFIX) {
            if hidden.is_empty() || hidden == b"." || hidden == b".." {
                return Err(Error::InvalidPath { path: lossy(&e.path), reason: "invalid whiteout name" });
            }
            let parent = self.ensure_dir(parent_path, &e.path)?;
            if self.tree.child(parent, hidden).is_some() {
                // OCI: a whiteout cannot hide an entry from its own layer; containerd rejects it.
                return Err(Error::InvalidPath { path: lossy(&e.path), reason: "whiteout for an entry already in this layer" });
            }
            self.note_time(e.meta.mtime);
            let id = self.tree.add(Node::whiteout(e.meta.mtime));
            self.tree.children_mut(parent).insert(hidden.to_vec(), id);
            return Ok(());
        }
        let parent = self.ensure_dir(parent_path, &e.path)?;
        let existing = self.tree.child(parent, name);
        let kind = match e.kind {
            EntryKind::Hardlink { target } => {
                let id = self.resolve_link(&e.path, &target)?;
                // Like containerd, apply the link header's metadata to the shared inode.
                self.note_time(e.meta.mtime);
                let node = &mut self.tree.nodes[id];
                if !matches!(node.kind, Kind::Symlink { .. }) {
                    node.meta.mode = e.meta.mode;
                }
                node.meta.uid = e.meta.uid;
                node.meta.gid = e.meta.gid;
                node.meta.mtime = e.meta.mtime;
                node.xattrs.extend(e.xattrs);
                self.tree.children_mut(parent).insert(name.to_vec(), id);
                return Ok(());
            }
            EntryKind::Dir => {
                self.note_time(e.meta.mtime);
                if let Some(id) = existing.filter(|&id| self.tree.nodes[id].is_dir()) {
                    self.merge_dir(id, e.meta, e.xattrs);
                    return Ok(());
                }
                Kind::Dir { children: BTreeMap::new(), implicit: false }
            }
            EntryKind::File { size } => Kind::File { size, data: data.expect("file entry without data") },
            EntryKind::Symlink { target } => Kind::Symlink { target },
            EntryKind::CharDev { major, minor } => Kind::CharDev { major, minor },
            EntryKind::BlockDev { major, minor } => Kind::BlockDev { major, minor },
            EntryKind::Fifo => Kind::Fifo,
        };
        self.note_time(e.meta.mtime);
        let mut meta = e.meta;
        if matches!(kind, Kind::Symlink { .. }) {
            // Linux ignores symlink permission bits; they always read back as 0777.
            meta.mode = 0o777;
        }
        let id = self.tree.add(Node { kind, meta, xattrs: e.xattrs });
        self.tree.children_mut(parent).insert(name.to_vec(), id);
        Ok(())
    }

    fn merge_dir(&mut self, id: NodeId, meta: Meta, xattrs: Xattrs) {
        let node = &mut self.tree.nodes[id];
        node.meta = meta;
        node.xattrs.extend(xattrs);
        if let Kind::Dir { implicit, .. } = &mut node.kind {
            *implicit = false;
        }
    }

    /// Walks `dir_path`, creating implicit directories as needed.
    fn ensure_dir(&mut self, dir_path: &[u8], full: &[u8]) -> Result<NodeId> {
        let mut cur = self.tree.root;
        for c in components(dir_path) {
            cur = match self.tree.child(cur, c) {
                Some(id) if self.tree.nodes[id].is_dir() => id,
                Some(_) => return Err(Error::ParentNotDirectory { path: lossy(full) }),
                None => {
                    let id = self.tree.add(Node::dir(Meta::default_dir(Timestamp::default()), true));
                    self.tree.children_mut(cur).insert(c.to_vec(), id);
                    self.created_implicitly.insert(id);
                    id
                }
            };
        }
        Ok(cur)
    }

    fn resolve_link(&self, path: &[u8], target: &[u8]) -> Result<NodeId> {
        let err = |reason| Error::InvalidHardlink { path: lossy(path), target: lossy(target), reason };
        if target == path {
            return Err(err("link to itself"));
        }
        if target.len() > path.len() && target.starts_with(path) && target[path.len()] == b'/' {
            // Replacing `path` removes its subtree, and the target with it.
            return Err(err("target is inside the entry being replaced"));
        }
        let id = self.tree.lookup(target).ok_or_else(|| err("target does not exist"))?;
        let node = &self.tree.nodes[id];
        if node.is_whiteout() {
            return Err(err("target does not exist"));
        }
        if node.is_dir() {
            return Err(err("target is a directory"));
        }
        Ok(id)
    }

    /// Resolves implicit directories and returns `(tree, base time, implicit paths)`.
    ///
    /// A directory still undescribed takes the inherited attributes (or defaults). One
    /// that a later header described keeps its own meta but, as in containerd, keeps the
    /// inherited xattrs underneath its own.
    pub fn finalize(mut self, inherited: &BTreeMap<Vec<u8>, DirAttrs>) -> (Tree, Timestamp, Vec<Vec<u8>>) {
        let base = self.base_time();
        let implicit = self.implicit_dirs();
        for path in &implicit {
            let id = self.tree.lookup(path).expect("implicit dir is reachable");
            let node = &mut self.tree.nodes[id];
            if matches!(node.kind, Kind::Dir { implicit: false, .. }) {
                if let Some(attrs) = inherited.get(path) {
                    let own = std::mem::take(&mut node.xattrs);
                    node.xattrs = attrs.xattrs.clone();
                    node.xattrs.extend(own);
                }
                continue;
            }
            let opaque = node.xattrs.get(&XattrKey::opaque()).cloned();
            match inherited.get(path) {
                Some(attrs) => {
                    node.meta = attrs.meta.clone();
                    node.xattrs = attrs.xattrs.clone();
                }
                None => node.meta = Meta::default_dir(base),
            }
            if let Some(v) = opaque {
                node.xattrs.insert(XattrKey::opaque(), v);
            }
            if let Kind::Dir { implicit, .. } = &mut node.kind {
                *implicit = false;
            }
        }
        let root = &mut self.tree.nodes[self.tree.root];
        if let Kind::Dir { implicit, .. } = &mut root.kind
            && *implicit
        {
            root.meta = Meta::default_dir(base);
            *implicit = false;
        }
        (self.tree, base, implicit)
    }
}
```

`#[cfg(test)] use crate::tree::FileData;` exists only so the test module's `use super::*` can see `FileData`.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p kiln-erofs apply::`
Expected: 13 tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/kiln-erofs/src
git commit -m "feat(erofs): tree model and layer semantics (merge, replace, hardlinks, whiteouts)

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P7rdQAESQYNEo2qFobUNxt"
```

---

### Task 6: Raw tar stream → entries, with limits

**Files:**
- Create: `crates/kiln-erofs/src/tarstream.rs`
- Modify: `crates/kiln-erofs/src/lib.rs` (add `mod tarstream;`)

**Interfaces:**
- Consumes:
  - `normalize` (Task 3), and `PaxState` and `parse_records` (Task 4).
  - `Entry` and `EntryKind` (Task 5); `XattrKey`, `Meta` and `Timestamp`; `Limits` and `Error`.
- Produces: `pub(crate) fn read_tar<R: Read>(reader: R, limits: &Limits, warnings: &mut Vec<String>, f: &mut dyn FnMut(Entry, &mut dyn Read) -> Result<()>) -> Result<u64>`.
  - It stops at the first end-of-archive zero block and returns the number of tar bytes it consumed.
  - For `File` entries, `f` must read exactly `size` bytes from the reader.

**Rules:**
- The `tar` crate runs in raw mode, so kiln itself handles PAX `x`/`g` and GNU `L`/`K` entries and their size limits.
- Path precedence is PAX `path`, then GNU long name, then the header name (which already includes any ustar prefix).
- Typeflag `\0` with a trailing `/` is a directory. That is the V7 convention, and Go's `archive/tar` follows it too.
- These are rejected:
  - GNU `S` and PAX `GNU.sparse.*`.
  - A PAX `size` override that differs from the header size (only used above 8 GiB).
  - Device numbers with major > 0xfff or minor > 0xfffff.
  - Empty symlink targets.
  - Unknown typeflags.
- Xattrs from `SCHILY.xattr.*` map through `XattrKey::from_full_name`. Unmappable names, and every `LIBARCHIVE.xattr.*` name, are dropped with a warning.
  - A name over 255 bytes or a value over 65535 bytes is `XattrUnencodable`.
  - A value over `max_header_record` is `LimitExceeded`.

- [ ] **Step 1: Write the failing tests**

`crates/kiln-erofs/src/tarstream.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ondisk::{XATTR_INDEX_SECURITY, XATTR_INDEX_USER};
    use crate::testtar::{Opts, TarBuilder};

    /// Entries with their data, warnings, and bytes consumed.
    type Collected = (Vec<(Entry, Vec<u8>)>, Vec<String>, u64);

    fn collect(tar: &[u8], limits: &Limits) -> Result<Collected> {
        let mut out = Vec::new();
        let mut warnings = Vec::new();
        let n = read_tar(tar, limits, &mut warnings, &mut |e: Entry, r: &mut dyn Read| {
            let mut data = Vec::new();
            if matches!(e.kind, EntryKind::File { .. }) {
                r.read_to_end(&mut data)?;
            }
            out.push((e, data));
            Ok(())
        })?;
        Ok((out, warnings, n))
    }

    #[test]
    fn decodes_every_kind() {
        let mut b = TarBuilder::new();
        b.dir("./etc/", &Opts::default().mode(0o755))
            .file("etc/hostname", b"kiln\n", &Opts::default().uid(1000).gid(1001))
            .symlink("etc/link", "../usr/x", &Opts::default())
            .hardlink("etc/hard", "./etc/hostname")
            .chardev("dev/null", 1, 3, &Opts::default().mode(0o666))
            .fifo("run/pipe", &Opts::default());
        let (es, _, _) = collect(&b.finish(), &Limits::default()).unwrap();
        assert_eq!(es[0].0.path, b"etc");
        assert!(matches!(es[0].0.kind, EntryKind::Dir));
        assert_eq!(es[0].0.meta.mode, 0o755);
        assert_eq!(es[1].1, b"kiln\n");
        assert_eq!((es[1].0.meta.uid, es[1].0.meta.gid), (1000, 1001));
        assert!(matches!(&es[2].0.kind, EntryKind::Symlink { target } if target == b"../usr/x"));
        assert!(matches!(&es[3].0.kind, EntryKind::Hardlink { target } if target == b"etc/hostname"));
        assert!(matches!(es[4].0.kind, EntryKind::CharDev { major: 1, minor: 3 }));
        assert!(matches!(es[5].0.kind, EntryKind::Fifo));
    }

    #[test]
    fn pax_overrides_and_xattrs() {
        let long = format!("{}/file", "d".repeat(150));
        let o = Opts::default()
            .pax("path", long.as_bytes())
            .pax("uid", b"70000")
            .pax("mtime", b"1700000000.25")
            .xattr("user.a", b"1")
            .xattr("security.capability", &[1, 0, 0, 2])
            .xattr("com.apple.quarantine", b"q")
            .pax("LIBARCHIVE.xattr.user.b", b"eA==");
        let mut b = TarBuilder::new();
        b.file("short", b"x", &o);
        let (es, warnings, _) = collect(&b.finish(), &Limits::default()).unwrap();
        let e = &es[0].0;
        assert_eq!(e.path, long.as_bytes());
        assert_eq!(e.meta.uid, 70000);
        assert_eq!(e.meta.mtime, Timestamp { sec: 1_700_000_000, nsec: 250_000_000 });
        let keys: Vec<(u8, Vec<u8>)> = e.xattrs.keys().map(|k| (k.index, k.name.clone())).collect();
        assert_eq!(keys, vec![(XATTR_INDEX_USER, b"a".to_vec()), (XATTR_INDEX_SECURITY, b"capability".to_vec())]);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
    }

    #[test]
    fn gnu_long_names_and_links() {
        let long = format!("{}/f", "x".repeat(120));
        let target = format!("{}/t", "y".repeat(130));
        let mut b = TarBuilder::new();
        b.file(&long, b"", &Opts::default()).symlink("s", &target, &Opts::default());
        let (es, _, _) = collect(&b.finish(), &Limits::default()).unwrap();
        assert_eq!(es[0].0.path, long.as_bytes());
        assert!(matches!(&es[1].0.kind, EntryKind::Symlink { target: t } if t == target.as_bytes()));
    }

    #[test]
    fn v7_directory_with_trailing_slash() {
        let mut b = TarBuilder::new();
        b.entry(b"olddir/", 0, b"", b"", (0, 0), &Opts::default());
        let (es, _, _) = collect(&b.finish(), &Limits::default()).unwrap();
        assert!(matches!(es[0].0.kind, EntryKind::Dir));
        assert_eq!(es[0].0.path, b"olddir");
    }

    #[test]
    fn rejects_sparse_bad_devices_and_empty_symlinks() {
        let l = Limits::default();
        let mut b = TarBuilder::new();
        b.entry(b"s", b'S', b"", b"", (0, 0), &Opts::default());
        assert!(matches!(collect(&b.finish(), &l), Err(Error::UnsupportedEntry { .. })));
        let mut b = TarBuilder::new();
        b.file("p", b"", &Opts::default().pax("GNU.sparse.major", b"1"));
        assert!(matches!(collect(&b.finish(), &l), Err(Error::UnsupportedEntry { .. })));
        let mut b = TarBuilder::new();
        b.chardev("c", 4096, 0, &Opts::default());
        assert!(matches!(collect(&b.finish(), &l), Err(Error::UnsupportedEntry { .. })));
        let mut b = TarBuilder::new();
        b.symlink("s", "", &Opts::default());
        assert!(matches!(collect(&b.finish(), &l), Err(Error::UnsupportedEntry { .. })));
        let mut b = TarBuilder::new();
        b.entry(b"v", b'V', b"", b"", (0, 0), &Opts::default());
        assert!(matches!(collect(&b.finish(), &l), Err(Error::UnsupportedEntry { .. })));
    }

    #[test]
    fn enforces_limits() {
        let mut b = TarBuilder::new();
        b.file("a", b"", &Opts::default()).file("b", b"", &Opts::default()).file("c", b"", &Opts::default());
        let l = Limits { max_entries: 2, ..Limits::default() };
        assert!(matches!(collect(&b.finish(), &l), Err(Error::LimitExceeded { limit: "entries per layer", .. })));

        let mut b = TarBuilder::new();
        b.file("a", b"", &Opts::default().pax("comment", &[b'c'; 200]));
        let l = Limits { max_header_record: 64, ..Limits::default() };
        assert!(matches!(collect(&b.finish(), &l), Err(Error::LimitExceeded { limit: "tar header record", .. })));

        let mut b = TarBuilder::new();
        b.file("big", &[7u8; 8192], &Opts::default());
        let l = Limits { max_layer_bytes: 4096, ..Limits::default() };
        assert!(matches!(collect(&b.finish(), &l), Err(Error::LimitExceeded { limit: "uncompressed bytes per layer", .. })));

        let mut b = TarBuilder::new();
        b.file("x", b"", &Opts::default().xattr("user.big", &vec![1u8; 70_000]));
        assert!(matches!(collect(&b.finish(), &Limits::default()), Err(Error::XattrUnencodable { .. })));
    }

    #[test]
    fn consumes_through_the_first_end_block() {
        let mut b = TarBuilder::new();
        b.file("a", b"hello", &Opts::default());
        let (_, _, n) = collect(&b.finish(), &Limits::default()).unwrap();
        let body = b.bytes().len() as u64;
        assert!(n >= body + 512 && n <= body + 1024, "consumed {n}, body {body}");
    }
}
```
Add `mod tarstream;` to `lib.rs`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p kiln-erofs tarstream::`
Expected: compile error `cannot find function read_tar`.

- [ ] **Step 3: Implement**

Prepend to `tarstream.rs`:
```rust
//! Raw tar stream decoding into `Entry` events (spec §7.4, §7.6).

use std::cell::Cell;
use std::io::{self, Read};
use std::rc::Rc;

use tar::{Archive, EntryType, Header};

use crate::apply::{Entry, EntryKind};
use crate::error::{lossy, Error, Result};
use crate::limits::Limits;
use crate::pax::{parse_records, PaxState};
use crate::path::normalize;
use crate::tree::{Meta, Timestamp, XattrKey, Xattrs};

/// Counts consumed bytes and fails reads past the layer byte limit.
struct Counting<R> {
    inner: R,
    count: Rc<Cell<u64>>,
    limit: u64,
    exceeded: Rc<Cell<bool>>,
}

impl<R: Read> Read for Counting<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        let total = self.count.get() + n as u64;
        self.count.set(total);
        if total > self.limit {
            self.exceeded.set(true);
            return Err(io::Error::other("kiln: layer byte limit exceeded"));
        }
        Ok(n)
    }
}

fn tar_err(e: io::Error) -> Error {
    match e.kind() {
        io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof | io::ErrorKind::Other => Error::MalformedTar(e.to_string()),
        _ => Error::Io(e),
    }
}

/// Reads entries up to the end-of-archive marker; returns tar bytes consumed.
pub(crate) fn read_tar<R: Read>(
    reader: R,
    limits: &Limits,
    warnings: &mut Vec<String>,
    f: &mut dyn FnMut(Entry, &mut dyn Read) -> Result<()>,
) -> Result<u64> {
    let count = Rc::new(Cell::new(0u64));
    let exceeded = Rc::new(Cell::new(false));
    let mut archive = Archive::new(Counting {
        inner: reader,
        count: Rc::clone(&count),
        limit: limits.max_layer_bytes,
        exceeded: Rc::clone(&exceeded),
    });
    match walk(&mut archive, limits, warnings, f) {
        Err(_) if exceeded.get() => Err(Error::LimitExceeded {
            limit: "uncompressed bytes per layer",
            max: limits.max_layer_bytes,
            path: String::new(),
        }),
        Err(e) => Err(e),
        Ok(()) => Ok(count.get()),
    }
}

fn walk<R: Read>(
    archive: &mut Archive<R>,
    limits: &Limits,
    warnings: &mut Vec<String>,
    f: &mut dyn FnMut(Entry, &mut dyn Read) -> Result<()>,
) -> Result<()> {
    let mut global = PaxState::default();
    let mut local = PaxState::default();
    let mut long_name: Option<Vec<u8>> = None;
    let mut long_link: Option<Vec<u8>> = None;
    let mut seen: u64 = 0;
    for item in archive.entries().map_err(tar_err)?.raw(true) {
        let mut ent = item.map_err(tar_err)?;
        let header = ent.header().clone();
        let et = header.entry_type();
        if et.is_pax_local_extensions() || et.is_pax_global_extensions() || et.is_gnu_longname() || et.is_gnu_longlink() {
            let data = read_record(&mut ent, &header, limits)?;
            if et.is_pax_local_extensions() {
                local.apply(parse_records(&data)?)?;
            } else if et.is_pax_global_extensions() {
                global.apply(parse_records(&data)?)?;
            } else {
                let trimmed = trim_nul(data);
                if et.is_gnu_longname() {
                    long_name = Some(trimmed);
                } else {
                    long_link = Some(trimmed);
                }
            }
            continue;
        }
        let pax = PaxState::overlay(&global, std::mem::take(&mut local));
        let raw_path = pax.path.clone().or(long_name.take()).unwrap_or_else(|| header.path_bytes().into_owned());
        let raw_link = pax.linkpath.clone().or(long_link.take()).or_else(|| header.link_name_bytes().map(|c| c.into_owned()));
        seen += 1;
        if seen > limits.max_entries {
            return Err(Error::LimitExceeded { limit: "entries per layer", max: limits.max_entries, path: lossy(&raw_path) });
        }
        let entry = build_entry(&header, et, &raw_path, raw_link, &pax, limits, warnings)?;
        f(entry, &mut ent)?;
    }
    Ok(())
}

fn read_record(ent: &mut dyn Read, header: &Header, limits: &Limits) -> Result<Vec<u8>> {
    let size = header.entry_size().map_err(tar_err)?;
    if size > limits.max_header_record {
        return Err(Error::LimitExceeded { limit: "tar header record", max: limits.max_header_record, path: lossy(&header.path_bytes()) });
    }
    let mut data = Vec::with_capacity(size as usize);
    ent.take(size).read_to_end(&mut data).map_err(tar_err)?;
    if data.len() as u64 != size {
        return Err(Error::MalformedTar("truncated header record".into()));
    }
    Ok(data)
}

fn trim_nul(mut v: Vec<u8>) -> Vec<u8> {
    while v.last() == Some(&0) {
        v.pop();
    }
    v
}

fn to_u32(v: u64, what: &'static str, path: &[u8]) -> Result<u32> {
    u32::try_from(v).map_err(|_| Error::UnsupportedEntry { path: lossy(path), kind: format!("{what} {v} exceeds 32 bits") })
}

fn build_entry(
    header: &Header,
    et: EntryType,
    raw_path: &[u8],
    raw_link: Option<Vec<u8>>,
    pax: &PaxState,
    limits: &Limits,
    warnings: &mut Vec<String>,
) -> Result<Entry> {
    let unsupported = |kind: &str| Error::UnsupportedEntry { path: lossy(raw_path), kind: kind.to_string() };
    if pax.sparse || et.is_gnu_sparse() {
        return Err(unsupported("sparse file"));
    }
    let path = normalize(raw_path, limits)?;
    let typeflag = header.as_bytes()[156];
    let size = header.entry_size().map_err(tar_err)?;
    if pax.size.is_some_and(|s| s != size) {
        return Err(unsupported("PAX size override (entries larger than 8 GiB)"));
    }
    let uid = to_u32(pax.uid.map_or_else(|| header.uid(), Ok).map_err(tar_err)?, "uid", &path)?;
    let gid = to_u32(pax.gid.map_or_else(|| header.gid(), Ok).map_err(tar_err)?, "gid", &path)?;
    let mtime = match pax.mtime {
        Some(t) => t,
        None => Timestamp { sec: header.mtime().map_err(tar_err)? as i64, nsec: 0 },
    };
    let meta = Meta { mode: header.mode().map_err(tar_err)? & 0o7777, uid, gid, mtime };
    let xattrs = convert_xattrs(pax, &path, limits, warnings)?;
    let device = || -> Result<(u32, u32)> {
        let major = header.device_major().map_err(tar_err)?.unwrap_or(0);
        let minor = header.device_minor().map_err(tar_err)?.unwrap_or(0);
        if major > 0xfff || minor > 0xf_ffff {
            return Err(unsupported("device number out of range"));
        }
        Ok((major, minor))
    };
    let kind = match et {
        _ if typeflag == 0 && raw_path.ends_with(b"/") => EntryKind::Dir,
        EntryType::Regular | EntryType::Continuous => EntryKind::File { size },
        EntryType::Directory => EntryKind::Dir,
        EntryType::Symlink => {
            let target = raw_link.unwrap_or_default();
            if target.is_empty() {
                return Err(unsupported("empty symlink target"));
            }
            if target.contains(&0) || target.len() > limits.max_path_len {
                return Err(Error::InvalidPath { path: lossy(&path), reason: "symlink target has NUL or is too long" });
            }
            EntryKind::Symlink { target }
        }
        EntryType::Link => {
            let target = raw_link.ok_or_else(|| Error::InvalidHardlink { path: lossy(&path), target: String::new(), reason: "missing target" })?;
            EntryKind::Hardlink { target: normalize(&target, limits)? }
        }
        EntryType::Char => {
            let (major, minor) = device()?;
            EntryKind::CharDev { major, minor }
        }
        EntryType::Block => {
            let (major, minor) = device()?;
            EntryKind::BlockDev { major, minor }
        }
        EntryType::Fifo => EntryKind::Fifo,
        other => return Err(unsupported(&format!("tar entry type {other:?}"))),
    };
    Ok(Entry { path, kind, meta, xattrs })
}

fn convert_xattrs(pax: &PaxState, path: &[u8], limits: &Limits, warnings: &mut Vec<String>) -> Result<Xattrs> {
    let mut out = Xattrs::new();
    for name in &pax.dropped {
        warnings.push(format!("dropping LIBARCHIVE xattr {:?} on {:?}", lossy(name), lossy(path)));
    }
    for (name, value) in &pax.xattrs {
        if value.len() as u64 > limits.max_header_record {
            return Err(Error::LimitExceeded { limit: "xattr value", max: limits.max_header_record, path: lossy(path) });
        }
        let Some(key) = XattrKey::from_full_name(name) else {
            warnings.push(format!("dropping xattr {:?} on {:?}: namespace not representable in erofs", lossy(name), lossy(path)));
            continue;
        };
        let bad = |reason| Error::XattrUnencodable { path: lossy(path), name: lossy(name), reason };
        if key.name.len() > 255 {
            return Err(bad("name longer than 255 bytes"));
        }
        if value.len() > 65_535 {
            return Err(bad("value larger than 65535 bytes"));
        }
        out.insert(key, value.clone());
    }
    Ok(out)
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p kiln-erofs tarstream::`
Expected: 7 tests pass. If `consumes_through_the_first_end_block` fails, the `tar` crate's end-marker handling differs from what's expected. Record the actual count in the assertion message and keep the bounds: the contract M1b needs is "stops within the end marker", which Task 8's `stops_at_end_of_archive` pins precisely.

- [ ] **Step 5: Commit**

```bash
git add crates/kiln-erofs/src
git commit -m "feat(erofs): raw tar decoding with PAX, GNU long names and limits

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P7rdQAESQYNEo2qFobUNxt"
```

---

### Task 7: Layout rules

**Files:**
- Create: `crates/kiln-erofs/src/layout.rs`
- Modify: `crates/kiln-erofs/src/lib.rs` (add `mod layout;`)

**Interfaces:**
- Consumes: `ondisk` helpers (Task 2); `Meta`, `Timestamp` and `Xattrs` (Task 5).
- Produces (`pub(crate)`):
  - `xattr_ibody_bound(&Xattrs) -> usize`.
  - `tail_fits_inline(tail_len: u64, &Xattrs) -> bool`.
  - `fits_compact(&Meta, nlink: u32, size: u64, base: Timestamp) -> bool`.
  - `pack_dir(&[&[u8]]) -> Vec<Range<usize>>`, `dir_size(&[&[u8]]) -> u64` and `encode_dir(&[(&[u8], u64, u8)]) -> Vec<u8>`.
  - `struct XattrPlan { table: Vec<u8>, ibodies: Vec<Vec<u8>> }` and `plan_xattrs(&[&Xattrs]) -> Result<XattrPlan>`.
  - `assign_nids(&[usize]) -> (Vec<u64>, u64)`.

**Rules:**
- **Inline tails:** the kernel requires an inline tail to sit right after the inode and its xattrs, within one block. `tail_fits_inline` uses the worst case: extended inode, all xattrs inline. Sharing only ever shrinks the xattr body, so a tail accepted while streaming always fits at `finish`.
- **Forced sharing:** an inode whose all-inline xattr body would exceed `4096 − 64` shares all of its xattrs.
- **Directory blocks:** each block holds dirents and then names, packed greedily. Non-last blocks are zero-padded to 4096.
- **nids:** slot 0 is reserved. A record that would cross a block boundary moves to the next block.

- [ ] **Step 1: Write the failing tests**

`crates/kiln-erofs/src/layout.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ondisk::{decode_dirent, FT_DIR, FT_REG_FILE, XATTR_INDEX_USER};
    use crate::tree::XattrKey;

    fn x(pairs: &[(&str, &str)]) -> Xattrs {
        pairs
            .iter()
            .map(|(n, v)| (XattrKey { index: XATTR_INDEX_USER, name: n.as_bytes().to_vec() }, v.as_bytes().to_vec()))
            .collect()
    }

    #[test]
    fn tail_inline_boundaries() {
        let none = Xattrs::new();
        assert!(!tail_fits_inline(0, &none));
        assert!(tail_fits_inline(4032, &none));
        assert!(!tail_fits_inline(4033, &none));
        let one = x(&[("a", "1")]); // bound = 12 + 8
        assert!(tail_fits_inline(4012, &one));
        assert!(!tail_fits_inline(4013, &one));
    }

    #[test]
    fn compact_rules() {
        let base = Timestamp { sec: 100, nsec: 5 };
        let m = Meta { mode: 0o644, uid: 65_535, gid: 0, mtime: base };
        assert!(fits_compact(&m, 1, (1 << 32) - 1, base));
        assert!(!fits_compact(&m, 1, 1 << 32, base));
        assert!(!fits_compact(&m, 65_536, 0, base));
        assert!(!fits_compact(&Meta { uid: 65_536, ..m.clone() }, 1, 0, base));
        assert!(!fits_compact(&Meta { mtime: Timestamp { sec: 100, nsec: 6 }, ..m }, 1, 0, base));
    }

    #[test]
    fn dir_packing() {
        let ones: Vec<Vec<u8>> = (0..316).map(|i| vec![b'a' + (i % 26) as u8]).collect();
        let refs: Vec<&[u8]> = ones.iter().map(|v| v.as_slice()).collect();
        assert_eq!(pack_dir(&refs), vec![0..315, 315..316]);
        let twenties: Vec<Vec<u8>> = (0..129).map(|i| format!("{i:020}").into_bytes()).collect();
        let refs: Vec<&[u8]> = twenties.iter().map(|v| v.as_slice()).collect();
        assert_eq!(pack_dir(&refs[..128]), vec![0..128], "12*128 + 20*128 == 4096 fits exactly");
        assert_eq!(dir_size(&refs[..128]), 4096);
        assert_eq!(pack_dir(&refs), vec![0..128, 128..129]);
        assert_eq!(dir_size(&refs), 4096 + 32);
        assert_eq!(dir_size(&[&b"."[..], &b".."[..]]), 27);
    }

    #[test]
    fn dir_encoding() {
        let enc = encode_dir(&[(&b"."[..], 1, FT_DIR), (&b".."[..], 1, FT_DIR), (&b"a"[..], 2, FT_REG_FILE)]);
        assert_eq!(enc.len(), 3 * 12 + 4);
        assert_eq!(decode_dirent(&enc[0..]), (1, 36, FT_DIR));
        assert_eq!(decode_dirent(&enc[12..]), (1, 37, FT_DIR));
        assert_eq!(decode_dirent(&enc[24..]), (2, 39, FT_REG_FILE));
        assert_eq!(&enc[36..], b"...a");

        let names: Vec<Vec<u8>> = (0..129).map(|i| format!("{i:020}").into_bytes()).collect();
        let ents: Vec<(&[u8], u64, u8)> = names.iter().map(|n| (n.as_slice(), 9, FT_REG_FILE)).collect();
        let enc = encode_dir(&ents);
        assert_eq!(enc.len(), 4096 + 32);
        assert_eq!(decode_dirent(&enc[4096..]).1, 12, "second block starts its own name area");
    }

    #[test]
    fn nid_assignment_never_crosses_blocks() {
        let (nids, meta_len) = assign_nids(&[32, 64, 4000, 100]);
        assert_eq!(nids, vec![1, 2, 128, 256]);
        assert_eq!(meta_len, 12_288);
    }

    #[test]
    fn xattr_sharing() {
        let a = x(&[("x", "1"), ("y", "2")]);
        let b = x(&[("x", "1")]);
        let c = Xattrs::new();
        let plan = plan_xattrs(&[&a, &b, &c]).unwrap();
        assert_eq!(plan.table, vec![1, XATTR_INDEX_USER, 1, 0, b'x', b'1', 0, 0]);
        assert_eq!(plan.ibodies[0].len(), 12 + 4 + 8);
        assert_eq!(plan.ibodies[0][4], 1, "shared count");
        assert_eq!(&plan.ibodies[0][12..16], &0u32.to_le_bytes());
        assert_eq!(plan.ibodies[1].len(), 16);
        assert!(plan.ibodies[2].is_empty());
    }

    #[test]
    fn oversized_inline_xattrs_are_forced_into_the_table() {
        let mut big = Xattrs::new();
        big.insert(XattrKey { index: XATTR_INDEX_USER, name: b"k".to_vec() }, vec![7u8; 4030]);
        let plan = plan_xattrs(&[&big]).unwrap();
        assert_eq!(plan.ibodies[0].len(), 16);
        assert_eq!(plan.table.len(), crate::ondisk::xattr_entry_len(1, 4030));
    }
}
```
Add `mod layout;` to `lib.rs`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p kiln-erofs layout::`
Expected: compile error `cannot find function tail_fits_inline`.

- [ ] **Step 3: Implement**

Prepend to `layout.rs`:
```rust
//! Layout decisions shared by the streaming writer and squash (spec §7.1–§7.3).

use std::collections::HashMap;
use std::ops::Range;

use crate::error::{Error, Result};
use crate::ondisk::{
    encode_dirent, encode_xattr_entry, encode_xattr_ibody_header, round_up, xattr_entry_len, BLOCK_SIZE,
    DIRENT_LEN, EXTENDED_INODE_LEN, MAX_SHARED_XATTRS, SLOT_SIZE, XATTR_IBODY_HEADER_LEN,
};
use crate::tree::{Meta, Timestamp, XattrKey, Xattrs};

/// Size of the xattr body if every xattr were inline (sharing only shrinks it).
pub(crate) fn xattr_ibody_bound(x: &Xattrs) -> usize {
    if x.is_empty() {
        return 0;
    }
    XATTR_IBODY_HEADER_LEN + x.iter().map(|(k, v)| xattr_entry_len(k.name.len(), v.len())).sum::<usize>()
}

/// Whether a tail of `tail_len` bytes fits inline whatever the final inode layout.
pub(crate) fn tail_fits_inline(tail_len: u64, xattrs: &Xattrs) -> bool {
    tail_len > 0 && tail_len as usize + EXTENDED_INODE_LEN + xattr_ibody_bound(xattrs) <= BLOCK_SIZE as usize
}

pub(crate) fn fits_compact(meta: &Meta, nlink: u32, size: u64, base: Timestamp) -> bool {
    meta.uid <= 0xffff && meta.gid <= 0xffff && nlink <= 0xffff && size < (1 << 32) && meta.mtime == base
}

/// Greedily packs sorted names into directory blocks.
pub(crate) fn pack_dir(names: &[&[u8]]) -> Vec<Range<usize>> {
    let mut blocks = Vec::new();
    let mut start = 0;
    let mut used = 0usize;
    for (i, name) in names.iter().enumerate() {
        let need = DIRENT_LEN + name.len();
        if used + need > BLOCK_SIZE as usize {
            blocks.push(start..i);
            start = i;
            used = 0;
        }
        used += need;
    }
    blocks.push(start..names.len());
    blocks
}

/// Byte length of the encoded directory (`i_size`).
pub(crate) fn dir_size(names: &[&[u8]]) -> u64 {
    let blocks = pack_dir(names);
    let last = blocks.last().expect("at least one block");
    let used: usize = names[last.clone()].iter().map(|n| DIRENT_LEN + n.len()).sum();
    (blocks.len() as u64 - 1) * BLOCK_SIZE + used as u64
}

/// Encodes `(name, nid, file_type)` entries, which must be sorted by name.
pub(crate) fn encode_dir(entries: &[(&[u8], u64, u8)]) -> Vec<u8> {
    let names: Vec<&[u8]> = entries.iter().map(|e| e.0).collect();
    let blocks = pack_dir(&names);
    let mut out = Vec::new();
    for (bi, range) in blocks.iter().enumerate() {
        let block_start = out.len();
        let mut nameoff = range.len() * DIRENT_LEN;
        for &(name, nid, ft) in &entries[range.clone()] {
            encode_dirent(nid, nameoff as u16, ft, &mut out);
            nameoff += name.len();
        }
        for &(name, _, _) in &entries[range.clone()] {
            out.extend_from_slice(name);
        }
        if bi + 1 < blocks.len() {
            out.resize(block_start + BLOCK_SIZE as usize, 0);
        }
    }
    out
}

pub(crate) struct XattrPlan {
    /// Shared xattr table (entries 4-byte aligned; id = offset / 4).
    pub table: Vec<u8>,
    /// Encoded xattr body per inode, in inode order (empty when none).
    pub ibodies: Vec<Vec<u8>>,
}

/// Decides shared vs inline xattrs for inodes given in inode-numbering order.
pub(crate) fn plan_xattrs(inodes: &[&Xattrs]) -> Result<XattrPlan> {
    let mut counts: HashMap<(&XattrKey, &[u8]), u32> = HashMap::new();
    for x in inodes {
        for (k, v) in x.iter() {
            *counts.entry((k, v.as_slice())).or_default() += 1;
        }
    }
    let mut ids: HashMap<(&XattrKey, &[u8]), u32> = HashMap::new();
    let mut table = Vec::new();
    let mut ibodies = Vec::with_capacity(inodes.len());
    for x in inodes {
        if x.is_empty() {
            ibodies.push(Vec::new());
            continue;
        }
        let force = xattr_ibody_bound(x) > BLOCK_SIZE as usize - EXTENDED_INODE_LEN;
        let mut shared = Vec::new();
        let mut inline = Vec::new();
        for (k, v) in x.iter() {
            let key = (k, v.as_slice());
            if force || counts[&key] >= 2 {
                let id = *ids.entry(key).or_insert_with(|| {
                    let id = (table.len() / 4) as u32;
                    encode_xattr_entry(k.index, &k.name, v, &mut table);
                    id
                });
                shared.push(id);
            } else {
                inline.push((k, v));
            }
        }
        if shared.len() > MAX_SHARED_XATTRS {
            return Err(Error::TooManyXattrs);
        }
        let mut body = Vec::new();
        encode_xattr_ibody_header(shared.len() as u8, &mut body);
        for id in shared {
            body.extend_from_slice(&id.to_le_bytes());
        }
        for (k, v) in inline {
            encode_xattr_entry(k.index, &k.name, v, &mut body);
        }
        ibodies.push(body);
    }
    Ok(XattrPlan { table, ibodies })
}

/// Assigns nids to inode records of the given byte lengths, in order. Slot 0 is
/// reserved and no record crosses a block. Returns nids and the metadata area length.
pub(crate) fn assign_nids(record_lens: &[usize]) -> (Vec<u64>, u64) {
    let mut off = SLOT_SIZE;
    let mut nids = Vec::with_capacity(record_lens.len());
    for &len in record_lens {
        let len = len as u64;
        assert!(len <= BLOCK_SIZE, "inode record of {len} bytes exceeds a block");
        if off % BLOCK_SIZE + len > BLOCK_SIZE {
            off = round_up(off, BLOCK_SIZE);
        }
        nids.push(off / SLOT_SIZE);
        off += round_up(len, SLOT_SIZE);
    }
    (nids, round_up(off, BLOCK_SIZE))
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p kiln-erofs layout::`
Expected: 7 tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/kiln-erofs/src
git commit -m "feat(erofs): directory packing, xattr planning, nid assignment

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P7rdQAESQYNEo2qFobUNxt"
```

---
### Task 8: Layer writer and image emission

**Files:**
- Create: `crates/kiln-erofs/src/writer.rs`, `crates/kiln-erofs/tests/writer.rs`
- Modify: `crates/kiln-erofs/src/lib.rs`

**Interfaces:**
- Consumes:
  - `read_tar` (Task 6) and `LayerBuilder`, `Entry` and `EntryKind` (Task 5).
  - From `layout` (Task 7): `assign_nids`, `dir_size`, `encode_dir`, `fits_compact`, `plan_xattrs` and `tail_fits_inline`.
  - `ondisk` (Task 2), and `Tree`, `Kind`, `Data`, `FileData` and `TailRef` (Task 5).
- Produces (`pub`):
  - `LayerWriter<W: Write + Seek>`:
    - `new(out: W, spill_dir: &Path, limits: Limits) -> Result<Self>`.
    - `append_tar<R: Read>(&mut self, tar: R) -> Result<()>` reads exactly up to the end-of-archive marker.
    - `implicit_dirs(&self) -> Vec<Vec<u8>>`.
    - `finish(self, inherited: &BTreeMap<Vec<u8>, DirAttrs>) -> Result<(W, LayerSummary)>`.
  - `LayerSummary { inodes: u64, image_bytes: u64, tar_bytes: u64, implicit_dirs: Vec<Vec<u8>>, warnings: Vec<String> }`.
  - `const FORMAT_VERSION: u32 = 1`.
- Produces (`pub(crate)`, used by squash in Task 10):
  - `DataStore<W>` with `new(out, spill_dir)`, `write_file(&mut dyn Read, size, inline_tail) -> Result<FileData>`, `write_blocks(&[u8]) -> Result<u32>` and `read_tail(TailRef) -> Result<Vec<u8>>`.
  - `trait ExternalData { fn open(&mut self, layer: usize, nid: u64) -> Result<Box<dyn Read + '_>>; }` and `NoExternal`.
  - `emit<W>(Tree, base: Timestamp, DataStore<W>, &mut dyn ExternalData) -> Result<(W, EmitStats)>` and `EmitStats { inodes, bytes }`.

**Image layout:**
```
block 0                    zeros, superblock at byte 1024 (written last)
blocks 1..                 streamed file data, in tar order
then                       external file data (squash), in inode order
then                       directory and symlink bodies that are not fully inline, in inode order
then (if any)              shared xattr table (xattr_blkaddr)
then                       metadata area (meta_blkaddr): slot 0 zero, inodes from nid 1, in inode order
```

- [ ] **Step 1: Write the failing tests**

`crates/kiln-erofs/tests/writer.rs`:
```rust
use std::collections::BTreeMap;
use std::io::Cursor;

use kiln_erofs::ondisk::{SuperBlock, BLOCK_SIZE, SUPER_OFFSET};
use kiln_erofs::testtar::{Opts, TarBuilder};
use kiln_erofs::{Error, LayerWriter, Limits};

fn write(tar: &[u8]) -> kiln_erofs::Result<Vec<u8>> {
    let dir = tempfile::tempdir().unwrap();
    let mut w = LayerWriter::new(Cursor::new(Vec::new()), dir.path(), Limits::default())?;
    w.append_tar(tar)?;
    let (out, _) = w.finish(&BTreeMap::new())?;
    Ok(out.into_inner())
}

fn sb(img: &[u8]) -> SuperBlock {
    SuperBlock::decode(&img[SUPER_OFFSET as usize..]).unwrap()
}

#[test]
fn empty_layer_is_two_blocks() {
    let img = write(&TarBuilder::new().finish()).unwrap();
    assert_eq!(img.len(), 8192);
    let s = sb(&img);
    assert_eq!((s.root_nid, s.inos, s.blocks, s.meta_blkaddr, s.xattr_blkaddr, s.epoch), (1, 1, 2, 1, 0, 0));
}

#[test]
fn image_is_block_aligned_and_counts_inodes() {
    let tar = TarBuilder::new()
        .dir("etc", &Opts::default().mode(0o755))
        .file("etc/a", &[1u8; 10], &Opts::default())
        .file("etc/b", &[2u8; 5000], &Opts::default())
        .symlink("etc/c", "a", &Opts::default())
        .finish();
    let img = write(&tar).unwrap();
    let s = sb(&img);
    assert_eq!(s.inos, 5);
    assert_eq!(img.len() as u64, u64::from(s.blocks) * BLOCK_SIZE);
}

#[test]
fn data_blocks_hold_file_bytes() {
    let tar = TarBuilder::new().file("f", &[0xAB; 8192], &Opts::default()).finish();
    let img = write(&tar).unwrap();
    assert!(img[4096..4096 + 8192].iter().all(|&b| b == 0xAB));
}

#[test]
fn same_input_same_bytes_across_spill_dirs() {
    let tar = TarBuilder::new()
        .dir("d", &Opts::default().xattr("user.k", b"v"))
        .file("d/f", &[3u8; 7000], &Opts::default().xattr("user.k", b"v"))
        .finish();
    assert_eq!(write(&tar).unwrap(), write(&tar).unwrap());
}

#[test]
fn truncated_tar_is_malformed() {
    let full = TarBuilder::new().file("f", &[1u8; 5000], &Opts::default()).finish();
    assert!(matches!(write(&full[..512 + 1000]), Err(Error::MalformedTar(_))));
}

#[test]
fn stops_at_end_of_archive() {
    let mut tar = TarBuilder::new().file("f", b"x", &Opts::default()).finish();
    tar.extend_from_slice(b"TRAILER");
    let mut rest: &[u8] = &tar;
    let dir = tempfile::tempdir().unwrap();
    let mut w = LayerWriter::new(Cursor::new(Vec::new()), dir.path(), Limits::default()).unwrap();
    w.append_tar(&mut rest).unwrap();
    assert!(rest.ends_with(b"TRAILER"), "the bytes after the archive must stay unread");
    assert!(rest.len() <= 512 + 7, "at most the second end block may remain unread");
    let (_, summary) = w.finish(&BTreeMap::new()).unwrap();
    assert_eq!(summary.tar_bytes, (tar.len() - rest.len()) as u64);
}

#[test]
fn implicit_dirs_are_reported_before_finish() {
    let tar = TarBuilder::new().file("a/b/f", b"", &Opts::default()).finish();
    let dir = tempfile::tempdir().unwrap();
    let mut w = LayerWriter::new(Cursor::new(Vec::new()), dir.path(), Limits::default()).unwrap();
    w.append_tar(tar.as_slice()).unwrap();
    assert_eq!(w.implicit_dirs(), vec![b"a".to_vec(), b"a/b".to_vec()]);
    let (_, summary) = w.finish(&BTreeMap::new()).unwrap();
    assert_eq!(summary.implicit_dirs, vec![b"a".to_vec(), b"a/b".to_vec()]);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p kiln-erofs --test writer`
Expected: compile error `unresolved import kiln_erofs::LayerWriter`.

- [ ] **Step 3: Implement `writer.rs`**

```rust
//! The layer writer: streams data, then lays out metadata (spec §7.1–§7.3).

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::apply::{Entry, EntryKind, LayerBuilder};
use crate::error::{Error, Result};
use crate::layout::{assign_nids, dir_size, encode_dir, fits_compact, plan_xattrs, tail_fits_inline};
use crate::limits::Limits;
use crate::ondisk::{
    encode_rdev, xattr_icount_for, DiskInode, SuperBlock, BLOCK_SIZE, COMPACT_INODE_LEN, EXTENDED_INODE_LEN,
    FT_BLKDEV, FT_CHRDEV, FT_DIR, FT_FIFO, FT_REG_FILE, FT_SYMLINK, LAYOUT_FLAT_INLINE, LAYOUT_FLAT_PLAIN,
    S_IFBLK, S_IFCHR, S_IFDIR, S_IFIFO, S_IFLNK, S_IFREG, SLOT_SIZE, SUPER_OFFSET,
};
use crate::tarstream::read_tar;
use crate::tree::{Data, DirAttrs, FileData, Kind, NodeId, TailRef, Timestamp, Tree, Xattrs};

const ZEROS: [u8; BLOCK_SIZE as usize] = [0; BLOCK_SIZE as usize];

fn too_many_blocks() -> Error {
    Error::LimitExceeded { limit: "image blocks", max: u64::from(u32::MAX), path: String::new() }
}

fn read_exact(r: &mut dyn Read, buf: &mut [u8]) -> Result<()> {
    r.read_exact(buf).map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            Error::MalformedTar("entry data truncated".into())
        } else {
            Error::Io(e)
        }
    })
}

fn copy_exact(r: &mut dyn Read, w: &mut impl Write, mut n: u64) -> Result<()> {
    let mut buf = vec![0u8; 128 * 1024];
    while n > 0 {
        let want = n.min(buf.len() as u64) as usize;
        read_exact(r, &mut buf[..want])?;
        w.write_all(&buf[..want])?;
        n -= want as u64;
    }
    Ok(())
}

fn write_zeros(w: &mut impl Write, mut n: u64) -> Result<()> {
    while n > 0 {
        let k = n.min(BLOCK_SIZE);
        w.write_all(&ZEROS[..k as usize])?;
        n -= k;
    }
    Ok(())
}

/// The output plus a spill file for inline tails. Between calls the output
/// position is always `next_blk * BLOCK_SIZE`.
pub(crate) struct DataStore<W> {
    out: W,
    next_blk: u32,
    spill: File,
    spill_len: u64,
}

impl<W: Write + Seek> DataStore<W> {
    pub fn new(mut out: W, spill_dir: &Path) -> Result<Self> {
        out.seek(SeekFrom::Start(0))?;
        out.write_all(&ZEROS)?;
        Ok(Self { out, next_blk: 1, spill: tempfile::tempfile_in(spill_dir)?, spill_len: 0 })
    }

    /// Streams `size` bytes from `r`. Full blocks go to the data area; a partial
    /// tail goes to the spill file when `inline_tail`, else to a zero-padded block.
    pub fn write_file(&mut self, r: &mut dyn Read, size: u64, inline_tail: bool) -> Result<FileData> {
        let tail_len = size % BLOCK_SIZE;
        let full = size - tail_len;
        let start = self.next_blk;
        copy_exact(r, &mut self.out, full)?;
        let mut blocks = full / BLOCK_SIZE;
        let mut tail = None;
        if tail_len > 0 {
            let mut buf = vec![0u8; tail_len as usize];
            read_exact(r, &mut buf)?;
            if inline_tail {
                self.spill.seek(SeekFrom::Start(self.spill_len))?;
                self.spill.write_all(&buf)?;
                tail = Some(TailRef { offset: self.spill_len, len: tail_len as u32 });
                self.spill_len += tail_len;
            } else {
                self.out.write_all(&buf)?;
                self.out.write_all(&ZEROS[..(BLOCK_SIZE - tail_len) as usize])?;
                blocks += 1;
            }
        }
        let blocks = u32::try_from(blocks).map_err(|_| too_many_blocks())?;
        self.next_blk = self.next_blk.checked_add(blocks).ok_or_else(too_many_blocks)?;
        Ok(FileData { start_blk: if blocks == 0 { 0 } else { start }, blocks, tail })
    }

    /// Writes `bytes` padded to whole blocks and returns the first block.
    pub fn write_blocks(&mut self, bytes: &[u8]) -> Result<u32> {
        Ok(self.write_file(&mut &bytes[..], bytes.len() as u64, false)?.start_blk)
    }

    pub fn read_tail(&mut self, t: TailRef) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; t.len as usize];
        self.spill.seek(SeekFrom::Start(t.offset))?;
        self.spill.read_exact(&mut buf)?;
        Ok(buf)
    }
}

/// Source of file bytes that still live in other images (squash).
pub(crate) trait ExternalData {
    fn open(&mut self, layer: usize, nid: u64) -> Result<Box<dyn Read + '_>>;
}

pub(crate) struct NoExternal;

impl ExternalData for NoExternal {
    fn open(&mut self, _layer: usize, _nid: u64) -> Result<Box<dyn Read + '_>> {
        Err(Error::Corrupt("external data in a streamed layer".into()))
    }
}

pub(crate) struct EmitStats {
    pub inodes: u64,
    pub bytes: u64,
}

struct InodeRec {
    extended: bool,
    size: u64,
    nlink: u32,
    isize: usize,
    inline_len: usize,
}

fn file_type(k: &Kind) -> u8 {
    match k {
        Kind::Dir { .. } => FT_DIR,
        Kind::File { .. } => FT_REG_FILE,
        Kind::Symlink { .. } => FT_SYMLINK,
        Kind::CharDev { .. } => FT_CHRDEV,
        Kind::BlockDev { .. } => FT_BLKDEV,
        Kind::Fifo => FT_FIFO,
    }
}

fn type_bits(k: &Kind) -> u32 {
    match k {
        Kind::Dir { .. } => S_IFDIR,
        Kind::File { .. } => S_IFREG,
        Kind::Symlink { .. } => S_IFLNK,
        Kind::CharDev { .. } => S_IFCHR,
        Kind::BlockDev { .. } => S_IFBLK,
        Kind::Fifo => S_IFIFO,
    }
}

/// Lays out `tree` after the data already in `store` and writes the image.
pub(crate) fn emit<W: Write + Seek>(
    mut tree: Tree,
    base: Timestamp,
    mut store: DataStore<W>,
    ext: &mut dyn ExternalData,
) -> Result<(W, EmitStats)> {
    // 1. Number inodes breadth-first in name order; count links; record parents.
    let mut index_of: Vec<Option<usize>> = vec![None; tree.nodes.len()];
    let mut order: Vec<NodeId> = vec![tree.root];
    let mut links: Vec<u32> = vec![1];
    let mut parent: Vec<usize> = vec![0];
    index_of[tree.root] = Some(0);
    let mut i = 0;
    while i < order.len() {
        if let Some(children) = tree.children(order[i]) {
            for &child in children.values() {
                match index_of[child] {
                    Some(ci) => links[ci] += 1,
                    None => {
                        index_of[child] = Some(order.len());
                        order.push(child);
                        links.push(1);
                        parent.push(i);
                    }
                }
            }
        }
        i += 1;
    }
    let n = order.len();

    // 2. Copy external (squash) file data, in inode order.
    for &id in &order {
        if let Kind::File { size, data: Data::External { layer, nid } } = &tree.nodes[id].kind {
            let (size, layer, nid) = (*size, *layer, *nid);
            let inline = tail_fits_inline(size % BLOCK_SIZE, &tree.nodes[id].xattrs);
            let written = {
                let mut r = ext.open(layer, nid)?;
                store.write_file(&mut r, size, inline)?
            };
            if let Kind::File { data, .. } = &mut tree.nodes[id].kind {
                *data = Data::Written(written);
            }
        }
    }

    // 3. Xattr plan and the size of every inode record.
    let plan = {
        let refs: Vec<&Xattrs> = order.iter().map(|&id| &tree.nodes[id].xattrs).collect();
        plan_xattrs(&refs)?
    };
    let mut recs = Vec::with_capacity(n);
    for (ix, &id) in order.iter().enumerate() {
        let node = &tree.nodes[id];
        let ibody = plan.ibodies[ix].len();
        let (nlink, size, streamed_tail) = match &node.kind {
            Kind::Dir { children, .. } => {
                let subdirs = children.values().filter(|&&c| tree.nodes[c].is_dir()).count() as u32;
                let mut names: Vec<&[u8]> = vec![&b"."[..], &b".."[..]];
                names.extend(children.keys().map(Vec::as_slice));
                names.sort();
                (2 + subdirs, dir_size(&names), None)
            }
            Kind::Symlink { target } => (links[ix], target.len() as u64, None),
            Kind::File { size, data: Data::Written(fd) } => (links[ix], *size, Some(fd.tail.map_or(0, |t| t.len as usize))),
            Kind::File { .. } => unreachable!("external data was copied in step 2"),
            _ => (links[ix], 0, Some(0)),
        };
        let extended = !fits_compact(&node.meta, nlink, size, base);
        let isize = if extended { EXTENDED_INODE_LEN } else { COMPACT_INODE_LEN };
        let inline_len = streamed_tail.unwrap_or_else(|| {
            let t = (size % BLOCK_SIZE) as usize;
            if t > 0 && t + isize + ibody <= BLOCK_SIZE as usize { t } else { 0 }
        });
        recs.push(InodeRec { extended, size, nlink, isize, inline_len });
    }
    let lens: Vec<usize> = recs.iter().zip(&plan.ibodies).map(|(r, b)| r.isize + b.len() + r.inline_len).collect();
    let (nids, meta_len) = assign_nids(&lens);

    // 4. Directory and symlink bodies (they need nids); record file locations.
    let mut placed = vec![FileData { start_blk: 0, blocks: 0, tail: None }; n];
    for (ix, &id) in order.iter().enumerate() {
        let node = &tree.nodes[id];
        let body = match &node.kind {
            Kind::Dir { children, .. } => {
                let mut ents: Vec<(&[u8], u64, u8)> =
                    vec![(&b"."[..], nids[ix], FT_DIR), (&b".."[..], nids[parent[ix]], FT_DIR)];
                for (name, &c) in children {
                    let ci = index_of[c].expect("every child is numbered");
                    ents.push((name.as_slice(), nids[ci], file_type(&tree.nodes[c].kind)));
                }
                ents.sort_by(|a, b| a.0.cmp(b.0));
                encode_dir(&ents)
            }
            Kind::Symlink { target } => target.clone(),
            Kind::File { data: Data::Written(fd), .. } => {
                placed[ix] = *fd;
                continue;
            }
            _ => continue,
        };
        debug_assert_eq!(body.len() as u64, recs[ix].size);
        placed[ix] = store.write_file(&mut body.as_slice(), body.len() as u64, recs[ix].inline_len > 0)?;
    }

    // 5. Shared xattr table.
    let xattr_blkaddr = if plan.table.is_empty() { 0 } else { store.write_blocks(&plan.table)? };

    // 6. Inode records, in nid order.
    let meta_blkaddr = store.next_blk;
    let mut written = 0u64;
    for ix in 0..n {
        let node = &tree.nodes[order[ix]];
        let rec = &recs[ix];
        let off = nids[ix] * SLOT_SIZE;
        write_zeros(&mut store.out, off - written)?;
        let i_u = match node.kind {
            Kind::CharDev { major, minor } | Kind::BlockDev { major, minor } => encode_rdev(major, minor),
            Kind::Fifo => 0,
            _ => placed[ix].start_blk,
        };
        let (mtime, mtime_nsec) =
            if rec.extended { (node.meta.mtime.sec as u64, node.meta.mtime.nsec) } else { (0, 0) };
        let inode = DiskInode {
            extended: rec.extended,
            layout: if rec.inline_len > 0 { LAYOUT_FLAT_INLINE } else { LAYOUT_FLAT_PLAIN },
            xattr_icount: xattr_icount_for(plan.ibodies[ix].len()),
            mode: (type_bits(&node.kind) | (node.meta.mode & 0o7777)) as u16,
            nlink: rec.nlink,
            size: rec.size,
            mtime,
            mtime_nsec,
            i_u,
            ino: (ix + 1) as u32,
            uid: node.meta.uid,
            gid: node.meta.gid,
        };
        let mut bytes = inode.encode();
        bytes.extend_from_slice(&plan.ibodies[ix]);
        if rec.inline_len > 0 {
            let tail = placed[ix].tail.expect("an inline record has a spilled tail");
            bytes.extend(store.read_tail(tail)?);
        }
        store.out.write_all(&bytes)?;
        written = off + bytes.len() as u64;
    }
    write_zeros(&mut store.out, meta_len - written)?;

    // 7. Superblock, written last.
    let blocks = u32::try_from(u64::from(meta_blkaddr) + meta_len / BLOCK_SIZE).map_err(|_| too_many_blocks())?;
    let sb = SuperBlock {
        root_nid: u16::try_from(nids[0]).expect("the root is the first inode"),
        inos: n as u64,
        epoch: base.sec as u64,
        fixed_nsec: base.nsec,
        blocks,
        meta_blkaddr,
        xattr_blkaddr,
    };
    let mut out = store.out;
    out.seek(SeekFrom::Start(SUPER_OFFSET))?;
    out.write_all(&sb.encode())?;
    let bytes = u64::from(blocks) * BLOCK_SIZE;
    out.seek(SeekFrom::Start(bytes))?;
    out.flush()?;
    Ok((out, EmitStats { inodes: n as u64, bytes }))
}

/// Result of converting one layer.
#[derive(Debug, Clone)]
pub struct LayerSummary {
    pub inodes: u64,
    pub image_bytes: u64,
    /// Tar bytes consumed, up to and including the end-of-archive marker.
    pub tar_bytes: u64,
    /// Implicit directories, sorted, as reported before `finish`.
    pub implicit_dirs: Vec<Vec<u8>>,
    pub warnings: Vec<String>,
}

/// Converts one layer tar into one erofs image (spec §6.2).
pub struct LayerWriter<W: Write + Seek> {
    store: DataStore<W>,
    builder: LayerBuilder,
    limits: Limits,
    warnings: Vec<String>,
    tar_bytes: u64,
}

impl<W: Write + Seek> LayerWriter<W> {
    /// `spill_dir` holds a temporary file for inline tails; it should be on local disk.
    pub fn new(out: W, spill_dir: &Path, limits: Limits) -> Result<Self> {
        Ok(Self { store: DataStore::new(out, spill_dir)?, builder: LayerBuilder::new(), limits, warnings: Vec::new(), tar_bytes: 0 })
    }

    /// Reads one (decompressed) layer tar up to its end-of-archive marker and
    /// leaves any following bytes unread. Call once per writer.
    pub fn append_tar<R: Read>(&mut self, tar: R) -> Result<()> {
        let Self { store, builder, limits, warnings, tar_bytes } = self;
        let consumed = read_tar(tar, limits, warnings, &mut |entry: Entry, data: &mut dyn Read| -> Result<()> {
            let file = match entry.kind {
                EntryKind::File { size } => {
                    let inline = tail_fits_inline(size % BLOCK_SIZE, &entry.xattrs);
                    Some(Data::Written(store.write_file(data, size, inline)?))
                }
                _ => None,
            };
            builder.apply(entry, file)
        })?;
        *tar_bytes += consumed;
        Ok(())
    }

    /// Paths whose attributes the caller should resolve from lower layers.
    pub fn implicit_dirs(&self) -> Vec<Vec<u8>> {
        self.builder.implicit_dirs()
    }

    /// Writes metadata and the superblock. `inherited` maps implicit directory
    /// paths to attributes from the merged lower layers (`resolve_inherited`).
    pub fn finish(self, inherited: &BTreeMap<Vec<u8>, DirAttrs>) -> Result<(W, LayerSummary)> {
        let (tree, base, implicit_dirs) = self.builder.finalize(inherited);
        let (out, stats) = emit(tree, base, self.store, &mut NoExternal)?;
        Ok((
            out,
            LayerSummary { inodes: stats.inodes, image_bytes: stats.bytes, tar_bytes: self.tar_bytes, implicit_dirs, warnings: self.warnings },
        ))
    }
}
```

Update `lib.rs`: add `mod layout;` (if it isn't already there), `mod tarstream;` and `mod writer;`, plus:
```rust
pub use writer::{LayerSummary, LayerWriter};

/// Version of kiln's erofs profile. Bump whenever output bytes change.
pub const FORMAT_VERSION: u32 = 1;
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p kiln-erofs --test writer`
Expected: 7 tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/kiln-erofs
git commit -m "feat(erofs): streaming layer writer and image emission

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P7rdQAESQYNEo2qFobUNxt"
```

---

### Task 9: Reader and round-trip tests

**Files:**
- Create: `crates/kiln-erofs/src/reader.rs`, `crates/kiln-erofs/tests/common/mod.rs`, `crates/kiln-erofs/tests/roundtrip.rs`
- Modify: `crates/kiln-erofs/src/lib.rs`

**Interfaces:**
- Consumes: `ondisk` (Task 2), `path::components` (Task 3), and `Timestamp`, `XattrKey` and `Xattrs` (Task 5).
- Produces (`pub`, re-exported):
  - `Image<R: Read + Seek>`:
    - `open(R) -> Result<Self>`, `superblock()` and `root_nid() -> u64`.
    - `inode(nid) -> Result<InodeInfo>` and `xattrs(nid) -> Result<Xattrs>`.
    - `read_dir(nid) -> Result<Vec<DirEntry>>`, which omits `.` and `..`.
    - `lookup(&[u8]) -> Result<Option<u64>>`.
    - `data_reader(nid) -> Result<DataReader<'_, R>>`, `read_data(nid) -> Result<Vec<u8>>` and `readlink(nid)`.
  - `InodeInfo { nid, mode /* full st_mode */, uid, gid, nlink, size, mtime, rdev: (u32, u32), layout, startblk, isize, ibody_len }` with `is_dir()` and `is_whiteout()`.
  - `DirEntry { name, nid, file_type }`.
  - `DataReader` (implements `Read`).
- Produces in `tests/common/mod.rs`:
  - `Seen { kind: char, mode, uid, gid, mtime: (i64, u32), nlink, xattrs: BTreeMap<Vec<u8>, Vec<u8>>, data, rdev, nid, compact: bool, layout }`.
  - `walk(&[u8]) -> BTreeMap<Vec<u8>, Seen>`, `convert(&[u8]) -> (Vec<u8>, LayerSummary)` and `p(&str) -> Vec<u8>`.

**Reader rules:**
- **Inline data** follows the kernel: the first `(ceil(size/4096) − 1)` blocks come from `startblk`, and the remainder sits at `iloc + isize + ibody`. A remainder that crosses a block is `Corrupt`.
- **Directories:** a directory block whose `nameoff0` is 0 or not a multiple of 12 is `Corrupt`, as are unsorted entries.
- **Superblock:** anything outside the profile is `ProfileViolation`, through `SuperBlock::decode`.

- [ ] **Step 1: Write the test helpers and failing tests**

`crates/kiln-erofs/tests/common/mod.rs`:
```rust
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::Cursor;

use kiln_erofs::ondisk::{S_IFBLK, S_IFCHR, S_IFDIR, S_IFIFO, S_IFLNK, S_IFMT, S_IFREG};
use kiln_erofs::{Image, LayerSummary, LayerWriter, Limits};

/// Everything a test may assert about one path in an image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    pub kind: char,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime: (i64, u32),
    pub nlink: u32,
    pub xattrs: BTreeMap<Vec<u8>, Vec<u8>>,
    pub data: Vec<u8>,
    pub rdev: (u32, u32),
    pub nid: u64,
    pub compact: bool,
    pub layout: u16,
}

pub fn p(s: &str) -> Vec<u8> {
    s.as_bytes().to_vec()
}

/// Reads every path in `img` (root is `b""`).
pub fn walk(img: &[u8]) -> BTreeMap<Vec<u8>, Seen> {
    let mut image = Image::open(Cursor::new(img)).unwrap();
    let mut out = BTreeMap::new();
    let mut queue = vec![(Vec::new(), image.root_nid())];
    while let Some((path, nid)) = queue.pop() {
        let info = image.inode(nid).unwrap();
        let kind = match info.mode & S_IFMT {
            S_IFDIR => 'd',
            S_IFREG => 'f',
            S_IFLNK => 'l',
            S_IFCHR => 'c',
            S_IFBLK => 'b',
            S_IFIFO => 'p',
            _ => '?',
        };
        let data = if kind == 'f' || kind == 'l' { image.read_data(nid).unwrap() } else { Vec::new() };
        let xattrs = image.xattrs(nid).unwrap().into_iter().map(|(k, v)| (k.full_name(), v)).collect();
        if kind == 'd' {
            for e in image.read_dir(nid).unwrap() {
                let mut child = path.clone();
                if !child.is_empty() {
                    child.push(b'/');
                }
                child.extend_from_slice(&e.name);
                queue.push((child, e.nid));
            }
        }
        out.insert(
            path,
            Seen {
                kind,
                mode: info.mode & 0o7777,
                uid: info.uid,
                gid: info.gid,
                mtime: (info.mtime.sec, info.mtime.nsec),
                nlink: info.nlink,
                xattrs,
                data,
                rdev: info.rdev,
                nid,
                compact: info.isize == 32,
                layout: info.layout,
            },
        );
    }
    out
}

/// Converts one tar with no lower layers.
pub fn convert(tar: &[u8]) -> (Vec<u8>, LayerSummary) {
    let dir = tempfile::tempdir().unwrap();
    let mut w = LayerWriter::new(Cursor::new(Vec::new()), dir.path(), Limits::default()).unwrap();
    w.append_tar(tar).unwrap();
    let (out, summary) = w.finish(&BTreeMap::new()).unwrap();
    (out.into_inner(), summary)
}
```

`crates/kiln-erofs/tests/roundtrip.rs`:
```rust
mod common;

use std::io::Cursor;

use common::{convert, p, walk};
use kiln_erofs::ondisk::{LAYOUT_FLAT_INLINE, LAYOUT_FLAT_PLAIN};
use kiln_erofs::testtar::{Opts, TarBuilder};
use kiln_erofs::{Error, Image};

fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 31 % 251) as u8).collect()
}

#[test]
fn every_kind_round_trips() {
    let tar = TarBuilder::new()
        .dir("etc", &Opts::default().mode(0o755))
        .file("etc/small", b"hello", &Opts::default())
        .file("etc/exact", &pattern(4096), &Opts::default())
        .file("etc/big", &pattern(10_000), &Opts::default())
        .symlink("etc/link", "../usr/bin/php", &Opts::default())
        .hardlink("etc/hard", "etc/small")
        .chardev("dev/null", 1, 3, &Opts::default().mode(0o666))
        .fifo("run/fifo", &Opts::default().mode(0o600))
        .finish();
    let w = walk(&convert(&tar).0);
    assert_eq!(w[&p("etc/small")].data, b"hello");
    assert_eq!(w[&p("etc/small")].layout, LAYOUT_FLAT_INLINE);
    assert_eq!(w[&p("etc/exact")].data, pattern(4096));
    assert_eq!(w[&p("etc/exact")].layout, LAYOUT_FLAT_PLAIN);
    assert_eq!(w[&p("etc/big")].data, pattern(10_000));
    assert_eq!(w[&p("etc/big")].layout, LAYOUT_FLAT_INLINE);
    assert_eq!((w[&p("etc/link")].kind, w[&p("etc/link")].data.as_slice()), ('l', &b"../usr/bin/php"[..]));
    assert_eq!(w[&p("etc/hard")].nid, w[&p("etc/small")].nid);
    assert_eq!(w[&p("etc/small")].nlink, 2);
    assert_eq!((w[&p("dev/null")].kind, w[&p("dev/null")].rdev, w[&p("dev/null")].mode), ('c', (1, 3), 0o666));
    assert_eq!(w[&p("run/fifo")].kind, 'p');
    assert_eq!((w[&p("")].kind, w[&p("")].mode), ('d', 0o755));
    assert_eq!(w[&p("dev")].mode, 0o755);
    assert_eq!(w[&p("etc")].nlink, 2);
    assert_eq!(w[&p("")].nlink, 5, "root has etc, dev, run");
}

#[test]
fn large_tail_with_xattrs_falls_back_to_plain() {
    let tar = TarBuilder::new().file("f", &pattern(4090), &Opts::default().xattr("user.k", &[9u8; 100])).finish();
    let w = walk(&convert(&tar).0);
    assert_eq!(w[&p("f")].layout, LAYOUT_FLAT_PLAIN);
    assert_eq!(w[&p("f")].data, pattern(4090));
    assert_eq!(w[&p("f")].xattrs[&p("user.k")], vec![9u8; 100]);
}

#[test]
fn many_entries_span_directory_blocks() {
    let mut b = TarBuilder::new();
    for i in 0..600 {
        b.file(&format!("d/f{i:04}"), b"", &Opts::default());
    }
    let (img, _) = convert(&b.finish());
    let mut image = Image::open(Cursor::new(img.as_slice())).unwrap();
    let d = image.lookup(b"d").unwrap().unwrap();
    let names: Vec<Vec<u8>> = image.read_dir(d).unwrap().into_iter().map(|e| e.name).collect();
    assert_eq!(names.len(), 600);
    assert!(names.windows(2).all(|w| w[0] < w[1]));
    assert!(image.lookup(b"d/f0000").unwrap().is_some());
    assert!(image.lookup(b"d/f0599").unwrap().is_some());
    assert!(image.lookup(b"d/f0600").unwrap().is_none());
}

#[test]
fn symlink_targets_inline_and_plain() {
    let mid = "m".repeat(3000);
    let full = "x".repeat(4096);
    let tar = TarBuilder::new().symlink("a", &mid, &Opts::default()).symlink("b", &full, &Opts::default()).finish();
    let w = walk(&convert(&tar).0);
    assert_eq!((w[&p("a")].data.clone(), w[&p("a")].layout), (mid.into_bytes(), LAYOUT_FLAT_INLINE));
    assert_eq!((w[&p("b")].data.clone(), w[&p("b")].layout), (full.into_bytes(), LAYOUT_FLAT_PLAIN));
}

#[test]
fn xattrs_shared_and_inline() {
    let tar = TarBuilder::new()
        .file("a", b"", &Opts::default().xattr("user.common", b"1").xattr("security.capability", &[1, 0, 0, 2]))
        .file("b", b"", &Opts::default().xattr("user.common", b"1"))
        .file("c", b"", &Opts::default().xattr("user.solo", b"s").xattr("system.posix_acl_access", &[2, 0, 0, 0]))
        .finish();
    let (img, _) = convert(&tar);
    let w = walk(&img);
    assert_eq!(w[&p("a")].xattrs[&p("user.common")], b"1");
    assert_eq!(w[&p("a")].xattrs[&p("security.capability")], vec![1, 0, 0, 2]);
    assert_eq!(w[&p("b")].xattrs[&p("user.common")], b"1");
    assert_eq!(w[&p("c")].xattrs[&p("user.solo")], b"s");
    assert_eq!(w[&p("c")].xattrs[&p("system.posix_acl_access")], vec![2, 0, 0, 0]);
    let image = Image::open(Cursor::new(img.as_slice())).unwrap();
    assert_ne!(image.superblock().xattr_blkaddr, 0, "user.common is shared");
}

#[test]
fn extended_inodes_only_when_needed() {
    let tar = TarBuilder::new()
        .file("plain", b"", &Opts::default())
        .file("bigid", b"", &Opts::default().uid(70_000))
        .file("later", b"", &Opts::default().mtime(1_700_000_001))
        .file("nsec", b"", &Opts::default().pax("mtime", b"1700000000.5"))
        .finish();
    let w = walk(&convert(&tar).0);
    assert!(w[&p("plain")].compact);
    assert!(!w[&p("bigid")].compact);
    assert_eq!(w[&p("bigid")].uid, 70_000);
    assert!(!w[&p("later")].compact);
    assert_eq!(w[&p("later")].mtime, (1_700_000_001, 0));
    assert!(!w[&p("nsec")].compact);
    assert_eq!(w[&p("nsec")].mtime, (1_700_000_000, 500_000_000));
    assert_eq!(w[&p("plain")].mtime, (1_700_000_000, 0));
}

#[test]
fn explicit_root_entry_sets_root_attrs() {
    let tar = TarBuilder::new().dir("./", &Opts::default().mode(0o700)).finish();
    assert_eq!(walk(&convert(&tar).0)[&p("")].mode, 0o700);
}

#[test]
fn whiteouts_and_opaque_are_stored_as_overlay_markers() {
    let tar = TarBuilder::new().whiteout("etc/gone").dir("var", &Opts::default()).opaque("var").finish();
    let w = walk(&convert(&tar).0);
    assert_eq!((w[&p("etc/gone")].kind, w[&p("etc/gone")].rdev), ('c', (0, 0)));
    assert_eq!(w[&p("var")].xattrs[&p("trusted.overlay.opaque")], b"y");
}

#[test]
fn non_utf8_names_round_trip() {
    let tar = TarBuilder::new().entry(b"caf\xe9", b'0', b"latin1", b"", (0, 0), &Opts::default()).finish();
    let w = walk(&convert(&tar).0);
    assert_eq!(w[&b"caf\xe9".to_vec()].data, b"latin1");
}

#[test]
fn names_sorting_before_dot() {
    let mut b = TarBuilder::new();
    for name in ["a", "-", "+", "#a", "!"] {
        b.file(name, name.as_bytes(), &Opts::default());
    }
    let (img, _) = convert(&b.finish());
    let mut image = Image::open(Cursor::new(img.as_slice())).unwrap();
    let root = image.root_nid();
    let names: Vec<Vec<u8>> = image.read_dir(root).unwrap().into_iter().map(|e| e.name).collect();
    assert_eq!(names, vec![p("!"), p("#a"), p("+"), p("-"), p("a")]);
    for name in ["!", "#a", "+", "-", "a"] {
        let nid = image.lookup(name.as_bytes()).unwrap().unwrap();
        assert_eq!(image.read_data(nid).unwrap(), name.as_bytes());
    }
}

#[test]
fn negative_mtime_round_trips() {
    let tar = TarBuilder::new()
        .file("old", b"", &Opts::default().pax("mtime", b"-1.5"))
        .file("epoch", b"", &Opts::default().mtime(0))
        .finish();
    let w = walk(&convert(&tar).0);
    assert_eq!(w[&p("old")].mtime, (-2, 500_000_000));
    assert!(w[&p("old")].compact, "the minimum mtime is the base time");
    assert_eq!(w[&p("epoch")].mtime, (0, 0));
    assert!(!w[&p("epoch")].compact);
}

#[test]
fn open_rejects_truncated_and_foreign_images() {
    let (img, _) = convert(&TarBuilder::new().file("f", b"x", &Opts::default()).finish());
    assert!(matches!(Image::open(Cursor::new(&img[..4096])), Err(Error::Corrupt(_))));
    let mut bad = img.clone();
    bad[1024] ^= 0xff;
    assert!(matches!(Image::open(Cursor::new(bad.as_slice())), Err(Error::ProfileViolation(_))));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p kiln-erofs --test roundtrip`
Expected: compile error `unresolved import kiln_erofs::Image`.

- [ ] **Step 3: Implement `reader.rs`**

```rust
//! Reading kiln-profile erofs images (spec §7.5).

use std::io::{self, Read, Seek, SeekFrom};

use crate::error::{Error, Result};
use crate::ondisk::{
    decode_dirent, decode_rdev, xattr_entry_len, xattr_ibody_len, DiskInode, SuperBlock, BLOCK_SIZE,
    COMPACT_INODE_LEN, DIRENT_LEN, EXTENDED_INODE_LEN, LAYOUT_FLAT_PLAIN, NAME_LEN_MAX, S_IFBLK, S_IFCHR,
    S_IFDIR, S_IFMT, SLOT_SIZE, SUPER_LEN, SUPER_OFFSET, XATTR_ENTRY_HEADER_LEN, XATTR_IBODY_HEADER_LEN,
};
use crate::path::components;
use crate::tree::{Timestamp, XattrKey, Xattrs};

/// A kiln-profile erofs image.
pub struct Image<R> {
    r: R,
    sb: SuperBlock,
}

/// Decoded inode attributes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InodeInfo {
    pub nid: u64,
    /// Full `st_mode` (type and permission bits).
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub nlink: u32,
    pub size: u64,
    pub mtime: Timestamp,
    /// `(major, minor)` for device inodes, else `(0, 0)`.
    pub rdev: (u32, u32),
    pub layout: u16,
    pub startblk: u32,
    /// On-disk inode size: 32 (compact) or 64 (extended).
    pub isize: usize,
    pub ibody_len: usize,
}

impl InodeInfo {
    pub fn is_dir(&self) -> bool {
        self.mode & S_IFMT == S_IFDIR
    }

    /// An overlayfs whiteout: a character device 0:0.
    pub fn is_whiteout(&self) -> bool {
        self.mode & S_IFMT == S_IFCHR && self.rdev == (0, 0)
    }
}

/// One directory entry other than `.` and `..`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: Vec<u8>,
    pub nid: u64,
    pub file_type: u8,
}

fn corrupt(what: String) -> Error {
    Error::Corrupt(what)
}

impl<R: Read + Seek> Image<R> {
    pub fn open(mut r: R) -> Result<Self> {
        let mut b = [0u8; SUPER_LEN];
        r.seek(SeekFrom::Start(SUPER_OFFSET))?;
        r.read_exact(&mut b).map_err(|_| corrupt("image shorter than a superblock".into()))?;
        let sb = SuperBlock::decode(&b)?;
        let len = r.seek(SeekFrom::End(0))?;
        if len < u64::from(sb.blocks) * BLOCK_SIZE {
            return Err(corrupt(format!("image is {len} bytes but declares {} blocks", sb.blocks)));
        }
        Ok(Self { r, sb })
    }

    pub fn superblock(&self) -> &SuperBlock {
        &self.sb
    }

    pub fn root_nid(&self) -> u64 {
        u64::from(self.sb.root_nid)
    }

    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<()> {
        self.r.seek(SeekFrom::Start(off))?;
        self.r.read_exact(buf).map_err(|_| corrupt(format!("read past the end of the image at offset {off}")))
    }

    fn iloc(&self, nid: u64) -> u64 {
        u64::from(self.sb.meta_blkaddr) * BLOCK_SIZE + nid * SLOT_SIZE
    }

    pub fn inode(&mut self, nid: u64) -> Result<InodeInfo> {
        let mut b = [0u8; EXTENDED_INODE_LEN];
        let at = self.iloc(nid);
        self.read_at(at, &mut b[..COMPACT_INODE_LEN])?;
        if b[0] & 1 == 1 {
            self.read_at(at + COMPACT_INODE_LEN as u64, &mut b[COMPACT_INODE_LEN..])?;
        }
        let di = DiskInode::decode(&b)?;
        let mtime = if di.extended {
            Timestamp { sec: di.mtime as i64, nsec: di.mtime_nsec }
        } else {
            Timestamp { sec: (self.sb.epoch as i64).wrapping_add(di.mtime as i64), nsec: self.sb.fixed_nsec }
        };
        let mode = u32::from(di.mode);
        let ft = mode & S_IFMT;
        let rdev = if ft == S_IFCHR || ft == S_IFBLK { decode_rdev(di.i_u) } else { (0, 0) };
        Ok(InodeInfo {
            nid,
            mode,
            uid: di.uid,
            gid: di.gid,
            nlink: di.nlink,
            size: di.size,
            mtime,
            rdev,
            layout: di.layout,
            startblk: di.i_u,
            isize: di.encoded_len(),
            ibody_len: xattr_ibody_len(di.xattr_icount),
        })
    }

    pub fn xattrs(&mut self, nid: u64) -> Result<Xattrs> {
        let info = self.inode(nid)?;
        let mut out = Xattrs::new();
        if info.ibody_len == 0 {
            return Ok(out);
        }
        let mut body = vec![0u8; info.ibody_len];
        self.read_at(self.iloc(nid) + info.isize as u64, &mut body)?;
        let shared = usize::from(body[4]);
        let mut pos = XATTR_IBODY_HEADER_LEN;
        if pos + 4 * shared > body.len() {
            return Err(corrupt(format!("xattr body of nid {nid} too short for {shared} shared ids")));
        }
        for _ in 0..shared {
            let id = u32::from_le_bytes(body[pos..pos + 4].try_into().expect("4 bytes"));
            pos += 4;
            let (k, v) = self.shared_xattr(id)?;
            out.insert(k, v);
        }
        while pos + XATTR_ENTRY_HEADER_LEN <= body.len() {
            let (k, v, len) = parse_xattr_entry(&body[pos..])?;
            out.insert(k, v);
            pos += len;
        }
        Ok(out)
    }

    fn shared_xattr(&mut self, id: u32) -> Result<(XattrKey, Vec<u8>)> {
        let off = u64::from(self.sb.xattr_blkaddr) * BLOCK_SIZE + u64::from(id) * 4;
        let mut h = [0u8; XATTR_ENTRY_HEADER_LEN];
        self.read_at(off, &mut h)?;
        let name_len = usize::from(h[0]);
        let value_len = usize::from(u16::from_le_bytes([h[2], h[3]]));
        let mut rest = vec![0u8; name_len + value_len];
        self.read_at(off + XATTR_ENTRY_HEADER_LEN as u64, &mut rest)?;
        Ok((XattrKey { index: h[1], name: rest[..name_len].to_vec() }, rest[name_len..].to_vec()))
    }

    fn segments(&self, info: &InodeInfo) -> Result<Vec<(u64, u64)>> {
        if info.size == 0 {
            return Ok(Vec::new());
        }
        let start = u64::from(info.startblk) * BLOCK_SIZE;
        if info.layout == LAYOUT_FLAT_PLAIN {
            return Ok(vec![(start, info.size)]);
        }
        let full = (info.size.div_ceil(BLOCK_SIZE) - 1) * BLOCK_SIZE;
        let tail_off = self.iloc(info.nid) + (info.isize + info.ibody_len) as u64;
        let tail_len = info.size - full;
        if tail_off % BLOCK_SIZE + tail_len > BLOCK_SIZE {
            return Err(corrupt(format!("inline data of nid {} crosses a block boundary", info.nid)));
        }
        let mut segs = Vec::new();
        if full > 0 {
            segs.push((start, full));
        }
        segs.push((tail_off, tail_len));
        Ok(segs)
    }

    pub fn data_reader(&mut self, nid: u64) -> Result<DataReader<'_, R>> {
        let info = self.inode(nid)?;
        let segs = self.segments(&info)?;
        Ok(DataReader { img: self, segs, seg: 0, pos: 0 })
    }

    pub fn read_data(&mut self, nid: u64) -> Result<Vec<u8>> {
        let mut v = Vec::new();
        self.data_reader(nid)?
            .read_to_end(&mut v)
            .map_err(|e| corrupt(format!("reading data of nid {nid}: {e}")))?;
        Ok(v)
    }

    pub fn readlink(&mut self, nid: u64) -> Result<Vec<u8>> {
        self.read_data(nid)
    }

    pub fn read_dir(&mut self, nid: u64) -> Result<Vec<DirEntry>> {
        if !self.inode(nid)?.is_dir() {
            return Err(corrupt(format!("nid {nid} is not a directory")));
        }
        let data = self.read_data(nid)?;
        let bad = || corrupt(format!("bad directory block in nid {nid}"));
        let mut out = Vec::new();
        for block in data.chunks(BLOCK_SIZE as usize) {
            if block.len() < DIRENT_LEN {
                return Err(bad());
            }
            let nameoff0 = usize::from(decode_dirent(block).1);
            if nameoff0 == 0 || nameoff0 % DIRENT_LEN != 0 || nameoff0 > block.len() {
                return Err(bad());
            }
            let count = nameoff0 / DIRENT_LEN;
            for i in 0..count {
                let (child, nameoff, ft) = decode_dirent(&block[i * DIRENT_LEN..]);
                let start = usize::from(nameoff);
                let end = if i + 1 < count {
                    usize::from(decode_dirent(&block[(i + 1) * DIRENT_LEN..]).1)
                } else {
                    let tail = block.get(start..).ok_or_else(bad)?;
                    start + tail.iter().position(|&b| b == 0).unwrap_or(tail.len())
                };
                if start >= end || end > block.len() || end - start > NAME_LEN_MAX {
                    return Err(bad());
                }
                let name = block[start..end].to_vec();
                if name != b"." && name != b".." {
                    out.push(DirEntry { name, nid: child, file_type: ft });
                }
            }
        }
        if !out.windows(2).all(|w| w[0].name < w[1].name) {
            return Err(corrupt(format!("directory nid {nid} is not strictly sorted")));
        }
        Ok(out)
    }

    /// Resolves a normalized path (`b""` is the root).
    pub fn lookup(&mut self, path: &[u8]) -> Result<Option<u64>> {
        let mut cur = self.root_nid();
        for c in components(path) {
            if !self.inode(cur)?.is_dir() {
                return Ok(None);
            }
            match self.read_dir(cur)?.into_iter().find(|e| e.name == c) {
                Some(e) => cur = e.nid,
                None => return Ok(None),
            }
        }
        Ok(Some(cur))
    }
}

fn parse_xattr_entry(b: &[u8]) -> Result<(XattrKey, Vec<u8>, usize)> {
    let name_len = usize::from(b[0]);
    let value_len = usize::from(u16::from_le_bytes([b[2], b[3]]));
    let end = XATTR_ENTRY_HEADER_LEN + name_len + value_len;
    if end > b.len() {
        return Err(corrupt("inline xattr entry overruns the xattr body".into()));
    }
    let name = b[XATTR_ENTRY_HEADER_LEN..XATTR_ENTRY_HEADER_LEN + name_len].to_vec();
    let value = b[XATTR_ENTRY_HEADER_LEN + name_len..end].to_vec();
    Ok((XattrKey { index: b[1], name }, value, xattr_entry_len(name_len, value_len)))
}

/// Sequential reader over one inode's data.
pub struct DataReader<'a, R> {
    img: &'a mut Image<R>,
    segs: Vec<(u64, u64)>,
    seg: usize,
    pos: u64,
}

impl<R: Read + Seek> Read for DataReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while let Some(&(off, len)) = self.segs.get(self.seg) {
            if self.pos >= len {
                self.seg += 1;
                self.pos = 0;
                continue;
            }
            let want = (len - self.pos).min(buf.len() as u64) as usize;
            self.img.r.seek(SeekFrom::Start(off + self.pos))?;
            self.img.r.read_exact(&mut buf[..want])?;
            self.pos += want as u64;
            return Ok(want);
        }
        Ok(0)
    }
}
```

Update `lib.rs`: add `mod reader;` and `pub use reader::{DataReader, DirEntry, Image, InodeInfo};`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p kiln-erofs --test roundtrip`
Expected: 12 tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/kiln-erofs
git commit -m "feat(erofs): reader with directory, xattr and inline-data support

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P7rdQAESQYNEo2qFobUNxt"
```

---

### Task 10: Parent inheritance and squash

**Files:**
- Create: `crates/kiln-erofs/src/merge.rs`, `crates/kiln-erofs/tests/stack.rs`
- Modify: `crates/kiln-erofs/src/lib.rs` (wire up the module and remove `#![allow(dead_code)]`), `crates/kiln-erofs/tests/common/mod.rs`

**Interfaces:**
- Consumes: `Image` and `InodeInfo` (Task 9); `DataStore`, `emit`, `ExternalData` and `LayerSummary` (Task 8); `Tree`, `Node`, `Kind`, `Data` and `XattrKey` (Task 5).
- Produces (`pub`):
  - `resolve_inherited<R: Read + Seek>(lowers: &mut [Image<R>], paths: &[Vec<u8>]) -> Result<BTreeMap<Vec<u8>, DirAttrs>>`.
    - `lowers` are bottom first.
    - A path is omitted when it is absent from the merged view or is not a directory there.
    - `trusted.overlay.*` keys are stripped from inherited xattrs.
  - `squash<R: Read + Seek, W: Write + Seek>(layers: &mut [Image<R>], out: W, spill_dir: &Path) -> Result<(W, LayerSummary)>` merges with overlayfs semantics into one bottom layer that has no whiteouts and no opaque markers.
- Produces in `tests/common/mod.rs`: `try_convert_stack(&[Vec<u8>]) -> kiln_erofs::Result<Vec<Vec<u8>>>`, `convert_stack(&[Vec<u8>]) -> Vec<Vec<u8>>` and `squash_all(&[Vec<u8>]) -> Vec<u8>`.

**Merged-view lookup** (overlayfs semantics), scanning layers from the top down:
- In each layer, walk the proper prefixes of the path, starting at the root.
- If a prefix is a whiteout or a non-directory, the path is hidden, so return `None`.
- If a prefix directory is opaque, the layers below it are hidden for this path. If the path is not found in this layer, return `None`.
- If the path itself exists in this layer:
  - as a whiteout, return `None`;
  - otherwise, this layer provides it.

**Squash:**
- Fold `overlay(merged, layer)` from the bottom up, starting from an empty tree. For each upper directory, breadth-first:
  - If the upper directory is opaque, clear the merged children.
  - A whiteout removes its name.
  - A directory over a directory takes the upper attributes (overlay shows the upper directory's attributes) and recurses.
  - Anything else replaces the name. Each upper node maps to one merged node, which keeps hardlinks within that layer.
- **Base time** is the minimum reachable mtime.

- [ ] **Step 1: Extend the test helpers**

Append to `tests/common/mod.rs`:
```rust
use kiln_erofs::{resolve_inherited, squash};

/// Converts layers bottom-up, resolving implicit parents against the layers below.
pub fn try_convert_stack(tars: &[Vec<u8>]) -> kiln_erofs::Result<Vec<Vec<u8>>> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    for tar in tars {
        let dir = tempfile::tempdir()?;
        let mut w = LayerWriter::new(Cursor::new(Vec::new()), dir.path(), Limits::default())?;
        w.append_tar(tar.as_slice())?;
        let implicit = w.implicit_dirs();
        let inherited = {
            let mut lowers = out.iter().map(|b| Image::open(Cursor::new(b.as_slice()))).collect::<kiln_erofs::Result<Vec<_>>>()?;
            resolve_inherited(&mut lowers, &implicit)?
        };
        let (cur, _) = w.finish(&inherited)?;
        out.push(cur.into_inner());
    }
    Ok(out)
}

pub fn convert_stack(tars: &[Vec<u8>]) -> Vec<Vec<u8>> {
    try_convert_stack(tars).unwrap()
}

pub fn squash_all(layers: &[Vec<u8>]) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let mut imgs: Vec<_> = layers.iter().map(|b| Image::open(Cursor::new(b.as_slice())).unwrap()).collect();
    let (out, _) = squash(&mut imgs, Cursor::new(Vec::new()), dir.path()).unwrap();
    out.into_inner()
}
```

- [ ] **Step 2: Write the failing tests**

`crates/kiln-erofs/tests/stack.rs`:
```rust
mod common;

use common::{convert_stack, p, squash_all, walk};
use kiln_erofs::testtar::{Opts, TarBuilder};

fn t(b: &mut TarBuilder) -> Vec<u8> {
    b.finish()
}

#[test]
fn implicit_dir_inherits_from_lower() {
    let l0 = t(TarBuilder::new().dir("tmp", &Opts::default().mode(0o1777).mtime(1000).xattr("user.k", b"v")));
    let l1 = t(TarBuilder::new().file("tmp/cache/x", b"", &Opts::default().mtime(2000)));
    let layers = convert_stack(&[l0, l1]);
    let w = walk(&layers[1]);
    assert_eq!(w[&p("tmp")].mode, 0o1777);
    assert_eq!(w[&p("tmp")].mtime, (1000, 0));
    assert_eq!(w[&p("tmp")].xattrs[&p("user.k")], b"v");
    assert_eq!((w[&p("tmp/cache")].mode, w[&p("tmp/cache")].mtime), (0o755, (2000, 0)));
}

#[test]
fn inheritance_respects_whiteouts_and_opaque() {
    let l0 = t(TarBuilder::new().dir("a", &Opts::default().mode(0o700)).dir("o", &Opts::default()).dir("o/p", &Opts::default().mode(0o711)));
    let l1 = t(TarBuilder::new().whiteout("a").dir("o", &Opts::default()).opaque("o"));
    let l2 = t(TarBuilder::new().file("a/f", b"", &Opts::default()).file("o/p/f", b"", &Opts::default()));
    let layers = convert_stack(&[l0, l1, l2]);
    let w = walk(&layers[2]);
    assert_eq!(w[&p("a")].mode, 0o755, "a is whited out below");
    assert_eq!(w[&p("o/p")].mode, 0o755, "o/p is hidden by the opaque o");
}

#[test]
fn inheritance_through_lower_non_dir_uses_defaults() {
    let l0 = t(TarBuilder::new().file("x", b"", &Opts::default().mode(0o600)));
    let l1 = t(TarBuilder::new().file("x/y", b"", &Opts::default()));
    let layers = convert_stack(&[l0, l1]);
    assert_eq!(walk(&layers[1])[&p("x")].mode, 0o755);
}

#[test]
fn inherited_opaque_is_not_copied() {
    let l0 = t(TarBuilder::new().dir("d", &Opts::default().mode(0o750)).opaque("d"));
    let l1 = t(TarBuilder::new().file("d/f", b"", &Opts::default()));
    let layers = convert_stack(&[l0, l1]);
    let w = walk(&layers[1]);
    assert_eq!(w[&p("d")].mode, 0o750);
    assert!(!w[&p("d")].xattrs.contains_key(&p("trusted.overlay.opaque")));
}

#[test]
fn squash_applies_whiteouts_and_drops_markers() {
    let l0 = t(TarBuilder::new()
        .dir("etc", &Opts::default())
        .file("etc/a", b"a", &Opts::default())
        .file("etc/b", b"b", &Opts::default())
        .dir("var", &Opts::default())
        .file("var/x", b"x", &Opts::default()));
    let l1 = t(TarBuilder::new().whiteout("etc/a").dir("var", &Opts::default()).opaque("var").file("var/y", b"y", &Opts::default()));
    let w = walk(&squash_all(&convert_stack(&[l0, l1])));
    assert!(w.contains_key(&p("etc/b")));
    assert!(!w.contains_key(&p("etc/a")));
    assert!(!w.contains_key(&p("var/x")));
    assert_eq!(w[&p("var/y")].data, b"y");
    assert!(w.values().all(|s| !(s.kind == 'c' && s.rdev == (0, 0))), "no whiteouts survive");
    assert!(w.values().all(|s| !s.xattrs.contains_key(&p("trusted.overlay.opaque"))));
}

#[test]
fn squash_preserves_hardlinks_and_splits_replaced_ones() {
    let l0 = t(TarBuilder::new()
        .file("a", b"old", &Opts::default())
        .hardlink("b", "a")
        .file("c", b"c", &Opts::default())
        .hardlink("d", "c"));
    let l1 = t(TarBuilder::new().file("a", b"new", &Opts::default()));
    let w = walk(&squash_all(&convert_stack(&[l0, l1])));
    assert_eq!((w[&p("a")].data.as_slice(), w[&p("a")].nlink), (&b"new"[..], 1));
    assert_eq!((w[&p("b")].data.as_slice(), w[&p("b")].nlink), (&b"old"[..], 1));
    assert_eq!(w[&p("c")].nid, w[&p("d")].nid);
    assert_eq!(w[&p("c")].nlink, 2);
}

#[test]
fn squash_uses_top_directory_attrs() {
    let l0 = t(TarBuilder::new().dir("d", &Opts::default().mode(0o700).xattr("user.k", b"v")));
    let l1 = t(TarBuilder::new().dir("d", &Opts::default().mode(0o755)));
    let w = walk(&squash_all(&convert_stack(&[l0, l1])));
    assert_eq!(w[&p("d")].mode, 0o755);
    assert!(w[&p("d")].xattrs.is_empty());
}

#[test]
fn squash_copies_file_data_and_is_deterministic() {
    let big: Vec<u8> = (0..20_000).map(|i| (i % 253) as u8).collect();
    let l0 = t(TarBuilder::new().file("big", &big, &Opts::default()).symlink("s", "big", &Opts::default()));
    let l1 = t(TarBuilder::new().file("small", b"s", &Opts::default()));
    let layers = convert_stack(&[l0, l1]);
    let a = squash_all(&layers);
    let w = walk(&a);
    assert_eq!(w[&p("big")].data, big);
    assert_eq!(w[&p("s")].data, b"big");
    assert_eq!(a, squash_all(&layers));
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p kiln-erofs --test stack`
Expected: compile error `unresolved imports kiln_erofs::resolve_inherited, kiln_erofs::squash`.

- [ ] **Step 4: Implement `merge.rs`**

```rust
//! Merged (overlayfs) views over kiln layers: parent inheritance (§6.3) and squash (§6.4).

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{Read, Seek, Write};
use std::path::Path;

use crate::error::{Error, Result};
use crate::ondisk::{S_IFBLK, S_IFCHR, S_IFDIR, S_IFIFO, S_IFLNK, S_IFMT, S_IFREG};
use crate::path::components;
use crate::reader::{Image, InodeInfo};
use crate::tree::{Data, DirAttrs, Kind, Meta, Node, NodeId, Timestamp, Tree, XattrKey};
use crate::writer::{emit, DataStore, ExternalData, LayerSummary};

fn meta_of(info: &InodeInfo) -> Meta {
    Meta { mode: info.mode & 0o7777, uid: info.uid, gid: info.gid, mtime: info.mtime }
}

/// Attributes of `paths` in the merged view of `lowers` (bottom first). Paths that
/// are absent or not directories there are omitted; overlay xattrs are stripped.
pub fn resolve_inherited<R: Read + Seek>(lowers: &mut [Image<R>], paths: &[Vec<u8>]) -> Result<BTreeMap<Vec<u8>, DirAttrs>> {
    let mut out = BTreeMap::new();
    for path in paths {
        let Some((layer, nid)) = merged_lookup(lowers, path)? else { continue };
        let img = &mut lowers[layer];
        let info = img.inode(nid)?;
        if !info.is_dir() {
            continue;
        }
        let mut xattrs = img.xattrs(nid)?;
        xattrs.retain(|k, _| !k.is_overlay());
        out.insert(path.clone(), DirAttrs { meta: meta_of(&info), xattrs });
    }
    Ok(out)
}

/// The topmost `(layer, nid)` providing `path` in the overlay of `layers`.
fn merged_lookup<R: Read + Seek>(layers: &mut [Image<R>], path: &[u8]) -> Result<Option<(usize, u64)>> {
    let comps: Vec<&[u8]> = components(path).collect();
    for layer in (0..layers.len()).rev() {
        let img = &mut layers[layer];
        let mut cur = img.root_nid();
        if comps.is_empty() {
            return Ok(Some((layer, cur)));
        }
        let mut opaque_above = img.xattrs(cur)?.contains_key(&XattrKey::opaque());
        for (i, c) in comps.iter().enumerate() {
            let Some(entry) = img.read_dir(cur)?.into_iter().find(|e| e.name == *c) else {
                break;
            };
            let info = img.inode(entry.nid)?;
            if info.is_whiteout() {
                return Ok(None);
            }
            if i + 1 == comps.len() {
                return Ok(Some((layer, entry.nid)));
            }
            if !info.is_dir() {
                return Ok(None);
            }
            if img.xattrs(entry.nid)?.contains_key(&XattrKey::opaque()) {
                opaque_above = true;
            }
            cur = entry.nid;
        }
        if opaque_above {
            return Ok(None);
        }
    }
    Ok(None)
}

/// Reads an image's tree; file data stays in the image (`Data::External`).
fn tree_from_image<R: Read + Seek>(img: &mut Image<R>, layer: usize) -> Result<Tree> {
    let root_nid = img.root_nid();
    let root_info = img.inode(root_nid)?;
    let mut tree = Tree::new(Node { kind: Kind::Dir { children: BTreeMap::new(), implicit: false }, meta: meta_of(&root_info), xattrs: img.xattrs(root_nid)? });
    let mut by_nid: HashMap<u64, NodeId> = HashMap::new();
    by_nid.insert(root_nid, tree.root);
    let mut queue = VecDeque::from([(root_nid, tree.root)]);
    while let Some((dir_nid, dir_id)) = queue.pop_front() {
        for e in img.read_dir(dir_nid)? {
            let id = match by_nid.get(&e.nid) {
                Some(&id) if tree.nodes[id].is_dir() => {
                    return Err(Error::Corrupt(format!("directory nid {} is linked twice", e.nid)));
                }
                Some(&id) => id,
                None => {
                    let info = img.inode(e.nid)?;
                    let kind = match info.mode & S_IFMT {
                        S_IFDIR => Kind::Dir { children: BTreeMap::new(), implicit: false },
                        S_IFREG => Kind::File { size: info.size, data: Data::External { layer, nid: e.nid } },
                        S_IFLNK => Kind::Symlink { target: img.readlink(e.nid)? },
                        S_IFCHR => Kind::CharDev { major: info.rdev.0, minor: info.rdev.1 },
                        S_IFBLK => Kind::BlockDev { major: info.rdev.0, minor: info.rdev.1 },
                        S_IFIFO => Kind::Fifo,
                        other => return Err(Error::Corrupt(format!("unsupported inode type {other:#o}"))),
                    };
                    let node = Node { kind, meta: meta_of(&info), xattrs: img.xattrs(e.nid)? };
                    let is_dir = node.is_dir();
                    let id = tree.add(node);
                    by_nid.insert(e.nid, id);
                    if is_dir {
                        queue.push_back((e.nid, id));
                    }
                    id
                }
            };
            tree.children_mut(dir_id).insert(e.name, id);
        }
    }
    Ok(tree)
}

fn set_dir_attrs(merged: &mut Tree, id: NodeId, src: &Node) {
    let node = &mut merged.nodes[id];
    node.meta = src.meta.clone();
    node.xattrs = src.xattrs.iter().filter(|(k, _)| !k.is_overlay()).map(|(k, v)| (k.clone(), v.clone())).collect();
}

/// Applies `upper` on top of `merged` with overlayfs semantics.
fn overlay(merged: &mut Tree, upper: &Tree) {
    let merged_root = merged.root;
    set_dir_attrs(merged, merged_root, &upper.nodes[upper.root]);
    let mut mapped: HashMap<NodeId, NodeId> = HashMap::new();
    let mut queue = VecDeque::from([(upper.root, merged_root)]);
    while let Some((upper_dir, merged_dir)) = queue.pop_front() {
        if upper.nodes[upper_dir].xattrs.contains_key(&XattrKey::opaque()) {
            merged.children_mut(merged_dir).clear();
        }
        for (name, &uc) in upper.children(upper_dir).expect("queued nodes are directories") {
            let un = &upper.nodes[uc];
            if un.is_whiteout() {
                merged.children_mut(merged_dir).remove(name);
                continue;
            }
            if un.is_dir() {
                let target = match merged.child(merged_dir, name) {
                    Some(m) if merged.nodes[m].is_dir() => m,
                    _ => {
                        let m = merged.add(Node::dir(un.meta.clone(), false));
                        merged.children_mut(merged_dir).insert(name.clone(), m);
                        m
                    }
                };
                set_dir_attrs(merged, target, un);
                queue.push_back((uc, target));
            } else {
                let m = *mapped.entry(uc).or_insert_with(|| merged.add(un.clone()));
                merged.children_mut(merged_dir).insert(name.clone(), m);
            }
        }
    }
}

struct Sources<'a, R>(&'a mut [Image<R>]);

impl<R: Read + Seek> ExternalData for Sources<'_, R> {
    fn open(&mut self, layer: usize, nid: u64) -> Result<Box<dyn Read + '_>> {
        Ok(Box::new(self.0[layer].data_reader(nid)?))
    }
}

/// Merges `layers` (bottom first) into one bottom layer with no overlay markers.
pub fn squash<R: Read + Seek, W: Write + Seek>(layers: &mut [Image<R>], out: W, spill_dir: &Path) -> Result<(W, LayerSummary)> {
    let mut merged = Tree::new(Node::dir(Meta::default_dir(Timestamp::default()), false));
    for (layer, img) in layers.iter_mut().enumerate() {
        let upper = tree_from_image(img, layer)?;
        overlay(&mut merged, &upper);
    }
    let base = merged.min_mtime();
    let store = DataStore::new(out, spill_dir)?;
    let (out, stats) = emit(merged, base, store, &mut Sources(layers))?;
    Ok((out, LayerSummary { inodes: stats.inodes, image_bytes: stats.bytes, tar_bytes: 0, implicit_dirs: Vec::new(), warnings: Vec::new() }))
}
```

Then make `lib.rs` final: remove the `#![allow(dead_code)]` line and its comment, and use this content:
```rust
//! Deterministic erofs writer and reader implementing kiln's erofs profile
//! (spec §7). One OCI layer tar becomes one erofs image.
#![forbid(unsafe_code)]

mod apply;
mod error;
mod layout;
mod limits;
mod merge;
pub mod ondisk;
mod path;
mod pax;
mod reader;
mod tarstream;
#[doc(hidden)]
pub mod testtar;
mod tree;
mod writer;

pub use error::{Error, Result};
pub use limits::Limits;
pub use merge::{resolve_inherited, squash};
pub use reader::{DataReader, DirEntry, Image, InodeInfo};
pub use tree::{DirAttrs, Meta, Timestamp, XattrKey, Xattrs};
pub use writer::{LayerSummary, LayerWriter};

/// Version of kiln's erofs profile. Bump whenever output bytes change.
pub const FORMAT_VERSION: u32 = 1;
```

- [ ] **Step 5: Run the tests to verify they pass, and that nothing is dead**

Run: `cargo test -p kiln-erofs && cargo clippy -p kiln-erofs --all-targets -- -D warnings`
Expected: all tests pass, and clippy reports no warnings. If clippy flags dead code, delete the unused item; nothing in this crate should be unused.

- [ ] **Step 6: Commit**

```bash
git add crates/kiln-erofs
git commit -m "feat(erofs): merged-view parent inheritance and squash

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P7rdQAESQYNEo2qFobUNxt"
```

---
### Task 11: Independent model and property tests

**Files:**
- Create: `crates/kiln-erofs/tests/model/mod.rs`, `crates/kiln-erofs/tests/model_props.rs`
- Modify: `crates/kiln-erofs/tests/common/mod.rs` (add `view` and `groups`)

**Interfaces:**
- Consumes: `common::{try_convert_stack, squash_all, walk, Seen}` (Tasks 9–10) and `testtar` (Task 4).
- Produces in `tests/model/mod.rs`:
  - `Op`, `MKind`, `MNode` and `type State = BTreeMap<String, MNode>`.
  - `initial() -> State`, `to_tar(&[Op]) -> Vec<u8>`, `apply_layer(&[Op], layer: u64, lower: &State) -> Result<State, String>`, `apply_layer_reporting(…) -> Result<(State, Vec<String>), String>` (also returns the implicit directories), `merge(&State, &State) -> State` and `compare(&State, &BTreeMap<Vec<u8>, Seen>) -> Result<(), String>`.
  - `layers_strategy() -> impl Strategy<Value = Vec<Vec<Op>>>` and `model_final(&[Vec<Op>]) -> Option<State>`.
  - `hits_deferred_dir_times(&[Vec<Op>]) -> bool` and `ambiguous_inheritance(&[Vec<Op>]) -> bool`, which Task 14 uses to skip stacks containerd cannot compare.
- Produces in `tests/common/mod.rs`:
  - `View` (a type alias) and `view(&BTreeMap<Vec<u8>, Seen>, ignore_dir_mtime: bool) -> BTreeMap<Vec<u8>, View>`.
  - `groups(&BTreeMap<Vec<u8>, Seen>) -> BTreeSet<Vec<Vec<u8>>>`.

The model is deliberately naive: a flat `path → node` map with string paths, written without looking at `apply.rs` or `merge.rs`. If kiln and the model disagree, one of them is wrong, and the failing case says which. The model encodes spec §7.4 per layer, then overlayfs merging across layers. The test squashes kiln's layers and compares the squash against the model's final state. That covers the writer, inheritance, reader and squash in one property.

**Generator design.** These choices were measured; without them most cases are invalid and the property is weak.
- **Namespace:** paths come from a tiny namespace so entries collide. Directories are 1–3 components from `{a, b}`. Leaves are 0–2 directory components plus a name from `{f, g, a}`: `a` sometimes collides with a directory name, forcing replace and parent-not-directory cases.
- **Hardlinks:** each one picks an earlier file in the same layer by index (85% of the time), so most links are valid.
- **Symlink targets:** these live outside the namespace (`t`, `/t/v`, …). containerd resolves parents through lower-layer symlinks, which real layers never rely on.
- **Repair:** four in five layers are repaired, meaning each op that would make the layer invalid on its own is dropped. The remaining fifth stay raw, so kiln and the model must also agree on which inputs are errors.
- **Result:** with these choices about 80% of stacks are valid, averaging 2.4 layers and 11 ops.

- [ ] **Step 1: Add the comparison helpers to `tests/common/mod.rs`**

```rust
use std::collections::BTreeSet;

/// The comparable projection of a `Seen`. Mtime is `None` for directories when ignored.
pub type View = (char, u32, u32, u32, Option<(i64, u32)>, BTreeMap<Vec<u8>, Vec<u8>>, Vec<u8>, (u32, u32));

pub fn view(m: &BTreeMap<Vec<u8>, Seen>, ignore_dir_mtime: bool) -> BTreeMap<Vec<u8>, View> {
    m.iter()
        .map(|(path, s)| {
            let mtime = if ignore_dir_mtime && s.kind == 'd' { None } else { Some(s.mtime) };
            let xattrs = s.xattrs.iter().filter(|(k, _)| !k.starts_with(b"trusted.overlay.")).map(|(k, v)| (k.clone(), v.clone())).collect();
            (path.clone(), (s.kind, s.mode, s.uid, s.gid, mtime, xattrs, s.data.clone(), s.rdev))
        })
        .collect()
}

/// Hardlink groups: sets of non-directory paths that share an inode (size ≥ 2).
pub fn groups(m: &BTreeMap<Vec<u8>, Seen>) -> BTreeSet<Vec<Vec<u8>>> {
    let mut by_nid: BTreeMap<u64, Vec<Vec<u8>>> = BTreeMap::new();
    for (path, s) in m {
        if s.kind != 'd' {
            by_nid.entry(s.nid).or_default().push(path.clone());
        }
    }
    by_nid.into_values().filter(|g| g.len() > 1).collect()
}
```

- [ ] **Step 2: Write the model**

`crates/kiln-erofs/tests/model/mod.rs`:
```rust
//! A deliberately naive model of kiln's layer semantics (spec §7.4) and overlayfs
//! merging. Written independently of `apply.rs`/`merge.rs`; used as a test oracle.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};

use kiln_erofs::testtar::{Opts, TarBuilder};
use proptest::prelude::*;

use crate::common::Seen;

#[derive(Debug, Clone)]
pub enum Op {
    Dir { path: String, mode: u32, mtime: (i64, u32), uid: u32, xattr: Option<String> },
    File { path: String, data: Vec<u8>, mode: u32, mtime: (i64, u32), uid: u32, xattr: Option<String> },
    Symlink { path: String, target: String, mtime: (i64, u32) },
    Hardlink { path: String, target: String, mode: u32, mtime: (i64, u32), uid: u32 },
    Whiteout { path: String, mtime: (i64, u32) },
    Opaque { dir: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MKind {
    Dir,
    File(Vec<u8>),
    Symlink(Vec<u8>),
    Whiteout,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MNode {
    pub kind: MKind,
    pub mode: u32,
    pub uid: u32,
    pub mtime: (i64, u32),
    pub xattrs: BTreeMap<Vec<u8>, Vec<u8>>,
    pub opaque: bool,
    pub implicit: bool,
    /// Created implicitly; keeps inherited xattrs even once a header describes it.
    pub inherits: bool,
    /// Inode identity; hardlinks copy it.
    pub ident: u64,
}

/// path → node; the root is `""`.
pub type State = BTreeMap<String, MNode>;

fn dir_node(implicit: bool) -> MNode {
    MNode { kind: MKind::Dir, mode: 0o755, uid: 0, mtime: (0, 0), xattrs: BTreeMap::new(), opaque: false, implicit, inherits: implicit, ident: 0 }
}

/// The merged state before any layer: an empty root.
pub fn initial() -> State {
    State::from([(String::new(), dir_node(false))])
}

fn opts(mode: u32, mtime: (i64, u32), uid: u32, xattr: &Option<String>) -> Opts {
    let mut o = Opts::default().mode(mode).uid(u64::from(uid)).mtime(mtime.0 as u64);
    if mtime.1 != 0 {
        o = o.pax("mtime", format!("{}.{:09}", mtime.0, mtime.1).as_bytes());
    }
    if let Some(v) = xattr {
        o = o.xattr("user.x", v.as_bytes());
    }
    o
}

pub fn to_tar(ops: &[Op]) -> Vec<u8> {
    let mut b = TarBuilder::new();
    for op in ops {
        match op {
            Op::Dir { path, mode, mtime, uid, xattr } => {
                b.dir(path, &opts(*mode, *mtime, *uid, xattr));
            }
            Op::File { path, data, mode, mtime, uid, xattr } => {
                b.file(path, data, &opts(*mode, *mtime, *uid, xattr));
            }
            Op::Symlink { path, target, mtime } => {
                // A non-0777 header mode: kiln must normalize it like Linux does.
                b.symlink(path, target, &opts(0o640, *mtime, 0, &None));
            }
            Op::Hardlink { path, target, mode, mtime, uid } => {
                b.entry(path.as_bytes(), b'1', b"", target.as_bytes(), (0, 0), &opts(*mode, *mtime, *uid, &None));
            }
            Op::Whiteout { path, mtime } => {
                let marker = match path.rsplit_once('/') {
                    Some((dir, name)) => format!("{dir}/.wh.{name}"),
                    None => format!(".wh.{path}"),
                };
                b.entry(marker.as_bytes(), b'0', b"", b"", (0, 0), &opts(0o644, *mtime, 0, &None));
            }
            Op::Opaque { dir } => {
                b.opaque(dir);
            }
        }
    }
    b.finish()
}

fn parent_prefixes(path: &str) -> Vec<String> {
    let parts: Vec<&str> = path.split('/').collect();
    (1..parts.len()).map(|i| parts[..i].join("/")).collect()
}

fn ensure_parents(s: &mut State, path: &str) -> Result<(), String> {
    for prefix in parent_prefixes(path) {
        match s.get(&prefix) {
            Some(n) if n.kind == MKind::Dir => {}
            Some(_) => return Err(format!("parent {prefix} of {path} is not a directory")),
            None => {
                s.insert(prefix, dir_node(true));
            }
        }
    }
    Ok(())
}

fn ensure_dir(s: &mut State, dir: &str) -> Result<(), String> {
    if dir.is_empty() {
        return Ok(());
    }
    ensure_parents(s, dir)?;
    match s.get(dir) {
        Some(n) if n.kind == MKind::Dir => Ok(()),
        Some(_) => Err(format!("{dir} is not a directory")),
        None => {
            s.insert(dir.to_string(), dir_node(true));
            Ok(())
        }
    }
}

fn remove_tree(s: &mut State, path: &str) {
    let prefix = format!("{path}/");
    s.retain(|k, _| k != path && !k.starts_with(&prefix));
}

fn xattrs_of(xattr: &Option<String>) -> BTreeMap<Vec<u8>, Vec<u8>> {
    xattr.iter().map(|v| (b"user.x".to_vec(), v.clone().into_bytes())).collect()
}

/// Applies one layer's ops (spec §7.4) and resolves implicit directories against
/// `lower`, the merged state of the layers below.
pub fn apply_layer(ops: &[Op], layer: u64, lower: &State) -> Result<State, String> {
    apply_layer_reporting(ops, layer, lower).map(|(state, _)| state)
}

/// Like `apply_layer`, also returning the implicit directory paths (root excluded).
pub fn apply_layer_reporting(ops: &[Op], layer: u64, lower: &State) -> Result<(State, Vec<String>), String> {
    let mut s = State::from([(String::new(), dir_node(true))]);
    let mut serial = 0u64;
    let mut next = || {
        serial += 1;
        (layer << 32) | serial
    };
    let mut base: Option<(i64, u32)> = None;
    let mut note = |t: (i64, u32)| base = Some(base.map_or(t, |b| b.min(t)));
    for op in ops {
        match op {
            Op::Dir { path, mode, mtime, uid, xattr } => {
                ensure_parents(&mut s, path)?;
                note(*mtime);
                match s.get_mut(path) {
                    Some(n) if n.kind == MKind::Dir => {
                        n.mode = *mode;
                        n.uid = *uid;
                        n.mtime = *mtime;
                        n.xattrs.extend(xattrs_of(xattr));
                        n.implicit = false;
                    }
                    _ => {
                        remove_tree(&mut s, path);
                        let node = MNode { kind: MKind::Dir, mode: *mode, uid: *uid, mtime: *mtime, xattrs: xattrs_of(xattr), opaque: false, implicit: false, inherits: false, ident: next() };
                        s.insert(path.clone(), node);
                    }
                }
            }
            Op::File { path, data, mode, mtime, uid, xattr } => {
                ensure_parents(&mut s, path)?;
                note(*mtime);
                remove_tree(&mut s, path);
                let node = MNode { kind: MKind::File(data.clone()), mode: *mode, uid: *uid, mtime: *mtime, xattrs: xattrs_of(xattr), opaque: false, implicit: false, inherits: false, ident: next() };
                s.insert(path.clone(), node);
            }
            Op::Symlink { path, target, mtime } => {
                ensure_parents(&mut s, path)?;
                note(*mtime);
                remove_tree(&mut s, path);
                let node = MNode { kind: MKind::Symlink(target.clone().into_bytes()), mode: 0o777, uid: 0, mtime: *mtime, xattrs: BTreeMap::new(), opaque: false, implicit: false, inherits: false, ident: next() };
                s.insert(path.clone(), node);
            }
            Op::Hardlink { path, target, mode, mtime, uid } => {
                ensure_parents(&mut s, path)?;
                if path == target {
                    return Err(format!("{path} links to itself"));
                }
                if target.starts_with(&format!("{path}/")) {
                    return Err(format!("hardlink {path} replaces its own target {target}"));
                }
                let t = s.get(target).cloned().ok_or_else(|| format!("hardlink target {target} missing"))?;
                if matches!(t.kind, MKind::Dir | MKind::Whiteout) {
                    return Err(format!("hardlink target {target} is not a regular entry"));
                }
                // The link header's metadata applies to the shared inode, i.e. every
                // path with the same identity (containerd semantics).
                note(*mtime);
                for n in s.values_mut().filter(|n| n.ident == t.ident) {
                    if !matches!(n.kind, MKind::Symlink(_)) {
                        n.mode = *mode;
                    }
                    n.uid = *uid;
                    n.mtime = *mtime;
                }
                let linked = s.values().find(|n| n.ident == t.ident).cloned().expect("target present");
                remove_tree(&mut s, path);
                s.insert(path.clone(), linked);
            }
            Op::Whiteout { path, mtime } => {
                ensure_parents(&mut s, path)?;
                if s.contains_key(path) {
                    return Err(format!("whiteout {path} names an entry already in this layer"));
                }
                note(*mtime);
                remove_tree(&mut s, path);
                let node = MNode { kind: MKind::Whiteout, mode: 0, uid: 0, mtime: *mtime, xattrs: BTreeMap::new(), opaque: false, implicit: false, inherits: false, ident: next() };
                s.insert(path.clone(), node);
            }
            Op::Opaque { dir } => {
                ensure_dir(&mut s, dir)?;
                s.get_mut(dir.as_str()).expect("ensured").opaque = true;
            }
        }
    }
    let base = base.unwrap_or((0, 0));
    let implicit: Vec<String> = s.iter().filter(|(p, n)| n.inherits && !p.is_empty()).map(|(p, _)| p.clone()).collect();
    for (path, n) in s.iter_mut() {
        if n.inherits && !n.implicit && !path.is_empty() {
            if let Some(l) = lower.get(path).filter(|l| l.kind == MKind::Dir) {
                let own = std::mem::take(&mut n.xattrs);
                n.xattrs = l.xattrs.clone();
                n.xattrs.extend(own);
            }
            n.inherits = false;
            continue;
        }
        if !n.implicit {
            continue;
        }
        match lower.get(path) {
            Some(l) if l.kind == MKind::Dir && !path.is_empty() => {
                n.mode = l.mode;
                n.uid = l.uid;
                n.mtime = l.mtime;
                n.xattrs = l.xattrs.clone();
            }
            _ => {
                n.mode = 0o755;
                n.uid = 0;
                n.mtime = base;
                n.xattrs.clear();
            }
        }
        n.implicit = false;
        n.inherits = false;
    }
    Ok((s, implicit))
}

/// overlayfs: `layer` on top of the merged state `lower`.
pub fn merge(lower: &State, layer: &State) -> State {
    let mut out = lower.clone();
    let root = &layer[""];
    if root.opaque {
        out.retain(|k, _| k.is_empty());
    }
    let r = out.get_mut("").expect("root");
    r.mode = root.mode;
    r.uid = root.uid;
    r.mtime = root.mtime;
    r.xattrs = root.xattrs.clone();
    for (path, n) in layer.iter().filter(|(p, _)| !p.is_empty()) {
        match n.kind {
            MKind::Whiteout => remove_tree(&mut out, path),
            MKind::Dir => {
                match out.get_mut(path) {
                    Some(o) if o.kind == MKind::Dir => {
                        o.mode = n.mode;
                        o.uid = n.uid;
                        o.mtime = n.mtime;
                        o.xattrs = n.xattrs.clone();
                    }
                    _ => {
                        remove_tree(&mut out, path);
                        out.insert(path.clone(), MNode { opaque: false, ..n.clone() });
                    }
                }
                if n.opaque {
                    let prefix = format!("{path}/");
                    out.retain(|k, _| !k.starts_with(&prefix));
                }
            }
            _ => {
                remove_tree(&mut out, path);
                out.insert(path.clone(), n.clone());
            }
        }
    }
    for n in out.values_mut() {
        n.opaque = false;
    }
    out
}

/// Compares the model's final state with a walked kiln image.
pub fn compare(model: &State, seen: &BTreeMap<Vec<u8>, Seen>) -> Result<(), String> {
    let model_paths: BTreeSet<Vec<u8>> = model.keys().map(|k| k.as_bytes().to_vec()).collect();
    let seen_paths: BTreeSet<Vec<u8>> = seen.keys().cloned().collect();
    if model_paths != seen_paths {
        return Err(format!(
            "paths differ: model-only {:?}, kiln-only {:?}",
            model_paths.difference(&seen_paths).map(|p| String::from_utf8_lossy(p).into_owned()).collect::<Vec<_>>(),
            seen_paths.difference(&model_paths).map(|p| String::from_utf8_lossy(p).into_owned()).collect::<Vec<_>>()
        ));
    }
    let mut model_groups: BTreeMap<u64, Vec<Vec<u8>>> = BTreeMap::new();
    for (path, m) in model {
        let s = &seen[path.as_bytes()];
        let (kind, data) = match &m.kind {
            MKind::Dir => ('d', Vec::new()),
            MKind::File(d) => ('f', d.clone()),
            MKind::Symlink(t) => ('l', t.clone()),
            MKind::Whiteout => return Err(format!("whiteout {path} survived the merge")),
        };
        let expect = (kind, m.mode, m.uid, m.mtime, &m.xattrs, &data);
        let got = (s.kind, s.mode, s.uid, s.mtime, &s.xattrs, &s.data);
        if expect != got {
            return Err(format!("{path:?}: model {expect:?} != kiln {got:?}"));
        }
        if kind != 'd' {
            model_groups.entry(m.ident).or_default().push(path.as_bytes().to_vec());
        }
    }
    let model_groups: BTreeSet<Vec<Vec<u8>>> = model_groups.into_values().filter(|g| g.len() > 1).collect();
    let kiln_groups = crate::common::groups(seen);
    if model_groups != kiln_groups {
        return Err(format!("hardlink groups differ: model {model_groups:?} kiln {kiln_groups:?}"));
    }
    for g in &kiln_groups {
        for p in g {
            if seen[p].nlink as usize != g.len() {
                return Err(format!("nlink of {:?} is {} but its group has {}", String::from_utf8_lossy(p), seen[p].nlink, g.len()));
            }
        }
    }
    Ok(())
}

fn dir_path() -> impl Strategy<Value = String> {
    prop::collection::vec(prop::sample::select(vec!["a", "b"]), 1..=3).prop_map(|v| v.join("/"))
}

/// A leaf path: up to two directory components, then a name that only sometimes
/// collides with a directory name (forcing replace and parent-not-dir cases).
fn leaf_path() -> impl Strategy<Value = String> {
    (prop::collection::vec(prop::sample::select(vec!["a", "b"]), 0..=2), prop::sample::select(vec!["f", "g", "f", "g", "a"]))
        .prop_map(|(mut v, leaf)| {
            v.push(leaf);
            v.join("/")
        })
}

fn mtime() -> impl Strategy<Value = (i64, u32)> {
    (0i64..3, prop::sample::select(vec![0u32, 500])).prop_map(|(s, n)| (1_700_000_000 + s, n))
}

/// Ops as generated; hardlinks name an earlier file by index so most are valid.
#[derive(Debug, Clone)]
enum Raw {
    Op(Op),
    Link { path: String, pick: Option<usize>, fallback: String, mode: u32, mtime: (i64, u32), uid: u32 },
}

fn raw_op() -> impl Strategy<Value = Raw> {
    let mode = || prop::sample::select(vec![0o755u32, 0o644, 0o1777, 0o4755]);
    let uid = || prop::sample::select(vec![0u32, 1000, 70_000]);
    let xattr = || prop::option::of(prop::sample::select(vec!["v1".to_string(), "v2".to_string()]));
    let data = (prop::sample::select(vec![0usize, 1, 100, 4031, 4032, 4096, 5000]), any::<u8>())
        .prop_map(|(n, seed)| (0..n).map(|i| seed.wrapping_add(i as u8)).collect::<Vec<u8>>());
    prop_oneof![
        3 => (dir_path(), mode(), mtime(), uid(), xattr()).prop_map(|(path, mode, mtime, uid, xattr)| Raw::Op(Op::Dir { path, mode, mtime, uid, xattr })),
        5 => (leaf_path(), data, mode(), mtime(), uid(), xattr()).prop_map(|(path, data, mode, mtime, uid, xattr)| Raw::Op(Op::File { path, data, mode, mtime, uid, xattr })),
        // Targets live outside the generated namespace: containerd resolves parents
        // through lower-layer symlinks, which real layers never rely on.
        1 => (leaf_path(), prop::sample::select(vec!["t", "t/u", "/t/v", "../t"]), mtime())
            .prop_map(|(path, target, mtime)| Raw::Op(Op::Symlink { path, target: target.to_string(), mtime })),
        2 => (leaf_path(), prop::option::weighted(0.85, any::<usize>()), leaf_path(), mode(), mtime(), uid())
            .prop_map(|(path, pick, fallback, mode, mtime, uid)| Raw::Link { path, pick, fallback, mode, mtime, uid }),
        2 => (prop_oneof![dir_path(), leaf_path()], mtime()).prop_map(|(path, mtime)| Raw::Op(Op::Whiteout { path, mtime })),
        1 => prop_oneof![Just(String::new()), dir_path()].prop_map(|dir| Raw::Op(Op::Opaque { dir })),
    ]
}

fn concretize(raw: Vec<Raw>) -> Vec<Op> {
    let mut files: Vec<String> = Vec::new();
    raw.into_iter()
        .map(|r| match r {
            Raw::Op(op) => {
                if let Op::File { path, .. } = &op {
                    files.push(path.clone());
                }
                op
            }
            Raw::Link { path, pick, fallback, mode, mtime, uid } => {
                let target = match pick {
                    Some(i) if !files.is_empty() => files[i % files.len()].clone(),
                    _ => fallback,
                };
                Op::Hardlink { path, target, mode, mtime, uid }
            }
        })
        .collect()
}

/// Drops each op that would make the layer invalid on its own, so most generated
/// layers are valid and reach the comparison.
fn repair(ops: Vec<Op>) -> Vec<Op> {
    let mut kept: Vec<Op> = Vec::new();
    for op in ops {
        kept.push(op);
        if apply_layer(&kept, 0, &initial()).is_err() {
            kept.pop();
        }
    }
    kept
}

/// One to four layers of up to twelve ops over a tiny namespace (to force collisions).
/// Four in five layers are repaired; the rest stay raw so kiln and the model must
/// also agree on which inputs are errors.
pub fn layers_strategy() -> impl Strategy<Value = Vec<Vec<Op>>> {
    let layer = (prop::collection::vec(raw_op(), 0..12), 0u8..5)
        .prop_map(|(raw, roll)| if roll == 0 { concretize(raw) } else { repair(concretize(raw)) });
    prop::collection::vec(layer, 1..5)
}

fn op_path(op: &Op) -> &str {
    match op {
        Op::Dir { path, .. } | Op::File { path, .. } | Op::Symlink { path, .. } | Op::Hardlink { path, .. } | Op::Whiteout { path, .. } => path,
        Op::Opaque { dir } => dir,
    }
}

/// containerd and moby set directory mtimes in a final pass over the layer's
/// directory headers. If a later entry replaced that directory or one of its
/// parents with a non-directory, the pass fails (or re-times the replacement),
/// so such layers are not comparable. kiln keeps each entry's own attributes.
pub fn hits_deferred_dir_times(layers: &[Vec<Op>]) -> bool {
    layers.iter().any(|ops| {
        ops.iter().enumerate().any(|(i, op)| {
            let Op::Dir { path, .. } = op else { return false };
            let prefix_of = |q: &str| q == path.as_str() || path.starts_with(&format!("{q}/"));
            let mut replaced = false;
            for later in &ops[i + 1..] {
                match later {
                    Op::Dir { path: q, .. } if q == path => replaced = false,
                    Op::Dir { .. } | Op::Opaque { .. } => {}
                    other if prefix_of(op_path(other)) => replaced = true,
                    _ => {}
                }
            }
            replaced
        })
    })
}

/// containerd resolves an implicit parent by searching each lower layer on its own,
/// skipping non-directories and ignoring whiteouts and opaque directories; kiln uses
/// the overlay view. They agree unless a lower layer has a non-directory at the path
/// or one of its parents, or an opaque parent; builders never omit such parents.
pub fn ambiguous_inheritance(layers: &[Vec<Op>]) -> bool {
    let mut merged = initial();
    let mut below: Vec<State> = Vec::new();
    for (i, ops) in layers.iter().enumerate() {
        let Ok((state, implicit)) = apply_layer_reporting(ops, i as u64, &merged) else { return false };
        for p in &implicit {
            let mut prefixes = vec![String::new()];
            prefixes.extend(parent_prefixes(p));
            for lower in &below {
                if prefixes.iter().any(|q| lower.get(q).is_some_and(|n| n.kind != MKind::Dir || n.opaque))
                    || lower.get(p).is_some_and(|n| n.kind != MKind::Dir)
                {
                    return true;
                }
            }
        }
        merged = merge(&merged, &state);
        below.push(state);
    }
    false
}

/// Runs the model over all layers; `None` if any layer is invalid.
pub fn model_final(layers: &[Vec<Op>]) -> Option<State> {
    let mut state = initial();
    for (i, ops) in layers.iter().enumerate() {
        let layer = apply_layer(ops, i as u64, &state).ok()?;
        state = merge(&state, &layer);
    }
    Some(state)
}
```

- [ ] **Step 3: Write the property test**

`crates/kiln-erofs/tests/model_props.rs`:
```rust
mod common;
mod model;

use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    #[test]
    fn kiln_matches_model(layers in model::layers_strategy()) {
        let tars: Vec<Vec<u8>> = layers.iter().map(|ops| model::to_tar(ops)).collect();
        let expected = model::model_final(&layers);
        let kiln = common::try_convert_stack(&tars);
        prop_assert_eq!(expected.is_some(), kiln.is_ok(), "model valid = {}, kiln = {:?}", expected.is_some(), kiln.as_ref().err());
        if let (Some(state), Ok(images)) = (expected, kiln) {
            let seen = common::walk(&common::squash_all(&images));
            if let Err(e) = model::compare(&state, &seen) {
                prop_assert!(false, "{}", e);
            }
        }
    }
}
```

- [ ] **Step 4: Run it**

Run: `cargo test -p kiln-erofs --test model_props`
Expected: PASS (256 cases). If it fails, proptest prints a minimized stack:
1. Decide whether kiln or the model disagrees with spec §7.4 or overlayfs semantics.
2. Fix the side that is wrong.
3. Add the minimized case as a named regression test in `tests/stack.rs`, then rerun.

Do not loosen `compare`.

- [ ] **Step 5: Commit**

```bash
git add crates/kiln-erofs/tests
git commit -m "test(erofs): independent model and property tests over random layer stacks

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P7rdQAESQYNEo2qFobUNxt"
```

---

### Task 12: Determinism goldens and the normative format doc

**Files:**
- Create: `crates/kiln-erofs/tests/golden.rs`, `crates/kiln-erofs/tests/golden/digests-v1.txt` (generated), `docs/format.md`
- Modify: `crates/kiln-erofs/tests/common/mod.rs` (add `fixtures`), `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: `common::convert` (Task 9), `testtar` (Task 4) and `FORMAT_VERSION` (Task 8).
- Produces:
  - `common::fixtures() -> Vec<(&'static str, Vec<u8>)>`.
  - `common::CAP_NET_BIND_SERVICE`, a valid `vfs_cap_data` v2 value. The Linux kernel rejects a malformed `security.capability` with EINVAL, which Task 13 would hit.
  - The golden file `tests/golden/digests-v{FORMAT_VERSION}.txt`.

**Rule:** a golden file is immutable once merged. Any change to output bytes needs a new `FORMAT_VERSION` and a new `digests-vN.txt`. The CI job `golden-immutable` fails a pull request that modifies or deletes an existing golden file.

- [ ] **Step 1: Add the fixtures to `tests/common/mod.rs`**

```rust
use kiln_erofs::testtar::{Opts, TarBuilder};

/// A valid `vfs_cap_data` v2 value granting cap_net_bind_service (the kernel
/// rejects malformed `security.capability` values with EINVAL).
pub const CAP_NET_BIND_SERVICE: [u8; 20] = [0, 0, 0, 2, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

/// Fixed layers covering every entry kind and layout decision.
pub fn fixtures() -> Vec<(&'static str, Vec<u8>)> {
    let pattern = |n: usize| -> Vec<u8> { (0..n).map(|i| (i * 7 % 256) as u8).collect() };
    let mixed = TarBuilder::new()
        .dir("etc", &Opts::default().mode(0o755))
        .file("etc/os-release", b"ID=kiln\n", &Opts::default().xattr("user.common", b"1"))
        .file("usr/bin/tool", &pattern(10_000), &Opts::default().mode(0o755).xattr("security.capability", &CAP_NET_BIND_SERVICE).xattr("user.common", b"1"))
        .symlink("usr/bin/alias", "tool", &Opts::default())
        .hardlink("usr/bin/tool2", "usr/bin/tool")
        .chardev("dev/console", 5, 1, &Opts::default().mode(0o600))
        .fifo("run/fifo", &Opts::default())
        .whiteout("etc/old")
        .dir("var/cache", &Opts::default().mode(0o755))
        .opaque("var/cache")
        .file("home/u/f", &pattern(4096), &Opts::default().uid(70_000).pax("mtime", b"1700000000.123456789"))
        .finish();
    let mut many = TarBuilder::new();
    for i in 0..700 {
        many.file(&format!("d/f{i:04}"), &pattern(i % 50), &Opts::default());
    }
    for sub in ["d/x", "d/y", "d/z"] {
        many.dir(sub, &Opts::default().mode(0o750));
    }
    vec![("empty", TarBuilder::new().finish()), ("mixed", mixed), ("many", many.finish())]
}
```

- [ ] **Step 2: Write the golden test**

`crates/kiln-erofs/tests/golden.rs`:
```rust
mod common;

use sha2::{Digest, Sha256};

fn hex(b: &[u8]) -> String {
    Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect()
}

fn golden_path() -> String {
    format!("{}/tests/golden/digests-v{}.txt", env!("CARGO_MANIFEST_DIR"), kiln_erofs::FORMAT_VERSION)
}

#[test]
fn golden_digests() {
    let actual: String = common::fixtures()
        .iter()
        .map(|(name, tar)| format!("{name} {}\n", hex(&common::convert(tar).0)))
        .collect();
    if std::env::var_os("KILN_UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(format!("{}/tests/golden", env!("CARGO_MANIFEST_DIR"))).unwrap();
        std::fs::write(golden_path(), &actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(golden_path())
        .unwrap_or_else(|_| panic!("missing {}; create it with KILN_UPDATE_GOLDEN=1", golden_path()));
    assert_eq!(actual, expected, "erofs output changed: bump FORMAT_VERSION and add a new golden file instead of editing this one");
}
```

- [ ] **Step 3: Run it to verify it fails**

Run: `cargo test -p kiln-erofs --test golden`
Expected: FAIL with `missing .../tests/golden/digests-v1.txt; create it with KILN_UPDATE_GOLDEN=1`.

- [ ] **Step 4: Generate the golden file, then verify it is stable**

Run: `KILN_UPDATE_GOLDEN=1 cargo test -p kiln-erofs --test golden && cargo test -p kiln-erofs --test golden && cat crates/kiln-erofs/tests/golden/digests-v1.txt`
Expected: both runs pass, and the file has three lines: `empty <64 hex>`, `mixed <64 hex>` and `many <64 hex>`. CI then checks that macOS and Linux produce the same digests.

- [ ] **Step 5: Add the CI immutability check**

Append this job to `.github/workflows/ci.yml`:
```yaml
  golden-immutable:
    if: github.event_name == 'pull_request'
    runs-on: ubuntu-24.04
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
      - name: Existing golden files must not change
        run: |
          changed=$(git diff --name-status "origin/${{ github.base_ref }}...HEAD" -- crates/kiln-erofs/tests/golden/ | awk '$1 != "A"')
          if [ -n "$changed" ]; then
            echo "Golden digests are immutable. Bump kiln_erofs::FORMAT_VERSION and add a new digests-vN.txt:"
            echo "$changed"
            exit 1
          fi
```

- [ ] **Step 6: Write `docs/format.md`**

````markdown
# kiln formats

This document is normative. It currently defines the **erofs profile** (format version 1). The image manifest (§5.1 of the design spec) and the control protocol (§9.5) are added by milestones M1b and M3.

## erofs profile, version 1

A kiln layer image is a Linux erofs filesystem restricted as follows. Readers reject anything outside the profile.

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
block 0          zeros, with the superblock at byte 1024
data area        file data in tar order (or inode order when squashed), then directory
                 and symlink bodies that are not fully inline, in inode order
xattr table      shared xattr entries, each 4-byte aligned (optional)
metadata area    32-byte slots; slot 0 is zero; inodes from nid 1, in inode order
```

The image ends at a 4096-byte boundary.

### Inodes

- **Numbering:** inodes are numbered breadth-first from the root, visiting directory entries in byte order of their names. A hardlinked inode takes its number at its first occurrence.
- **Data layouts:** only `FLAT_PLAIN` (0) and `FLAT_INLINE` (2). `i_format` bit 4 is never set.
- **Inline tails:** a tail is inline only when it fits after the inode and its xattrs within one block.
  - For regular files, that is decided while streaming, assuming the worst case (a 64-byte inode and all xattrs inline): `tail + 64 + 12 + Σ entry sizes ≤ 4096`.
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
- Entries are sorted in strict byte order, including `.` and `..`, and packed greedily into 4096-byte blocks of dirents followed by names. Non-last blocks are zero-padded.
- `nlink` is 2 plus the number of subdirectories.

### Xattrs

- **Name indexes:** `user.` 1, `system.posix_acl_access` 2, `system.posix_acl_default` 3, `trusted.` 4, `security.` 6. Other namespaces are dropped with a warning.
- **Shared table:** an `(index, name, value)` triple goes into the shared table when two or more inodes carry it, or when an inode's all-inline body would exceed 4032 bytes (in which case all of that inode's xattrs are shared).
  - Shared entries are ordered by first use in inode order.
  - Within an inode, shared ids come first, then inline entries, each sorted by `(index, name)`.
- `h_name_filter` is 0.

### Overlay markers

- A whiteout is a character device 0:0, mode 0, uid and gid 0.
- An opaque directory carries `trusted.overlay.opaque = "y"`.
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
  - `.wh..wh..opq` marks its directory opaque.
- **Unsupported entries:** sparse entries, PAX size overrides, and devices with major > 4095 or minor > 1048575 are errors. Xattrs outside the five namespaces above are dropped with a warning.

### Implicit directories

A directory created because a descendant, whiteout or opaque marker needed it inherits from the same path in the overlay of the lower layers, with `trusted.overlay.*` keys stripped. That means the topmost lower layer providing the path, where whiteouts, opaque directories and non-directories in nearer layers hide farther ones.
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
````

- [ ] **Step 7: Run everything and commit**

Run: `cargo test -p kiln-erofs`
Expected: all tests pass.

```bash
git add crates/kiln-erofs/tests docs/format.md .github/workflows/ci.yml
git commit -m "test(erofs): immutable golden digests; docs: normative erofs profile

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P7rdQAESQYNEo2qFobUNxt"
```

---

### Task 13: Linux kernel validation (`fsck.erofs`, loop mounts, overlayfs)

**Files:**
- Create: `crates/kiln-erofs/tests/kernel.rs`, `scripts/run-root-test.sh`
- Modify: `crates/kiln-erofs/tests/common/mod.rs` (add `walk_fs` and `Mount`, Linux only), `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: `common::{fixtures, convert, convert_stack, squash_all, walk, view, groups}`.
- Produces (Linux only, in `common`):
  - `walk_fs(&Path) -> BTreeMap<Vec<u8>, Seen>`. `nid` holds `st_ino`, and `compact`/`layout` are unset.
  - `Mount` (a RAII guard) with `Mount::erofs(img: &Path, target: &Path)` and `Mount::overlay(lowers_top_first: &[&Path], target: &Path)`.
  - `is_root() -> bool`.

These tests prove that the Linux kernel reads kiln images exactly as kiln's own reader does, and that overlayfs over kiln layers equals kiln's squash. They run only when `KILN_KERNEL_TESTS` is set. Mount tests also need root and skip with a message otherwise.

- [ ] **Step 1: Add the Linux helpers**

```bash
cargo add -p kiln-erofs --dev --target 'cfg(target_os = "linux")' xattr
```

Append to `tests/common/mod.rs`:
```rust
#[cfg(target_os = "linux")]
#[allow(unused_imports)]
pub use linux::*;

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::BTreeMap;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use kiln_erofs::ondisk::{S_IFBLK, S_IFCHR, S_IFDIR, S_IFIFO, S_IFLNK, S_IFMT, S_IFREG};

    use super::Seen;

    pub fn is_root() -> bool {
        Command::new("id").arg("-u").output().map(|o| o.stdout == b"0\n").unwrap_or(false)
    }

    /// Walks a mounted tree without following symlinks.
    pub fn walk_fs(root: &Path) -> BTreeMap<Vec<u8>, Seen> {
        let mut out = BTreeMap::new();
        let mut stack = vec![(Vec::new(), root.to_path_buf())];
        while let Some((rel, path)) = stack.pop() {
            // overlayfs lists a whiteout that hides nothing in a directory that exists in
            // only one layer, but lstat says ENOENT; such names are not visible files.
            let md = match std::fs::symlink_metadata(&path) {
                Ok(md) => md,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => panic!("lstat {}: {e}", path.display()),
            };
            let kind = match md.mode() & S_IFMT {
                S_IFDIR => 'd',
                S_IFREG => 'f',
                S_IFLNK => 'l',
                S_IFCHR => 'c',
                S_IFBLK => 'b',
                S_IFIFO => 'p',
                _ => '?',
            };
            let data = match kind {
                'f' => std::fs::read(&path).unwrap(),
                'l' => std::fs::read_link(&path).unwrap().as_os_str().as_bytes().to_vec(),
                _ => Vec::new(),
            };
            let mut xattrs = BTreeMap::new();
            for name in xattr::list(&path).unwrap() {
                if let Some(v) = xattr::get(&path, &name).unwrap() {
                    xattrs.insert(name.as_bytes().to_vec(), v);
                }
            }
            if kind == 'd' {
                for e in std::fs::read_dir(&path).unwrap() {
                    let e = e.unwrap();
                    let mut child = rel.clone();
                    if !child.is_empty() {
                        child.push(b'/');
                    }
                    child.extend_from_slice(e.file_name().as_bytes());
                    stack.push((child, e.path()));
                }
            }
            let dev = md.rdev();
            let major = (((dev >> 8) & 0xfff) | ((dev >> 32) & 0xffff_f000)) as u32;
            let minor = ((dev & 0xff) | ((dev >> 12) & 0xffff_ff00)) as u32;
            let rdev = if kind == 'c' || kind == 'b' { (major, minor) } else { (0, 0) };
            out.insert(
                rel,
                Seen {
                    kind,
                    mode: md.mode() & 0o7777,
                    uid: md.uid(),
                    gid: md.gid(),
                    mtime: (md.mtime(), md.mtime_nsec() as u32),
                    nlink: md.nlink() as u32,
                    xattrs,
                    data,
                    rdev,
                    nid: md.ino(),
                    compact: false,
                    layout: 0,
                },
            );
        }
        out
    }

    /// Unmounts on drop. Declare outer mounts after inner ones so they drop first.
    pub struct Mount(PathBuf);

    impl Mount {
        fn run(args: &[&str], target: &Path) -> Mount {
            std::fs::create_dir_all(target).unwrap();
            let status = Command::new("mount").args(args).arg(target).status().unwrap();
            assert!(status.success(), "mount {args:?} {} failed", target.display());
            Mount(target.to_path_buf())
        }

        pub fn erofs(img: &Path, target: &Path) -> Mount {
            Self::run(&["-t", "erofs", "-o", "loop,ro", img.to_str().unwrap()], target)
        }

        pub fn overlay(lowers_top_first: &[&Path], target: &Path) -> Mount {
            let lower: Vec<&str> = lowers_top_first.iter().map(|p| p.to_str().unwrap()).collect();
            let opts = format!("lowerdir={},xino=on,redirect_dir=off,index=off,metacopy=off", lower.join(":"));
            Self::run(&["-t", "overlay", "overlay", "-o", &opts], target)
        }
    }

    impl Drop for Mount {
        fn drop(&mut self) {
            let _ = Command::new("umount").arg(&self.0).status();
        }
    }
}
```

`scripts/run-root-test.sh`:
```bash
#!/usr/bin/env bash
# Builds one kiln-erofs integration test as the current user, then runs it as root.
# Usage: scripts/run-root-test.sh <test-name>
set -euo pipefail
test_name="$1"
bin=$(cargo test -p kiln-erofs --test "$test_name" --no-run --message-format=json \
  | jq -r --arg t "$test_name" 'select(.reason == "compiler-artifact" and .target.name == $t and .executable != null) | .executable' \
  | tail -1)
sudo --preserve-env=KILN_KERNEL_TESTS,KILN_ORACLE "$bin" --test-threads=1
```
Run: `chmod +x scripts/run-root-test.sh`

- [ ] **Step 2: Write the kernel tests**

`crates/kiln-erofs/tests/kernel.rs`:
```rust
#![cfg(target_os = "linux")]

mod common;

use std::path::Path;
use std::process::Command;

use common::{convert, convert_stack, fixtures, groups, is_root, squash_all, view, walk, walk_fs, Mount};
use kiln_erofs::testtar::{Opts, TarBuilder};

fn enabled() -> bool {
    std::env::var_os("KILN_KERNEL_TESTS").is_some()
}

fn stack() -> Vec<Vec<u8>> {
    let l0 = TarBuilder::new()
        .dir("etc", &Opts::default())
        .file("etc/a", b"a", &Opts::default())
        .file("etc/b", b"b", &Opts::default())
        .dir("var", &Opts::default())
        .file("var/x", b"x", &Opts::default())
        .file("h1", b"h", &Opts::default())
        .hardlink("h2", "h1")
        .dir("tmp", &Opts::default().mode(0o1777).xattr("user.k", b"v"))
        .finish();
    let l1 = TarBuilder::new()
        .whiteout("etc/a")
        .dir("var", &Opts::default())
        .opaque("var")
        .file("var/y", b"y", &Opts::default())
        .file("h1", b"new", &Opts::default())
        .file("tmp/cache/z", b"z", &Opts::default())
        .finish();
    convert_stack(&[l0, l1])
}

fn write_tmp(dir: &Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, bytes).unwrap();
    p
}

#[test]
fn fsck_accepts_every_image() {
    if !enabled() {
        eprintln!("skipping: set KILN_KERNEL_TESTS=1");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut images: Vec<(String, Vec<u8>)> = fixtures().into_iter().map(|(n, t)| (n.to_string(), convert(&t).0)).collect();
    let layers = stack();
    images.push(("squash".into(), squash_all(&layers)));
    for (i, l) in layers.into_iter().enumerate() {
        images.push((format!("layer{i}"), l));
    }
    for (name, img) in images {
        let path = write_tmp(dir.path(), &format!("{name}.erofs"), &img);
        let out = Command::new("fsck.erofs").arg(&path).output().expect("fsck.erofs is installed (erofs-utils)");
        assert!(out.status.success(), "fsck.erofs {name}: {}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    }
}

#[test]
fn kernel_mount_matches_reader() {
    if !enabled() || !is_root() {
        eprintln!("skipping: needs KILN_KERNEL_TESTS=1 and root");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    for (name, tar) in fixtures() {
        let img = convert(&tar).0;
        let path = write_tmp(dir.path(), &format!("{name}.erofs"), &img);
        let mnt = dir.path().join(format!("{name}.mnt"));
        let _m = Mount::erofs(&path, &mnt);
        let kernel = walk_fs(&mnt);
        let ours = walk(&img);
        assert_eq!(view(&kernel, false), view(&ours, false), "fixture {name}: kernel view differs from kiln's reader");
        assert_eq!(groups(&kernel), groups(&ours), "fixture {name}: hardlink groups differ");
        for (p, s) in &ours {
            assert_eq!(kernel[p].nlink, s.nlink, "fixture {name}: nlink of {:?}", String::from_utf8_lossy(p));
        }
    }
}

#[test]
fn overlay_over_layers_equals_squash() {
    if !enabled() || !is_root() {
        eprintln!("skipping: needs KILN_KERNEL_TESTS=1 and root");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let layers = stack();
    let p0 = write_tmp(dir.path(), "l0.erofs", &layers[0]);
    let p1 = write_tmp(dir.path(), "l1.erofs", &layers[1]);
    let m0 = dir.path().join("m0");
    let m1 = dir.path().join("m1");
    let merged = dir.path().join("merged");
    let _l0 = Mount::erofs(&p0, &m0);
    let _l1 = Mount::erofs(&p1, &m1);
    let _ov = Mount::overlay(&[&m1, &m0], &merged);
    let overlay = walk_fs(&merged);
    let squashed = walk(&squash_all(&layers));
    assert!(!overlay.contains_key(b"etc/a".as_slice()));
    assert!(!overlay.contains_key(b"var/x".as_slice()));
    assert_eq!(overlay[&b"tmp".to_vec()].mode, 0o1777, "implicit tmp inherited 1777");
    assert_eq!(view(&overlay, false), view(&squashed, false));
    assert_eq!(groups(&overlay), groups(&squashed));
}
```

- [ ] **Step 3: Run the tests locally (on any Linux host or in Lima)**

On macOS the file compiles to nothing (`#![cfg(target_os = "linux")]`). On Linux:
```bash
sudo apt-get install -y erofs-utils jq
KILN_KERNEL_TESTS=1 cargo test -p kiln-erofs --test kernel
KILN_KERNEL_TESTS=1 scripts/run-root-test.sh kernel
```
Expected: the first command passes `fsck_accepts_every_image`, and the mount tests print `skipping`. The second command passes all 3 tests as root.

- [ ] **Step 4: Add the CI job**

Append to `.github/workflows/ci.yml`:
```yaml
  linux-kernel:
    runs-on: ubuntu-24.04
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: sudo apt-get update && sudo apt-get install -y erofs-utils jq
      - run: KILN_KERNEL_TESTS=1 cargo test -p kiln-erofs --test kernel
      - run: KILN_KERNEL_TESTS=1 scripts/run-root-test.sh kernel
```

- [ ] **Step 5: Commit**

```bash
git add crates/kiln-erofs scripts .github/workflows/ci.yml
git commit -m "test(erofs): fsck.erofs, kernel loop mounts and overlayfs-vs-squash on Linux

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P7rdQAESQYNEo2qFobUNxt"
```

---

### Task 14: containerd oracle (spec §11.2)

**Files:**
- Create: `tools/oracle/go.mod`, `tools/oracle/main.go`, `crates/kiln-erofs/tests/oracle.rs`
- Modify: `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: `common::{try_convert_stack, walk_fs, view, groups, Mount, is_root}` and `model::{layers_strategy, model_final}` (Task 11).
- Produces: the `oracle` binary, `oracle <layer-dir> <layer.tar> [<parent-dir> ...]`. Parent directories are passed **nearest first**, as containerd's `WithParents` expects.

For each stack, the test builds two merged views and compares them:
- **A:** containerd's overlayfs-snapshotter view. Each layer tar is applied with `archive.Apply` into its own directory, with `OverlayConvertWhiteout` and `WithParents`, and the directories are mounted with overlayfs.
- **B:** kiln's view. Each kiln erofs layer is loop-mounted, and the mounts are combined with overlayfs using kiln's mount options.

A single-layer stack compares the layer directory against the erofs mount directly, overlay markers included.

**What is compared.** Directory and whiteout mtimes are excluded, because containerd creates parents and whiteout devices at extraction time. Everything else must match: types, modes, uid and gid, file mtimes, xattrs (excluding `trusted.overlay.*` in merged views), content, symlink targets, device numbers and hardlink groups.

**Sampled stacks.** Besides the handcrafted stacks, the test runs 200 model-valid sampled stacks. It skips two kinds of stack where containerd's applier itself is quirky; neither occurs in layers that image builders produce, and both divergences are documented in `docs/format.md`:
- **Deferred directory times** (`model::hits_deferred_dir_times`): containerd sets directory mtimes in a final pass. That pass fails, or re-times the replacement, when a later entry in the same layer replaced the directory or one of its parents with a non-directory.
- **Ambiguous inheritance** (`model::ambiguous_inheritance`): containerd resolves an implicit parent by searching each lower layer on its own. It skips non-directories, ignores whiteouts and opaque directories, and errors on a file in the middle of the path. kiln resolves through the overlay view. The two differ only when a layer omits a parent whose type changed below it.

About 60% of valid sampled stacks are comparable, averaging 1.9 layers.

**Validation history.** While this plan was written, this test found four real semantic differences, all now folded into Task 5:
- symlink modes are always 0777;
- hardlink headers' metadata applies to the shared inode;
- a whiteout naming an entry already in its layer is an error;
- a described implicit directory keeps its inherited xattrs.

**Container note.** Run it on a host whose `/tmp` is not overlayfs. Inside Docker, pass `--tmpfs /tmp:exec`, because containerd cannot create whiteout devices on overlayfs. GitHub runners use ext4.

- [ ] **Step 1: Create the Go helper**

Write `tools/oracle/main.go` first (below), then resolve modules:
```bash
cd tools/oracle
go mod init kiln.dev/tools/oracle
go mod tidy
go doc github.com/containerd/containerd/v2/pkg/archive WithParents
go doc github.com/containerd/containerd/v2/pkg/archive OverlayConvertWhiteout
```
Expected: `go mod tidy` resolves containerd v2 (v2.4.1 at the time of writing). Both `go doc` calls print a declaration; for example, `func OverlayConvertWhiteout(hdr *tar.Header, path string) (bool, error)`.

`tools/oracle/main.go`:
```go
// Command oracle applies one OCI layer tar into a directory exactly as
// containerd's overlayfs snapshotter does. Used by kiln's oracle tests.
//
// Usage: oracle <layer-dir> <layer.tar> [<parent-dir> ...]  (parents nearest first)
package main

import (
	"context"
	"fmt"
	"os"

	"github.com/containerd/containerd/v2/pkg/archive"
)

func main() {
	if len(os.Args) < 3 {
		fmt.Fprintln(os.Stderr, "usage: oracle <layer-dir> <layer.tar> [<parent-dir> ...]")
		os.Exit(2)
	}
	f, err := os.Open(os.Args[2])
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	defer f.Close()
	if err := os.MkdirAll(os.Args[1], 0o755); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	_, err = archive.Apply(context.Background(), os.Args[1], f,
		archive.WithConvertWhiteout(archive.OverlayConvertWhiteout),
		archive.WithParents(os.Args[3:]))
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}
```
Run: `go build -o ../../target/oracle . && cd ../..`
Expected: `target/oracle` exists.

- [ ] **Step 2: Write the oracle test**

`crates/kiln-erofs/tests/oracle.rs`:
```rust
#![cfg(target_os = "linux")]

mod common;
mod model;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::{groups, is_root, try_convert_stack, view, walk_fs, Mount};
use kiln_erofs::testtar::{Opts, TarBuilder};
use proptest::strategy::{Strategy, ValueTree};
use proptest::test_runner::TestRunner;

fn oracle_bin() -> Option<PathBuf> {
    std::env::var_os("KILN_ORACLE").map(PathBuf::from)
}

fn handcrafted() -> Vec<Vec<Vec<u8>>> {
    let o = Opts::default;
    vec![
        vec![TarBuilder::new()
            .dir("etc", &o().mode(0o755))
            .file("etc/f", b"f", &o())
            .symlink("etc/l", "f", &o())
            .hardlink("etc/h", "etc/f")
            .whiteout("gone")
            .dir("op", &o())
            .opaque("op")
            .finish()],
        vec![
            TarBuilder::new().dir("tmp", &o().mode(0o1777).uid(5).xattr("user.k", b"v")).finish(),
            TarBuilder::new().file("tmp/cache/x", b"x", &o()).finish(),
        ],
        vec![
            TarBuilder::new().dir("d", &o()).file("d/a", b"a", &o()).file("d/b", b"b", &o()).finish(),
            TarBuilder::new().whiteout("d/a").file("d", b"now a file", &o()).finish(),
            TarBuilder::new().dir("d", &o().mode(0o700)).file("d/c", b"c", &o()).finish(),
        ],
        vec![
            TarBuilder::new().file("a", b"old", &o()).hardlink("b", "a").finish(),
            TarBuilder::new().file("a", b"new", &o()).dir("x", &o().xattr("user.a", b"1")).finish(),
            TarBuilder::new().dir("x", &o().xattr("user.b", b"2")).opaque("").file("y", b"y", &o()).finish(),
        ],
    ]
}

fn sampled(n: usize) -> Vec<Vec<Vec<u8>>> {
    let mut runner = TestRunner::deterministic();
    let strategy = model::layers_strategy();
    let mut out = Vec::new();
    while out.len() < n {
        let layers = strategy.new_tree(&mut runner).unwrap().current();
        let comparable = !model::hits_deferred_dir_times(&layers) && !model::ambiguous_inheritance(&layers);
        if model::model_final(&layers).is_some() && comparable {
            out.push(layers.iter().map(|ops| model::to_tar(ops)).collect());
        }
    }
    out
}

fn compare_stack(bin: &Path, tars: &[Vec<u8>], case: usize) {
    let Ok(images) = try_convert_stack(tars) else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let mut c_dirs = Vec::new();
    let mut k_dirs = Vec::new();
    let mut mounts = Vec::new();
    for (i, (tar, img)) in tars.iter().zip(&images).enumerate() {
        let tar_path = dir.path().join(format!("l{i}.tar"));
        std::fs::write(&tar_path, tar).unwrap();
        let c_dir = dir.path().join(format!("c{i}"));
        let mut cmd = Command::new(bin);
        cmd.arg(&c_dir).arg(&tar_path);
        for parent in c_dirs.iter().rev() {
            cmd.arg(parent);
        }
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "case {case}: containerd rejected layer {i} that kiln accepted: {}", String::from_utf8_lossy(&out.stderr));
        c_dirs.push(c_dir);
        let img_path = dir.path().join(format!("l{i}.erofs"));
        std::fs::write(&img_path, img).unwrap();
        let k_dir = dir.path().join(format!("k{i}"));
        mounts.push(Mount::erofs(&img_path, &k_dir));
        k_dirs.push(k_dir);
    }
    let (a, b) = if tars.len() == 1 {
        (walk_fs(&c_dirs[0]), walk_fs(&k_dirs[0]))
    } else {
        let c_lowers: Vec<&Path> = c_dirs.iter().rev().map(PathBuf::as_path).collect();
        let k_lowers: Vec<&Path> = k_dirs.iter().rev().map(PathBuf::as_path).collect();
        let c_merged = dir.path().join("c-merged");
        let k_merged = dir.path().join("k-merged");
        mounts.push(Mount::overlay(&c_lowers, &c_merged));
        mounts.push(Mount::overlay(&k_lowers, &k_merged));
        (walk_fs(&c_merged), walk_fs(&k_merged))
    };
    if tars.len() == 1 {
        // containerd creates whiteout devices and parent directories "now", so their
        // mtimes are not comparable; everything else is.
        let keep_markers = |m: &std::collections::BTreeMap<Vec<u8>, common::Seen>| {
            m.iter()
                .map(|(p, s)| {
                    let whiteout = s.kind == 'c' && s.rdev == (0, 0);
                    let mtime = if s.kind == 'd' || whiteout { None } else { Some(s.mtime) };
                    (p.clone(), (s.kind, s.mode, s.uid, s.gid, mtime, s.xattrs.clone(), s.data.clone(), s.rdev))
                })
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        assert_eq!(keep_markers(&a), keep_markers(&b), "case {case}: single layer differs from containerd");
    } else {
        assert_eq!(view(&a, true), view(&b, true), "case {case}: merged view differs from containerd");
    }
    assert_eq!(groups(&a), groups(&b), "case {case}: hardlink groups differ from containerd");
    while let Some(m) = mounts.pop() {
        drop(m);
    }
}

#[test]
fn kiln_matches_containerd() {
    let Some(bin) = oracle_bin() else {
        eprintln!("skipping: set KILN_ORACLE to the oracle binary");
        return;
    };
    if !is_root() {
        eprintln!("skipping: needs root");
        return;
    }
    let mut stacks = handcrafted();
    stacks.extend(sampled(200));
    for (case, tars) in stacks.iter().enumerate() {
        compare_stack(&bin, tars, case);
    }
}
```

Mounts are dropped in reverse order: the overlays first, then the erofs layers.

- [ ] **Step 3: Run it (Linux, as root)**

```bash
(cd tools/oracle && go build -o ../../target/oracle .)
KILN_ORACLE="$PWD/target/oracle" scripts/run-root-test.sh oracle
```
Expected: `kiln_matches_containerd ... ok`. If a case fails, the assertion names it.
1. Reproduce that case on its own.
2. Decide which behaviour matches containerd's overlay snapshotter, which is the reference.
3. Fix kiln, or fix the model if it encoded the wrong rule, so that `model_props` and this test agree.
4. Add the case to `handcrafted()`.

- [ ] **Step 4: Add the CI job**

Append to the `linux-kernel` job in `.github/workflows/ci.yml`:
```yaml
      - uses: actions/setup-go@v5
        with:
          go-version: stable
          cache-dependency-path: tools/oracle/go.sum
      - run: (cd tools/oracle && go build -o "$GITHUB_WORKSPACE/target/oracle" .)
      - run: KILN_ORACLE="$GITHUB_WORKSPACE/target/oracle" scripts/run-root-test.sh oracle
```

- [ ] **Step 5: Commit**

```bash
git add tools/oracle crates/kiln-erofs/tests/oracle.rs .github/workflows/ci.yml
git commit -m "test(erofs): containerd overlay-snapshotter oracle on Linux CI

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P7rdQAESQYNEo2qFobUNxt"
```

---

## Spec coverage (M1a)

| Spec item | Task |
|---|---|
| §7.1 Layout (4096-byte blocks, single pass, `meta_blkaddr`, nid 0 reserved, padding, sorted directories) | 2, 7, 8, 12 |
| §7.2 Compact vs extended inodes | 7, 9 |
| §7.3 Determinism rules | 7, 8, 12 |
| §7.4 Layer semantics (merge, replace, hardlinks, whiteouts, entry types, headers, paths) | 3, 4, 5, 6, 11, 14 |
| §7.5 Reader | 9 |
| §7.6 Resource limits (per layer) | 1, 3, 6 |
| §6.3 Implicit parents | 5, 10, 11, 14 |
| §6.4 Squash (erofs in, erofs out, markers removed) | 10, 11, 13 |
| §6.6 Determinism | 8, 12 |
| §11.1 Unit, property, golden, `fsck.erofs` and loop-mount tests | 2–13 |
| §11.2 Oracle | 14 |
| T3 (resource bounds) | 6 |

The rest of §6, §11.5 and §12 M1 belong to plan **M1b**: the store, registry, OCI inputs, verified fetch, the per-image and ratio limits, the CLI and the hostile-registry tests.
