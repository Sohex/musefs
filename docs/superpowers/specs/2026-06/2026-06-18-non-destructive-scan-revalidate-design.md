# Non-destructive `scan` / `revalidate` — design

**Date:** 2026-06-18
**Status:** Draft for review

## Problem

Re-running `musefs scan <dir>` over an already-scanned library **silently
destroys curated tags**. The scan pipeline re-probes every file and routes it
through `ingest_into`, which calls `replace_tags` (`musefs-core/src/scan.rs`):
the track's DB tags — the layer external tools (Picard, beets) and the user
curate — are blown away and reseeded from the file's frozen embedded metadata.

The store is the source of truth for tags/art; the backing file is the source
of truth only for its audio bytes. The default `scan` inverts that contract.
`scan --revalidate` (the skip-unchanged maintenance mode) preserves DB edits for
*unchanged* files, but still clobbers any file whose stamp changed, still prunes
gone tracks by default, and is an opt-in flag most users never reach for. The
`--restore-backing` flag is unrelated — it is a beets-only merge-semantics
control, not a guard against core re-ingest.

**Guiding principle (user):** no bare command may ever delete or overwrite user
data. Overwriting curated tags or deleting tracks must require an explicit
confirmation flag.

## Decisions (settled in brainstorming — not re-litigable)

1. Tags/art/binary-tags (Layer B) are seeded from a file's embedded metadata
   **once**, at first ingest. Thereafter the DB is authoritative; only an
   external writer or an explicit `--force` may overwrite them.
2. `scan` operates on files **not** in the DB; `revalidate` operates on tracks
   **already** in the DB. They are separate verbs (separate subcommands).
3. `--force` (scan-only) is the **only** path that overwrites Layer B.
4. `--prune` (revalidate-only) is the **only** path that deletes tracks.
5. Binary tags are Layer B (preserved), consistent with the beets merge contract.
6. A **moved** file (same bytes, new path) retargets the existing row, preserving
   Layer B — this happens under bare `scan` and is not gated by `--force`.

## The two-layer data model

Every track's stored data splits cleanly into two layers:

**Layer A — structural / serving facts.** Derived from the actual audio bytes;
must stay in sync with the file or playback breaks.
- `audio_offset`, `audio_length`
- backing stamp (size / mtime / ctime)
- checksums (fingerprint, content hash)
- FLAC structural blocks (`STREAMINFO`, `SEEKTABLE`)

**Layer B — curated metadata.** The DB is authoritative; external writers own it.
- text tags
- art / pictures
- binary tags (`APPLICATION` / `CUESHEET`, opaque ID3 frames, M4A freeform atoms)

Binary tags are classified as Layer B (preserved): the beets contract already
treats them as curated metadata that survives merges and deletes.

## Command model

**`scan` operates on files _not_ in the DB. `revalidate` operates on tracks
_already_ in the DB.**

| Command | File not in DB | In DB, unchanged | In DB, changed on disk | Track whose file is gone |
| --- | --- | --- | --- | --- |
| `scan` | **ingest** | skip | skip | leave |
| `scan --force` | ingest | re-probe + **clobber Layer B** | re-probe + **clobber Layer B** | leave |
| `revalidate` | skip | skip | refresh **Layer A only**, preserve Layer B | leave |
| `revalidate --prune` | skip | skip | refresh Layer A, preserve Layer B | **delete** + orphan-art GC |

Invariants:
- No bare command deletes or overwrites user data.
- `--force` (scan-only) is the **only** path that overwrites curated Layer B —
  it re-seeds tags/art/binary-tags from the file. `scan --force <one-file>` is
  the surgical "re-read this file's tags from disk" escape hatch.
- `--prune` (revalidate-only) is the **only** path that deletes tracks. Orphan-art
  GC piggybacks on it (it only removes art nothing references, so it is not
  independent data loss).
- **Moved file** (a path not in the DB whose bytes uniquely match an existing
  track whose old backing file is gone): bare `scan` retargets the existing row
  via `retarget_track` (path/stamp/bounds/checksum `UPDATE`), **preserving Layer
  B**. This is existing `ingest_unit` behavior and is *not* gated by `--force`.
  A moved file is therefore not "ingested fresh" and does not lose curated tags.

### Accepted edge

Re-encoding a file in place under a stable path leaves a stale `CUESHEET`/binary
tag (Layer B, preserved) paired with refreshed structural blocks until a
`scan --force`. Revalidate *closes* the Layer-A staleness window (structural
blocks, bounds, and checksums refresh), so the only residual inconsistency is a
re-encode that also changed an embedded binary tag — which `--force` reconciles.
Re-encoding under a stable path is already off-model.

## CLI surface

`revalidate` becomes its own subcommand, matching the two-verb mental model:

- `musefs scan <targets...> [--force] [--jobs] [--follow-symlinks] [--quiet] [--checksum] [--fast] [--strict]`
- `musefs revalidate <targets...> [--prune] [--jobs] [--follow-symlinks] [--quiet] [--checksum]`

