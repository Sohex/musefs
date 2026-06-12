# Serve-time Front-Read Cap Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Cap serve-time front/header reads at the scanner's `MAX_PROBE_BYTES` (64 MiB) so a hostile `tracks.audio_offset` cannot force an unbounded allocation in `read_front`.

**Architecture:** Promote the scanner's existing `MAX_PROBE_BYTES` constant to crate-shared, enforce it inside `read_front` (the single serve-time allocator keyed on `audio_offset`) before any file open or allocation, surface a new `CoreError::HeaderTooLarge` that maps to `EIO`, and lock the behaviour with one direct unit test plus one end-to-end serve test per vulnerable format path (WAV, Ogg, FLAC legacy fallback).

**Tech Stack:** Rust workspace (`musefs-db` → `musefs-format` → `musefs-core` → `musefs-fuse`), `thiserror` errors, `cargo test`, `tempfile` for fixtures.

**Spec:** `docs/superpowers/specs/2026-06-12-serve-front-read-cap-design.md`

---

## Background the implementer needs

- `read_front` (`musefs-core/src/reader.rs:80`) currently allocates `vec![0u8; usize_from(n)]` with `n = track.bounds.audio_offset()`. Three call sites pass a DB-controlled offset: FLAC legacy fallback (`reader.rs:183`, taken only when `db.get_structural_blocks(track.id)` is empty), WAV (`reader.rs:253`), and Ogg / Opus / Vorbis / OggFlac (`reader.rs:268`).
- Two pre-existing guards run **before** `read_front`, so a test fixture must satisfy both to reach it:
  - `resolve` (`reader.rs:119`) rejects unless `meta.len() == track.backing_size`.
  - `build` (`reader.rs:156-163`) rejects unless `audio_offset + audio_length <= meta.len()` (else `BackingChanged`).
  - The schema (`musefs-db/src/schema.rs`) also enforces `audio_offset + audio_length <= backing_size`.
  - **Consequence for fixtures:** with `audio_offset = CAP + 1` and `audio_length = 1`, the backing file must be exactly `CAP + 2` bytes and `backing_size` must equal that. Use a sparse file (`File::set_len`) so no real 64 MiB is written; `tempfile::tempdir()` lives on `/tmp` (tmpfs/RAM on this host).
- `errno()` (`musefs-fuse/src/lib.rs:94`) matches **exhaustively** on `CoreError`. Adding a variant without adding its match arm breaks `musefs-fuse` compilation, and an unconstructed variant trips the `-D warnings` clippy gate. Therefore the variant, its construction in `read_front`, and the `errno` arm must all land in **one commit** (Task 1).
- The pre-commit hook runs the **full workspace test suite** and `clippy -D warnings`; every commit must be green. No schema change here, so no Python schema-mirror regen.
- `MAX_PROBE_BYTES = 64 << 20 = 67_108_864`. Throughout the tests, `CAP + 1 = 67_108_865` and `CAP + 2 = 67_108_866`.

---

## Task 1: Cap mechanism + error variant + errno mapping

Establishes the whole enforcement mechanism in one green commit: the shared constant, the new error, the cap check in `read_front`, the FUSE errno mapping, and the two unit tests (direct `read_front`, errno mapping).

**Files:**
- Modify: `musefs-core/src/scan.rs:25` (make `MAX_PROBE_BYTES` `pub(crate)`)
- Modify: `musefs-core/src/error.rs:37-43` (add `HeaderTooLarge` after `ArtTooLarge`)
- Modify: `musefs-core/src/reader.rs:80-86` (`read_front` signature + cap check)
- Modify: `musefs-fuse/src/lib.rs:101-108` (errno EIO arm) and `:596-605` (errno test)
- Test: new `mod serve_cap_tests` at the end of `musefs-core/src/reader.rs`

- [ ] **Step 1: Write the failing direct unit test**

Append a new module at the very end of `musefs-core/src/reader.rs` (after the existing `mod binary_tag_serve_tests { ... }`):

