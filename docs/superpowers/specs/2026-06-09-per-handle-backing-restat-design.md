# Per-handle read path re-stats the backing file (issue #186)

**Date:** 2026-06-09
**Issue:** [#186](https://github.com/Sohex/musefs/issues/186) — Per-handle read
path skips backing re-stat: in-place backing rewrite under a live fd serves
stale-offset bytes silently.

## Problem

The per-handle read fast path in `Musefs::read_into`
(`musefs-core/src/facade.rs`) never re-validates the backing file between
DB-side refreshes. An in-place rewrite of a backing file while a handle is open
(same inode — `rsync --inplace`, btrfs dedup/reflink swap, in-place re-encode),
with no corresponding DB change, causes positioned reads to splice the new
file's bytes at the *old* layout's audio offsets: silent wrong bytes, no error.

This violates the cardinal invariant (served audio is byte-identical to a
coherent backing file) and contradicts the freshness guarantee documented at
`ARCHITECTURE.md:178-181` ("**every** resolve re-stats the backing file and
errors with `BackingChanged` if its size or mtime drifted").

### Why the existing signals miss it

The fast path only re-resolves when:
- `refresh_gen` advances (a DB refresh landed), or
- for binary-tag layouts, `content_version` drifts.

Both are **DB-side** signals. A pure backing-file rewrite touches neither, so
the path goes straight to `read_at_with_file_into` → `read_segments_into` →
`f.read_exact_at(...)` at cached offsets, with no `stat`, no size/mtime check.

### Failure modes (from the issue)

- **Truncate-shorter:** `read_exact_at` past the new EOF errors out (raw io
  error).
- **Same-length rewrite**, or **rewrite-longer** where old offsets stay in
  bounds: positioned reads return wrong bytes with **no error** — the Inline
  metadata region comes from the old DB-derived layout while audio is read at
  old offsets from the new file (incoherent splice, silent corruption).

Atomic-replace (temp file + `rename`) is **not** affected: the held fd stays on
the old unlinked inode, which remains coherent with the old layout.

## The validation primitive already exists

`validate_opened_backing(file, resolved)` (`facade.rs:110`) does an `fstat` on
an already-open fd and compares `len`/`mtime_secs` against the captured
`resolved.backing_size`/`resolved.backing_mtime_secs`, returning
`CoreError::BackingChanged` on drift. It is called exactly once today — in
`open_handle` (`facade.rs:1070`). The fix is to call it again on the hot path.

## Approach

Add a single `validate_opened_backing(&h.file, r)?` call inside the per-handle
fast path of `read_into`, on each retry iteration, **after** loading the
resolved layout (`let r: &ResolvedFile = &resolved;`) and **before** the
`self.pool.with(...)` block that serves the read.

```rust
let resolved = h.resolved.load();
let r: &ResolvedFile = &resolved;
validate_opened_backing(&h.file, r)?;   // re-stat the held fd; BackingChanged on drift
let served = self.pool.with(|db| -> Result<Option<()>> { /* unchanged */ })?;
```

### Why this placement

- **Reuses the proven primitive.** Same function `open_handle` already relies
  on; same `fstat`-on-fd (no path traversal).
- **Error propagates immediately, not through the retry loop.** A genuine
  out-of-band backing rewrite is terminal — `?` returns `BackingChanged`
  straight to the caller. It must *not* be treated as a stale-layout/DB-retag
  race (which the loop retries); the loop is for DB-side version drift only.
- **Independent of the DB.** A filesystem stat does not belong inside the
  `pool.with` / `begin_read` snapshot, so it sits just before it. It runs the
  same way for both the plain and the `has_binary_tag` branches.
- **Runs every read.** The `fstat` is on an already-open fd (~microseconds),
  negligible next to the pread and FUSE round-trip.

### Why not push the stat into `read_segments_into`

`read_segments_into` is shared with the fallback path (`read_at_into`), which
has already validated via `resolve()` and opened a fresh fd. Statting there
would either double-stat the fallback or require a flag to suppress it — added
branching on a hot, shared path for no benefit.

### Why "stat every read" over "stat only on backing-touching reads"

Considered skipping the stat for reads landing entirely in the small `Inline`
metadata header (those bytes are DB-derived and correct regardless of the
backing). Rejected: a synthesized audio file is a small inline header followed
by one large `BackingAudio` segment to ~EOF, so nearly every read of any size
overlaps a backing segment. The optimization would spare the cheap `fstat` on
only the first read or two of a file, at the cost of read-range/segment-overlap
branching on the hot path. Not worth it.

### Why "stat every read" over a throttled / time-windowed re-stat

A throttle (stat at most once per handle per N ms) would open a bounded window
where stale bytes are served — relaxing the cardinal invariant. Off the table.

## Effect on the reported failure modes

Because the `fstat` runs *before* any `read_exact_at`:

- **Rewrite-longer** → `len` differs → `BackingChanged`. ✓
- **Same-length rewrite** → caught when the new `mtime_secs` differs from the
  captured one. The captured `backing_mtime_secs` comes from the DB scan
  (typically well in the past) and a live rewrite stamps a fresh mtime, so this
  differs in virtually all real cases. ✓
- **Truncate-shorter** → `len` differs → `BackingChanged`, caught by the stat
  *before* the pread. This **normalizes** the previous raw-io-error behavior to
  `BackingChanged` with no special-casing. ✓

### Residual, documented-as-inherent limitations

- **Seconds-granularity mtime.** `mtime_secs` is whole-seconds, shared with
  `resolve()`. A same-length rewrite landing in the *exact same wall-clock
  second* that was captured evades detection. Pre-existing; not introduced by
  this path. Closing it would require nanosecond mtime through the scanner and
  the DB schema — out of scope (YAGNI for this fix).
- **Stat→pread TOCTOU.** A rewrite landing in the narrow window *between* the
  `fstat` and a subsequent `read_exact_at` within one read call can still
  surface a raw io error (e.g. truncate past EOF). No stat-then-read scheme can
  close this; documented as inherent rather than normalized.

## Testing

Add a test to `musefs-core/tests/facade.rs` (which already drives
`open_handle` + `read_into` through the full facade) that exercises validation
**through the per-handle read path** — the exact gap the issue names. Existing
tests (`resolve_errors_when_backing_file_changes` at `tests/reader.rs:96`, the
`validate_opened_backing` unit test at `facade.rs:1085`) exercise validation
directly, never via `open_handle -> read`.

Test shape:

1. Build a store + backing file; `open_handle(inode)`.
2. First `read_into(.., Some(fh), ..)` succeeds (warms the per-handle path).
3. In-place rewrite the backing file at the **same inode** (open the existing
   path for write / truncate, not temp+rename), in two variants:
   - **rewrite-longer or same-length** with an advanced mtime → assert the next
     `read_into` on the *same handle* returns `CoreError::BackingChanged`.
   - **truncate-shorter** → assert the next `read_into` on the same handle also
     returns `CoreError::BackingChanged` (confirms normalization, not a raw io
     error).
4. (Optional) Assert no DB change occurred between the reads, to make explicit
   that the detection is backing-side, not DB-side.

mtime control: set the backing file's mtime explicitly (e.g. via
`std::fs::File::set_times` / `filetime`) so the same-length variant is
deterministic and not dependent on test wall-clock crossing a second boundary.

## Docs

`ARCHITECTURE.md:178-181` currently describes the freshness guarantee in terms
of `resolve()`; after this fix the per-handle hot path honors the same
guarantee. Add a one-line note that the per-handle read path also re-stats the
held fd on every read, so the documented guarantee holds on the hot path and
not only through `resolve()`.

## Scope / non-goals

- No change to `read_segments_into`, `read_at_with_file_into`, `resolve`, or the
  fallback (`read_at_into`) path.
- No mtime-granularity change (no scanner/schema change).
- No new error variant — reuse `CoreError::BackingChanged`.
- No change to atomic-replace behavior (already correct).

## Files touched

- `musefs-core/src/facade.rs` — one `validate_opened_backing` call in
  `read_into`'s fast path.
- `musefs-core/tests/facade.rs` — new through-the-handle test.
- `ARCHITECTURE.md` — one-line freshness note.