`--fast` / `--strict` govern moved-file retarget confirmation, which only happens
on an unknown-path insert — a scan concern. `revalidate` processes only known
paths and never retargets, so it omits them. It keeps `--checksum` (it backfills
missing checksums).

`--force` gets `MUSEFS_FORCE`; `--prune` gets `MUSEFS_PRUNE` (boolish parsers,
matching the existing flag conventions).

### Deprecated `scan --revalidate` alias

The current `scan --revalidate` flag and its `MUSEFS_REVALIDATE` env var are kept
for **one release** as a deprecated alias that forwards to the new `revalidate`
path, then removed (target: the release after this one — record the version in
the changelog). Mechanics the implementation must pin:

- **Behavior shift is intentional and must warn loudly.** The alias inherits the
  *new* `revalidate` semantics: it no longer prunes by default and no longer
  ingests new files. Anyone scripting `scan --revalidate` for cleanup silently
  stops pruning, so the deprecation warning must say so explicitly (e.g.
  *"`scan --revalidate` is deprecated; use `revalidate` (now non-pruning — add
  `--prune` to delete gone tracks). This alias will be removed in vX.Y."*) and
  the changelog must call it out beyond a one-liner.
- **Flag conflicts (error, not silent precedence):**
  - `scan --force --revalidate` → hard error (clobber vs preserve are
    contradictory).
  - `--prune` is rejected by the `scan` subcommand entirely (revalidate-only).
  - `--force` is rejected by the `revalidate` subcommand entirely (scan-only).
- **Env-var binding:** `MUSEFS_REVALIDATE` stays bound to `scan` (the alias)
  only; the standalone `revalidate` subcommand gets no env equivalent for it.
  `MUSEFS_FORCE` binds to `scan`; `MUSEFS_PRUNE` binds to `revalidate`.

## Implementation

### Layer-A-only write path (`musefs-core/src/scan.rs`)

Today `ingest_into` writes everything in one shot: `upsert_track` +
`set_track_checksums` + `replace_tags` + `set_binary_tags` +
`set_structural_blocks` + `set_track_art`.

Add a sibling that writes **Layer A only** — `upsert_track` +
`set_track_checksums` + `set_structural_blocks` — and skips the three Layer-B
writes (`replace_tags`, `set_binary_tags`, `set_track_art`). This is trivial: by
the time the writer runs, `Probed` already carries separate `structural_blocks`,
`binary_tags`, `tags`, and `pictures` fields (the FLAC `split_preserved` runs at
*probe* time, `scan.rs:~422`), so the Layer-A path simply reads
`probed.structural_blocks` and ignores the rest — no FLAC special-casing.

### Write policy in the pipeline

The pipeline writer calls `ingest_unit` (not `ingest_into` directly) —
`ingest_unit` is the branch point that either upserts a known path
(`ingest_into`) or **retargets** a moved file (`retarget_track`, Layer-B
preserving). Parameterize the write **at the `ingest_unit` level**, not
`ingest_into`, so retarget is preserved across modes:

- **Scan, not `--force`:** a **new** main-thread pre-filter (a cheap
  path-membership check — load the existing `backing_path` set once and skip
  probing any file already present; *not* a reuse of `revalidate_with`'s richer
  stamp-comparison filter). Surviving units are either genuinely new (→
  `ingest_into`) or **moved** (unknown path, unique fingerprint match → retarget,
  Layer B preserved). `--force` does **not** change retarget behavior.
- **Scan, `--force`:** no pre-filter; probe everything. Known paths take the
  `ingest_into` (full, Layer-B-clobbering) branch instead of being skipped;
  new/moved files behave as above. This is today's destructive full re-import.
