# Phase 0 + Phase 1: Data Loss & Conventions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop the plugin data-loss regression (#82) and settle two foundational conventions — internal error diagnostics (#95) and mutex poison-recovery (#96) — before the later issues that inherit them.

**Architecture:** Three independent parts. Part A is a one-line SQL scope fix in two Python plugins. Part B enriches two Rust error types (`musefs-format` `LayoutError`/`FormatError`, `musefs-core` `tree.rs`). Part C implements "recover-by-reset" poison handling in `musefs-core` via a small `lock.rs` helper module with three categories (clear caches, flag-and-rebuild VFS state, recover scalars).

**Tech Stack:** Rust (workspace crates `musefs-format`, `musefs-core`; `thiserror`, `log`), Python 3 + pytest (beets/picard plugins, `sqlite3`).

**Source spec:** `docs/superpowers/specs/2026-06-03-phase0-1-data-loss-and-conventions-design.md`

**Conventions for every task below:**
- Work test-first (TDD): write the failing test, see it fail for the right reason, implement the minimum, see it pass, commit.
- Rust: run `cargo test -p <crate>` for the touched crate; before each commit run `cargo fmt --all` and `cargo clippy --all-targets -- -D warnings` (the pre-commit hook enforces fmt/clippy/test/ruff — a failed hook means the commit did NOT happen; fix and re-commit, never `--amend`, never `--no-verify`).
- Python: run `python -m pytest` from inside the plugin dir; `ruff check` + `ruff format --check` are enforced by the same hook.
- Stage files by name; never `git add -A`.

---

## Part A — #82: preserve scanner-written binary tags (Python plugins)

The two plugins have a byte-identical `replace_tags` that does an unscoped
`DELETE FROM tags WHERE track_id = ?`, destroying scanner-written binary rows
(`value_blob IS NOT NULL`). Scope the delete to the plugin-owned text rows
(`value_blob IS NULL`). Same change in both copies; each gets its own tests.

### Task A1: beets plugin — scope the delete + tests

**Files:**
- Modify: `contrib/beets/beetsplug/_core.py:171`
- Test: `contrib/beets/tests/test_db.py` (append)

- [ ] **Step 1: Write the failing test (binary row survives)**

Append to `contrib/beets/tests/test_db.py`:

```python
def test_replace_tags_preserves_binary_rows(db_path, make_track):
    # A scanner-written binary tag row (value_blob NOT NULL, value '') must
    # survive a plugin sync that replaces text tags. Regression test for #82.
    tid = make_track("/music/a.flac")
    conn = connect(db_path)
    try:
        conn.execute(
            "INSERT INTO tags (track_id, key, value, value_blob, ordinal) "
            "VALUES (?, 'APPLICATION', '', ?, 0)",
            (tid, b"\x00\x01\x02binary"),
        )
        # Seed a committed text row too, to prove text rows are still replaced.
        conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal) "
            "VALUES (?, 'title', 'Old', 0)",
            (tid,),
        )
        conn.commit()

        replace_tags(conn, tid, [("title", "New")])
        conn.commit()

        binary = conn.execute(
            "SELECT value, value_blob FROM tags "
            "WHERE track_id=? AND key='APPLICATION'",
            (tid,),
        ).fetchall()
        assert binary == [("", b"\x00\x01\x02binary")]

        titles = conn.execute(
            "SELECT value FROM tags WHERE track_id=? AND key='title'", (tid,)
        ).fetchall()
        assert titles == [("New",)]
    finally:
        conn.close()


def test_default_vocabulary_disjoint_from_binary_keys():
    # The scoped delete is collision-free for the default vocabulary because
    # DIRECT_FIELDS values never coincide with the scanner's binary tag keys
    # (uppercase ID3/FLAC keys, MP4 '----:...'). Documents the #82 assumption.
    from beetsplug._core import DIRECT_FIELDS

    binary_keys = {"APPLICATION", "CUESHEET", "PRIV", "GEOB", "APIC"}
    text_keys = set(DIRECT_FIELDS.values())
    assert text_keys.isdisjoint(binary_keys)
    assert not any(k.startswith("----") for k in text_keys)
```

- [ ] **Step 2: Run the tests to verify the first one fails**

Run: `cd contrib/beets && python -m pytest tests/test_db.py::test_replace_tags_preserves_binary_rows tests/test_db.py::test_default_vocabulary_disjoint_from_binary_keys -v`
Expected: `test_replace_tags_preserves_binary_rows` FAILS (the binary row was deleted, so `binary == []` ≠ expected); the disjointness test PASSES already.

- [ ] **Step 3: Scope the delete**

In `contrib/beets/beetsplug/_core.py`, in `replace_tags` (line 171), change:

```python
    conn.execute("DELETE FROM tags WHERE track_id = ?", (track_id,))
```

to:

```python
    # Scope to the plugin-owned text rows: scanner-written binary tags
    # (value_blob NOT NULL) must survive a sync (#82).
    conn.execute(
        "DELETE FROM tags WHERE track_id = ? AND value_blob IS NULL", (track_id,)
    )
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cd contrib/beets && python -m pytest tests/test_db.py -v`
Expected: all PASS (including the pre-existing `test_replace_tags_*`).

- [ ] **Step 5: Commit**

```bash
git add contrib/beets/beetsplug/_core.py contrib/beets/tests/test_db.py
git commit -m "fix(beets): preserve scanner-written binary tags on sync (#82)"
```

