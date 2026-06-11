# content_version freshness: make the freshness key a true superset

**Issues:** #271, #272, #279
**Branch:** `content-version-freshness`
**Date:** 2026-06-11

## Problem

`content_version` (per-track column) is documented as *the* answer to "did
this track's served bytes change?". The whole refresh + cache architecture
keys freshness on it: a single `i64` compare gates every cache hit
(`HeaderCache` layouts in `reader.rs`, `size_cache` attrs in `facade.rs`), and
the incremental refresh path drops cache entries only for *removed* tracks
(`facade.rs` `poll_refresh_notify`), relying on the `content_version` compare
to lazily invalidate *changed* tracks.

The defect class — restated across all three issues — is that `content_version`
is **not a superset** of every input that affects synthesized bytes. Three
inputs change served bytes or attrs without bumping it:

- **#271 — `art` rows.** The `art` table has no triggers. Mutating
  `art.data`/`mime`/`byte_len`/`width`/`height` in place changes synthesized
  bytes but bumps nothing. (`track_art` *link* edits bump; the blob itself
  changing does not.) `art` is content-addressed by `sha256` with
  `CHECK(byte_len = length(data))`, so an in-place data edit already violates
  the content-addressing contract.
- **#272 — scanner geometry.** `format`, `audio_offset`, `audio_length`,
  `backing_size`, `backing_mtime`, and FLAC `structural_blocks` can change
  without bumping `content_version`. `resolve` re-stats and validates the
  *current* track row against disk, but a cache hit only compares
  `content_version`, so it can return an **old** `ResolvedFile` (old offsets)
  paired with the freshly-validated current row.

  The genuine exposure is narrower than "every rescan." `upsert_track`
  (`tracks.rs`/`bulk.rs`) writes geometry but not `content_version`, and the
  integrated scan path's other writes mask this on the common path:
  `replace_tags` is DELETE+INSERT (`tags.rs:177`) so re-tagging a re-probed
  file already fires `tags_a{d,i}` and bumps; incremental rescan skips files
  whose size/mtime are unchanged (`scan.rs`), so a re-probe implies the file
  changed and the tag rewrite covers it. The real residual gaps are: **(1)** a
  re-probed file with **zero text tags** (geometry changes, no tag rows to
  rewrite, nothing bumps); **(2)** the one-time V2 structural-block backfill
  pass (`scan.rs`), which populates `structural_blocks` on *unchanged* files —
  flipping `reader.rs` synthesis from the legacy front-read fallback to the
  streamed fast path, a layout change, with no geometry or tag edit; **(3)**
  hostile raw-SQL geometry edits. `rescan_does_not_reset_content_version`
  pins gap (1) as intended behavior today.
