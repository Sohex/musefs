# Scan counter semantics (#301) + symlink dedup (#302)

## Problem

Two related defects make `musefs scan` accounting dishonest, both in
`musefs-core/src/scan.rs`.

**#301 — counters contradict their documented contract.** Collection filters
candidates to *supported extensions* only, so non-audio files (`.txt`, `.jpg`,
`.cue`) never reach probing and are counted nowhere. A malformed file *with* a
supported extension (e.g. a garbage `bad.flac`) makes `probe_body` return
`None` → `ProbeOutcome::Unsupported` → `skipped += 1`. But the doc comment on
`scan_directory_with` states: "Unsupported-format files increment
`ScanStats::skipped`; files with a per-file I/O or parse error increment
`ScanStats::failed`." So today the only thing incrementing `skipped` is a parse
failure, which the docs say should be `failed`, and genuinely unsupported files
are silently dropped. Operators cannot tell from the summary whether files were
unsupported, malformed, or filtered.

**#302 — duplicate canonical backing paths under symlink following.** With
`follow_symlinks` enabled, `collect_audio_inner` enqueues both a real supported
file and a symlink to it. Workers `canonicalize` only *after* probing, so both
candidates are probed, both `ingest_bulk` upsert the same `backing_path` row
(`ON CONFLICT(backing_path) DO UPDATE`), and both increment `scanned`. A
directory with `song.flac` and `link.flac -> song.flac` scanned with
`follow_symlinks: true` reports `scanned = 2` while `list_tracks().len() == 1`,
and re-runs tag/art replacement for the same track within one batch.

## Target contract for `ScanStats`

- `scanned` — distinct tracks ingested (one per canonical `backing_path`).
- `skipped` — files ignored because their extension is not a supported audio
  format.
- `failed` — supported-extension files we tried but could not probe/ingest
  (parse error, I/O error, or `canonicalize` failure).
- `raced` — file changed under us between the pre- and post-probe `fstat`.

This is exactly what the existing `scan_directory_with` doc comment already
claims; the fix makes the code match it.

## #301 — counter reclassification

Three code moves in `musefs-core/src/scan.rs`:

1. **Count unsupported-extension files at collection.** `collect_audio` /
   `collect_audio_inner` silently drop non-audio files today. Thread a skipped
   counter so every regular file (and symlink-to-file under follow) whose
   extension is not supported increments `skipped`. `collect_audio` returns the
   count; both `scan_directory_with` and `scan_directory_full_oracle` consume
   it. A single-file root that is not supported audio → `skipped = 1`. Broken
   symlinks and non-file entries stay logged-and-uncounted — they are not
   "unsupported audio".

2. **Reclassify unparseable supported files as `failed`.** Because collection
   only ever passes supported-extension files to the probe,
   `ProbeOutcome::Unsupported` actually means "supported extension that would
   not parse" — a parse failure. Rename it `Unparseable` and map it to `failed`
   in `run_pipeline`. Remove the `skipped` atomic from `run_pipeline` entirely:
   the pipeline now yields only `scanned`/`failed`/`raced`, and `skipped` comes
   solely from collection.

3. **Keep the oracle equivalent.** `scan_directory_full_oracle` maps
   `probe_full` → `None` to `skipped` today; change it to `failed` and source
   `skipped` from collection, so the bounded-vs-full equivalence property still
   holds.

`revalidate_with` needs no counter wiring — its `RevalidateStats` has no
`skipped` field — but reclassification means a tracked file that became
malformed now correctly surfaces as `failed` rather than being miscounted,
addressing the issue's "revalidate skip-pass failures" point.

## #302 — symlink dedup

Add a file-level `HashSet<(dev, ino)>` visited set beside the existing
directory-cycle set (`dir_key`), **active only when `follow_symlinks` is
true**:

- Before pushing a supported regular file or symlink-to-file under follow,
  fetch its `(dev, ino)`. The symlink branch already has the resolved target
  `meta` from `std::fs::metadata(&path)`; the regular-file branch fetches
  `metadata` only when following. If the inode is already present, skip it and
  debug-log a duplicate-target message. A deduped duplicate is counted in **no**
  bucket — it collapses into the single track, keeping `scanned` = distinct
  tracks.
- When `follow_symlinks` is false: unchanged — no extra `stat`, no dedup,
  hardlink behavior preserved.

This dedups both issue cases — a symlink to a file, and a symlinked directory
reaching the same file via two paths — since both resolve to the same target
inode. Reusing `(dev, ino)` is consistent with the existing cycle guard and
avoids a per-file `canonicalize`. Because `collect_audio` is shared,
`revalidate_with` inherits the dedup for free.

## Testing

Adjust the existing `scan_directory_counts_scanned_and_skipped` test (its
garbage `bad.flac` now asserts `failed == 1`; add a `notes.txt` asserting
`skipped == 1`).

**#301 regressions:**

- unsupported extension (`notes.txt`) → `skipped == 1`, never probed.
- garbage `bad.flac` (supported ext, unparseable) → `failed == 1`.
- unreadable file (permission denied) → `failed`.
- file disappearing between collection and probe (open fails) → `failed`.
- revalidate of a tracked file that became malformed → counted `failed` in
  `RevalidateStats`.

**#302 regressions:**

- real file + symlink to it, `follow_symlinks: true` → `scanned == 1` and
  `list_tracks().len() == 1`.
- symlinked directory reaching the same file via two paths → `scanned == 1`.
- assert full stats (`scanned`/`skipped`/`failed`) and the final track count.
- existing bounded-vs-full oracle equivalence still green (oracle reclassified
  in lockstep).

Permission-denied and disappearing-file cases belong in the existing
`hardening_tests` module; note that permission tests are inert when the suite
runs as root.

## Documentation

- Tighten the `scan_directory_with` doc comment to state the contract precisely.
- Update the `probe_file` doc comment (`Unsupported` → `Unparseable`).
- Update the ARCHITECTURE.md scanning section if it describes counter
  semantics.
- The CLI summary string (`scanned N: M file(s), skipped X, failed Y`) needs no
  format change — its semantics now match the words.

## Out of scope

- Hardlink dedup when `follow_symlinks` is false (no behavior change).
- New `ScanStats` fields or CLI summary format changes — we chose
  reclassification over adding distinct `unsupported`/`malformed` counters.
