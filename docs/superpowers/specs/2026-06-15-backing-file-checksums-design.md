# Backing-file content checksums and move re-identification

Issue: [#464](https://github.com/Sohex/musefs/issues/464)
Related: [#422](https://github.com/Sohex/musefs/issues/422) (deferred — second pass)
Date: 2026-06-15
Status: design approved, reviewer pass applied, pending user spec review

## Problem

Tracks reference their backing audio by path (`tracks.backing_path`). The link
from a track's synthesized metadata/art to its backing file therefore breaks the
moment the backing library is moved or reorganized: relocating, renaming, or
restructuring directories leaves every affected track pointing at a path that no
longer exists. The backing bytes are unchanged and the data to rebuild the link
still exists on disk, but musefs cannot tell that the file now at a new path is
the same one it previously tagged, so a `scan` inserts it as a fresh track and
the carefully-built tags/art are orphaned (and eventually pruned by
`revalidate`).

This is distinct from [#276](https://github.com/Sohex/musefs/issues/276)
(in-place mutation at a fixed path): here the path changes while the content
stays the same.

## Goals

- A path-independent content identity per backing file, so a moved or
  reorganized library can be re-identified and existing store rows retargeted to
  the new locations rather than orphaned.
- Re-identification happens on an ordinary `musefs scan`: after moving files
  around, a regular scan updates each row in place no matter where the file
  landed.
- Checksumming is opt-in and tiered, with cost proportional to the guarantee:
  a near-free fingerprint for routine move detection, an eager full-file hash
  for collision/forgery-proof confirmation.
- Checksum columns populate independently and incrementally, so a library can
  climb the tiers over time (bare → fingerprinted → full-hashed) across repeated
  passes without a forced re-read.

## Non-goals

- **Source-side deletion handling (#422).** Wiring beets/Lidarr deletion events
  to store pruning is a deliberate second pass, taken once this lands. This spec
  changes no plugin code.
- **Audio-region-only hashing.** The full hash covers the entire backing file,
  not just the audio range. musefs users tag in the store rather than retagging
  originals, so the in-place-retag scenario that an audio-region hash would
  survive is thin, and full-file hashing is simpler. (Both the fingerprint and
  the full hash are over file bytes, so neither survives an in-place retag of the
  original; that is acceptable and consistent with the decision to drop the
  audio-region approach.)
- **Changing `revalidate` semantics.** `revalidate` keeps prune-on-missing
  exactly as today (see "Scan vs. revalidate" below).

## Content identity: two checksums

Two checksums are stored per track, both nullable, both **scanner-owned and
read-only-derived** — like `structural_blocks`, they are never part of the
editable tag contract and external tools never write them.

### Fingerprint — cheap candidate filter

A hash over the **probe's parsed output** — the `Probed` result the scan already
produces for every file (format, `audio_offset`/`audio_length`, the ordered tag
set, and the embedded art / binary-tag / structural-block payloads). Computed at
**zero extra I/O**: the data is already parsed in memory by the time it reaches
the writer, so this reuses the existing probe machinery rather than adding a
near-duplicate fixed-region read path.

The probe's *read* is adaptive (the window widens on `NeedMore`, varies with
`--window`, and M4A reads `moov` via a seek reader rather than a front window —
`probe_body`, `scan.rs:564`), but its *parsed result* is **deterministic per
file**: the same file always parses to the same `Probed` (widening converges to
the same parseable extent, capped at the constant `MAX_PROBE_BYTES`, independent
of `--window`). Hashing the parsed output — not the raw read buffer — is
therefore content-stable across scans and uniform across formats. (Hashing the
raw adaptive buffer would *not* be deterministic; that distinction is the whole
reason to hash the output.)

Large embedded payloads (art, binary tags) are folded in by their **digest**
(length + content hash) rather than their raw bytes, so the fingerprint stays
cheap even for a file carrying a 16 MB cover — it never re-hashes multi-megabyte
blobs per file.

**Excluded: every `BackingStamp` field — `mtime_ns`, `ctime_ns`, and `size`.**
`ctime` bumps on a rename, so it changes on the very move we are trying to
survive; `mtime` changes on any in-place retag; both would defeat move-matching.
`size` is content-stable but redundant — `audio_length` is already in the parsed
output and gives the same length-based discrimination. The fingerprint is thus
defined purely from content the probe parsed.

Its role is to cheaply answer "which orphaned row might this new file be?"
without reading whole files. A pure `mv` leaves the parsed `Probed` identical, so
a moved file's fingerprint matches its old row's stored fingerprint. This means
**the fingerprint alone already recovers the common move** — and since the
vanished original cannot be re-read, a fingerprint match is in fact the only
thing available when no full hash was pre-computed.

The fingerprint is **not collision-proof**: it is derived from parsed metadata
plus `audio_length`, and (for FLAC) the STREAMINFO structural block, which embeds
an MD5 of the decoded audio — near-perfect identity. For non-FLAC formats no raw
audio is sampled, so two tracks with identical tags, identical cover, and the
same `audio_length` could in principle collide. `audio_length` makes that rare,
and collisions are harmless under the two-tier design below: a fingerprint
collision costs at most one extra full-hash confirmation, never a wrong retarget.

The fingerprint hash function starts as **SHA-256** (the same primitive as the
full hash and `art.sha256`), to keep the first implementation single-primitive.
The benchmark below measures a scan with and without the fingerprint using
SHA-256; if that overhead is non-negligible we revisit with a faster non-crypto
hash (the fingerprint is a filter, not an integrity guarantee, so it does not
need to be cryptographic). This is a hash-choice decision made from numbers, not
up front.

Because the fingerprint is derived from the probe's parsed output, changing the
probe/parse logic or the canonical encoding invalidates stored fingerprints (the
"regen if the ingest mechanism changes" cost, accepted up front). Regenerating
them is cheap — a metadata-only reprobe (bounded reads), not a whole-library
read.

### Full hash — authoritative arbiter

SHA-256 of the entire backing file, stored as 64-char hex (matching the existing
`art.sha256` convention). Computing it requires a full-file read, so it is the
expensive tier.

It must be computed **eagerly**, while the file is present: at re-identification
time the original path is gone and only the *new* copy can be hashed and matched
against the stored value. A "compute on demand" scheme is impossible because it
would need the bytes that have already left. Its role is collision-free
confirmation before a (destructive) retarget, and the forgery guard for the
paranoid case (a crafted file matching a fingerprint but carrying junk audio).

## Schema

A new migration adds two nullable columns to `tracks` and bumps `user_version`:

```sql
ALTER TABLE tracks ADD COLUMN fingerprint  TEXT;  -- cheap hash of the probe's parsed output
ALTER TABLE tracks ADD COLUMN content_hash TEXT;  -- SHA-256 of the whole file, 64-char hex
CREATE INDEX tracks_fingerprint_idx ON tracks(fingerprint);
```

`content_hash` carries a `length(content_hash) = 64` check (nullable). The
`fingerprint` index backs the refind lookup.

Migration chores per repo conventions:

- Regenerate the Python schema mirror: `MUSEFS_REGEN_SCHEMA_PY=1 cargo test -p
  musefs-db schema_py`, then re-vendor the `contrib` mirrors.
- Bump the hardcoded `user_version` assertion in the Picard
  `test_conftest_sanity` test.
- The new columns are scanner-owned; the Python `store` API gains no writer for
  them.

## Checksum tiers (per-scan flag)

A scan runs at one of three tiers, selected by flag:

- **`none`** — no checksums (today's behavior).
- **`fingerprint`** — compute and store the fingerprint (rides the probe).
- **`full`** — fingerprint *and* eager full-file `content_hash`.

The columns populate independently, so the tiers compose across passes: a bare
ingest, then a background `fingerprint` (or `full`) pass over the whole library,
then per-album `full` checksums as automation ingests new albums. A higher tier
on a later pass fills in the missing column without disturbing the other.

**A lower tier never clobbers a higher tier's column.** The ingest upsert writes
a checksum column *only* when the scan's tier computes it: a `none`-tier scan
leaves both columns intact, a `fingerprint`-tier scan refreshes `fingerprint` but
preserves any existing `content_hash`. (`ingest_into`'s upsert,
`scan.rs:959`, must therefore not blanket-NULL the checksum columns on
re-ingest.) On a retarget the content is unchanged, so the moved row keeps its
`content_hash` and its `fingerprint` is identical to the matched value anyway.

**Default tier is `fingerprint`, pending a benchmark** confirming that hashing
the probe's parsed output adds negligible overhead to a scan (corpus backed on
tmpfs per the bench harness). If the overhead is non-negligible, the default
stays `none` and `fingerprint` is opt-in. Either way `none` remains available.

## Re-identification on `scan`

Re-identification lives entirely in the normal `scan` path (`scan_directory_with`
→ `run_pipeline` → ingest), which today re-probes every walked file and upserts
purely by `backing_path` and never prunes. The new step runs on the writer side,
before the upsert, for any probed file whose path is not already a row:

1. Compute the fingerprint, look up rows with a matching fingerprint **whose own
   `backing_path` no longer exists on disk** (the copy-vs-move guard — if the old
   path still exists the new file is a duplicate/copy, not a move).
2. **Unique** missing candidate → **retarget**: `UPDATE` that row's
   `backing_path` (and refresh the validation stamp and audio bounds) to the new
   file, keeping its `id`. Tags and art, keyed by `track_id`, are preserved.
3. **Zero** candidates → insert a fresh track (today's behavior).
4. **Multiple** candidates (genuine duplicate-content tracks, or a fingerprint
   collision) → do not guess: insert fresh and log a warning.

**Destination-occupied guard.** `tracks.backing_path` is UNIQUE, so a retarget
`UPDATE` to a path that is already a row would abort the write. The refind
precondition (the new path is not yet a row) plus the copy-vs-move guard make
this unreachable in normal operation, but the retarget must still be defensive:
if the destination is already occupied, skip the retarget and fall through to the
fresh-insert path rather than letting the UNIQUE violation abort the batch.

**Within-scan claim safety.** Two new files in one scan can both fingerprint-match
the same orphan row. The refind decision runs on the single writer against
up-to-date state, so the first claims (retargets) the orphan and the second then
sees that orphan's `backing_path` occupied (no longer a missing candidate) and
inserts fresh. The implementation must make refind decisions against committed/
in-progress writer state, not a stale pre-batch snapshot, so an orphan is never
double-claimed within a batch.

**NULL-fingerprint rows are invisible to refind.** The lookup keys on a non-NULL
fingerprint (the incoming file always has one), so rows scanned at the `none`
tier — lacking a fingerprint — cannot be retargeted until a `fingerprint`/`full`
pass backfills them. That is the intended consequence of incremental tiers, not a
bug, but it means move recovery only protects rows that were fingerprinted before
the move.

**`content_version` note.** A retarget refreshes the validation stamp, and the
`tracks_geometry_au` trigger (`schema.rs:178`) bumps `content_version` on any
`backing_mtime_ns` change. So a retarget *does* bump `content_version` even for a
byte-identical move. This is harmless: `content_version` is compared only for
equality (freshness invalidation), so a spurious bump costs at most one
header-cache refresh — the same documented monotone churn the structural-block
triggers already accept.

### Match strictness

Strictness governs only how a fingerprint match is *confirmed* before
retargeting, and is **independent of the scan tier** (the tier decides what gets
stored for scanned files; strictness decides what a retarget trusts). Confirming
with a full hash always requires reading the *new* file's bytes, whatever the
tier — `--strict`/auto-escalate will full-hash the new file even on a
`fingerprint`-tier scan.

A precondition for any refind: the candidate rows must already carry a
fingerprint. A `none`-tier scan computes no fingerprint for the incoming file
either, so **refind requires at least the `fingerprint` tier** — under `none` no
retarget is attempted regardless of strictness.

The match × matrix:

- **default (auto-escalate)** — if the matched row has a stored `content_hash`,
  full-hash the new file and require it to match before retargeting; if the row
  has only a fingerprint, retarget on the fingerprint. Strictness follows the
  data already paid for.
- **`--fast`** — fingerprint match is always sufficient; never read the full
  file, even when a `content_hash` exists.
- **`--strict`** — require a `content_hash` match: full-hash the new file and
  compare. If the matched row has no stored `content_hash` (nothing to compare
  against), refuse the retarget and insert fresh (with a warning).
- **`none` tier (any strictness)** — no fingerprint computed, so no refind.

This aligns the security guarantee with cost: a `full`-tier library
automatically gets collision/forgery-proof retargeting under the default; a
`fingerprint`-only library gets best-effort move detection.

## Scan vs. revalidate

`revalidate` keeps its **prune-on-missing behavior** unchanged: it prunes any
track under the revalidated root whose backing file is missing. It additionally
gains checksum computation/backfill (see "Incremental computation"), but it never
attempts re-identification — refind/retarget is a `scan` concern only.

Consequence (documented caveat, accepted for this pass): if automation runs
`revalidate` on a schedule and files are then moved, `revalidate` prunes the
moved-from rows before a `scan` can retarget them. **Move recovery requires
running `scan` after a move, and ideally before any `revalidate`.** Whether
`revalidate` should attempt refind-before-prune is left to the #422 second pass.

## Incremental computation

Checksum work is gated so a file is hashed once per content lifetime, not every
pass:

- On `revalidate`, the existing size/mtime/ctime skip-unchanged short-circuit is
  extended: an unchanged file is still re-processed if it is **missing the
  checksum the requested tier requires** (the same backfill pattern already used
  for legacy FLAC structural blocks). This makes a tier upgrade over an unchanged
  library a backfill rather than a no-op.
- A `scan` re-probes every file regardless (its existing behavior), so the
  fingerprint is recomputed for free; the full-file read for `content_hash` is
  taken only at the `full` tier.

## Layering and where the work runs

- `musefs-db`: the migration, the two columns + index, the model fields, and a
  retarget operation (update `backing_path` + stamp + audio bounds for an
  existing `id`) plus a fingerprint-lookup query.
- `musefs-core/src/scan.rs`: fingerprint computation in the probe; full-hash
  computation in the probe worker at the `full` tier (keeping the single writer
  cheap); the refind/retarget decision on the writer side; the tier and
  strictness threaded through `ScanOptions`; the revalidate backfill gate.
- `musefs-cli` / `musefs` binary: the `--checksum=none|fingerprint|full`,
  `--fast`, and `--strict` flags; stay thin.

## Testing

- Schema migration round-trip and `user_version` (the existing `schema` test
  tiers); Python schema-mirror regeneration.
- Fingerprint stability and determinism: identical bytes at a different path
  produce the same fingerprint; **the same file scanned at two different
  `--window` values produces the same fingerprint** (parsed-output determinism);
  a metadata edit the probe parses changes it; `mtime`/`ctime`/`size` changes
  alone (e.g. `touch`) do not.
- Refind matrix on `scan`: pure move (retarget, tags/art preserved, `id`
  stable); copy with original still present (fresh insert, original untouched);
  ambiguous multi-candidate (fresh insert + warning); no match (fresh insert);
  two new files matching one orphan (one retargets, one inserts fresh — no
  double-claim); destination-occupied (no UNIQUE-violation abort).
- Tier interactions: `none`-tier scan attempts no refind; a `none`/`fingerprint`
  re-ingest preserves an existing `content_hash` (no clobber-to-NULL);
  NULL-fingerprint rows are not retargeted.
- Strictness: `--strict` refuses retarget without a candidate `content_hash` and
  full-hashes the new file even on a `fingerprint`-tier scan; `--fast` retargets
  on fingerprint despite a present `content_hash`; default auto-escalates and
  rejects a forged fingerprint-match whose full hash differs.
- Incremental backfill: an unchanged library re-`revalidate`d at a higher tier
  gains the missing checksum without a redundant re-read of files that already
  have it.
- Benchmark: scan throughput with and without the fingerprint (SHA-256) on a
  representative library (corpus on tmpfs). Settles both the default-tier
  decision and whether to revisit the fingerprint hash function.