- **#279 — getattr size-cache.** A `size_cache` hit in `getattr` returns
  size/mtime with **no backing stat** (by design — "no backing stat, no
  synthesis"). Read/open re-stat and degrade to `BackingChanged`; `getattr`
  does not, so it is the one metadata surface that advertises stale attrs
  after an on-disk backing change with no DB write.

## Design principle

Make `content_version` a genuine superset of everything the **database can
know** affects served bytes (#271, #272), and add a stat for the one thing the
DB **cannot** know — an on-disk backing change with no DB write (#279). This
stays on the existing grain: musefs already re-stats the backing file on every
`resolve` and on every per-handle read, treating rows as untrusted input and
degrading to a controlled `BackingChanged` rather than splicing at stale
offsets. The bug is only that the *caches* short-circuit that discipline on a
`content_version` match.

Keeping the fix in the schema for #271/#272 means the caches' single-`i64` key
stays valid and the hot path is untouched; only `getattr` (#279) gains a stat,
aligning the one inconsistent surface with read/open.

## Part A — #271: `art` immutability + delete-bump (schema, V5)

Two new triggers on `art`:

1. **`art_reject_content_update`** — `BEFORE UPDATE ON art`, `WHEN` any content
   column (`data`, `sha256`, `mime`, `byte_len`, `width`, `height`) differs
   from `OLD` → `RAISE(ABORT, 'art rows are immutable; insert a new
   content-addressed row and relink via track_art')`. Art is content-addressed;
   the new-row-and-relink path (which already bumps `content_version` through
   the `track_art_*` triggers) becomes the only way to change a track's art. A
   pure no-op `UPDATE` (no content column changed) still passes, so idempotent
   writes are not penalized.

   The full content-column lock (including `width`/`height`/`mime`, not just
   `data`) is deliberate: `mime` and dimensions feed synthesized art headers,
   and a content-addressed row whose `sha256` no longer matches its `data` is
   incoherent regardless of which column drifted.

2. **`art_ad`** — `AFTER DELETE ON art`, bump `content_version` (and
   `updated_at`) for every track linked through `track_art` to `OLD.id`:

   ```sql
   UPDATE tracks SET content_version = content_version + 1,
                     updated_at = CAST(strftime('%s','now') AS INTEGER)
   WHERE id IN (SELECT track_id FROM track_art WHERE art_id = OLD.id);
   ```

   A deleted-but-still-referenced art row (an orphan an external writer can
   produce with FK enforcement disabled) forces a rebuild of the referencing
   tracks → an honest serve-time `EIO` on the orphan instead of streaming
   stale bytes from an old cached layout. This is the "both" decision: reject
   in-place mutation *and* handle the delete path explicitly rather than
   leaving it solely to serve-time `EIO`.

   **`art_ad` is inert on the normal delete path, by design.** The only art
   deletion the codebase performs is `gc_orphan_art` (`art.rs`), which deletes
   art with **no** `track_art` references — so the subquery returns zero rows
   and nothing is bumped (correct: a GC'd orphan has no referencing tracks to
   invalidate). On a track delete, the FK `ON DELETE CASCADE` removes the
   `track_art` link *before* any later art GC, so again the subquery is empty.
   `art_ad` does real work **only** in the FK-disabled-orphan case — exactly
   the hostile/degraded write the contract already treats as untrusted.

   `art_ad` bumps `updated_at` (unlike the Part B geometry trigger): an
   external writer's art delete is not otherwise stamped, whereas the geometry
   trigger's writer (`upsert_track`) already sets `updated_at` itself.

## Part B — #272: scanner geometry bumps `content_version` (schema, V5)

1. **`tracks_geometry_au`** — `AFTER UPDATE ON tracks`, `WHEN` any of `format`,
   `audio_offset`, `audio_length`, `backing_size`, `backing_mtime` differs from
   `OLD`:

   ```sql
   UPDATE tracks SET content_version = content_version + 1 WHERE id = NEW.id;
   ```

   The `WHEN` guard is false on the nested self-fire (the bump changes only
   `content_version`, leaving the geometry columns equal), so the recursion
   terminates after exactly one bump. This catches **all** writers — including
   a hostile raw-SQL edit — not just the scanner's `upsert_track`. `updated_at`
   is already maintained by `upsert_track` itself, so the geometry trigger only
   touches `content_version`. The trigger is `AFTER UPDATE`, so it runs after
   the V4 `CHECK (audio_offset + audio_length <= backing_size)` constraint and
   neither relaxes nor interacts with it.

2. **`structural_blocks_ai` / `_ad`** — bump the owning track's
   `content_version` on insert/delete of `structural_blocks` (FLAC
   `STREAMINFO`/`SEEKTABLE`/`APPLICATION`/`CUESHEET` feed synthesized headers),
   targeting `track_id` from `NEW`/`OLD`. **No `_au` trigger:**
   `set_structural_blocks` (`structural.rs`/`bulk.rs`) is DELETE-then-INSERT,
   never UPDATE, so an update trigger would be dead code. Because every rewrite
   is a DELETE followed by INSERTs, a rescan bumps `content_version` by more
   than one (and bumps even when the re-written blocks are byte-identical).
   This **over-bump is accepted as harmless**: `content_version` is a monotone
   generation counter compared only for equality, so magnitude never affects
   correctness — only how many times a cache entry rebuilds. The cases that
   trigger it (a structural rewrite, or the one-time V2 backfill) are not hot
   paths.

This keeps the caches' single-`i64` key valid and untouched — no hot-path
change for #272. The incremental-refresh path then invalidates these tracks
through the same `content_version`-rose signal it already uses, with no new
cache-maintenance code.

**Changelog double-pump.** Each `content_version` bump trigger does an `UPDATE
tracks`, which fires `tracks_changelog_au` and appends a `track_changes` row.
So a single `upsert_track` that changes geometry now produces **two** changelog
rows (the scanner's own UPDATE plus the trigger's), and a structural rewrite
adds more. This is functionally harmless — `changelog_since` collapses to
distinct track ids — but tests that assert an exact `track_changes` row count
(e.g. the analog of `v4_metadata_edit_bumps_version_and_appends_one_changelog_row`)
must expect the higher count rather than one.

**Alternative considered (rejected):** validate geometry at cache-hit time in
`reader.rs`/`facade.rs` — store the full geometry in `ResolvedFile`/`SizeEntry`
and compare on hit instead of bumping in the schema. It needs no migration and
also catches hostile edits, but it spreads freshness logic across two files,
requires a structural-block fingerprint, and fights the architecture that
already centralizes freshness on `content_version`. The schema bump is the
smaller, more cohesive change.

**Contract/test impact:** `rescan_does_not_reset_content_version`
(`musefs-db/tests/tracks.rs`) currently changes `audio_offset`/`audio_length`
on rescan and asserts `content_version` is unchanged — it pins the #272 bug as
intended behavior. It is inverted: a geometry-changing rescan now bumps
`content_version`; a separate case asserts an identical-geometry rescan still
does not bump (the `WHEN` guard). Rename to reflect the new contract (e.g.
`rescan_with_changed_geometry_bumps_content_version`).

## Part C — #279: getattr stats the backing on size-cache hits (code)

- Extend `SizeEntry` (`facade.rs`) with `backing_size: u64` and
  `backing_mtime_secs: i64`, populated from the resolved file on the miss path.
- On a `size_cache` hit in `getattr`, re-stat `track.backing_path` and compare
  `meta.len()`/`mtime_secs(&meta)` to the stamp. On drift, degrade to
  `CoreError::BackingChanged(track.backing_path)` — the same controlled error
  read/open already return — instead of returning stale attrs.

This adds one `stat` per `getattr` hit. `getattr` already performs a
`get_track` per call, and read/open stat on every read, so this only aligns the
single surface that could outrun a backing change. Record the stat via
`crate::metrics::on_stat()` for consistency with `resolve`.

## Cross-cutting work

- **Migration V5** (`musefs-db/src/schema.rs`): append-only; `MIGRATIONS`
  grows by one entry and `user_version` advances. V5 only *adds* triggers (no
  table rebuild), so unlike V4 it does not need to stash/refill rows or
  recreate existing triggers.
- **Existing-test fallout (must land in the V5 commit).**
  `track_art_to_inputs`'s malformed-row test (`musefs-core/src/mapping.rs`)
  plants a bad art row via `UPDATE art SET byte_len = -1` under
  `PRAGMA ignore_check_constraints`. That pragma disables CHECKs but **not
  triggers**, so `art_reject_content_update` would `RAISE(ABORT)` and fail the
  test — and the pre-commit hook runs the full suite, so the commit would be
  red. Rework the test to plant the malformed row via a **direct INSERT** (the
  immutability trigger guards only UPDATE; a fresh malformed INSERT is the
  realistic FK/CHECK-disabled external write and still reaches the row-reader
  defensive path the test pins) rather than mutating an existing row. This is
  the one place the "all commits stay green" claim needs active work, not just
  additive triggers.
- **Python schema mirror** (per CLAUDE.md): regenerate with
  `MUSEFS_REGEN_SCHEMA_PY=1 cargo test -p musefs-db schema_py`, then re-vendor
  into `contrib/python-musefs/`. Verified: the contrib writers
  (`contrib/python-musefs/.../store.py` and the Picard copy) only ever
  `INSERT ... ON CONFLICT DO NOTHING` on `art` and never UPDATE it, so the
  immutability trigger breaks no contrib code.
- **Docs**:
  - `ARCHITECTURE.md` external-writer contract — document that `art` rows are
    immutable (insert a new content-addressed row and relink via `track_art`;
    do not mutate in place; a multi-row `UPDATE art` touching any content
    column aborts the whole statement via `RAISE(ABORT)`) and that
    `content_version` is now a superset of scanner geometry and structural-block
    changes, not only tag/art edits.
  - `ARCHITECTURE.md` "Freshness: two version counters" — update the
    `content_version` description to reflect the superset semantics.
  - `ARCHITECTURE.md` store-schema section — add the V5 entry.
  - `contrib/python-musefs/` contract docs — mirror the art-immutability rule.
- **Fuzz crate**: no format-layer signature change, so `fuzz/` is unaffected
  (no `cargo +nightly fuzz build` needed beyond the usual gate).

## Testing

Each issue's repro sketch becomes a test:

- **#271:** an in-place `UPDATE` of each `art` content column
  (`data`/`sha256`/`mime`/`byte_len`/`width`/`height`) is rejected
  (`RAISE(ABORT)`); a no-op `UPDATE` (no content column changed) succeeds;
  inserting a new art row and relinking via `track_art` succeeds and bumps;
  deleting an FK-orphan `art` row (links left dangling) bumps `content_version`
  for every referencing track, while a `gc_orphan_art`-style delete of an
  unreferenced row bumps nothing.
- **#272:** the test must reproduce a gap that fails *without* the fix — use a
  **tagless** track (so no `replace_tags` masks the geometry change): upsert it,
  resolve to cache a layout, change geometry via a second `upsert_track`, and
  assert `content_version` rose by **exactly 1** (proving the `WHEN` guard
  terminates the self-fire rather than looping) and that a subsequent `resolve`
  rebuilds the layout with the new offsets. Assert an identical-geometry rescan
  does **not** bump. Assert a `structural_blocks` rewrite bumps the owning
  track (accepting the DELETE+INSERT over-bump — assert `> 0`, not `== 1`).
  Invert/rename `rescan_does_not_reset_content_version`.
- **#279:** populate `size_cache` via an initial `getattr`; change the backing
  file's **size** (truncate or extend — a same-size, same-second in-place
  rewrite is out of scope and would make the test flaky) without changing DB
  `content_version`; assert the next `getattr` returns
  `BackingChanged`/I-O error rather than stale attrs. Preserve a cache-hit test
  asserting an unchanged backing file still serves the cached attrs with no
  spurious error.

All commits stay green: the pre-commit hook runs the full workspace test suite,
so the schema-trigger changes and their tests land together — including the
reworked `mapping.rs` malformed-row test (see Cross-cutting) and the inverted
rescan test, both of which land in the same commit as the V5 migration.

## Out of scope

- Reworking the cache key away from `content_version` (the single-`i64` model
  is preserved deliberately).
- Same-size/same-second backing rewrites (#276) beyond what the #279 stat
  already catches when size or mtime differs — a rewrite that preserves both
  size and mtime-to-the-second remains outside this design, as it is for
  read/open today.
