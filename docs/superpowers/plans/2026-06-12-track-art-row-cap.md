# Track-Art Row-Count Cap Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Cap the number of `track_art` rows materialized per track on the serve path, mirroring the existing tag-count cap, so a crafted SQLite store returns a controlled error instead of allocating attacker-proportional vectors.

**Architecture:** Add a `pub const MAX_ART_ROWS_PER_TRACK` and a `check_art_count` guard (mirroring `MAX_TAGS_PER_TRACK` / `check_tag_count`) in `musefs-db`. Call the guard inside `Db::get_track_art` after each row is pushed, so the `Vec<TrackArt>` is bounded at cap+1 before erroring. A new `DbError::TooManyArtRows` variant propagates through the existing `#[from]` conversion to `CoreError::Db`, so `mapping::track_art_to_inputs` and everything downstream inherit the bound with no code change. Reader-guard only — no schema/migration change, because a per-track row `COUNT` cannot be expressed as a column `CHECK`.

**Tech Stack:** Rust, `rusqlite`, `thiserror`. Crate: `musefs-db`. Spec: `docs/superpowers/specs/2026-06-12-track-art-cap-design.md`.

**Pre-commit note:** the hook runs `cargo fmt`, `clippy -D warnings`, and the **full workspace test suite**; a commit with red tests or any clippy warning is rejected. Each task below ends green and warning-free. In particular, `check_art_count` is wired into `get_track_art` (a non-test use) in the **same** commit it is introduced, so it never trips dead-code/unused-function lints.

---

## File structure

- `musefs-db/src/limits.rs` — add `MAX_ART_ROWS_PER_TRACK` next to `MAX_TAGS_PER_TRACK`; extend the `cap_values_are_pinned` test.
- `musefs-db/src/error.rs` — add `DbError::TooManyArtRows` variant and the `check_art_count` helper; extend the `guard_helper_tests` module with a boundary test.
- `musefs-db/src/art.rs` — call `check_art_count` inside `get_track_art`; add two integration tests to the `guard_tests` module.

No other files change. No schema migration, no `user_version` bump, no Python schema-mirror regeneration.

---

## Task 1: Add the `MAX_ART_ROWS_PER_TRACK` constant

**Files:**
- Modify: `musefs-db/src/limits.rs` (constant near line 26; test near line 42-50)

- [ ] **Step 1: Add the constant**

In `musefs-db/src/limits.rs`, immediately after the `MAX_TAGS_PER_TRACK` definition (currently lines 24-26), add:

```rust
/// Max `track_art` rows materialized per track on the serve path. Art is
/// low-cardinality (cover/back/leaflet/per-disc), so this is a crafted-DB
/// corruption backstop, not a semantic limit. Mirrors `MAX_TAGS_PER_TRACK`'s
/// reader-guard role (a per-track row COUNT cannot be a column CHECK, so there is
/// no write-time enforcement to lean on).
pub const MAX_ART_ROWS_PER_TRACK: usize = 4096;
```

- [ ] **Step 2: Pin the value in the existing test**

In the same file, inside `mod tests`'s `cap_values_are_pinned` (currently ends with the `STRUCTURAL_KINDS` assert at line 49), add one line before the closing brace:

```rust
        assert_eq!(MAX_ART_ROWS_PER_TRACK, 4096);
```

- [ ] **Step 3: Run the test to verify it passes**

Run: `cargo test -p musefs-db --lib limits::tests::cap_values_are_pinned`
Expected: PASS (1 test).

- [ ] **Step 4: Confirm no warnings**

Run: `cargo clippy -p musefs-db --all-targets -- -D warnings`
Expected: clean (the constant is `pub`, so an as-yet-unused public const produces no dead-code warning).

- [ ] **Step 5: Commit**

```bash
git add musefs-db/src/limits.rs
git commit -m "feat(db): add MAX_ART_ROWS_PER_TRACK cap constant (#316)"
```

---

## Task 2: Add the `TooManyArtRows` error + `check_art_count` guard and wire it into `get_track_art`

**Files:**
- Modify: `musefs-db/src/error.rs` (enum near lines 29-37; helper near lines 65-74; test module near lines 76-96)
- Modify: `musefs-db/src/art.rs` (import line 1; `get_track_art` lines 77-99; `guard_tests` module before its closing brace at line 265)

- [ ] **Step 1: Add the `TooManyArtRows` error variant**

In `musefs-db/src/error.rs`, inside `enum DbError`, immediately after the `TooManyValues { … }` variant (currently ending at line 36, before the closing brace at line 37), add:

```rust
    #[error(
        "track {track_id} has {count} track_art rows, exceeds the {max}-row cap (crafted or corrupt DB)"
    )]
    TooManyArtRows {
        track_id: i64,
        count: usize,
        max: usize,
    },
```

- [ ] **Step 2: Add the `check_art_count` helper**

In the same file, immediately after the `check_tag_count` function (currently ends at line 74), add:

```rust
/// Reject a track whose materialized `track_art` row count exceeds the per-track
/// cap. There is a single art reader (`get_track_art`), so this helper is not
/// about sharing across callers the way `check_tag_count` is; it exists for
/// fidelity with that pattern and to keep the single `>` comparison as one
/// mutation-gate target.
pub(crate) fn check_art_count(track_id: i64, count: usize) -> Result<()> {
    if count > crate::limits::MAX_ART_ROWS_PER_TRACK {
        return Err(DbError::TooManyArtRows {
            track_id,
            count,
            max: crate::limits::MAX_ART_ROWS_PER_TRACK,
        });
    }
    Ok(())
}
```

- [ ] **Step 3: Add the boundary unit test**

In the same file, inside `mod guard_helper_tests` (currently ends at line 96), add this test before the module's closing brace, mirroring `tag_count_accepts_at_cap_rejects_above`:

```rust
    #[test]
    fn art_count_accepts_at_cap_rejects_above() {
        use crate::limits::MAX_ART_ROWS_PER_TRACK;
        // Boundary is inclusive: exactly the cap is accepted, one over rejected.
        // Pins the single `>` site so a `>`->`>=`/`==` mutant cannot survive.
        assert!(super::check_art_count(1, MAX_ART_ROWS_PER_TRACK).is_ok());
        assert!(super::check_art_count(1, MAX_ART_ROWS_PER_TRACK + 1).is_err());
    }
```

- [ ] **Step 4: Run the boundary test to verify it passes**

Run: `cargo test -p musefs-db --lib guard_helper_tests::art_count_accepts_at_cap_rejects_above`
Expected: PASS (1 test). (The helper is not yet called from non-test code; that is fixed in Step 7 before any commit, so the eventual clippy gate stays clean.)

- [ ] **Step 5: Write the failing integration tests**

In `musefs-db/src/art.rs`, inside `mod guard_tests`, add these two tests before the module's closing brace (currently line 265). They reuse the existing `db_track_art()` helper (returns `(Db, track_id, art_id)`) and plant rows via raw `INSERT` sharing the one `art_id`, mirroring the tag-count test `per_track_count_cap_text_and_binary` (`tags.rs:440-463`):

```rust
    #[test]
    fn get_track_art_rejects_excess_rows() {
        let (db, track, art) = db_track_art();
        // 4097 track_art rows sharing one art_id -> TooManyArtRows. Raw INSERT
        // (not set_track_art) keeps the fixture to a single planted blob; the
        // PRIMARY KEY (track_id, ordinal) is satisfied by the distinct ordinals.
        let tx = db.conn.unchecked_transaction().unwrap();
        let mut stmt = tx
            .prepare(
                "INSERT INTO track_art (track_id, art_id, picture_type, description, ordinal) \
                 VALUES (?1, ?2, 3, '', ?3)",
            )
            .unwrap();
        for i in 0..4097 {
            stmt.execute(rusqlite::params![track, art, i]).unwrap();
        }
        drop(stmt);
        tx.commit().unwrap();
        let err = db.get_track_art(track).unwrap_err();
        assert!(matches!(err, DbError::TooManyArtRows { .. }), "{err:?}");
    }

    #[test]
    fn get_track_art_accepts_rows_at_cap() {
        let (db, track, art) = db_track_art();
        let tx = db.conn.unchecked_transaction().unwrap();
        let mut stmt = tx
            .prepare(
                "INSERT INTO track_art (track_id, art_id, picture_type, description, ordinal) \
                 VALUES (?1, ?2, 3, '', ?3)",
            )
            .unwrap();
        for i in 0..4096 {
            stmt.execute(rusqlite::params![track, art, i]).unwrap();
        }
        drop(stmt);
        tx.commit().unwrap();
        assert_eq!(db.get_track_art(track).unwrap().len(), 4096);
    }
```

- [ ] **Step 6: Run the integration tests to verify the cap test fails**

Run: `cargo test -p musefs-db --lib guard_tests::get_track_art_rejects_excess_rows guard_tests::get_track_art_accepts_rows_at_cap`
Expected: `get_track_art_accepts_rows_at_cap` PASSES; `get_track_art_rejects_excess_rows` FAILS — `get_track_art` currently returns `Ok` with 4097 rows, so `unwrap_err()` panics. This proves the guard is absent.