```rust
#[cfg(test)]
mod serve_cap_tests {
    use super::*;

    const CAP: u64 = crate::scan::MAX_PROBE_BYTES;

    #[test]
    fn read_front_rejects_oversize_before_open() {
        // Nonexistent path: if the cap check did NOT fire first, File::open would
        // error and we'd get an Io error instead of HeaderTooLarge. So this also
        // pins the fail-closed ordering (check precedes any open/allocation).
        let err = read_front(std::path::Path::new("/nonexistent/musefs/front"), CAP + 1)
            .unwrap_err();
        match err {
            CoreError::HeaderTooLarge { requested, cap } => {
                assert_eq!(requested, CAP + 1);
                assert_eq!(cap, CAP);
            }
            other => panic!("expected HeaderTooLarge, got {other:?}"),
        }
    }
}
```

Also append the new errno assertion inside the existing `maps_core_errors_to_errno` test in `musefs-fuse/src/lib.rs`, immediately after the `ArtTooLarge` assertion block (ends at `:605`):

```rust
        assert_eq!(
            errno(&CoreError::HeaderTooLarge {
                requested: 67_108_865,
                cap: 67_108_864,
            })
            .code(),
            libc::EIO
        );
```

- [ ] **Step 2: Run the tests to verify they fail (compile error)**

Run: `cargo test -p musefs-core serve_cap_tests`
Expected: FAIL — compile error, `no variant named HeaderTooLarge found for enum CoreError` (and `read_front` returns `std::io::Result`, so the `CoreError::HeaderTooLarge` match arm is unreachable type-wise).

- [ ] **Step 3: Make `MAX_PROBE_BYTES` crate-visible**

In `musefs-core/src/scan.rs:25`, change:

```rust
const MAX_PROBE_BYTES: u64 = 64 << 20; // 64 MiB
```

to:

```rust
pub(crate) const MAX_PROBE_BYTES: u64 = 64 << 20; // 64 MiB
```

- [ ] **Step 4: Add the `HeaderTooLarge` error variant**

In `musefs-core/src/error.rs`, insert immediately after the `ArtTooLarge { ... }` variant (after line 43):

```rust
    #[error("front/header read of {requested} bytes exceeds the {cap}-byte serve cap")]
    HeaderTooLarge { requested: u64, cap: u64 },
```

- [ ] **Step 5: Enforce the cap in `read_front`**

In `musefs-core/src/reader.rs`, replace the whole `read_front` function (lines 80-86):

```rust
fn read_front(path: &Path, n: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    crate::metrics::on_open();
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; usize_from(n)];
    f.read_exact(&mut buf)?;
    Ok(buf)
}
```

with:

```rust
fn read_front(path: &Path, n: u64) -> crate::Result<Vec<u8>> {
    use std::io::Read;
    // Fail closed before any allocation/open: a hostile DB row can request an
    // arbitrary `audio_offset`, but no legitimately-scanned file has a front
    // larger than the scanner's probe ceiling. Bounding `n` here also retires a
    // 32-bit `usize_from` truncation footgun.
    if n > crate::scan::MAX_PROBE_BYTES {
        return Err(CoreError::HeaderTooLarge {
            requested: n,
            cap: crate::scan::MAX_PROBE_BYTES,
        });
    }
    crate::metrics::on_open();
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; usize_from(n)];
    f.read_exact(&mut buf)?;
    Ok(buf)
}
```

The three call sites already use `?` inside a `Result<_, CoreError>` function, so the inner `std::io::Error` still converts via the existing `#[from]`. No call-site edits are needed.

- [ ] **Step 6: Add the errno EIO arm**

In `musefs-fuse/src/lib.rs`, in the `errno` function's EIO group (lines 101-108), add the new variant before `CoreError::Format(_)`:

```rust
        CoreError::BackingChanged(_)
        | CoreError::Db(_)
        | CoreError::DbOpen { .. }
        | CoreError::Mp4MetadataTooLarge { .. }
        | CoreError::OrphanedArt { .. }
        | CoreError::ArtTooLarge { .. }
        | CoreError::InvalidPictureType { .. }
        | CoreError::HeaderTooLarge { .. }
        | CoreError::Format(_) => fuser::Errno::EIO,
```

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test -p musefs-core serve_cap_tests && cargo test -p musefs-fuse maps_core_errors_to_errno`
Expected: PASS (both tests).

- [ ] **Step 8: Verify the full gate is green (clippy + workspace + metrics feature)**

Run:
```bash
cargo clippy --all-targets -- -D warnings \
  && cargo test --workspace \
  && cargo test -p musefs-core --features metrics