- **Revalidate:** the existing main-thread skip-unchanged pass (stat vs stored
  stamp) now yields only changed files **already in the DB**; new-on-disk files
  are *not* added to the changed set (that is `scan`'s job). Each changed unit
  takes the **Layer-A** write path. Because revalidate only feeds known paths,
  the retarget branch never fires.

`run_pipeline` gains the write-policy parameter; thread it through **both**
production callers — `scan_directory_with` and `revalidate_with` — plus
`scan_directory_full_oracle` if it drives the pipeline, in one compilable commit.

### Backfill must use the Layer-A path (fixes a current bug)

Revalidate today re-ingests FLAC-V1 tracks (missing structural blocks) and
checksum-less tracks even when the backing file is byte-identical — and routes
them through `ingest_into`, which calls `replace_tags`/`set_track_art`/
`set_binary_tags`. **That is a live data-loss bug: a backfill clobbers curated
Layer B today.** Both the structural backfill and the checksum backfill must run
through the new Layer-A path (a checksum backfill sets `fingerprint`/
`content_hash` — itself a Layer-A write), so a backfill refreshes structure
without ever touching Layer B.

### Stats / reporting

`scan` reports ingested (new) and a count of files skipped because they are
already in the DB. **Use a distinct counter** (e.g. `already_present`) — do not
overload `ScanStats.skipped`, which already means *unsupported-extension* files
(`scan_directory_with` sets `stats.skipped = tally.total`); conflating the two in
the summary line would be misleading. `revalidate` keeps
`updated / unchanged / pruned / failed`, with `pruned` always 0 unless `--prune`.

## Companion change: contrib wrappers + beets autoscan

The subprocess invocation is built by a shared `run_scan` wrapper that is
**vendored in two places** (a CI drift-test keeps the Picard copy in sync), both
hardcoding the `scan` verb and `--revalidate`:

- `contrib/python-musefs/src/musefs_common/scan.py` (`run_scan`, builds
  `[binary, "scan", *targets, "--db", db]`, appends `--revalidate`).
- `contrib/picard/musefs/_common/scan.py` — re-vendored copy of the same.

Update `run_scan` to emit the new CLI directly (first-party code ships with this
change — no reason to route our own callers through the deprecated alias):

- gain a `force=False` param → append `--force` when set;
- `revalidate=True` → emit the **`revalidate`** subcommand (`[binary,
  "revalidate", *targets, "--db", db]`), not `scan --revalidate`. Add a `prune`
  param if a caller needs it (beets' `--revalidate` option forwards prune intent).

Then **re-vendor** the Picard copy and run the drift-test, or CI fails.

Callers:
- `contrib/beets/beetsplug/musefs.py` `_run_scan` autoscan path calls bare
  `musefs scan` to reset existing tracks to backing tags before re-merging its
  managed set `M`. Bare `scan` no longer resets, so the autoscan call must pass
  `force=True` (→ `scan --force`).
- The beets `--revalidate` option (`musefs.py:75`) forwards to `run_scan(...,
  revalidate=True)` → the new `revalidate` subcommand. Decide whether it also
  forwards `--prune` (its help text mentions pruning); if so, thread `prune`
  through.

Picard's *sync* path is otherwise unaffected (it writes the DB directly;
full-replace is correct there) — only its vendored `run_scan` copy changes.

## Docs to update

- `docs/src/architecture/tree-scanning.md` — scan vs revalidate semantics, two-layer model.
- `docs/src/architecture/store.md` — the external-writer contract note on what re-scan touches.
- `README.md` — `scan` / `revalidate` / `--force` / `--prune` usage.
- `docs/src/changelog.md` — **breaking**: bare `scan` is now additive; old full-reimport is `scan --force`; `revalidate` is a subcommand (no longer prunes by default); `scan --revalidate` deprecated.
- `CLAUDE.md` (everyday commands) if scan invocation examples change.
- Vendored wrapper docstrings (`musefs_common/scan.py`, Picard `_common/scan.py`) — update to the new verbs and re-vendor.
- Contrib docs that reference `scan --revalidate` (e.g. `docs/src/integrations/*`, beets/Picard help text).

## Verification

Each assertion below must be a checkable test.

- **Layer split (`musefs-core`):** seed a track, edit its DB tags, then assert:
  (a) bare `scan` again → tags byte-identical, track row untouched; (b) bump the
  backing file's stamp + `revalidate` → `audio_offset`/`audio_length`/stamp/
  structural blocks/checksums refreshed, tags+art+binary-tags byte-identical;
  (c) `scan --force` → tags reseeded from the file (changed).
- **Additive scan:** add a new file under an already-scanned root, `scan` →
  exactly the new file ingested, existing rows untouched, and the pre-filter does
  not re-probe existing files (assert via probe count / the `already_present`
  counter).
- **Moved file under bare `scan`:** rename a file (old path gone), `scan` (no
  `--force`) → the existing row is retargeted to the new path and its edited DB
  tags survive (regression guard for B1 — bare scan must not clobber a move).
- **Revalidate scope:** a new-on-disk file under a revalidated root is **not**
  ingested by `revalidate`; only `scan` picks it up.
- **Prune gating:** delete a backing file → `revalidate` leaves the row;
  `revalidate --prune` deletes it and GCs now-orphan art.
- **Backfill non-destructive (regression for a current bug):** a V1 FLAC track
  with edited DB tags, `revalidate`d while the backing file is byte-identical →
  structural blocks (and any missing checksums) gained, **tags/art/binary-tags
  unchanged**. This fails on `main` today (backfill routes through
  `replace_tags`).
- **Deprecated alias:** `scan --revalidate` forwards to the revalidate path,
  prints the deprecation warning, does **not** prune; `scan --force --revalidate`
  errors; `scan --prune` and `revalidate --force` error.
- **beets:** autoscan reset still works against `scan --force` (existing beets
  suite + the reset-to-backing assertion); `run_scan(revalidate=True)` emits the
  `revalidate` subcommand. Re-vendor drift-test passes for the Picard copy.
- **metrics feature:** re-run `cargo test -p musefs-core --features metrics`
  (getattr/read counts unaffected, but scan-path code moved).
- **fuzz crate:** `cargo +nightly fuzz build` if any format-layer signature
  shifts (out-of-workspace, breaks silently otherwise).

## Out of scope

- A combined "full sync" verb (new + changed + prune in one command). The
  workflow is `scan` then `revalidate --prune`; keeping them separate is the
  point.
- Changing Picard's full-replace semantics.
- Re-classifying binary tags as structural.