### Task A2: picard plugin — scope the delete + tests

**Files:**
- Modify: `contrib/picard/musefs/_core.py:201`
- Test: `contrib/picard/tests/test_core_db.py` (append)

- [ ] **Step 1: Write the failing test (binary row survives)**

The picard tests use the same `db_path`/`make_track` fixtures (`contrib/picard/tests/conftest.py`). Add `replace_tags` and `DIRECT_FIELDS` to the import block at the top of `contrib/picard/tests/test_core_db.py`:

```python
from musefs._core import (
    DIRECT_FIELDS,
    SchemaMismatch,
    check_schema_version,
    realpath_key,
    replace_tags,
    sniff_mime,
)
```

Then append:

```python
def test_replace_tags_preserves_binary_rows(db_path, make_track):
    # Regression test for #82: a plugin sync must not delete scanner-written
    # binary tags (value_blob NOT NULL).
    tid = make_track("/music/a.flac")
    conn = sqlite3.connect(db_path)
    try:
        conn.execute(
            "INSERT INTO tags (track_id, key, value, value_blob, ordinal) "
            "VALUES (?, 'APPLICATION', '', ?, 0)",
            (tid, b"\x00\x01\x02binary"),
        )
        conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal) "
            "VALUES (?, 'title', 'Old', 0)",
            (tid,),
        )
        conn.commit()

        replace_tags(conn, tid, [("title", "New")])
        conn.commit()

        binary = conn.execute(
            "SELECT value, value_blob FROM tags "
            "WHERE track_id=? AND key='APPLICATION'",
            (tid,),
        ).fetchall()
        assert binary == [("", b"\x00\x01\x02binary")]

        titles = conn.execute(
            "SELECT value FROM tags WHERE track_id=? AND key='title'", (tid,)
        ).fetchall()
        assert titles == [("New",)]
    finally:
        conn.close()


def test_default_vocabulary_disjoint_from_binary_keys():
    binary_keys = {"APPLICATION", "CUESHEET", "PRIV", "GEOB", "APIC"}
    text_keys = set(DIRECT_FIELDS.values())
    assert text_keys.isdisjoint(binary_keys)
    assert not any(k.startswith("----") for k in text_keys)
```

(`sqlite3` is already imported at the top of `test_core_db.py`.)

- [ ] **Step 2: Run the tests to verify the first one fails**

Run: `cd contrib/picard && python -m pytest tests/test_core_db.py::test_replace_tags_preserves_binary_rows tests/test_core_db.py::test_default_vocabulary_disjoint_from_binary_keys -v`
Expected: `test_replace_tags_preserves_binary_rows` FAILS; disjointness test PASSES.

- [ ] **Step 3: Scope the delete**

In `contrib/picard/musefs/_core.py`, in `replace_tags` (line 201), change:

```python
    conn.execute("DELETE FROM tags WHERE track_id = ?", (track_id,))
```

to:

```python
    # Scope to the plugin-owned text rows: scanner-written binary tags
    # (value_blob NOT NULL) must survive a sync (#82).
    conn.execute(
        "DELETE FROM tags WHERE track_id = ? AND value_blob IS NULL", (track_id,)
    )
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cd contrib/picard && python -m pytest tests/test_core_db.py -v`
Expected: all PASS.

- [ ] **Step 5: Commit**

```bash
git add contrib/picard/musefs/_core.py contrib/picard/tests/test_core_db.py
git commit -m "fix(picard): preserve scanner-written binary tags on sync (#82)"
```

---

## Part B — #95: internal error types carry diagnostics (Rust)

### Task B1: `musefs-format` — carry `LayoutError` through `FormatError`

`LayoutError` (`musefs-format/src/layout.rs`) currently derives only
`Debug, Clone, PartialEq, Eq` — no `Display`/`Error`, which `#[from]` and the
`{0}` format require. Make it a `thiserror` error, then make
`FormatError::InvalidLayout` carry it and add a `ProducerBug` variant for the one
manual guard (mp4.rs:822).

**Files:**
- Modify: `musefs-format/src/layout.rs:1-8`
- Modify: `musefs-format/src/error.rs:2-18`
- Modify (collapse to `?`): `musefs-format/src/flac.rs:303`, `mp3.rs:410`, `wav.rs:252`, `ogg/mod.rs:275`, `mp4.rs:835`
- Modify (manual guard → ProducerBug): `musefs-format/src/mp4.rs:822`
- Test: `musefs-format/src/error.rs` (inline `#[cfg(test)]`)

- [ ] **Step 1: Write the failing test**

Append a test module at the end of `musefs-format/src/error.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::FormatError;
    use crate::layout::LayoutError;

    #[test]
    fn invalid_layout_carries_inner_layout_error() {
        let e: FormatError = LayoutError::EmptySegment.into();
        assert!(matches!(e, FormatError::InvalidLayout(LayoutError::EmptySegment)));
        // Display includes the inner reason, not just a generic string.
        assert!(e.to_string().contains("zero length"));
    }

    #[test]
    fn producer_bug_carries_reason() {
        let e = FormatError::ProducerBug("no leading Inline");
        assert!(e.to_string().contains("no leading Inline"));
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-format invalid_layout_carries_inner_layout_error 2>&1 | head -30`
Expected: FAILS to COMPILE — `FormatError::InvalidLayout` takes no arguments and `From<LayoutError>` is not implemented.

- [ ] **Step 3: Make `LayoutError` a `thiserror` error**