- [ ] **Step 7: Wire the guard into `get_track_art`**

In `musefs-db/src/art.rs`, change the import on line 1 from:

```rust
use crate::error::check_field_len;
```

to:

```rust
use crate::error::{check_art_count, check_field_len};
```

Then, inside `get_track_art` (lines 77-99), add the count check immediately after the existing `out.push(TrackArt { … });` (the push currently ends at line 97), so the loop body's tail reads:

```rust
            out.push(TrackArt {
                art_id: r.get(1)?,
                picture_type: r.get(2)?,
                description: r.get(3)?,
                ordinal: r.get(4)?,
            });
            check_art_count(track_id, out.len())?;
```

The check after the push matches the tag readers (`tags.rs:31` etc.): the `Vec` holds at most `MAX_ART_ROWS_PER_TRACK + 1` elements at the moment it errors. Do not move the check before the push — that would diverge from the mirrored pattern.

- [ ] **Step 8: Run the integration tests to verify both pass**

Run: `cargo test -p musefs-db --lib guard_tests::get_track_art_rejects_excess_rows guard_tests::get_track_art_accepts_rows_at_cap`
Expected: both PASS.

- [ ] **Step 9: Run the full db crate test suite**

Run: `cargo test -p musefs-db`
Expected: PASS — existing `get_track_art_accepts_description_at_cap` / `_rejects_oversize_description` and all other tests still green (the guard only triggers above 4096 rows, well clear of the single-row fixtures).

- [ ] **Step 10: Confirm fmt + clippy are clean**

Run: `cargo fmt -p musefs-db --check && cargo clippy -p musefs-db --all-targets -- -D warnings`
Expected: clean — `check_art_count` now has a non-test caller (`get_track_art`), so no unused-function warning.

- [ ] **Step 11: Commit**

```bash
git add musefs-db/src/error.rs musefs-db/src/art.rs
git commit -m "feat(db): cap per-track track_art row count on the serve path (#316)"
```

---

## Task 3: Verify cross-crate propagation and the full gate

No code changes — this task confirms the bound propagates to the core serve path and the whole workspace stays green, matching how the pre-commit hook will validate the branch.

**Files:** none modified.

- [ ] **Step 1: Confirm the error reaches `CoreError` with no core change**

The conversion already exists: `mapping::track_art_to_inputs` calls `db.get_track_art(track_id)?` (`musefs-core/src/mapping.rs:39`), and `CoreError` has `Db(#[from] musefs_db::DbError)` (`musefs-core/src/error.rs:3`). So `DbError::TooManyArtRows` becomes `CoreError::Db` automatically. Verify by reading those two lines — no edit expected.

Run: `cargo build -p musefs-core`
Expected: compiles (sanity check that the new variant did not break the `#[from]` conversion).

- [ ] **Step 2: Run the full workspace test suite**

Run: `cargo test`
Expected: PASS across all crates (this is what the pre-commit hook runs; it must be green for any commit to land).

- [ ] **Step 3: Run the metrics-feature core tests**

The default `cargo test` skips `musefs-core`'s `metrics` feature (CI's `check` job runs it); this change does not touch getattr/read stat counts, but run it to stay consistent with the documented pre-push step.

Run: `cargo test -p musefs-core --features metrics`
Expected: PASS.

- [ ] **Step 4: Confirm the fuzz crate still builds (format-layer untouched, but cheap to verify)**

This change is db-layer only and does not alter any format-layer signature, so the out-of-workspace `fuzz/` crate is not expected to break. Skip only if a nightly toolchain is unavailable.

Run: `cargo +nightly fuzz build` (from repo root)
Expected: builds, or skip if nightly/`cargo-fuzz` is absent.

- [ ] **Step 5: No commit**

This task adds no files; Tasks 1 and 2 already produced the commits. The branch is ready for `requesting-code-review` / merge.

---

## Done-when

- `Db::get_track_art` returns `Err(DbError::TooManyArtRows { .. })` for any track with more than 4096 `track_art` rows, and `Ok` at exactly 4096.
- The `Vec<TrackArt>` never grows beyond 4097 elements before erroring (guard after push).
- `MAX_ART_ROWS_PER_TRACK` is pinned at 4096 and the `>` boundary is pinned by an inclusive-boundary unit test.
- `cargo test` (full workspace) is green; `cargo fmt --check` and `cargo clippy --all-targets -D warnings` are clean.
- No schema migration, `user_version` bump, or Python schema-mirror change was introduced.
