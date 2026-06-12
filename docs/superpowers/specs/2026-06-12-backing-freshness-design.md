# Strengthen backing-file freshness checks (issue #276)

## Problem

The freshness stamp that decides whether a `tracks` row still matches its
backing file is keyed only on `backing_size` plus a **whole-second** `mtime`.
The same weak stamp is collected and compared in several places:

- `musefs-core/src/scan.rs` records `meta.len()` and `mtime_secs(&meta)` taken
  from a `stat(path)` (`scan.rs:720`, `:734-735`) that is **separate** from the
  `open(path)` `probe_file` performs internally (`scan.rs:279`). The stored
  stamp and the probed bytes can therefore come from two different versions of
  the file.
- `HeaderCache::resolve` (`musefs-core/src/reader.rs:119`) and
  `validate_opened_backing` (`musefs-core/src/facade.rs:117`) reject drift only
  when size or whole-second mtime differs.
- Tests work around the whole-second truncation by forcing a distinct second
  before expecting `BackingChanged`.

Two concrete silent-drift windows follow:

1. **Same-second, same-size in-place rewrite.** A file rewritten in place with
   identical length inside the same clock second evades the guard. musefs then
   pairs a cached/synthesized metadata layout from one version with backing
   audio bytes from another.
2. **Scan-time TOCTOU.** A worker stats a path, then opens/probes it
   separately, then stores the earlier stamp. If the path changes between those
   operations the committed row combines a probe of one version with a
   validation stamp from another.

## Goals

- Make the stored backing identity strong enough that a busy *or adversarial*
  same-size rewrite is detected, without a content hash on the hot path.
- Close the scan-time race so the committed stamp and the probed bytes provably
  come from one descriptor / one inode, held still across the whole probe.
- Keep the serve path's existing doctrine: the DB is untrusted input; a
  mismatch degrades to a controlled `BackingChanged`, never undefined behavior.

## Non-goals

- No content hashing (rejected in the issue as too expensive for the hot path).
- No device/inode in the stamp. Inode/dev in a freshness key risk spurious
  `BackingChanged` across a remount or on inode-reassigning filesystems
  (network/overlay), and the size+mtime+ctime stamp already covers the in-place
  and rename-swap rewrite cases this issue is about. Deferred.
- No backwards-compatibility / migration upgrade path: there are no extant
  deployed databases, so the schema is edited in place (see Migration).

## Design

### The freshness stamp

Replace the `(backing_size, whole-second mtime)` pair with a three-integer
stamp, each timestamp stored as **nanoseconds since the Unix epoch** in a
single `INTEGER` column:

```rust
struct BackingStamp {
    size: u64,
    mtime_ns: i64,
    ctime_ns: i64,
}
```

Captured on unix from one `fstat` via `std::os::unix::fs::MetadataExt`:

```rust
let mtime_ns = meta.mtime() * 1_000_000_000 + meta.mtime_nsec();
let ctime_ns = meta.ctime() * 1_000_000_000 + meta.ctime_nsec();
```

An `i64` of nanoseconds-since-epoch is good until ~2262.

**Why ctime as well as nanosecond mtime.** Nanosecond mtime closes the
same-second window for a merely-busy writer: a normal `write()` stamps the
current time at nanosecond resolution, so a collision is effectively
impossible. It does **not** stop an adversary who resets mtime backward with
`utimensat` to the stored value. `ctime` (inode change time) is bumped by any
write and **cannot be set backward** short of clock manipulation, so it closes
the adversarial-reset case. Both come free from the same `fstat`.

`BackingStamp` lives in `musefs-core` (the integration layer) and consolidates
the three duplicated `mtime_secs` helpers (`reader.rs:73`, `facade.rs:108`,
`scan.rs:52`) into one `from_metadata(&Metadata)` constructor plus an equality
comparison. `musefs_db::Track` and `musefs_db::NewTrack` carry the two
timestamp integers; `ResolvedFile` carries the whole stamp for the per-handle
read path.