In `musefs-format/src/layout.rs`, replace lines 1-8:

```rust
/// Validation errors discovered in a layout at synthesis time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutError {
    /// A segment reported zero length.
    EmptySegment,
    /// Total length overflowed u64.
    TotalOverflow,
}
```

with:

```rust
/// Validation errors discovered in a layout at synthesis time.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LayoutError {
    /// A segment reported zero length.
    #[error("a segment reported zero length")]
    EmptySegment,
    /// Total length overflowed u64.
    #[error("total layout length overflowed u64")]
    TotalOverflow,
}
```

- [ ] **Step 4: Make `FormatError::InvalidLayout` carry the source + add `ProducerBug`**

In `musefs-format/src/error.rs`, replace the `InvalidLayout` variant (line 17-18):

```rust
    #[error("synthesized region layout violates producer invariants")]
    InvalidLayout,
```

with:

```rust
    #[error("synthesized region layout violates producer invariants: {0}")]
    InvalidLayout(#[from] crate::layout::LayoutError),
    #[error("producer invariant violated: {0}")]
    ProducerBug(&'static str),
```

- [ ] **Step 5: Collapse the five `validated(...).map_err(...)` sites to `?`**

In each of `flac.rs:303`, `mp3.rs:410`, `wav.rs:252`, `ogg/mod.rs:275`, `mp4.rs:835`, change:

```rust
    RegionLayout::validated(segments).map_err(|_| FormatError::InvalidLayout)
```

to:

```rust
    Ok(RegionLayout::validated(segments)?)
```

(These are the trailing expression of functions returning `Result<RegionLayout>` = `Result<RegionLayout, FormatError>`; `?` converts `LayoutError` via the new `#[from]`, and the value is re-wrapped in `Ok`.)

- [ ] **Step 6: Replace the manual mp4 guard with `ProducerBug`**

In `musefs-format/src/mp4.rs:822`, change:

```rust
        // build_udta always yields a leading Inline; anything else is a producer bug.
        return Err(FormatError::InvalidLayout);
```

to:

```rust
        // build_udta always yields a leading Inline; anything else is a producer bug.
        return Err(FormatError::ProducerBug(
            "build_udta did not yield a leading Inline framing segment",
        ));
```

- [ ] **Step 7: Run tests + clippy to verify pass**

Run: `cargo test -p musefs-format 2>&1 | tail -20 && cargo clippy -p musefs-format --all-targets -- -D warnings 2>&1 | tail -5`
Expected: all tests PASS; clippy clean. `needless_question_mark` does NOT fire on `Ok(RegionLayout::validated(segments)?)` because `?` performs the `LayoutError → FormatError` conversion (the lint only triggers when the inner and outer error types are identical). Keep the `Ok(...?)` form — do **not** "simplify" it to a bare `RegionLayout::validated(segments)`, which returns `Result<_, LayoutError>` and won't typecheck against the `Result<RegionLayout>` (= `FormatError`) return type. If some future clippy version objects, use `.map_err(FormatError::from)`.

- [ ] **Step 8: Commit**

```bash
git add musefs-format/src/layout.rs musefs-format/src/error.rs \
        musefs-format/src/flac.rs musefs-format/src/mp3.rs \
        musefs-format/src/wav.rs musefs-format/src/ogg/mod.rs \
        musefs-format/src/mp4.rs
git commit -m "feat(format): carry LayoutError diagnostics through FormatError (#95)"
```

### Task B2: `musefs-core/tree.rs` — `RebuildError` instead of `Result<(), ()>`

**Files:**
- Modify: `musefs-core/src/tree.rs` (add `RebuildError`; change `rebuild_subtree` @301, `apply_changes` @337; four `ok_or(())` sites @326/352/384/403)
- Modify: `musefs-core/src/facade.rs:315-316` (test-injection arm), `:345-346` (fallback log)
- Test: `musefs-core/src/tree.rs` (inline `#[cfg(test)]`, alongside existing tree tests)

- [ ] **Step 1: Write the failing test**

Add to the existing `#[cfg(test)]` area of `musefs-core/src/tree.rs` (near the other `rebuild_subtree_*` tests):

```rust
#[test]
fn rebuild_subtree_reports_missing_rendered_path() {
    use std::collections::HashMap;
    let mut alloc = InodeAllocator::new();
    let mut tree = VirtualTree::build_with(&[(10, "Alice/Song.flac".into())], &mut alloc);
    let dir = tree.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let new_paths: HashMap<i64, String> = HashMap::new(); // omits track 10
    let err = tree.rebuild_subtree(dir, &new_paths, &mut alloc).unwrap_err();
    assert_eq!(err, RebuildError::MissingRenderedPath(10));
}
```

(This mirrors the existing `rebuild_subtree_*` tests' setup: `InodeAllocator::new()`, `VirtualTree::build_with(&[(id, path)], &mut alloc)`, `tree.lookup(VirtualTree::ROOT, "Alice")`.)

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-core rebuild_subtree_reports_missing_rendered_path 2>&1 | head -30`
Expected: FAILS to COMPILE — `RebuildError` does not exist and `rebuild_subtree` returns `Result<(), ()>`.

- [ ] **Step 3: Define `RebuildError`**

Add near the top of `musefs-core/src/tree.rs` (after the imports, before `VirtualTree`):

```rust
/// Why an incremental tree mutation could not complete; the caller falls back to
/// a full rebuild. Carries diagnostics instead of `()` (#95).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebuildError {
    /// A track collected for rebuild had no entry in `new_paths`.
    MissingRenderedPath(i64),
    /// Test-only injected failure (`force_apply_fail`).
    TestInjected,
}
```

- [ ] **Step 4: Change the two signatures and the four `ok_or` sites**

In `rebuild_subtree` (`tree.rs:300-306`): delete the `#[allow(clippy::result_unit_err)]` line and change the return type to `std::result::Result<(), RebuildError>`. At line 326 change (the enclosing loop is `for id in ids` over an owned `Vec<i64>`, so `id` is already `i64` — pass it by value, no `*`):

