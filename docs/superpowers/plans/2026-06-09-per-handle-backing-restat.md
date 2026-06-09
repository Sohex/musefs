# Per-handle backing re-stat (issue #186) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the per-handle read fast path re-stat the held backing fd on every read so an in-place backing rewrite under a live handle is detected (`BackingChanged`) instead of silently splicing stale-offset bytes.

**Architecture:** Add one `validate_opened_backing(&h.file, r)?` call in `Musefs::read_into`'s per-handle fast path, after the resolved layout is loaded and before the DB-read serve block, on each retry iteration. The error propagates immediately (a genuine out-of-band rewrite is terminal — not a DB-retag race the loop retries). This reuses the exact validation `open_handle` already performs and makes the hot path consistent with `resolve()`, which already stats on every call.

**Tech Stack:** Rust (edition 2024), `std::os::unix` file metadata, `std::fs::File::set_times` / `std::fs::FileTimes` for deterministic test mtime control, `tempfile`, the in-crate `musefs-db` + `scan_directory` test harness.

**Spec:** `docs/superpowers/specs/2026-06-09-per-handle-backing-restat-design.md`

---

## Background the implementer needs

The fix and tests live in two files:

- **`musefs-core/src/facade.rs`** — the one-line fix in `read_into`, and the
  primitive `validate_opened_backing`.
- **`musefs-core/tests/facade.rs`** — an *integration* test crate (external to
  `musefs-core`). It already imports everything needed:
  `use musefs_core::{CoreError, MountConfig, Musefs, VirtualTree, scan_directory};`
  and has helpers `config()`, `scanned_db(dir)`, `make_flac`, `streaminfo_body`,
  `vorbis_comment_body`.

Key facts (verified against the current tree):

- `read_into` fast path: `musefs-core/src/facade.rs:949-1037`. The insertion
  point is between these two existing lines (currently around line 982-984):

  ```rust
  let resolved = h.resolved.load();
  let r: &ResolvedFile = &resolved;
  let served = self.pool.with(|db| -> Result<Option<()>> {
  ```

- `validate_opened_backing(file, resolved) -> Result<()>`
  (`musefs-core/src/facade.rs:110`) fstat's the fd and returns
  `Err(CoreError::BackingChanged(path))` when `meta.len() != resolved.backing_size`
  **or** `mtime_secs(&meta) != resolved.backing_mtime_secs`. It is `Ok(())`
  otherwise.

- `mtime_secs` is **whole seconds** (`facade.rs:103-108`). The scanner stamps
  `backing_mtime` from the file's real mtime at scan time. In a test,
  write→scan→read→rewrite all happen in one wall-clock second, so a same-length
  rewrite leaves `mtime_secs` unchanged unless the test **explicitly** sets a
  distinct mtime. This is correctness-load-bearing, not flake avoidance.

- `scanned_db` creates `a.flac` (a real FLAC, backing audio = `[0xAB; 64]`)
  under `dir`, scans it, returns an in-memory `Db`. `config()` uses template
  `$artist/$title`, tags `ARTIST=Alice` / `TITLE=Song`.

- `Musefs::read(inode, fh, offset, size) -> Result<Vec<u8>>` is the allocating
  wrapper over `read_into`; tests call it.

**Commit discipline (critical):** the pre-commit hook runs the **full workspace
test suite** and rejects any commit with red tests. A red-test-only commit is
impossible here. Therefore the failing tests and the fix must land in **one
commit**: write tests (red, uncommitted) → confirm red by running them directly
→ implement the fix → confirm green → update docs → commit everything together.

---

## File Structure

- `musefs-core/src/facade.rs` — modify `read_into` (one added line). No new
  symbols, no new error variant.
- `musefs-core/tests/facade.rs` — convert one existing test; add three tests.
- `ARCHITECTURE.md` — one-line freshness note.

---

## Task 1: Tests + fix for per-handle backing re-stat (single commit)

**Files:**
- Modify: `musefs-core/tests/facade.rs` (convert `read_uses_cached_handle_after_backing_grows`; add three test fns)
- Modify: `musefs-core/src/facade.rs:982-984` (`read_into` fast path)
- Modify: `ARCHITECTURE.md` (freshness note)

---

- [ ] **Step 1: Convert the bug-encoding test into the rewrite-longer regression test**

In `musefs-core/tests/facade.rs`, replace the existing function
`read_uses_cached_handle_after_backing_grows` (currently at lines 1041-1061,
which asserts the grown-backing read *succeeds*) entirely with the version
below. The append grows the file → size drift → `BackingChanged`.