### Schema (edit in place — no new migration)

There are no extant databases, so the schema is edited in place rather than by
appending a `MIGRATION_V6`. `user_version` stays at 5, so the Picard conftest
`user_version` sanity assertion needs no bump.

In `MIGRATION_V5` (`musefs-db/src/schema.rs`), before the trigger definitions:

```sql
ALTER TABLE tracks RENAME COLUMN backing_mtime TO backing_mtime_ns;
ALTER TABLE tracks ADD COLUMN backing_ctime_ns INTEGER NOT NULL DEFAULT 0;
```

- The column is **renamed** (not silently repurposed) so a column named
  `backing_mtime` cannot end up holding nanoseconds — the unit is in the name.
- SQLite (`legacy_alter_table` off, the default) rewrites the existing
  `CHECK (backing_mtime >= 0)` to reference `backing_mtime_ns` automatically on
  `RENAME COLUMN`. Mirror it for the new column: `backing_ctime_ns >= 0`
  semantics are preserved (ctime is `>= 0`; no tighter bound).
- The `tracks_geometry_au` trigger (`schema.rs:327`) is written against the new
  name. `backing_ctime_ns` is **not** added to its `WHEN` clause — see
  Deliberate non-changes.
- Columns are `NOT NULL`; every row is written with a full stamp by the updated
  scanner. A stale developer database is simply rescanned (a `DEFAULT 0` ctime
  on any pre-existing row mismatches the real file and forces a re-probe, which
  is correct).

The schema string is the source of truth; regenerate the Python mirror
(`MUSEFS_REGEN_SCHEMA_PY=1 cargo test -p musefs-db schema_py`) and re-vendor.

### DB read/write plumbing

- `track_select!` column list (`tracks.rs:13`) and `row_to_track`
  (`tracks.rs:32`) gain `backing_mtime_ns` (renamed) and `backing_ctime_ns`.
- `upsert_track` (`tracks.rs:187`) and the bulk-writer insert (`ingest_bulk`'s
  SQL) add both columns to the INSERT column list, the `VALUES`, and the
  `ON CONFLICT DO UPDATE SET`.
- `Track` (`models.rs:124`) and `NewTrack` (`models.rs:137`) replace
  `backing_mtime: i64` with `backing_mtime_ns: i64` and add
  `backing_ctime_ns: i64`.

### Scanner — fd-based stamp + fstat sandwich

`probe_file` (`scan.rs:279`) becomes the single point that opens, stamps, and
probes one descriptor:

1. `open(path)` once.
2. `fstat` → **S1**. Its `len` is the `file_len` the probe already needs, and
   S1 is the candidate stored stamp.
3. Probe from that descriptor (existing probe logic unchanged).
4. `fstat` → **S2**.
5. If `S1 != S2` (size, mtime_ns, or ctime_ns moved) the file changed under us
   mid-probe → return a "raced" outcome; commit nothing for this file.

`probe_file` returns `Option<(Probed, BackingStamp)>`. The worker
(`run_pipeline`, ~`scan.rs:716`) stores **that** stamp instead of the
path-stat values, so the committed stamp and the probed bytes share one inode
held still across the probe. The redundant pre-probe `std::fs::metadata(&path)`
at `scan.rs:720` is removed: an `open` failure inside `probe_file` already
returns `Err`, which the worker routes to the `failed` arm.

Add a `raced` counter to `ScanStats` / `RevalidateStats` so mid-probe races are
observable and assertable. (Logged at `warn`.)

`Unit` (`scan.rs`), `ingest` (`scan.rs:507`), and `ingest_bulk`
(`scan.rs:587`) carry and persist the three-integer stamp instead of
`meta_len` / `meta_mtime`.

### Serve-path validation

Both compare sites switch to `BackingStamp`, re-stat, full three-field compare,
`BackingChanged` on any mismatch:

- `HeaderCache::resolve` (`reader.rs:110`): re-stat on every resolve
  (`reader.rs:118`), compare `BackingStamp::from_metadata(&meta)` against the
  track's stored stamp (`reader.rs:119`).