```rust
            let path = new_paths.get(&id).ok_or(())?;
```

to:

```rust
            let path = new_paths.get(&id).ok_or(RebuildError::MissingRenderedPath(id))?;
```

In `apply_changes` (`tree.rs:336-344`): delete its `#[allow(clippy::result_unit_err)]` line and change the return type to `std::result::Result<(), RebuildError>`. Change the three sites:

- Line 352: `let new_path = new_paths.get(&id).ok_or(())?;` →
  `let new_path = new_paths.get(&id).ok_or(RebuildError::MissingRenderedPath(id))?;`
- Line 384: `let rendered = new_paths.get(&id).ok_or(())?;` →
  `let rendered = new_paths.get(&id).ok_or(RebuildError::MissingRenderedPath(id))?;`
- Line 403: `let rendered = new_paths.get(&id).ok_or(())?;` →
  `let rendered = new_paths.get(&id).ok_or(RebuildError::MissingRenderedPath(id))?;`

(The `self.rebuild_subtree(d, new_paths, alloc)?;` at line 423 now propagates `RebuildError` unchanged — both functions share the type.)

- [ ] **Step 5: Update the facade caller (test injection + fallback log)**

In `musefs-core/src/facade.rs`, the test-injection arm (line 315-316) currently reads:

```rust
        let applied = if self.force_apply_fail.swap(false, Ordering::AcqRel) {
            Err(()) // test injection
        } else {
```

Change `Err(())` to `Err(crate::tree::RebuildError::TestInjected)`.

Then the fallback arm (line 345-346):

```rust
            Err(()) => {
                eprintln!("musefs: incremental tree mutation failed; falling back to full rebuild");
```

Change to bind the reason and interpolate it. **Keep `eprintln!` here** — `musefs-core` does not depend on `log` until Task C1, so a `log::warn!` in this commit would fail to compile (`error[E0433]: use of undeclared crate log`). Task C4 Step 2 converts this single `eprintln!` to `log::warn!` after C1 adds the dependency.

```rust
            Err(reason) => {
                eprintln!(
                    "musefs: incremental tree mutation failed ({reason:?}); falling back to full rebuild"
                );
```

- [ ] **Step 6: Run tests + clippy to verify pass**

Run: `cargo test -p musefs-core 2>&1 | tail -20 && cargo clippy -p musefs-core --all-targets -- -D warnings 2>&1 | tail -5`
Expected: all PASS; clippy clean (the `result_unit_err` allows are gone with no new warnings).

- [ ] **Step 7: Commit**

```bash
git add musefs-core/src/tree.rs musefs-core/src/facade.rs
git commit -m "feat(core): RebuildError carries tree-mutation diagnostics (#95)"
```

### Task B3: record the convention in `CLAUDE.md`

**Files:**
- Modify: `CLAUDE.md` (the `## Conventions` section)

- [ ] **Step 1: Add the convention bullet**

In `CLAUDE.md`, under `## Conventions`, after the `- Errors:` bullet, add:

```markdown
- Internal error paths do not discard diagnostics: no `Result<_, ()>`, and no
  `.map_err(|_| …)` that drops a source. Each error variant carries its source
  (`#[from]`) or a static reason describing the broken invariant.
```

- [ ] **Step 2: Commit**

```bash
git add CLAUDE.md
git commit -m "docs: record the no-discarded-diagnostics error convention (#95)"
```

---

## Part C — #96: mutex poison recovery-by-reset (Rust)

Only `std::sync::Mutex` fields in `musefs-core` `facade.rs` and `reader.rs` are in
scope (the VFS-serving state). Out of scope, by spec: `byte_budget.rs` (→ #93),
`db_pool.rs` (→ #94), and `scan.rs` test/scan-internal locks. Three recovery
categories, each a helper in a new `lock.rs` module.

### Task C1: add `log` dep + the `lock.rs` helper module

**Files:**
- Modify: `musefs-core/Cargo.toml` (add `log`)
- Create: `musefs-core/src/lock.rs`
- Modify: `musefs-core/src/lib.rs` (add `mod lock;`)
- Test: inline in `musefs-core/src/lock.rs`

- [ ] **Step 1: Write the failing test**

Create `musefs-core/src/lock.rs` with the test first (the impls come in Step 4):

```rust
//! Poison-recovery policy for the daemon's in-memory mutexes (#96).
//!
//! musefs is read-only and the SQLite store is the source of truth, so on a
//! poisoned lock we reset the guarded state to a known-good value rather than
//! serve possibly-inconsistent state:
//!   * caches  -> `lock_or_clear`  (clear; next access cold-resolves from the DB)
//!   * VFS state -> `lock_or_flag` (schedule a full rebuild via `poll_refresh`)
//!   * scalars -> `lock_recover`   (replace-only writes can't be half-written)

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

