# Phase 0 + Phase 1: Stop Data Loss, Settle Two Foundational Conventions

**Date:** 2026-06-03
**Issues:** #82 (active data loss), #95 (error diagnostics), #96 (mutex poison policy)
**Status:** Design approved; ready for implementation planning.

## Overview

This spec covers the first two phases of the open-issue backlog (see
`docs/ROADMAP.md`). It does two things:

1. **Stops active data loss** introduced by the binary-tags work (#77–#81): a
   plugin sync currently deletes scanner-written binary tags.
2. **Settles two foundational conventions** — internal error diagnostics (#95)
   and mutex poison-recovery policy (#96) — *before* the later issues that
   inherit them are written (#89/#90/#91/#92/#94).

Two independent codebases are touched and can progress in parallel:

- **Python plugins** — #82 (`contrib/beets/`, `contrib/picard/`).
- **Rust core/format** — #95, #96 (`musefs-format`, `musefs-core`).

The cardinal project invariant (original audio bytes are never copied or
modified) is untouched by all three changes.

## #82 — Preserve scanner-written binary tags

### Problem

Both `contrib/beets/beetsplug/_core.py:171` and
`contrib/picard/musefs/_core.py:201` implement `replace_tags` with a byte-identical
body that does an unscoped delete:

```python
conn.execute("DELETE FROM tags WHERE track_id = ?", (track_id,))
```

Schema V2 (`musefs-db/src/schema.rs:79-80`) added the `value_blob` column and
defines binary tag rows as exactly those with `value_blob IS NOT NULL` (binary
rows store `''` in `value`). The Rust scanner writes binary tags (ID3 APIC/opaque
frames, MP4 `----`, FLAC APPLICATION/CUESHEET) as such rows. The plugins only ever
write **text** rows (`value` set, `value_blob` left NULL).

Because the delete is unscoped, a plugin sync removes the scanner-written binary
rows along with the text rows it intends to replace. Since `--revalidate` skips
unchanged backing files, a later scan does not re-add them, so binary tags are
permanently lost after the first sync.

### Fix

Scope the delete to the plugin-owned (text) rows in **both** files:

```python
conn.execute(
    "DELETE FROM tags WHERE track_id = ? AND value_blob IS NULL", (track_id,)
)
```

For the **default** tag vocabulary, the `(track_id, key, ordinal)` primary key
cannot collide between a surviving binary row and a re-inserted text row: the
built-in keys (`DIRECT_FIELDS` in `_core.py`) are lowercase canonical names,
disjoint from the binary key set (ID3 `PRIV`/`GEOB`/`APIC`, MP4 `----:…`, FLAC
`APPLICATION`/`CUESHEET`). A user-configured `musefs_fields` / `extra_fields`
mapping is unconstrained, so a key like `"PRIV"` *could* collide with a surviving
binary row. The failure mode is **benign and safe**: SQLite raises an
`IntegrityError` that aborts the sync transaction with no data loss (the binary
rows are preserved, the partial text write is rolled back). We do not add key
validation now (YAGNI); we only document this and pin the default-vocabulary
disjointness with a test.

### Out of scope

The two `_core.py` modules are near-duplicates. Deduplicating them into a shared
module is a separate concern and is **not** part of this work; the one-line fix is
applied independently to each copy.

### Testing

For each plugin, add a test that:

1. Seeds a track with one binary tag row (`value_blob IS NOT NULL`) and one text
   tag row.
2. Runs a `replace_tags` / sync.
3. Asserts the binary row survives unchanged and the text rows are replaced.

Plus a static assertion that the default tag vocabulary (`DIRECT_FIELDS`) is
disjoint from the known binary key set, documenting why the scoped delete is
collision-free without an explicit guard.

## #95 — Internal error types carry diagnostics

### Problem

Two internal error paths discard context:

- **`musefs-format`** — `RegionLayout::validated(segments).map_err(|_| FormatError::InvalidLayout)`
  at five synthesis sites (flac.rs:303, mp3.rs:410, wav.rs:252, ogg/mod.rs:275,
  mp4.rs:835) throws away the inner `LayoutError`, which distinguishes
  `EmptySegment` from `TotalOverflow`. `FormatError::InvalidLayout` is a unit
  variant. A sixth site (mp4.rs:822) is a manual `return Err(FormatError::InvalidLayout)`
  guarding a *producer invariant* (`build_udta` must yield a leading `Inline`
  segment) — this is not a `LayoutError`.
- **`musefs-core/src/tree.rs`** — `rebuild_subtree` and `apply_changes` return
  `Result<(), ()>` (with `#[allow(clippy::result_unit_err)]`). When a tree mutation
  fails and the caller falls back to a full rebuild (`facade.rs:345`), there is no
  information about why.

### Fix — `musefs-format`

First, make `LayoutError` (`layout.rs`) a real error type — it currently derives
only `Debug, Clone, PartialEq, Eq` and implements neither `Display` nor
`std::error::Error`, which `#[from]`/`{0}` interpolation require:

```rust
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LayoutError {
    #[error("a segment reported zero length")]
    EmptySegment,
    #[error("total layout length overflowed u64")]
    TotalOverflow,
}
```

Then make `InvalidLayout` carry its source and add a distinct variant for the
producer guard:

```rust
#[error("synthesized region layout violates producer invariants: {0}")]
InvalidLayout(#[from] LayoutError),

#[error("producer invariant violated: {0}")]
ProducerBug(&'static str),
```

- The five `validated(...).map_err(|_| ...)` sites collapse to `?` (via `#[from]`).
- The manual guard at mp4.rs:822 becomes
  `return Err(FormatError::ProducerBug("build_udta did not yield a leading Inline framing segment"));`.

### Fix — `musefs-core/tree.rs`

Replace the unit error with a named enum (and drop the
`#[allow(clippy::result_unit_err)]` attributes):

```rust
pub enum RebuildError {
    /// A track collected for rebuild had no entry in `new_paths`.
    MissingRenderedPath(i64),
    /// Test-only injected failure (facade `force_apply_fail`).
    TestInjected,
}
```

- `rebuild_subtree` and `apply_changes` return `Result<(), RebuildError>`.
- The **four** `new_paths.get(&id).ok_or(())` sites (`tree.rs:326` in
  `rebuild_subtree`; `352`, `384`, `403` in `apply_changes`) become
  `.ok_or(RebuildError::MissingRenderedPath(id))`. The `rebuild_subtree(...)?`
  propagation at `tree.rs:423` works unchanged once both functions share the
  `RebuildError` type.
- The `force_apply_fail` test-injection arm (`facade.rs:316`) yields
  `Err(RebuildError::TestInjected)`.
- The fallback site (`facade.rs:345-346`) logs the actual `RebuildError` instead of
  the generic `eprintln!` string.

### Convention

Record in this spec and in `CLAUDE.md` ("Conventions"):

> Internal error paths do not discard diagnostics. No `Result<_, ()>`; no
> `.map_err(|_| …)` that drops a source. Each error variant carries its source
> (`#[from]`) or a static reason describing the broken invariant.

Issues #91 and #92 adopt this convention from the start.

### Testing

- `musefs-format`: assert that a synthesis path producing an invalid layout
  surfaces the specific `LayoutError` (e.g. `EmptySegment` vs `TotalOverflow`)
  through `FormatError::InvalidLayout`, and that the mp4 producer guard surfaces
  `ProducerBug`.
- `musefs-core`: assert `rebuild_subtree` returns `MissingRenderedPath(id)` when a
  collected track is absent from `new_paths`.

## #96 — Mutex poison recovery-by-reset

### Decision

The codebase recovers from poisoned mutexes throughout with
`.unwrap_or_else(std::sync::PoisonError::into_inner)`, silently continuing on the
inner value (the one exception is `ByteBudget`, which uses `.lock().unwrap()` and
*panics* on poison — deferred to #93, below). Every external reviewer flagged the
recovery pattern as a daemon-correctness risk: after a panic *while mutating*
global VFS state, the daemon serves from potentially-inconsistent state.

The adopted policy is **recover-by-reset**: on a poisoned lock, reset the guarded
state to a known-good value rather than serve suspect state, and log at every
site. This is provably correct here because every datum under these mutexes is
either derivable from the SQLite store (the source of truth — including the
`snapshot` render state, rebuilt by the refresh path) or a single-word scalar. It
is preferred over:

- **Plain log-and-recover** — treats the symptom (visibility) without restoring
  correctness.
- **Fail-fast (EIO / crash)** — more invasive (threads error paths through call
  sites that cannot fail today) and worse UX (unmounts a read-only convenience
  filesystem on a possibly-benign panic).
- **`parking_lot::Mutex`** — cleans the call sites but *deletes* the poison
  signal entirely, foreclosing both logging and any future fail-fast, while
  leaving the underlying inconsistent-state behavior unchanged.

### Mechanism — three recovery categories

Only `std::sync::Mutex` fields participate. Atomics (`last_data_version:
AtomicI64`, `refresh_gen`, `refreshing`, …) and the `DashMap` (`size_cache`)
cannot poison and are outside this taxonomy. The poison-bearing mutexes, by
category — each with a defined, correct recovery action, logged at every site:

1. **Caches** → `lock_or_clear()`.
   The header-layout LRU shards (`reader.rs` `HeaderCache`) and the per-entry Ogg
   `ResolvedFile::last_page` cache (`reader.rs:30`). On poison, take the inner
   guard, `.clear()` it / reset it to `None`, and return it. The next access
   cold-resolves from the DB / re-reads the page. A cleared cache cannot be
   inconsistent, so this is provably safe. (`size_cache` is a `DashMap`, not a
   mutex — no treatment needed; its staleness is already handled by `retain` on
   refresh, `facade.rs:444`.)

2. **Rebuildable VFS state** → `lock_or_flag()`.
   The inode allocator (`inodes`, `facade.rs:142`) and the render-state map
   (`snapshot`, `facade.rs:145`) — both reconstructible from the DB. On poison,
   set an `AtomicBool needs_rebuild`, log, and return the inner guard so the
   current op completes best-effort (no rebuild while holding the poisoned lock —
   avoids reentrancy/deadlock). `poll_refresh_notify` already fires on every
   metadata op (`lookup`/`getattr`/`readdir` → `fire_poll_refresh`,
   `musefs-fuse/src/lib.rs:194/212/272`). It must be extended so that, when
   `needs_rebuild` is set, it **bypasses its three early-return gates** — the
   debounce gate (`facade.rs:385`), the failed-refresh backoff (`395`), and the
   `data_version`-equality gate (`405`) — and performs a **full** rebuild
   (`rebuild_full`, `facade.rs:249`) instead of the usual `rebuild_incremental`
   (`428`), then clears the flag. The state self-heals within one metadata-op
   cycle — no `EIO` to the kernel, no daemon crash, no unmount. Inodes are stable
   across rebuilds by design, so an open handle survives.

3. **Transient scalars** → recover inner.
   `last_poll: Mutex<Instant>` (`facade.rs:131`) and `last_failed_refresh:
   Mutex<Option<Instant>>` (`facade.rs:133`) are replace-only single-word writes
   that cannot be left half-written, so `into_inner` recovery is already correct.

### Audit

As part of this work, perform a one-time audit of every state-mutating lock,
recording in a module-level comment which category it falls into and why the reset
is correct (e.g. "outright replacement" per gemini's safe case, or "rebuilt from
DB on next refresh"). This documents the policy at the point of use.

### Logging

`musefs-fuse` already depends on `log = "0.4"`; `musefs-core` does not (it has a
single `eprintln!` at `facade.rs:346`). Add `log = "0.4"` to `musefs-core` and
route poison-recovery messages (and the existing fallback message) through
`log::error!` / `log::warn!`.

### Deferred to #93

`ByteBudget` is the one lock whose correct reset is subtle: after a panic between
its guard check and the `in_flight` increment, the true in-flight count is
unknowable. Its reset semantics are scoped into **#93** (the `byte_budget`
overflow-asymmetry issue), following the same category-2/3 pattern established
here.

### Testing

Unit tests per recovery category:

- `lock_or_clear`: a poisoned cache lock is observed cleared on the next access.
- `lock_or_flag`: a poisoned VFS-state lock sets `needs_rebuild`, and the next
  `poll_refresh` performs a full rebuild that restores a consistent tree.
- transient scalar: a poisoned scalar lock recovers the inner value unchanged.

The `#[ignore]` FUSE end-to-end mount suite must remain green.

## Sequencing

1. **#82** first — stops live data loss; fully independent; can run on the plugin
   track in parallel with the Rust track.
2. **#95** — lands the error-diagnostics convention that #91/#92 will follow.
3. **#96** — largest scope; depends on nothing from #95.

(The ROADMAP lists Phase 1 as #96-then-#95; #95 and #96 are independent, so the
order is a free choice. This spec does #95 first because it is the smaller,
lower-risk change and lands the error convention sooner.)

Each change ships test-driven with the project's existing review gates.

## Out of scope

- Deduplicating the two `_core.py` plugin modules.
- `ByteBudget` reset semantics (belongs to #93).
- Migration to `parking_lot::Mutex`.
- Any other backlog issue (Phases 2–7).
