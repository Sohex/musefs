# Phase 6 — Performance optimization SPs (#69, #67, #68, #70)

*Date: 2026-06-03. Status: approved design.*

Phase 6 of the open-issue backlog (`docs/ROADMAP.md`) closes the four
bench-tracked performance residuals left by the 2026-05-30 optimization pass:

| Issue | Area | Origin |
|---|---|---|
| #69 | Refresh: `rebuild_incremental` is O(library), not O(changed) | SP2 residual |
| #67 | Scan: every front-anchored file pays an unused ID3v1 tail read | SP1 residual |
| #68 | Scan: `ingest_bulk` clones picture/binary-tag/structural-block bytes | SP1 residual |
| #70 | Serve: art/binary chunks pass through intermediate buffers; per-read `Vec` alloc | SP3 deferred |

## Structure

One spec (this document), three sequential PRs grouped by code area:

1. **PR 1 — #69** (refresh): changelog migration + in-place snapshot mutation.
   First because it carries the schema migration; the other PRs are oblivious
   to it.
2. **PR 2 — #67 + #68** (scan pair): both live in `scan.rs`, were surfaced by
   the same SP1 bench run, and share `bench_ingest` as their measurement.
3. **PR 3 — #70** (serve): chunk direct-write + per-worker output-buffer reuse.

The cardinal invariant binds every PR: **original audio bytes are never copied
or modified, and served audio stays byte-identical.**

## PR 1 — #69: refresh O(library) → O(changed)

### Problem

A single-track re-tag triggers `rebuild_incremental`
(`musefs-core/src/facade.rs`), which performs two O(N) steps before the
O(changed) `apply_changes`: a full `Db::list_render_keys` identity scan
(`SELECT id, content_version, format FROM tracks`) and a from-scratch
`new_snapshot` `HashMap` rebuild that clones the cached rendered path for every
unchanged track. Refresh latency scales with library size.

### Schema: `MIGRATION_V3` — trigger-maintained changelog

Every metadata change already funnels through an `UPDATE` on the `tracks` row:
the V1 triggers on `tags` / `track_art` bump `tracks.content_version`, and
`structural_blocks` is written only by the scan, which also updates the tracks
row. So a changelog needs triggers on `tracks` alone.

```sql
CREATE TABLE track_changes (
    seq      INTEGER PRIMARY KEY AUTOINCREMENT,
    track_id INTEGER NOT NULL
);
-- AFTER INSERT / AFTER UPDATE / AFTER DELETE ON tracks:
--   INSERT INTO track_changes (track_id) VALUES (NEW.id / OLD.id);
-- AFTER INSERT ON track_changes (self-pruning bounded ring):
--   DELETE FROM track_changes WHERE seq <= NEW.seq - CAP;   -- CAP = 8192, literal in the migration SQL
```

The ring is maintained by whoever writes (scan, plugins, raw SQL) — the
mount's read-only WAL connections never write it. `AUTOINCREMENT` guarantees
monotonic, never-reused seqs; rows are deleted only from the old end, so the
retained seq range is contiguous.

The design depends on SQLite **nested** trigger activation: `tags_ai` /
`track_art_*` do `UPDATE tracks …`, which must fire the new `tracks` changelog
trigger, whose `INSERT` in turn fires the prune trigger. Nested (non-cyclic)
activation is on by default — it is distinct from `PRAGMA recursive_triggers`,
which nothing in the codebase sets — but the dependency is load-bearing, so PR
1 adds a schema test: a bare `INSERT INTO tags` must produce a `track_changes`
row.

Migrations are append-only: `MIGRATIONS` grows to length 3, `user_version`
2→3. Contract mirror: bump `EXPECTED_USER_VERSION` in
`contrib/python-musefs/src/musefs_common/constants.py`, re-vendor into the
Picard plugin (`vendor_to_picard.py`); the drift-guard test enforces it.

### Refresh path

`Musefs` keeps an in-memory `last_seq` watermark beside `last_data_version`.
On a `data_version` bump:

1. `SELECT DISTINCT track_id FROM track_changes WHERE seq > :last_seq`, plus
   the table's `MAX(seq)` and oldest retained seq.
2. **Gap check:** if the oldest retained seq is `> last_seq + 1`, the mount
   slept past the ring (e.g. a bulk scan wrote more than CAP rows). Fall back
   to the existing full `list_render_keys` path — which is retained verbatim
   as both the fallback and the fresh-mount initial-build path. A bulk-change
   gap makes the full rebuild the right answer anyway.