/// State that can be reset to empty for `lock_or_clear`.
pub(crate) trait Clearable {
    fn reset(&mut self);
}

impl<T> Clearable for Option<T> {
    fn reset(&mut self) {
        *self = None;
    }
}

/// Category 3 — transient scalar. Recover the inner value, logging the poison.
pub(crate) fn lock_recover<'a, T>(m: &'a Mutex<T>, what: &str) -> MutexGuard<'a, T> {
    m.lock().unwrap_or_else(|e| {
        log::error!("recovered poisoned scalar lock ({what}); continuing on inner value");
        e.into_inner()
    })
}

/// Category 1 — cache. On poison, clear the cache so the next access
/// cold-resolves; a cleared cache cannot be inconsistent.
pub(crate) fn lock_or_clear<'a, T: Clearable>(m: &'a Mutex<T>, what: &str) -> MutexGuard<'a, T> {
    match m.lock() {
        Ok(g) => g,
        Err(e) => {
            log::error!("cleared poisoned cache lock ({what})");
            let mut g = e.into_inner();
            g.reset();
            g
        }
    }
}

/// Category 2 — rebuildable VFS state. On poison, flag a full rebuild (run by the
/// next `poll_refresh`) and recover the inner value for best-effort completion.
pub(crate) fn lock_or_flag<'a, T>(
    m: &'a Mutex<T>,
    needs_rebuild: &AtomicBool,
    what: &str,
) -> MutexGuard<'a, T> {
    m.lock().unwrap_or_else(|e| {
        log::error!("poisoned VFS-state lock ({what}); scheduling full rebuild");
        needs_rebuild.store(true, Ordering::Release);
        e.into_inner()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn poison<T: Send + 'static>(m: Arc<Mutex<T>>) {
        let m2 = Arc::clone(&m);
        let _ = std::thread::spawn(move || {
            let _g = m2.lock().unwrap();
            panic!("poison it");
        })
        .join();
        assert!(m.is_poisoned());
    }

    #[test]
    fn recover_returns_inner_after_poison() {
        let m = Arc::new(Mutex::new(7u32));
        poison(Arc::clone(&m));
        assert_eq!(*lock_recover(&m, "scalar"), 7);
    }

    #[test]
    fn clear_empties_cache_after_poison() {
        let m = Arc::new(Mutex::new(Some(42u32)));
        poison(Arc::clone(&m));
        assert!(lock_or_clear(&m, "cache").is_none());
    }

    #[test]
    fn flag_set_after_poison() {
        let m = Arc::new(Mutex::new(0u32));
        let flag = AtomicBool::new(false);
        poison(Arc::clone(&m));
        let _g = lock_or_flag(&m, &flag, "vfs");
        assert!(flag.load(Ordering::Acquire));
    }
}
```

- [ ] **Step 2: Wire the module + dependency**

Add to `musefs-core/Cargo.toml` under `[dependencies]`:

```toml
log = "0.4"
```

Add to `musefs-core/src/lib.rs` (with the other `mod` declarations):

```rust
mod lock;
```

- [ ] **Step 3: Run the test to verify it passes**

Run: `cargo test -p musefs-core lock:: 2>&1 | tail -20`
Expected: the three `lock::tests::*` PASS.

- [ ] **Step 4: Commit**

```bash
git add musefs-core/Cargo.toml musefs-core/src/lock.rs musefs-core/src/lib.rs Cargo.lock
git commit -m "feat(core): lock.rs poison recovery-by-reset helpers (#96)"
```

### Task C2: category 1 — clear caches on poison (reader + ogg_index)

**Files:**
- Modify: `musefs-core/src/reader.rs` (`HeaderCache::shard` @183-188, `retain` @190-196; add `Clearable for Shard`)
- Modify: `musefs-core/src/ogg_index.rs:58, 181, 195` (the `LastPageMemo` accesses)

- [ ] **Step 1: Make `Shard` clearable + use `lock_or_clear` in `shard()`**

In `musefs-core/src/reader.rs`, add a `Clearable` impl for `Shard` (place it next to the `Shard` definition). `Shard` already has `retain_keys`; clearing means dropping all entries — use whatever the struct's reset is. If `Shard` has no `clear`, add one that empties its map and LRU list and resets its byte counter:

```rust
impl crate::lock::Clearable for Shard {
    fn reset(&mut self) {
        self.retain_keys(&std::collections::HashSet::new());
    }
}
```

> `retain_keys(&empty_set)` drops every entry, which is exactly the "clear" semantics we want and reuses existing, tested pruning logic.

Change `HeaderCache::shard` (lines 183-188) from:

```rust
    fn shard(&self, track_id: i64) -> std::sync::MutexGuard<'_, Shard> {
        let idx = (track_id as u64 % CACHE_SHARDS as u64) as usize;
        self.shards[idx]
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
```

to:

```rust
    fn shard(&self, track_id: i64) -> std::sync::MutexGuard<'_, Shard> {
        let idx = (track_id as u64 % CACHE_SHARDS as u64) as usize;
        crate::lock::lock_or_clear(&self.shards[idx], "header-cache shard")
    }
```

In `retain` (lines 190-196) change:

```rust
    pub fn retain(&self, live: &HashSet<i64>) {
        for s in &self.shards {
            s.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retain_keys(live);
        }
    }
