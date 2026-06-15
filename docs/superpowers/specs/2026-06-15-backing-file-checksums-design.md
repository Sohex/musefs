# Backing-file content checksums and move re-identification

Issue: [#464](https://github.com/Sohex/musefs/issues/464)
Related: [#422](https://github.com/Sohex/musefs/issues/422) (deferred — second pass)
Date: 2026-06-15
Status: design approved, pending spec review

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

A hash over the bytes the probe already reads (the bounded head window plus the
128-byte tail) folded together with the file `size`. Computed *during the probe*
at ~zero extra I/O, since those bytes are already in hand.

Its role is to cheaply answer "which orphaned row might this new file be?"
without reading whole files. A pure `mv` preserves the probe region byte-for-byte
and the size is unchanged, so a moved file's fingerprint matches its old row's
stored fingerprint. This means **the fingerprint alone already recovers the
common move** — and since the vanished original cannot be re-read, a fingerprint
match is in fact the only thing available when no full hash was pre-computed.

The fingerprint is **not collision-proof**: the probe region is dominated by
metadata (headers, tags, embedded art), which is the least content-unique and
most mutable part of a file, so two tracks with near-identical tags and the same
cover can collide. Folding `size` into it makes that rare (such files almost
always differ in size), and collisions are harmless under the two-tier design
below: a fingerprint collision costs at most one extra full-hash confirmation,
never a wrong retarget.

Because the fingerprint is defined in terms of the probe's read region, changing
that region's definition (e.g. the head-window size) invalidates stored
fingerprints. Regenerating them is cheap — a metadata-only rescan (bounded
reads), not a whole-library read.

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
ALTER TABLE tracks ADD COLUMN fingerprint  TEXT;  -- cheap probe-region + size hash
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

**Default tier is `fingerprint`, pending a benchmark** confirming the
probe-region hash adds negligible overhead to a scan (corpus backed on tmpfs per
the bench harness). If the overhead is non-negligible, the default stays `none`
and `fingerprint` is opt-in. Either way `none` remains available.

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

A retarget of a byte-identical move does not bump `content_version` (the content
is unchanged).

### Match strictness

How a fingerprint match is confirmed before retargeting:

- **default (auto-escalate)** — if the matched row has a stored `content_hash`,
  full-hash the new file and require it to match before retargeting; if the row
  has only a fingerprint, retarget on the fingerprint. Strictness follows the
  data already paid for.
- **`--fast`** — fingerprint match is always sufficient; never read the full
  file, even when a `content_hash` exists.
- **`--strict`** — require a `content_hash` match; if the matched row has no
  stored full hash, refuse the retarget and insert fresh (with a warning).

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
- Fingerprint stability: identical bytes at a different path produce the same
  fingerprint; a metadata edit that the probe reads changes it.
- Refind matrix on `scan`: pure move (retarget, tags/art preserved); copy with
  original still present (fresh insert, original untouched); ambiguous
  multi-candidate (fresh insert + warning); no match (fresh insert).
- Strictness: `--strict` refuses retarget without a `content_hash`; `--fast`
  retargets on fingerprint despite a present `content_hash`; default
  auto-escalates and rejects a forged fingerprint-match whose full hash differs.
- Incremental backfill: an unchanged library re-`revalidate`d at a higher tier
  gains the missing checksum without a redundant re-read of files that already
  have it.
- Benchmark: probe-region fingerprint overhead on a representative library
  (corpus on tmpfs), to settle the default-tier decision.