- `validate_opened_backing` (`facade.rs:115`): same, against the held
  descriptor's `file.metadata()` on every read. `ResolvedFile`
  (`reader.rs:~26`, fields `backing_size` / `backing_mtime_secs`) carries the
  full stamp, captured at resolve time (`reader.rs:~314`).

### Revalidate skip pass

The pre-dispatch skip check (`scan.rs:924-929`) compares the **full** stored
stamp `(size, mtime_ns, ctime_ns)` against the candidate file; any difference
forces a re-probe. This means a ctime-only change — e.g. an adversarial
mtime-reset after an in-place rewrite — is no longer skipped as "unchanged".
The existing `needs_backfill` FLAC-structural hook is unaffected.

### Deliberate non-changes

- `backing_ctime_ns` (and the nanosecond precision of `backing_mtime_ns`) are
  **not** added to the `tracks_geometry_au` trigger's `WHEN` clause. They are
  pure freshness identity, not inputs to synthesized bytes; their enforcement
  is the serve-path re-stat, not `content_version`. Keeping them out preserves
  `content_version`'s documented "minimal superset of served-byte inputs"
  property (ARCHITECTURE.md, V5) and avoids cache churn on byte-identical
  re-probes. A cache entry is keyed on `content_version`, but `resolve`
  re-reads the track row and re-stats *before* the cache lookup, so a changed
  stamp is always enforced regardless of `content_version`.

## Testing

- **Same-second sub-second rewrite** → `BackingChanged`. Removes the test-only
  "force a distinct second" workaround.
- **Adversarial reset**: rewrite in place, then `utimensat` mtime back to the
  stored value → still caught via `ctime_ns`.
- **Scan-time mutation**: mutate the file between S1 and S2 (a probe hook) →
  `raced`, no row committed.
- **Revalidate ctime-only change**: file whose ctime moved but size+mtime did
  not is re-probed, not skipped.
- **Serve-path per-read guard**: `validate_opened_backing` rejects a held
  handle after a same-size sub-second rewrite.
- Schema mirror regenerated; full workspace suite green, including
  `cargo test -p musefs-core --features metrics` (the stat/open-count
  assertions: the extra scan-time `fstat` and the removed path-stat shift
  scanner stat counts — update expectations) and the FUSE e2e tier where
  relevant.
- `cargo +nightly fuzz build` if any format-layer signature is touched (it is
  not expected to be).

## Documentation updates

- `ARCHITECTURE.md`: the freshness sections (the `tracks` column list at
  `:157-158`, "What musefs defends at serve time" at `:193-203`, and "Freshness:
  two version counters" at `:249-268`) describe the stamp as size + mtime;
  update to size + nanosecond mtime + ctime, and note the scanner now stamps
  from the probed descriptor with a stat sandwich.
- The external-writer contract's scanner-owned column list (`ARCHITECTURE.md`
  "Ownership") replaces `backing_mtime` with `backing_mtime_ns` and adds
  `backing_ctime_ns`.

## Affected files

| File | Change |
| ---- | ------ |
| `musefs-db/src/schema.rs` | V5 rename + add column; trigger + CHECK; Python mirror regen |
| `musefs-db/src/models.rs` | `Track`, `NewTrack` stamp fields |
| `musefs-db/src/tracks.rs` | `track_select!`, `row_to_track`, `upsert_track`, bulk insert |
| `musefs-core/src/reader.rs` | `BackingStamp`, `ResolvedFile`, `resolve` compare |
| `musefs-core/src/facade.rs` | `validate_opened_backing` compare; drop dup `mtime_secs` |
| `musefs-core/src/scan.rs` | `probe_file` fd-stat sandwich, `raced` counter, `Unit`/`ingest`/revalidate skip; drop dup `mtime_secs` |
| `ARCHITECTURE.md` | freshness + contract docs |
| Python schema mirror + vendored copy | regenerate |