```

to:

```rust
    pub fn retain(&self, live: &HashSet<i64>) {
        for s in &self.shards {
            crate::lock::lock_or_clear(s, "header-cache shard (retain)").retain_keys(live);
        }
    }
```

- [ ] **Step 2: Use `lock_or_clear` for the `LastPageMemo` (category-1 cache)**

In `musefs-core/src/ogg_index.rs`, the three `m.lock().unwrap()` sites on the
`LastPageMemo` (`Mutex<Option<(u64,u64,Vec<u8>)>>`, already `Clearable` via the
blanket `Option<T>` impl) become `lock_or_clear`:

- Line 58: `let guard = m.lock().unwrap();` →
  `let guard = crate::lock::lock_or_clear(m, "ogg last-page memo");`
- Line 181: `let g = m.lock().unwrap();` →
  `let g = crate::lock::lock_or_clear(m, "ogg last-page memo");`
- Line 195: `*m.lock().unwrap() = Some((page_rel, total_len, patched_hdr.clone()));` →
  `*crate::lock::lock_or_clear(m, "ogg last-page memo") = Some((page_rel, total_len, patched_hdr.clone()));`

(The test at `ogg_index.rs:653` uses `.lock().unwrap()` on a local memo and may stay as-is — it is test code asserting a populated memo, not a serving-path recovery.)

- [ ] **Step 3: Run tests + clippy**

Run: `cargo test -p musefs-core 2>&1 | tail -20 && cargo clippy -p musefs-core --all-targets -- -D warnings 2>&1 | tail -5`
Expected: all PASS; clippy clean.

- [ ] **Step 4: Commit**

```bash
git add musefs-core/src/reader.rs musefs-core/src/ogg_index.rs
git commit -m "feat(core): clear header-cache + ogg memo on poison (#96)"
```

### Task C3: category 2 — flag VFS state + self-healing rebuild

Add a `needs_rebuild: AtomicBool` field, route the `inodes`/`snapshot` locks
through `lock_or_flag`, and extend `poll_refresh_notify` so a set flag bypasses
the three early-return gates and forces a full rebuild.

**Files:**
- Modify: `musefs-core/src/facade.rs` (struct field + initializer; lock sites @243, @259, @313, @426, @457; `poll_refresh_notify` @384; new `force_full_rebuild` helper)
- Test: `musefs-core/src/facade.rs` (inline `#[cfg(test)]`, or the crate's existing facade test file if one exists)

- [ ] **Step 1: Write the failing test**

Add this test to the inline `#[cfg(test)] mod tests` in `facade.rs` (it reuses
the same temp-dir + `scan_directory` + `Musefs::open` scaffolding as the existing
`open_handle_reresolves_after_content_version_bump` test):

```rust
#[test]
fn needs_rebuild_flag_forces_full_rebuild_on_next_poll() {
    use crate::scan::scan_directory;
    use id3::TagLike;
    use std::collections::BTreeMap;

    let dir = tempfile::tempdir().unwrap();
    {
        let mut tag = id3::Tag::new();
        tag.set_artist("Pix");
        tag.set_title("Song");
        let mut bytes = Vec::new();
        tag.write_to(&mut bytes, id3::Version::Id3v24).unwrap();
        bytes.extend_from_slice(&[0xFF, 0xFB, 1, 2, 3, 4]);
        std::fs::write(dir.path().join("a.mp3"), &bytes).unwrap();
    }
    let db_path = dir.path().join("m.db");
    {
        let db = musefs_db::Db::open(&db_path).unwrap();
        scan_directory(&db, dir.path()).unwrap();
    }
    let cfg = MountConfig {
        template: "$artist/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode: Mode::Synthesis,
        poll_interval: std::time::Duration::ZERO,
    };
    let fs = Musefs::open(musefs_db::Db::open(&db_path).unwrap(), cfg).unwrap();

    // data_version is unchanged since open, so a normal poll is a no-op.
    assert!(!fs.poll_refresh().unwrap(), "baseline poll must be a no-op");

    // Simulate recovery from a poisoned VFS-state lock.
    fs.mark_needs_rebuild_for_test();
    assert!(fs.poll_refresh().unwrap(), "a set needs_rebuild flag must force a rebuild");
    assert!(!fs.needs_rebuild_is_set_for_test(), "flag cleared after rebuild");
}
```

Add the two `#[doc(hidden)]` test accessors next to the existing
`force_rebuild_errors_for_test` (facade.rs:518):

```rust
    #[doc(hidden)]
    pub fn mark_needs_rebuild_for_test(&self) {
        self.needs_rebuild.store(true, std::sync::atomic::Ordering::Release);
    }

    #[doc(hidden)]
    pub fn needs_rebuild_is_set_for_test(&self) -> bool {
        self.needs_rebuild.load(std::sync::atomic::Ordering::Acquire)
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-core needs_rebuild_flag_forces_full_rebuild_on_next_poll 2>&1 | head -30`
Expected: FAILS to COMPILE — the `needs_rebuild` field and accessors don't exist yet.

- [ ] **Step 3: Add the `needs_rebuild` field + initializer**

In the `Musefs` struct (`facade.rs`, near `force_apply_fail` at line 147) add:

```rust
    /// Set when a poisoned VFS-state lock is recovered; the next `poll_refresh`
    /// forces a full rebuild from the DB and clears it (#96).
    needs_rebuild: AtomicBool,
```

In every `Musefs { … }` constructor literal (the one near line 180 where
`force_apply_fail: AtomicBool::new(false)` is set — and any other constructor),
add:

```rust
            needs_rebuild: AtomicBool::new(false),
```

- [ ] **Step 4: Route `inodes`/`snapshot` locks through `lock_or_flag`**

Replace each of these `*.lock().unwrap_or_else(std::sync::PoisonError::into_inner)` acquisitions with the helper:

- `facade.rs:243` (snapshot write in `refresh`): the expression `self.snapshot.lock().unwrap_or_else(std::sync::PoisonError::into_inner)` becomes `crate::lock::lock_or_flag(&self.snapshot, &self.needs_rebuild, "snapshot")`.
- `facade.rs:259` (inodes in `rebuild_full`): becomes `crate::lock::lock_or_flag(&self.inodes, &self.needs_rebuild, "inodes")`.
- `facade.rs:313` (inodes in `rebuild_incremental`): becomes `crate::lock::lock_or_flag(&self.inodes, &self.needs_rebuild, "inodes")`.
- `facade.rs:426` (snapshot read in `poll_refresh_notify`): becomes `crate::lock::lock_or_flag(&self.snapshot, &self.needs_rebuild, "snapshot")`.
- `facade.rs:457` (snapshot write in `poll_refresh_notify`): becomes `crate::lock::lock_or_flag(&self.snapshot, &self.needs_rebuild, "snapshot")`.

Each retains its surrounding `.clone()` / `= value` usage — only the guard acquisition changes. Example for line 240-243:

```rust
        *crate::lock::lock_or_flag(&self.snapshot, &self.needs_rebuild, "snapshot") = snapshot;
```

- [ ] **Step 5: Add `force_full_rebuild` and the gate-bypass in `poll_refresh_notify`**

Add a private helper to the `impl Musefs` block (near `rebuild_full`):

```rust
    /// Full rebuild used to self-heal after a poisoned VFS-state lock: rebuild
    /// from the DB, publish the tree, diff for cache invalidation, and clear the
    /// flag. Bypasses the poll gates (the caller checks `needs_rebuild`).
    fn force_full_rebuild(&self, on_changed: &mut impl FnMut(u64)) -> Result<bool> {
        let old_tree = self.tree.load_full();
        let old_snapshot = crate::lock::lock_or_flag(&self.snapshot, &self.needs_rebuild, "snapshot").clone();
        let new_snapshot = self.rebuild_full()?;
        let new_tree = self.tree.load();
        let live = new_tree.track_ids();
        self.cache.retain(&live);
        self.size_cache.retain(|k, _| live.contains(k));
        Self::notify_changed(&old_snapshot, &new_snapshot, &old_tree, &new_tree, on_changed);
        *crate::lock::lock_or_flag(&self.snapshot, &self.needs_rebuild, "snapshot") = new_snapshot;
        self.refresh_gen.fetch_add(1, Ordering::AcqRel);
        self.needs_rebuild.store(false, Ordering::Release);
        self.stamp_successful_poll();
        Ok(true)
    }
```

At the very top of `poll_refresh_notify` (before the debounce gate at line 385), insert:

```rust
        // A poisoned VFS-state lock scheduled a full rebuild: do it now,
        // bypassing the debounce / backoff / data_version gates (#96).
        if self.needs_rebuild.load(Ordering::Acquire) {
            // Single-flight with the same flag the normal path uses.
            if self
                .refreshing
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return Ok(false);
            }
            let _guard = RefreshGuard(&self.refreshing);
            return self.force_full_rebuild(&mut on_changed);
        }
```

- [ ] **Step 6: Run the test + full crate tests + clippy**

Run: `cargo test -p musefs-core 2>&1 | tail -25 && cargo clippy -p musefs-core --all-targets -- -D warnings 2>&1 | tail -5`
Expected: the new test PASSES; all existing `poll_refresh`/facade tests still PASS; clippy clean.

- [ ] **Step 7: Commit**

```bash
git add musefs-core/src/facade.rs
git commit -m "feat(core): flag + self-heal VFS state on lock poison (#96)"
```

### Task C4: category 3 — recover scalars + finish the log migration