```
Expected: all PASS. (The `metrics` feature run guards the read/open-path counter behaviour per the project's CI `check` job; the cap check sits before `on_open()`, so capped reads don't increment the open counter and no existing metrics assertion changes.)

- [ ] **Step 9: Commit**

```bash
git add musefs-core/src/scan.rs musefs-core/src/error.rs musefs-core/src/reader.rs musefs-fuse/src/lib.rs
git commit -m "$(cat <<'EOF'
fix(core): cap serve-time front reads at MAX_PROBE_BYTES (#265)

read_front allocated vec![0u8; audio_offset] before reading, so a hostile
tracks row with a huge audio_offset and a matching sparse backing file could
force an unbounded allocation via getattr/open/read. Enforce the scanner's
MAX_PROBE_BYTES ceiling inside read_front before any open/allocation, failing
closed with CoreError::HeaderTooLarge (mapped to EIO). MAX_PROBE_BYTES is now
pub(crate) so serve and scan share one source of truth.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: End-to-end serve test — WAV path

A regression test proving the cap fires through the real `resolve` → `build` path for WAV, past both pre-existing guards. The mechanism already exists from Task 1, so this test passes on first run; its job is to lock the WAV serve entry point against regressions. It also exercises the test helpers (`sparse_file`, `hostile_track`) reused by Tasks 3-4.

**Files:**
- Test: `musefs-core/src/reader.rs` — add helpers + test to `mod serve_cap_tests`

- [ ] **Step 1: Add the shared fixture helpers**

In `musefs-core/src/reader.rs`, inside `mod serve_cap_tests`, first add the DB-type imports just under `use super::*;` (the e2e tests need them; Task 1 deliberately omitted them to avoid an unused-import error):

```rust
    use musefs_db::{Db, Format, NewTrack};
```

Then add these helpers (after the `const CAP` line, before the existing `read_front_rejects_oversize_before_open` test):

```rust
    /// A sparse backing file of `len` bytes (no real bytes written — `set_len`
    /// only extends the file's logical size, which tmpfs keeps sparse).
    fn sparse_file(dir: &std::path::Path, name: &str, len: u64) -> std::path::PathBuf {
        let path = dir.join(name);
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(len).unwrap();
        path
    }

    /// Insert a `tracks` row whose `audio_offset` exceeds the cap while still
    /// satisfying both serve guards (`backing_size == meta.len()` and
    /// `audio_offset + audio_length <= meta.len()`). Returns the track id.
    fn hostile_track<M>(db: &Db<M>, path: &std::path::Path, format: Format) -> i64 {
        let meta = std::fs::metadata(path).unwrap();
        db.upsert_track(&NewTrack {
            backing_path: path.to_string_lossy().into_owned(),
            format,
            audio_offset: CAP + 1,
            audio_length: 1,
            backing_size: meta.len(),
            backing_mtime: mtime_secs(&meta),
        })
        .unwrap()
    }

    /// Assert a resolve attempt fails closed with the cap error for `audio_offset`.
    fn assert_capped(result: crate::Result<std::sync::Arc<ResolvedFile>>) {
        match result {
            Err(CoreError::HeaderTooLarge { requested, cap }) => {
                assert_eq!(requested, CAP + 1);
                assert_eq!(cap, CAP);
            }
            Err(other) => panic!("expected HeaderTooLarge, got {other:?}"),
            Ok(_) => panic!("expected HeaderTooLarge, resolve unexpectedly succeeded"),
        }
    }
```

- [ ] **Step 2: Write the WAV serve test**

Add to the same module:

```rust
    #[test]
    fn wav_serve_caps_hostile_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = sparse_file(dir.path(), "hostile.wav", CAP + 2);
        let db = Db::open_in_memory().unwrap();
        let track_id = hostile_track(&db, &path, Format::Wav);

        let cache = HeaderCache::new(Mode::Synthesis);
        assert_capped(cache.resolve(&db, track_id));
    }
```

- [ ] **Step 3: Run the test**

Run: `cargo test -p musefs-core wav_serve_caps_hostile_offset`
Expected: PASS. (Sanity: if it fails with `BackingChanged`, the fixture sizing is wrong — confirm `CAP + 2` for both the file and `backing_size`. If it fails by succeeding/allocating, Task 1 was not applied.)

- [ ] **Step 4: Commit**

```bash
git add musefs-core/src/reader.rs
git commit -m "$(cat <<'EOF'
test(core): WAV serve path caps hostile audio_offset (#265)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: End-to-end serve test — Ogg path

Same regression lock for the Ogg formats (Opus / Vorbis / OggFlac), which share the `reader.rs:268` `read_front` call. One representative format (`Opus`) covers the shared code path.

**Files:**
- Test: `musefs-core/src/reader.rs` — add test to `mod serve_cap_tests`

- [ ] **Step 1: Write the Ogg serve test**

Add to `mod serve_cap_tests` (reusing the Task 2 helpers):

```rust
    #[test]
    fn ogg_serve_caps_hostile_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = sparse_file(dir.path(), "hostile.opus", CAP + 2);
        let db = Db::open_in_memory().unwrap();
        let track_id = hostile_track(&db, &path, Format::Opus);

        let cache = HeaderCache::new(Mode::Synthesis);
        assert_capped(cache.resolve(&db, track_id));
    }
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p musefs-core ogg_serve_caps_hostile_offset`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add musefs-core/src/reader.rs
git commit -m "$(cat <<'EOF'
test(core): Ogg serve path caps hostile audio_offset (#265)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: End-to-end serve test — FLAC legacy fallback path

The FLAC `read_front` (`reader.rs:183`) is reached only when `db.get_structural_blocks(track.id)` is empty (the legacy, pre-structural-store fallback). The fixture therefore inserts **no** structural-block rows. Because the cap fires before `flac::read_metadata`, the sparse/garbage front is never parsed — the test sees `HeaderTooLarge`, not a FLAC `Format` error.

**Files:**
- Test: `musefs-core/src/reader.rs` — add test to `mod serve_cap_tests`

- [ ] **Step 1: Write the FLAC legacy-fallback serve test**

Add to `mod serve_cap_tests`:

```rust
    #[test]
    fn flac_legacy_serve_caps_hostile_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = sparse_file(dir.path(), "hostile.flac", CAP + 2);
        let db = Db::open_in_memory().unwrap();
        // No structural-block rows inserted -> build() takes the legacy fallback
        // branch (rows.is_empty()) that calls read_front.
        let track_id = hostile_track(&db, &path, Format::Flac);
        assert!(db.get_structural_blocks(track_id).unwrap().is_empty());

        let cache = HeaderCache::new(Mode::Synthesis);
        assert_capped(cache.resolve(&db, track_id));
    }
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p musefs-core flac_legacy_serve_caps_hostile_offset`
Expected: PASS.

- [ ] **Step 3: Run the whole new module + full gate once more**

Run:
```bash
cargo test -p musefs-core serve_cap_tests \
  && cargo clippy --all-targets -- -D warnings \
  && cargo test --workspace
```
Expected: all PASS (4 tests in `serve_cap_tests`: the direct unit test plus WAV/Ogg/FLAC).

- [ ] **Step 4: Commit**

```bash
git add musefs-core/src/reader.rs
git commit -m "$(cat <<'EOF'
test(core): FLAC legacy fallback serve path caps hostile audio_offset (#265)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Self-review checklist (for the implementer before finishing)

- [ ] All four `serve_cap_tests` tests pass, plus `musefs-fuse` `maps_core_errors_to_errno`.
- [ ] `cargo clippy --all-targets -- -D warnings` is clean (no dead-code warning — `HeaderTooLarge` is both constructed in `read_front` and matched in `errno`).
- [ ] `cargo test --workspace` and `cargo test -p musefs-core --features metrics` both green.
- [ ] No `musefs-db` schema change was made (so no Python mirror regen needed).
- [ ] `pub(crate) const MAX_PROBE_BYTES` is referenced from both `scan.rs` and `reader.rs` — single source of truth.
