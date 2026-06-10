# Fail on orphaned `track_art` rows (issue #202)

## Problem

`track_art_to_inputs` (`musefs-core/src/mapping.rs:33-54`) builds a track's
synthesis art inputs by iterating `track_art` rows and looking up each one's
metadata with `db.get_art_meta(ta.art_id)`. When that lookup returns
`Ok(None)` — a `track_art` row whose `art_id` has no corresponding `art` row —
the code silently drops the art inside an `if let Some(meta)` and a comment
asserts the branch is impossible because `track_art.art_id` is a foreign key.

That assumption is unsafe at the external-writer boundary. SQLite FK
enforcement is per-connection and off by default; an external writer can
disable it, import a partial DB, or otherwise create an orphaned `track_art`
row. The current behavior turns that broken-contract state into silent content
loss: musefs serves a synthesized file missing art rather than reporting the
corruption.

This is distinct from the already-handled malformed-row case at
`mapping.rs:274-282`, where an `art` row *exists* but carries an invalid
`byte_len` and already errors at row-read. Issue #202 is specifically the
*missing referenced row* case.

## Decision

Make a missing referenced `art` row a **hard error**, consistent with how the
negative-`byte_len` case already fails and with the CLAUDE.md framing that the
SQLite store is the source of truth and the external-writer contract. The
degraded-mode alternative (keep skipping, document it) was considered and
rejected.

## Changes

### 1. New error variant — `musefs-core/src/error.rs`

Add to `CoreError`:

```rust
#[error("track {track_id} references art {art_id}, which has no metadata row (orphaned track_art — DB contract violation)")]
OrphanedArt { track_id: i64, art_id: i64 },
```

Both ids are carried so the `EIO` reply and the existing serve-path warning
point directly at the offending rows. (No new logging is needed: `reply_errno`
in `musefs-fuse/src/lib.rs` already routes non-routine `CoreError`s through a
`warn!` arm, so an orphan is logged with its ids automatically.)

### 2. The fix — `musefs-core/src/mapping.rs:33-54`

In `track_art_to_inputs`, replace the silent-skip `if let Some(meta)` with a
`let … else` that returns `CoreError::OrphanedArt { track_id, art_id: ta.art_id }`
on `None`. Rewrite the now-incorrect comment: an orphaned row is a contract
violation we surface, not an impossible branch, because external writers can
disable FK enforcement.

`track_art_images` (`mapping.rs:72`) needs no change — it runs only after
`track_art_to_inputs` succeeds, so orphans are rejected upstream before it is
reached.

### 3. errno mapping — `musefs-fuse/src/lib.rs:87-91`

Add `CoreError::OrphanedArt { .. }` to the `EIO` arm alongside `Db`,
`DbOpen`, `Mp4MetadataTooLarge`, and `Format`. An orphan is structural
corruption, so a read of the affected file fails loudly with `EIO` instead of
serving art-less bytes.

### 4. Tests

- `musefs-core/src/mapping.rs` tests: a new test that creates a track and an
  `art` row, wires them with `set_track_art`, then orphans the row via a raw
  `rusqlite::Connection`. This **must** be a raw connection: the production
  `Db` sets `PRAGMA foreign_keys=true` (`musefs-db/src/lib.rs:78`) and
  `track_art.art_id` has no `ON DELETE`, so a delete on the production handle
  would RESTRICT-fail; a fresh raw `rusqlite` connection defaults FK
  enforcement off, allowing the orphan. Assert the `DELETE FROM art WHERE
  id = ?` affected exactly one row (guards against a no-op false pass), then
  assert `track_art_to_inputs` returns `Err` and pin the variant + ids with
  `matches!(err, CoreError::OrphanedArt { track_id, art_id } if track_id == tid && art_id == orphan_id)`
  — a bare `is_err()` lets a wrong-variant mutant survive the gate. Keep the
  existing happy-path assertions (well-formed art still yields inputs) so the
  "always error" mutant is also killed. Mirrors the raw-connection mutation
  technique of the negative-`byte_len` test at lines 274-282.
- `musefs-fuse/src/lib.rs`: extend the existing `maps_core_errors_to_errno`
  test (around line 520) with
  `assert_eq!(errno(&CoreError::OrphanedArt { track_id: 1, art_id: 2 }).code(), libc::EIO);`.

### 5. Docs

One line in ARCHITECTURE.md's external-writer contract section: an orphaned
`track_art` row (an `art_id` with no `art` row) now fails the serve with `EIO`
rather than silently dropping the art.

## Out of scope

- No schema change → no Python schema-mirror regeneration.
- No format-layer signature change → fuzz targets unaffected.
- No broader audit of other `if let Some` sites in core.