**Files:**
- Modify: `musefs-core/src/facade.rs` scalar lock sites: `389` (last_poll read), `398` (last_failed_refresh read), `434` (last_failed_refresh write), `510` (last_poll write), `515` (last_failed_refresh write), `543` (last_poll write, test helper); and the `eprintln!` fallback at `346` from Task B2.
- Modify: `musefs-core/src/db_pool.rs`? — **no** (out of scope, #94).

- [ ] **Step 1: Route the scalar locks through `lock_recover`**

Replace each `*.lock().unwrap_or_else(std::sync::PoisonError::into_inner)` on a scalar mutex with `lock_recover`:

- Line 389 (`last_poll` debounce read): `self.last_poll.lock().unwrap_or_else(std::sync::PoisonError::into_inner)` → `crate::lock::lock_recover(&self.last_poll, "last_poll")`.
- Line 398 (`last_failed_refresh` read): → `crate::lock::lock_recover(&self.last_failed_refresh, "last_failed_refresh")`.
- Line 434 (`last_failed_refresh` write): → `crate::lock::lock_recover(&self.last_failed_refresh, "last_failed_refresh")`.
- Line 510 (`last_poll` write in `stamp_successful_poll`): → `crate::lock::lock_recover(&self.last_poll, "last_poll")`.
- Line 515 (`last_failed_refresh` write in `stamp_successful_poll`): → `crate::lock::lock_recover(&self.last_failed_refresh, "last_failed_refresh")`.
- Line 543 (`last_poll` write in `expire_poll_debounce_for_test`): → `crate::lock::lock_recover(&self.last_poll, "last_poll")`.

Each keeps its surrounding `*… = value` / `.elapsed()` / `if let Some(..) = *…` usage; only the acquisition changes.

- [ ] **Step 2: Convert the Task B2 fallback `eprintln!` to `log::warn!`**

Now that `log` is a dependency (Task C1), change the fallback arm in `poll_refresh_notify` (the `Err(reason) =>` block, ~line 345) from:

```rust
                eprintln!(
                    "musefs: incremental tree mutation failed ({reason:?}); falling back to full rebuild"
                );
```

to:

```rust
                log::warn!(
                    "incremental tree mutation failed ({reason:?}); falling back to full rebuild"
                );
```

- [ ] **Step 3: Run tests + clippy**

Run: `cargo test -p musefs-core 2>&1 | tail -20 && cargo clippy -p musefs-core --all-targets -- -D warnings 2>&1 | tail -5`
Expected: all PASS; clippy clean. Confirm no un-policied serving-path lock acquisition remains:

Run: `grep -rn "PoisonError::into_inner" musefs-core/src/facade.rs musefs-core/src/reader.rs`
Expected: no matches.

Run: `grep -rn "\.lock()\.unwrap()" musefs-core/src/facade.rs musefs-core/src/reader.rs musefs-core/src/ogg_index.rs`
Expected: only the test-code sites in `ogg_index.rs` (e.g. ~653/666/698) remain; no serving-path `.lock().unwrap()`.

- [ ] **Step 4: Commit**

```bash
git add musefs-core/src/facade.rs
git commit -m "feat(core): recover scalar locks on poison; route logs via log crate (#96)"
```

### Task C5: audit comment + scope note

**Files:**
- Modify: `musefs-core/src/lock.rs` (append the audit) or `musefs-core/src/facade.rs` (module-level comment)

- [ ] **Step 1: Record the per-lock audit**

Append to the module doc-comment in `musefs-core/src/lock.rs` a table classifying every `Mutex` reached on the serving path and why its reset is correct:

```rust
//! Audit (every serving-path `std::sync::Mutex`):
//!   facade.rs `inodes`            -> cat 2 (flag): InodeAllocator, rebuilt by build_full from the DB.
//!   facade.rs `snapshot`          -> cat 2 (flag): per-track render state, rebuilt by rebuild_full from the DB.
//!   facade.rs `last_poll`         -> cat 3 (recover): Instant, replace-only single write.
//!   facade.rs `last_failed_refresh` -> cat 3 (recover): Option<Instant>, replace-only single write.
//!   reader.rs HeaderCache shards  -> cat 1 (clear): pure cache, repopulated from the DB.
//!   ResolvedFile::last_page (reader.rs:30, locked in ogg_index.rs as LastPageMemo)
//!                                 -> cat 1 (clear): deterministic one-entry cache, re-derived.
//! Out of scope (handled elsewhere): byte_budget.rs (#93, currently panics on
//! poison), db_pool.rs (#94), scan.rs ENV_LOCK / work-queue (test/scan-internal,
//! not on the FUSE serving path).
```

- [ ] **Step 2: Verify build + commit**

Run: `cargo test -p musefs-core 2>&1 | tail -5`
Expected: PASS.

```bash
git add musefs-core/src/lock.rs
git commit -m "docs(core): audit serving-path locks for poison policy (#96)"
```

### Task C6: full-workspace verification

- [ ] **Step 1: Run the whole workspace test + lint suite**

Run: `cargo fmt --all --check && cargo clippy --all-targets -- -D warnings 2>&1 | tail -5 && cargo test --workspace 2>&1 | tail -30`
Expected: fmt clean, clippy clean, all tests PASS (the `#[ignore]`d FUSE e2e tests stay ignored).

- [ ] **Step 2: (optional) Run the FUSE e2e suite if `/dev/fuse` is available**

Run: `cargo test -p musefs-fuse -- --ignored 2>&1 | tail -20`
Expected: PASS (real mounts; requires `/dev/fuse` + libfuse). If the environment lacks `/dev/fuse`, note it and rely on the workspace suite.

- [ ] **Step 3: Confirm the fuzz crate still builds (signatures changed in Part B)**

Run: `cargo +nightly fuzz build mp4 2>&1 | tail -5` (the format-layer error change is the only fuzz-visible surface; mp4 exercises `ProducerBug`/`InvalidLayout`).
Expected: builds. If nightly/cargo-fuzz is unavailable, note it — CI's fuzz smoke job covers it.

---

## Notes for the executor

- **Part ordering:** A, B, then C is the spec's sequence, but A (Python) is fully independent and can run in parallel. Within Part B, do B1 before B2 only if convenient — they touch different crates. **Do Task C1 before B2's commit if you want `log::warn!` in the fallback immediately**; otherwise B2 uses `eprintln!` and C4 converts it (the plan is written for that path).
- **No `--amend`, no `--no-verify`.** A failed pre-commit hook means the commit didn't happen: fix, re-stage, new commit.
- If any cited line number has drifted (the spec/plan were written against a specific tree state), locate the symbol by name (`replace_tags`, `synthesize_layout`, `apply_changes`, `poll_refresh_notify`, `HeaderCache::shard`) rather than trusting the number.