3. Otherwise fetch render keys for only the changelog ids
   (`SELECT id, content_version, format FROM tracks WHERE id IN (…)`) and
   partition against the retained snapshot:
   - in changelog, present in `tracks`, present in snapshot → **changed**
     (skipped if the render key `(content_version, format)` is unchanged — a
     no-op touch);
   - present in `tracks`, absent from snapshot → **added**;
   - in changelog, absent from `tracks` → **removed**.
4. **In-place snapshot mutation:** the snapshot is mutated (insert/update
   changed+added with freshly rendered state, remove removed) instead of being
   reconstructed. No clones for unchanged tracks. `apply_changes` on the tree
   is already O(changed).
5. `last_seq` advances **only after** a successful rebuild — the same
   stamp-after-success discipline `last_data_version` uses. A failed refresh
   leaves both unstamped so the next poll retries.

**Post-rebuild maintenance must also become ChangeSet-driven, or the O(N) just
moves.** Two consumers currently scan full structures after a rebuild and are
rescoped to the changed/removed sets, preserving observable behavior:

- `notify_changed` (`facade.rs`) iterates both the old and new snapshots to
  find inodes needing `--keep-cache` invalidation. In-place mutation removes
  the "old snapshot" it compares against, so it is reworked to take the
  ChangeSet plus the displaced old states — `HashMap::insert`/`remove` during
  the in-place mutation return exactly the old `TrackRenderState`s for
  changed/removed ids, which is all the old-side information `notify_changed`
  uses (content-version rise with stable path → new inode; path moved or track
  gone → old inode; added tracks notify nothing).
- The header/size cache pruning currently retains against the full live track
  set (`tree.track_ids()` + `retain`); on the changelog path it instead
  removes exactly the removed ids. (The full-scan prune remains on the
  fallback/fresh-mount path.)

The `cfg(debug_assertions)` equivalence reference build stays O(N) by design —
it is the oracle, not the product.

Fresh mount: full build, then `last_seq = COALESCE(MAX(seq), 0)`. Multiple
mounts of one DB each keep an independent in-memory watermark; reads don't
contend and pruning is writer-side.

### Error handling

No new conventions. The gap fallback is normal control flow, not an error. A
changelog-query failure propagates as the existing `CoreError`-wrapped DB
error with `last_seq`/`last_data_version` unstamped — identical semantics to
today's refresh failures.

### Testing & acceptance

- The SP2 equivalence machinery (64-case proptest + per-refresh debug-assert
  vs `build_with`) extends to cover the changelog path.
- A forced-gap test exercises the fallback (write > CAP changelog rows, assert
  full rebuild and correct tree).
- `bench_refresh` library-size sweep (100 / 1000 / 5000) goes **flat**:
  refresh-1 at 5000 tracks ≈ refresh-1 at 100.
- FUSE byte-identical e2e green.

## PR 2 — #67 + #68: scan pair

### #67 — lazy ID3v1 tail

`probe_file` (`musefs-core/src/scan.rs`) calls `read_tail_128` before format
dispatch for every front-anchored file; only the MP3 arm of `probe_prefix`
consumes the tail. FLAC/Ogg/WAV pay one syscall + 128 bytes per file for
nothing.

Fix: replace the eager read with a memoized lazy lookup. `probe_file` holds a
`tail: Option<Option<[u8; 128]>>` slot (outer `Option` = not yet read, inner =
file shorter than 128 bytes), filled on first request and persisting across
the widen-retry loop so MP3 never reads it twice. `probe_prefix` already
takes `file_len`; the only signature change is swapping today's
`tail: Option<&[u8; 128]>` for the `&File` plus the memo slot, with the MP3
arm filling the slot on first use.
`metrics::on_scan_read(128)` fires only when the read happens. MP3 behavior is byte-for-byte unchanged; the `probe_full`
fallback is unaffected (it reads the whole file, tail included).

### #68 — move, don't clone, ingested bytes

`ingest_bulk` takes `&Probed`, forcing `pic.data.clone()` per picture — and,
the same pattern in the same function, `b.payload.clone()` per binary tag and
`body.clone()` per structural block. The issue names pictures; all three are
in scope since they are one fix.

Fix: the pipeline batch already owns its `Probed`s — hand them to
`ingest_bulk` by value (drain the batch) and **move** the byte buffers into
`NewArt` / `BinaryTag` / `StructuralBlock`. No DB-layer signature changes:
those structs already own `Vec<u8>`; the clones exist only because the caller
holds a borrow. One ordering constraint: the byte-budget backpressure releases
each unit's `weight` **after** `bw.commit()`, so the weight must be captured
before the `Probed` is moved out of the batch.

### Testing & acceptance

