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
   counter so every regular file whose extension is not supported increments
   `skipped`. `collect_audio` returns the count; both `scan_directory_with` and
   `scan_directory_full_oracle` consume it. Specifics that must be implemented
   precisely (the walk has several entry/branch points):
   - **Single-file root.** The `root.is_file()` branch in *both*
     `scan_directory_with` (scan.rs:746) and `scan_directory_full_oracle`
     (scan.rs:917) short-circuits before `collect_audio` is ever called, so the
     "unsupported single-file root → `skipped = 1`" increment must be added
     directly in each entry point's `else` arm — it cannot come from threading a
     counter through `collect_audio`.
   - **Symlink-to-file under follow.** In the `is_symlink` / `meta.is_file()`
     arm of `collect_audio_inner` (scan.rs:145-150), an unsupported-extension
     target counts once as `skipped`, symmetric with the regular-file arm.
   - **Broken symlinks, non-file entries, and symlinks skipped because
     `follow_symlinks` is false** stay logged-and-uncounted — they are not
     "unsupported audio" and we do not pretend to know their target format.

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

Add a **separate** file-level `HashSet<(dev, ino)>` visited set (distinct from
the existing directory-cycle set, for clarity — file and directory inodes never
collide within a device, but a separate set keeps the two concerns independent),
**active only when `follow_symlinks` is true**:

- The dedup check happens **only after** `is_supported_audio` passes, so we
  never `stat` the many unsupported files in a music tree just to dedup them.
- The symlink-to-file arm already has the resolved target `meta` from
  `std::fs::metadata(&path)` (scan.rs:144); reuse it. The regular-file arm
  (scan.rs:132-135) currently calls only `entry.file_type()`, so under follow it
  must add one `std::fs::metadata(&path)` *after* the `is_supported_audio`
  check.
- **Dedup is best-effort.** If that `metadata` call fails, do **not** drop the
  candidate — skip the dedup for it and push it normally. The pipeline then
  probes it and counts it (`scanned`, or `failed` if the error is real),
  matching the no-silent-drop spirit of #301 and avoiding a new error arm in the
  walk.
- If the inode is already present, skip the candidate and debug-log a
  duplicate-target message. A deduped duplicate is counted in **no** bucket — it
  collapses into the single track, keeping `scanned` = distinct tracks.
- When `follow_symlinks` is false: unchanged — no extra `stat`, no dedup.

This dedups both issue cases — a symlink to a file, and a symlinked directory
reaching the same file via two paths — since both resolve to the same target
inode. Reusing `(dev, ino)` is consistent with the existing cycle guard and
avoids a per-file `canonicalize`. Because `collect_audio` is shared,
`revalidate_with` inherits the dedup for free.

**Acknowledged behavior change:** because `(dev, ino)` cannot distinguish "a
symlink to X" from "a second hardlink to X", two hardlinks to the same inode
reached via two paths *under `follow_symlinks: true`* now collapse to one
`scanned` track (today they `canonicalize` to two distinct paths → two tracks).
This is the intended outcome — one set of audio bytes is one logical track — and
is confined to the opt-in follow mode. It is called out so it is not mistaken
for a regression.

## Testing

**External test files that encode the OLD semantics must be updated in lockstep
— missing them means a red pre-commit suite.** Beyond the in-module tests in
`scan.rs`, two integration files in `musefs-core/tests/` assert the current
behavior, and some assertions carry **line-anchored mutation-gate kill
comments** (`// kills scan L<n> …`) that move when the counter arithmetic moves:

- `musefs-core/tests/scan_counters.rs`:
  - `oracle_counts_scanned_and_skipped_exactly` (line ~344) writes garbage
    `bad.flac` and asserts `stats.skipped == 1` via `scan_directory_full_oracle`
    — must flip to `stats.failed == 1`. Its `// kills scan L858 stats.skipped
    += 1 …` anchor (line ~342) refers to the oracle line we are changing from
    `skipped += 1` to `failed += 1`; update the comment's line number, the cited
    expression, and re-confirm the mutant is still killed.
  - `revalidate_failed_carries_scan_failure` (line ~294, `// kills scan L711`)
    relies on `RevalidateStats.failed == scan.failed + skip_failed`. The field
    set is unchanged, but reclassifying `Unparseable → failed` changes what
    `scan.failed` *contains*; re-verify this test passes rather than assuming it
    is inert, and refresh its line anchor if shifted.
  - Audit every other `// kills scan L…` anchor in this file (lines ~81, ~124,
    ~170, ~219, ~260) — any that reference lines after the edits shift and need
    their numbers refreshed.
- `musefs-core/tests/probe_equivalence.rs` compares the bounded path against
  `scan_directory_full_oracle`. Equivalence holds only if *both* sides
  reclassify `None → failed` and source `skipped` from collection identically;
  verify its corpus does not contain unsupported-extension files that would now
  diverge.

After the edits, re-run the in-diff mutation gate locally (per project
convention: `cargo mutants --in-place`, serial) to confirm no anchor turned a
killed mutant into a survivor or a false pass, and `cargo fmt --all --check`
(the line anchors are fmt-fragile).

Adjust the existing in-module `scan_directory_counts_scanned_and_skipped` test
(its garbage `bad.flac` now asserts `failed == 1`; add a `notes.txt` asserting
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
- Rename `ProbeOutcome::Unsupported → Unparseable` at every site: the enum
  variant (scan.rs:37-45), its construction in `probe_file` (scan.rs:333), the
  `run_pipeline` match arm (scan.rs:811), both mentions in the `probe_file` doc
  comment (scan.rs:308-316), and the unit test
  `oversize_unparseable_file_is_skipped_not_read_whole` (scan.rs:~2309) — whose
  name/assert may also need to reflect that the file is now counted `failed`,
  not skipped.
- Update the ARCHITECTURE.md scanning section if it describes counter
  semantics.
- The CLI summary string (`scanned N: M file(s), skipped X, failed Y`) needs no
  format change, but the *meaning* shifts for operators: malformed files move
  from `skipped` to `failed`, and non-audio files now appear in `skipped`. Add a
  one-line CHANGELOG / release note so the behavior change is discoverable.

## Out of scope

- Hardlink dedup when `follow_symlinks` is false (no behavior change). Hardlink
  dedup *under* follow happens as a side effect of `(dev, ino)` — see the
  acknowledged behavior change in the #302 section.
- New `ScanStats` fields or CLI summary format changes — we chose
  reclassification over adding distinct `unsupported`/`malformed` counters.