```rust
#[test]
fn read_through_handle_errors_after_backing_grows_in_place() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let db = scanned_db(dir.path());
    let fs = Musefs::open(db, config()).unwrap();
    let artist = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let (_, file_inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();
    let size = fs.getattr(file_inode).unwrap().size;

    let fh = fs.open_handle(file_inode).unwrap();
    // Warm the per-handle fast path.
    let warm = fs.read(file_inode, Some(fh), 0, size).unwrap();
    assert_eq!(warm.len() as u64, size);

    // In-place grow (same inode), no DB change.
    {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.path().join("a.flac"))
            .unwrap();
        f.write_all(&[0u8; 64]).unwrap();
    }

    // Size drift must be detected on the held fd, not served silently.
    let err = fs.read(file_inode, Some(fh), 0, size).unwrap_err();
    assert!(matches!(err, CoreError::BackingChanged(_)), "got {err:?}");
}
```

- [ ] **Step 2: Add the truncate-shorter test**

Append this new function to `musefs-core/tests/facade.rs` (after the function
from Step 1). A shorter file is size drift, caught before any positioned read —
this confirms the normalization to `BackingChanged` (not a raw io error from
reading past EOF).

```rust
#[test]
fn read_through_handle_errors_after_backing_truncated_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let db = scanned_db(dir.path());
    let fs = Musefs::open(db, config()).unwrap();
    let artist = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let (_, file_inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();
    let size = fs.getattr(file_inode).unwrap().size;

    let fh = fs.open_handle(file_inode).unwrap();
    fs.read(file_inode, Some(fh), 0, size).unwrap(); // warm

    // Truncate in place (same inode): std::fs::write create+truncates the path.
    std::fs::write(dir.path().join("a.flac"), [0xCDu8; 8]).unwrap();

    let err = fs.read(file_inode, Some(fh), 0, size).unwrap_err();
    assert!(matches!(err, CoreError::BackingChanged(_)), "got {err:?}");
}
```

- [ ] **Step 3: Add the same-length-rewrite test (the core gap)**

Append this new function. This is the only variant exercising the mtime branch
and the issue's silent-corruption-at-unchanged-length mode. It **must** set a
distinct mtime, or `mtime_secs` stays equal to the scan-time value and the
re-stat returns `Ok` — passing while detecting nothing.

```rust
#[test]
fn read_through_handle_errors_after_same_length_rewrite_with_new_mtime() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.flac");
    let db = scanned_db(dir.path());
    let fs = Musefs::open(db, config()).unwrap();
    let artist = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let (_, file_inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();
    let size = fs.getattr(file_inode).unwrap().size;

    let fh = fs.open_handle(file_inode).unwrap();
    fs.read(file_inode, Some(fh), 0, size).unwrap(); // warm

    // Same-length in-place rewrite (same inode), then a distinct mtime second.
    let original_len = std::fs::metadata(&path).unwrap().len() as usize;
    std::fs::write(&path, vec![0xEEu8; original_len]).unwrap();
    // mtime_secs is whole-second; the rewrite above lands in the scan's second,
    // so set a deterministic, distinct timestamp. (Year ~2001 — well clear of
    // the scan second regardless of wall clock.)
    let distinct = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
    let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    f.set_times(std::fs::FileTimes::new().set_modified(distinct)).unwrap();

    let err = fs.read(file_inode, Some(fh), 0, size).unwrap_err();
    assert!(matches!(err, CoreError::BackingChanged(_)), "got {err:?}");
}
```

- [ ] **Step 4: Add the positive-guard test**

Append this new function. With no rewrite and no DB change, repeated reads on
the same handle must still succeed — guards against the new `?` becoming
over-eager.

```rust
#[test]
fn read_through_handle_keeps_succeeding_when_backing_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let db = scanned_db(dir.path());
    let fs = Musefs::open(db, config()).unwrap();
    let artist = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let (_, file_inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();
    let size = fs.getattr(file_inode).unwrap().size;

    let fh = fs.open_handle(file_inode).unwrap();
    let first = fs.read(file_inode, Some(fh), 0, size).unwrap();
    let second = fs.read(file_inode, Some(fh), 0, size).unwrap();
    assert_eq!(first, second);
    assert_eq!(first.len() as u64, size);
}
```

- [ ] **Step 5: Run the new/changed tests to confirm they fail (red)**

Run:
```bash
cargo test -p musefs-core --test facade read_through_handle 2>&1 | tail -30
```
Expected: the three new error-expecting tests FAIL because the current fast
path serves successfully (so `unwrap_err()` panics on an `Ok`), or — for the
truncate case — surfaces a non-`BackingChanged` io error. The positive-guard
test (`..._keeps_succeeding_when_backing_unchanged`) PASSES already. Confirm the
three `errors_after_*` tests are red before implementing the fix.

- [ ] **Step 6: Implement the fix in `read_into`**