- The bounded≡full probe-equivalence gate and byte-identical PCM e2e stay
  green.
- `bench_ingest` before/after per format: #67 shows as −1 `pread` / −128
  `bytes_read` per non-MP3 file; #68 as wall-time improvement on art-heavy
  corpora.
- No format breaches the >10% regression-gate convention.

## PR 3 — #70: serve-path copies

### Half A — chunk direct-write

`read_segments` (`musefs-core/src/reader.rs`) already reads `BackingAudio`
into `out`'s reserved tail (SP3); the DB-blob arms still allocate an
intermediate `Vec` per chunk via `read_art_chunk` / `read_binary_tag_chunk`.

Fix: add `Db::read_art_chunk_into(art_id, offset, &mut [u8])` and
`Db::read_binary_tag_chunk_into(…)` — same SQLite incremental-blob I/O,
reading into the caller's slice, preserving `read_at_exact`'s
short-read-is-an-error contract. Convert the three raw arms (`ArtImage`,
`BinaryTag`, `OggArtSlice { base64: false }`) to the resize-then-read-into
pattern. The **base64 arm keeps an input buffer**: it is a genuine transform
(raw bytes in, base64 chars out) with a bounded input window. The allocating
`read_art_chunk` remains for non-hot-path callers.

### Half B — per-worker output-buffer reuse

**Finding that rescopes the issue:** fuser 0.17's `ReplyData::data` sends a
borrowed iovec (`ResponseSlice(&[u8])` → `with_iovec` → vectored write); there
is **no userspace copy at the fuser layer**. The remaining kernel-boundary
`writev` cannot be eliminated from userspace. The real cost is the fresh
per-read `Vec` allocation (up to `max_readahead`, typically 128 KiB–1 MiB).

Fix: add `Musefs::read_into(ino, fh, offset, size, &mut Vec<u8>)` (clear +
fill) — and thread the `&mut Vec<u8>` **all the way down to `read_segments`**,
where the allocation actually lives (`reader.rs`): `read_at` /
`read_at_with_file` / `read_segments` gain `*_into` forms and the existing
`Vec`-returning names become thin delegating wrappers for their current
callers and tests. Stopping the buffer at the facade layer would add a copy
instead of removing one. Each FUSE worker gets a `thread_local!`
`RefCell<Vec<u8>>` scratch buffer; the read path fills it and replies from it.
Memory stays bounded — one buffer per bounded pool worker, with a `shrink_to`
cap (~2 MiB) so one giant read doesn't pin memory.

When the PR lands, comment on #70 that the claimed reply-buffer copy does not
exist at fuser 0.17.

### Testing & acceptance

- `sequential_read` medians held-or-improved across all six formats (every
  corpus file's front exercises the chunk arms); `concurrent_read_walk` held;
  ogg `cold_first_read` / `seek_read` held (SP4's gate).
- Byte-identical gate: `proptest_read_fidelity`, `musefs-format --features
  fuzzing`, FUSE e2e PCM-sha — all green.
- The `--features metrics` serve-site counter tests keep passing unchanged:
  counters fire identically; only the buffer destination changes.

## Cross-cutting validation (every PR)

- Before/after benchmark tables in **`BENCHMARKS.md` only** (full per-format
  table + reproduce commands, matching the existing section style). The
  2026-05-30 tracking README's results log was scoped to that pass and is not
  updated for Phase 6.
- Local in-diff mutation gate mirroring CI (`TMPDIR` under `/home`, diff
  sanity-checked non-empty, 0 missed); CI's gate remains authoritative.
- `cargo fmt --all --check`, workspace tests + clippy, and the `#[ignore]`
  FUSE e2e suite on `/dev/fuse` for PRs 1 and 3 (PR 2 runs the scan-side e2e:
  bounded≡full + PCM-sha).
- The >10% `sequential_read` median rise is a regression for any PR touching
  the read path.

## Docs riders

- PR 1: fix the stale "SP4 | Not started" status row in
  `docs/superpowers/specs/2026-05-30-optimization-pass/README.md` (SP4 shipped
  2026-06-01).
- Each PR strikes through its ROADMAP Phase 6 entries in the Phase 0–5 style
  and closes its issues via the PR body; PR 3 adds the fuser-0.17 correction
  comment on #70.

## Out of scope

- True zero-copy into the kernel (`splice`/`FUSE_PASSTHROUGH`-style) — not
  exposed by fuser 0.17.
- Changelog-based change detection for anything beyond refresh (e.g. cache
  invalidation already rides `content_version`; unchanged).
- Parallelizing the per-changed-track render (SP2 YAGNI item; the changed set
  is small by assumption).
