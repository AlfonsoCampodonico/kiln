# P2: The snapshot artifact in kiln — Implementation Plan

**Goal:** chunk a vmkit snapshot bundle into a content-addressed OCI artifact, push and pull it through `kiln-store` and `kiln-registry`, and document it, per the design's §5.
**Design:** `../specs/2026-10-21-lazy-restore-design.md` §5 (format, limits, threat model) and §6.2 (`lazy.json`).
**Repo:** kiln, branch `feature/kiln-lazy-restore-1b2b15`.
**Platform:** everything builds and tests on macOS; nothing here needs Linux or `vmkit`.
**Rules:** TDD, `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `#![forbid(unsafe_code)]`, and no attribution trailers. Match the surrounding crates' style: `thiserror` errors, typed limits, doc comments that say why.

---

### Task 1: Crate skeleton and format types

**Files:** `crates/kiln-snapshot/{Cargo.toml, src/lib.rs, src/error.rs, src/format.rs}`.

**Types**
- Constants:
  - `SNAPSHOT_ARTIFACT = "application/vnd.kiln.snapshot.v1"`
  - `SNAPSHOT_CONFIG = "application/vnd.kiln.snapshot.config.v1+json"`
  - `SNAPSHOT_CHUNK = "application/vnd.kiln.snapshot.chunk.v1"`
  - `SCHEMA_VERSION = 1`
- `SnapshotConfig { schema_version, chunk_size, bundle: serde_json::Value, files: Vec<ChunkedFile> }` and `ChunkedFile { name, size, chunks: Vec<Option<Digest>> }`.
  - camelCase serde, `deny_unknown_fields`, canonical sorted-key serialisation (reuse how `kiln-image` serialises).
- `Limits` with defaults:
  - `max_files` 64
  - `max_file_size` 1 TiB
  - `max_total_size` 4 TiB
  - chunk size 64 KiB..=16 MiB, power of two
  - config ≤ `MAX_METADATA_BLOB`
- `SnapshotConfig::check(&self, layers: &[Descriptor], limits)`. Names follow `[a-z0-9][a-z0-9._-]{0,63}`, excluding `bundle.json`/`lazy.json`, with no duplicates. It also checks:
  - `chunks.len() == ceil(size / chunk_size)`;
  - each non-null chunk is in `layers`, with size = covered length;
  - no unused layer, no duplicate layer;
  - `bundle.version == 1`;
  - `schemaVersion == 1`.

**Tests first:** one per rule (`a_chunk_missing_from_layers_is_refused`, `a_short_middle_chunk_is_refused`, `names_cannot_escape_the_bundle`, …) and `config_serialises_canonically`.

**Commit:** `feat(kiln-snapshot): the snapshot artifact format and its checks`

### Task 2: Chunking a bundle into the store

**Function:** `chunk_bundle(store, dir, chunk_size) -> Result<Built { manifest_digest, config, manifest }>`. It:
1. reads `bundle.json` (≤ 64 KiB, `O_NOFOLLOW`, regular), parsed as a JSON value with `version == 1`;
2. lists the other entries: regular files only, names valid; anything else (a subdirectory, a symlink) is an error;
3. sorts the files by name;
4. reads each file chunk by chunk:
   - an all-zero chunk becomes `null`;
   - any other chunk is stored with `store.put_bytes`;
5. writes the config and the manifest:
   - `artifactType` is set;
   - the layers are the distinct chunks in first-use order;
   - there are no annotations.

**Tests first**
- `zero_chunks_are_not_stored`
- `identical_chunks_are_stored_once`
- `the_same_bundle_gives_the_same_digest`
- `a_symlink_or_directory_in_the_bundle_is_refused`
- `the_last_chunk_may_be_short`

**Commit:** `feat(kiln-snapshot): chunk a bundle into the store`

### Task 3: Tagging, loading and assembling

**Functions**
- `tag(store, name, digest)`.
- `load(store, digest) -> Loaded { manifest, config }`. It verifies `artifactType`, the config media type and `check`.
- `assemble(store, loaded, dest)`. It writes every file with holes for null chunks (`set_len` and then seek-writes), then `bundle.json` from `config.bundle` (pretty, sorted). It refuses a non-empty `dest`.
- `write_lazy(loaded, dest)`. It writes `bundle.json` and `lazy.json` (design §6.2): `{"chunk_size", "files": {name: {"chunks", "size"}}, "version": 1}`.

**Tests first**
- `assemble_round_trips_a_bundle_byte_for_byte`
- `assemble_leaves_zero_chunks_as_holes`
- `lazy_json_lists_every_file`
- `load_refuses_an_image_manifest`

**Commit:** `feat(kiln-snapshot): load, assemble and the lazy index`

### Task 4: Push and pull (registry)

**Functions**
- `push(store, client, name_or_dir, reference)`. It works like `kiln_image::push_image`: push the blobs (skipping ones the registry has), then the config, then the manifest by tag.
- `pull(store, client, reference, tag) -> Loaded`. It runs `resolve_manifest` (verified) and requires the snapshot `artifactType`. Then it runs `fetch_blob` on the config and `check`s it. Only then does it tag.
- `fetch_chunks(store, client, repo, loaded)`. It fetches the missing chunks, verified, for an eager pull.

**Tests first.** Against `kiln_registry::testregistry`, the way the `kiln-image` registry tests do it:
- `push_then_pull_round_trips`
- `push_skips_chunks_the_registry_has`
- `a_tampered_chunk_is_refused_and_nothing_is_tagged`
- `a_config_over_the_limits_is_refused_before_any_chunk_is_fetched`

**Commit:** `feat(kiln-snapshot): push and pull through kiln-registry`

### Task 5: CLI

**Files:** `crates/kiln/src/snapshot_cmd.rs`, `crates/kiln/src/main.rs`.

**Commands**
- `kiln snapshot push <dir> <ref> [--chunk-size SIZE]`
- `kiln snapshot pull <ref> <dir> [--lazy]`
- `kiln snapshot inspect <ref|name>`

**Behaviour**
- `kiln ls` shows snapshot refs with type `snapshot`.
- `kiln run` and `kiln inspect` give a clear error for a snapshot ref. Check what `kiln_image::load` returns for a snapshot manifest and map it.
- Printed strings from the artifact are sanitised (T8; reuse the existing sanitiser).

**Tests:** `crates/kiln/tests/cli.rs` cases against the test registry.

**Commit:** `feat(kiln): snapshot push, pull and inspect`

### Task 6: `docs/format.md`

Add a section "kiln snapshot, schema version 1". It covers the manifest, config, chunks, zero chunks, limits, the reader's checks, `lazy.json`, and the vmkit bundle (`bundle.json`) as an informative summary pointing to vmkit. Add T10 to the threat-model summary if `format.md` has one.

**Commit:** `docs(format): the snapshot artifact`

**Done when:** `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test -p kiln-snapshot -p kiln` pass on macOS.