In `musefs-core/src/facade.rs`, in the per-handle fast path of `read_into`,
insert the re-stat between the resolved-layout load and the serve block. The
surrounding lines are currently:

```rust
                    let resolved = h.resolved.load();
                    let r: &ResolvedFile = &resolved;
                    let served = self.pool.with(|db| -> Result<Option<()>> {
```

Change to (add the one `validate_opened_backing` line):

```rust
                    let resolved = h.resolved.load();
                    let r: &ResolvedFile = &resolved;
                    // Re-stat the held fd every read: a pure in-place backing
                    // rewrite (same inode) leaves both DB-side staleness signals
                    // unchanged, so this is the only check that catches it. A
                    // genuine drift is terminal — propagate, don't retry the loop.
                    validate_opened_backing(&h.file, r)?;
                    let served = self.pool.with(|db| -> Result<Option<()>> {
```

Use the Serena editing tools for this change (it edits inside the `read_into`
method body — `replace_content` with the three-line anchor above is the precise
way).

- [ ] **Step 7: Run the facade tests to confirm green**

Run:
```bash
cargo test -p musefs-core --test facade read_through_handle 2>&1 | tail -20
```
Expected: all four `read_through_handle_*` tests PASS.

- [ ] **Step 8: Run the full musefs-core test crate**

Run:
```bash
cargo test -p musefs-core 2>&1 | tail -20
```
Expected: all pass. (Confirms no other test depended on the old
serve-after-grow behavior. The only test asserting it was the one converted in
Step 1.)

- [ ] **Step 9: Add the ARCHITECTURE.md freshness note**

Open `ARCHITECTURE.md` around lines 178-181 (the freshness paragraph beginning
"every resolve re-stats the backing file..."). Add a sentence making explicit
that the per-handle hot path honors the same guarantee. Use Read to find the
exact surrounding text, then append a sentence such as:

> The per-handle read path also re-stats the held descriptor on every read, so
> this guarantee holds on the hot path and not only through `resolve()`.

Keep it to one sentence; match the surrounding prose style.

- [ ] **Step 10: Pre-commit dry run (fmt + clippy + full suite)**

Run:
```bash
cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo test --workspace 2>&1 | tail -15
```
Expected: fmt clean, clippy clean (`-D warnings`), all tests pass. Fix any
issue before committing — the pre-commit hook runs the same gates and will
reject a red commit.

- [ ] **Step 11: Commit (tests + fix + doc together)**

```bash
git add musefs-core/src/facade.rs musefs-core/tests/facade.rs ARCHITECTURE.md
git commit -m "$(cat <<'EOF'
fix(core): re-stat held backing fd on per-handle read path (#186)

The per-handle read fast path only re-resolved on DB-side signals
(refresh_gen / content_version), so an in-place backing rewrite under a
live fd (same inode: rsync --inplace, reflink swap, in-place re-encode)
spliced new bytes at the cached layout's audio offsets with no error —
silent corruption violating the cardinal invariant and the ARCHITECTURE.md
freshness guarantee.

Call the existing validate_opened_backing on the fast path before serving,
on each retry iteration, propagating BackingChanged immediately rather than
through the stale-layout retry loop. This makes the hot path consistent with
resolve(), which already stats on every call. The fstat is on an already-open
fd (~microseconds), negligible next to the pread and FUSE round-trip.

Convert read_uses_cached_handle_after_backing_grows (which asserted the old
serve-after-grow behavior) into a BackingChanged regression test, and add
truncate-shorter, same-length-with-new-mtime, and unchanged-backing
positive-guard tests through the per-handle read path.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Out of scope (do not do)

- No change to `read_segments_into`, `read_at_with_file_into`, `resolve`, or the
  fallback `read_at_into` path.
- No mtime-granularity change (no scanner/schema change). The same-second miss
  and the stat→pread TOCTOU are documented inherent residuals.
- No new `CoreError` variant — reuse `BackingChanged`.
- No `filetime` dev-dependency — `std::fs::File::set_times` is stable on this
  edition-2024 toolchain.

---

## Self-review notes (already reconciled with the spec)

- **Spec coverage:** fix (Step 6) ✓; rewrite-longer (Step 1) ✓; truncate-shorter
  (Step 2) ✓; same-length+mtime (Step 3) ✓; positive guard (Step 4) ✓;
  ARCHITECTURE.md note (Step 9) ✓.
- **Single-commit discipline** is required by the pre-commit full-suite gate —
  tests and fix cannot be split across commits.
- **Type/name consistency:** `validate_opened_backing(&std::fs::File, &ResolvedFile) -> Result<()>`,
  `CoreError::BackingChanged(String)`, `Musefs::read(...) -> Result<Vec<u8>>` —
  all match the current tree.
