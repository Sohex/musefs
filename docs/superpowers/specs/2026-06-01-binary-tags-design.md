# musefs — Binary Tag Handling — Design

**Date:** 2026-06-01
**Status:** Approved design, ready for implementation planning
**Closes:** #66 (Binary tag data is silently dropped at scan time)

## Overview

musefs's `tags` table stores only text (`value TEXT NOT NULL`), and every format
parser skips non-text tag frames at scan time (`mp3::read_tags`: *"Other/binary
frames are skipped."*). Binary tag data present in real files — ID3v2 `PRIV`,
`POPM`, `UFID`, `GEOB`, `SYLT`; FLAC `APPLICATION`/`CUESHEET`; MP4 `----` atoms —
is therefore permanently lost on ingestion, and a file served through the mount is
missing it entirely. A reader relying on that data (a player using `UFID`
MusicBrainz IDs, `POPM` ratings, Serato `GEOB` analysis) sees a degraded tag set
compared to the original.

This design makes binary tag data survive the round trip: extracted at scan,
stored in the DB, and re-synthesized into the served file.

### Guiding principle: musefs transports, tools interpret

musefs is a storage/transport layer, not a tag interpreter. Its responsibility is
to ensure binary tag data lands in the DB and round-trips faithfully back into the
served file. **Whether and how that data is interpreted is the media manager's job**
(beets/picard/etc., which write the DB out-of-band per the roadmap). A media
manager that wants to author a FLAC `CUESHEET` from a sidecar `.cue` parses the cue
and writes a `CUESHEET` blob to the DB; musefs faithfully synthesizes it into the
FLAC and never decodes a cue itself.

This yields one consistent rule with a single, narrow exception:

- **Opaque blob, stored in the DB** for everything binary — preserved verbatim at
  the payload level, never interpreted by musefs.
- **Promote to canonical text** *only* for frames with a universal semantic
  equivalent every manager already reads/writes as a plain field: `POPM` →
  `rating`/`playcount`, MusicBrainz `UFID` → `musicbrainz_trackid`. Here the
  editable text field *is* the representation tools use, so promotion serves the
  same goal as preservation.

### What "preservation" means in practice

Whole-file byte-identity of the tag region is already off the table — synthesis
regenerates the metadata region (frame ordering, padding, header version: musefs
always emits ID3v2.4), so the original tag's exact byte layout is gone regardless.
What is preserved is **payload-exact** fidelity: the bytes a reader consumes from a
given frame survive unchanged. We store the frame *payload* (post-header body) and
regenerate version-correct framing at synthesis. This is the maximum fidelity that
is actually correct — verbatim splicing of original frame headers is unsafe across
ID3v2.3↔2.4 (plain vs syncsafe frame-size encoding), FLAC, and MP4 size-prefix
boundaries.

## Scope

In scope:

- **ID3v2 (MP3 + WAV)** — both flow through `mp3::build_id3v2_segments` (WAV embeds
  an `id3 ` chunk). Semantic promotion (`POPM`, MusicBrainz `UFID`) + opaque
  passthrough for all other binary frames. **This is the primary gap.**
- **MP4/M4A** — opaque passthrough for custom `----` atoms with non-UTF-8 data.
- **FLAC** — `APPLICATION` + `CUESHEET` become DB-backed, tool-editable blobs
  (moved off the file-front re-read path). `STREAMINFO` + `SEEKTABLE` move into a
  separate read-only **structural store**, eliminating the FLAC front re-read
  entirely (see "FLAC structural store" below).

Out of scope:

- **Ogg (Vorbis/Opus/OggFLAC)** — Vorbis comments are already all-text `key=value`;
  the only binary payload is embedded art, already handled via base64
  `METADATA_BLOCK_PICTURE`. No binary-frame gap exists.
- **Any interpretation of binary payloads** — no cue-sheet transcoding, no
  POPM-rating scale conversion beyond passing the raw value through, no decoding of
  `GEOB`/`APPLICATION` contents. Media managers own all semantics.
- **`.cue` sidecar ingestion / directory-aware scanning** — a media-manager
  concern; if a tool wants a `CUESHEET`, it writes the blob to the DB.

## Architecture

The change touches the full chain — scan → schema → resolve → synthesis → read —
in the affected formats. Each piece mirrors machinery that already exists for
embedded art (the `ArtImage`/`read_art_chunk`/`MAX_ART_BYTES` triad), keeping the
new code idiomatic to the codebase.

### 1. Schema — migration V2 (`musefs-db/src/schema.rs`)

Append a second entry to `MIGRATIONS` and bump `user_version` → 2.

```sql
-- Binary tag payloads live alongside text tags. A row is binary iff
-- value_blob IS NOT NULL; binary rows store '' in value.
ALTER TABLE tags ADD COLUMN value_blob BLOB;

-- Read-only, derived-from-file structural metadata. NOT part of the editable
-- `tags` contract: external tools never read or write this table. Written by
-- musefs at scan; consumed at resolve to avoid re-reading the backing file.
CREATE TABLE structural_blocks (
    track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    kind     TEXT NOT NULL,      -- 'STREAMINFO' | 'SEEKTABLE'
    ordinal  INTEGER NOT NULL DEFAULT 0,
    body     BLOB NOT NULL,
    PRIMARY KEY (track_id, kind, ordinal)
);
```

Rationale:

- `value_blob` is added with `ADD COLUMN` (no table rebuild). `value` stays
  `NOT NULL`; binary rows set it to `''`. The PK `(track_id, key, ordinal)` is
  unchanged; the implicit `rowid` is the streaming handle for synthesis.
- The existing `tags_ai/au/ad` triggers fire on any `tags` write, so
  `content_version` bumps cover binary tags **with no trigger change**.
- `structural_blocks` is deliberately separate from `tags`: `STREAMINFO`/`SEEKTABLE`
  are structural data derived from the backing file, not user metadata. Keeping them
  out of `tags` makes them read-only by construction (a tool doing `SELECT * FROM
  tags` never sees them) and keeps the external-tool contract clean. No
  `content_version` trigger: these change only on a rescan of a changed file, which
  is already governed by `backing_size`/`backing_mtime` (`BackingChanged`).

**Migration backfill for existing FLAC tracks.** A SQL migration cannot reparse
files, so V2 adds the column/table but does **not** populate `structural_blocks` for
FLAC tracks scanned under V1. A FLAC track that is migrated but not yet rescanned has
no structural rows, and §7 removes the unconditional FLAC front re-read — so resolve
must **fall back** to the existing `read_front` + `flac::read_metadata` path when a
FLAC track's `structural_blocks` is empty (this also covers a missing-row edge case
generally). `read_front` survives regardless (WAV/Ogg call it — `reader.rs:296`,
`:308`), so the fallback is free. Persistence is not done in the read path: the next
`scan`/`scan --revalidate` backfills `structural_blocks` (and migrates
`APPLICATION`/`CUESHEET` into `tags.value_blob`) for any FLAC track lacking them.
Consequence: the §7 "zero file reads" perf win applies to rescanned tracks; legacy
tracks degrade gracefully to today's behavior until their first rescan. No data is
lost and no forced full rescan is required on upgrade.

### 2. Storage model: why inline in `tags`, not a content-addressed side table

Binary tag payloads go in `tags.value_blob`, not a new content-addressed table like
`art`. The `art` table is content-addressed for **dedup** — one cover shared across
an album's tracks is stored once. Binary tag frames are the opposite: `POPM`
ratings, `UFID` identifiers, and per-track Serato `GEOB` analysis are unique per
track, so dedup buys ~nothing while costing a join table and a second write path.

Worst case is a fully DJ-tagged library (~20–50 KB of `GEOB`/`APPLICATION` per
track); at 50k tracks ~2.5 GB of blob, which SQLite handles fine (large blobs live
on overflow pages, off the b-tree). The one real risk — blobs leaking into hot read
paths — is neutralized by the query-split discipline (§6) and a scan-time size cap
(§5). The pathological case (a `GEOB` embedding a multi-MB file) is bounded by
policy via `MAX_BINARY_TAG_BYTES`.

### 3. Layout & reader (`musefs-format/src/layout.rs`, `musefs-core/src/reader.rs`)

New streamed segment, mirroring `Segment::ArtImage`:

```rust
Segment::BinaryTag { payload_id: i64, len: u64 }  // payload_id == the `tags` rowid
```

Field is named `payload_id` (not `tag_rowid`) to match `BinaryTagInput.payload_id`
and the existing `ArtImage`/`ArtInput` `art_id` convention — it carries the `tags`
rowid as an opaque streaming handle.

- Length-only; the payload is **never** materialized into the cached
  `ResolvedFile`. This honors the cardinal "blob bytes are never held in memory"
  invariant — the reason `ArtImage` exists.
- `reader::read_at` gains an arm that streams it in chunks via a new
  `db.read_binary_tag_chunk(rowid, offset, len)` — a near-copy of `read_art_chunk`
  (`art.rs:69`), opening `tags.value_blob` by rowid with `blob_open` (`tags` is a
  normal rowid table, not `WITHOUT ROWID`). It reuses `read_art_chunk`'s
  short-read-is-error contract (a short read means the row changed underneath us —
  see the rowid invariant below); **no new error variant** is introduced, so the
  FUSE `errno()` exhaustive match (`musefs-fuse/src/lib.rs:64`) is untouched.
- `RegionLayout::validate`'s empty-segment rule means a zero-length payload must be
  skipped at scan (same rule that already skips empty art).
- **Cache-budget accounting needs no change:** `HeaderCache`'s `cache_bytes` sums
  only `Inline` segment lengths (`Segment::BinaryTag` falls into the `_ => 0` arm,
  exactly like `ArtImage`), so the LRU budget stays correct automatically.

**`payload_id` (rowid) validity invariant.** A `payload_id` is valid **only within
the lifetime of the `ResolvedFile` that captured it.** `replace_tags` (`tags.rs:5`)
is `DELETE` + `INSERT`, so rowids churn on every re-tag — but that same write bumps
`content_version`, which invalidates the cached `ResolvedFile` and forces a
re-resolve that reads fresh rowids before any stale one is used. An implementer must
**not** cache a `BinaryTagInput`/`payload_id` across a refresh; it is always obtained
fresh during `resolve`. This is the binary-tag analogue of how `ArtImage`'s `art_id`
stays valid (art rows are content-addressed and not deleted on re-tag; binary rows
are, hence the explicit invariant).

### 4. Format-layer input (`musefs-format/src/input.rs`)

```rust
pub struct BinaryTagInput {
    pub key: String,      // namespaced identifier the owning format understands
    pub payload_id: i64,  // tag rowid; opaque handle, exactly like ArtInput::art_id
    pub len: u64,
}
```

`synthesize_layout` for each affected format gains a `binary_tags:
&[BinaryTagInput]` parameter alongside `tags`/`arts`. The `key` namespace is
format-private (the parser writes a key its own synthesis path decodes):

- ID3: the 4-char frame id (`PRIV`, `GEOB`, `SYLT`, …).
- FLAC: `APPLICATION` (payload = full block body incl. 4-byte app id) and
  `CUESHEET`; `ordinal` disambiguates repeats.
- MP4: `----:<mean>:<name>` (payload = the `data` atom value bytes).

Opaque keys use the raw frame/block identifier; these do not collide with the
canonical text-tag keys (`artist`, `rating`, …) because they are reserved
frame/block names. Promotion (§5) is what keeps a frame *out* of the opaque path,
so no frame is double-stored.

### 5. Per-format parse + synthesize

**ID3 (MP3 + WAV) — `mp3.rs`:**

- *Parse* (`read_tags` + new `read_binary_tags`): non-text, non-`APIC` frames are
  classified.
  - `POPM` → `rating` + `playcount` text tags. The raw POPM rating byte (0–255) is
    stored as `rating`; the play counter as `playcount`. **The original owner-email
    string is not preserved**, and POPM is regenerated at synthesis with an empty
    owner. Dropping the owner is established practice for format-agnostic
    players/managers, which treat rating as a library-level fact, not tied to the
    original tagger's identity.
  - `UFID` with owner `http://musicbrainz.org` → `musicbrainz_trackid` text. `UFID`
    with any other owner → opaque (preserved, not promoted).
  - Every other binary frame (`PRIV`, `GEOB`, `SYLT`, unpromoted `POPM`/`UFID`, …)
    → opaque `(frame-id, payload)` written to `value_blob`.
- *Synthesize* (`build_id3v2_segments`): after the existing text/`TXXX`/`APIC`
  frames, emit (a) rebuilt semantic frames via new `popm_frame_data` /
  `ufid_frame_data` builders fed from the `rating`/`playcount`/`musicbrainz_trackid`
  text tags, then (b) each `BinaryTagInput` as `push_frame_header(id, len)` +
  `Segment::BinaryTag`. The existing 28-bit syncsafe tag-size guard now also bounds
  binary frame lengths.
- *WAV parse side:* the WAV scan path extracts the embedded `id3 ` chunk and runs the
  **same** `read_binary_tags` over its bytes (WAV's binary frames are ID3 frames in
  an `id3 ` chunk); promotion and opaque classification are identical to MP3. Only
  the chunk-extraction differs from MP3's whole-file ID3 read.

**MP4 — `mp4.rs`:** custom `----` atoms parsed to opaque `(----:<mean>:<name>,
payload)` (the `mean` and `name` strings are encoded into the `key` so synthesis can
rebuild the `mean`/`name` child atoms; the `payload` is the `data` atom value bytes
only). This is **not** a near-copy of the art path and requires a real `build_udta`
change:

- Today `build_udta` (`mp4.rs:504`) returns `(Vec<u8> prefix, u64 art_len)` and
  assumes **exactly one** streamed segment (cover art) that is the **last** child of
  `ilst`; `synthesize_layout` (`mp4.rs:609`) emits `[Inline head][ArtImage][Inline
  mdat_header]`, and every enclosing box size (`covr`→`ilst`→`meta`→`udta`) is the
  materialized prefix length plus that single `art_len`.
- To stream **N** `----` atoms we must interleave N `Segment::BinaryTag` *inside*
  `udta`, each splitting the inline buffer, with **every enclosing box length
  accounting for all binary payload lengths at the correct nesting depth.** So
  `build_udta`'s contract changes from *(prefix, len)* to **an ordered list of
  inline/streamed segments** (mirroring how `build_id3v2_segments` already returns
  `Vec<Segment>`), and `new_moov_size`/`delta` sum all binary `len`s into
  `udta_total` *before* computing `delta`. Multi-`----` (N>1) **is in scope** — real
  iTunes files carry many freeform atoms.
- Materializing `----` payloads inline is explicitly rejected: it would violate the
  no-blobs-in-memory invariant §3 relies on. Payloads stream via
  `Segment::BinaryTag`.
- The existing `stco`/`co64` patching is unaffected: binary lengths are known at
  resolve, so `new_moov_size` (and thus `delta`) stays computable before patching;
  the u32 atom-size guard now bounds the binary-inclusive box sizes.

**FLAC — `flac.rs`:**

- The scan block-type filter (`flac.rs:59`/`:116`) splits its four current
  preserved types:
  - `APPLICATION`, `CUESHEET` → DB-backed editable blobs in `tags.value_blob`
    (keyed `APPLICATION`/`CUESHEET`, `ordinal` for repeats).
  - `STREAMINFO`, `SEEKTABLE` → `structural_blocks` (read-only structural store).
- `synthesize_layout`'s signature changes: instead of a `FlacScan` (rebuilt from a
  file re-read), it takes the structural blocks (`STREAMINFO`/`SEEKTABLE` bodies) +
  `tags`/`binary_tags`/`arts`. It assembles, in this **fixed canonical order**:
  `fLaC` + `STREAMINFO` + `SEEKTABLE` (inline, from `structural_blocks`) +
  regenerated `VORBIS_COMMENT` + `APPLICATION`/`CUESHEET` (`Segment::BinaryTag`,
  streamed) + `PICTURE` (`ArtImage`) + backing audio.
- **Behavior change to call out:** today `parse_blocks` (`flac.rs:36`) preserves the
  original file's block *order*; the new path imposes the canonical order above.
  This is valid (the FLAC spec only mandates `STREAMINFO` first) but means
  re-synthesized block order may differ from the source — a round-trip proptest must
  assert payload/round-trip fidelity, **not** byte-identical block ordering, and a
  reviewer must not treat reordering as a regression.
- **`is-last` rule (concrete):** total metadata-block count =
  `structural_blocks.len()` (STREAMINFO + SEEKTABLE) `+ 1` (VORBIS_COMMENT) `+`
  number of `APPLICATION`/`CUESHEET` binary blocks `+` nonempty pictures. The
  last-block flag is set on the block **header** that precedes the final body, even
  when that body is a streamed `Segment::BinaryTag` or `ArtImage` (the header is
  emitted into the preceding inline buffer, exactly as the current PICTURE-as-last
  code does). `STREAMINFO` stays first.
- `APPLICATION`/`CUESHEET` bodies are bounded by the existing 24-bit FLAC block-length
  guard.

**Scan size cap (`scan.rs`):** a new `MAX_BINARY_TAG_BYTES` mirroring
`MAX_ART_BYTES` (`scan.rs:24`). Oversize binary payloads are logged-and-skipped
exactly like oversize art (`scan.rs:371`); zero-length payloads are skipped
(empty-segment rule). Parsers return a new `EmbeddedBinaryTag { key, payload }`;
`scan_directory`/`ingest_bulk` persist them to `value_blob` and structural blocks to
`structural_blocks` within the same upsert transaction.

### 6. The query-split discipline (the one cost of inlining)

`tags` is read in two very different paths; the blob must not leak into the
text-only one. **The `value_blob IS NULL` filter belongs in the DB layer
(`musefs-db/src/tags.rs`), not `mapping.rs`** — the template path consumes rows the
DB layer hands it and issues no SQL of its own. After migration a binary row stores
`value=''`, so an unfiltered query would feed the template renderer empty-string
values keyed by `PRIV`/`GEOB`/`----:…`, polluting the field map (`tags_to_fields`,
`mapping.rs:15`). Every `SELECT … FROM tags` is enumerated and classified:

- **Template / tree paths — add `WHERE value_blob IS NULL`:**
  - `tags_grouped` (`tags.rs:77`) — full-tree rebuild on `poll_refresh`.
  - `tags_for_tracks` (`tags.rs:49`) — the SP2 incremental-refresh path.
  These never name `value_blob`, so SQLite never touches its overflow pages and tree
  rebuilds stay as cheap as today.
- **Synthesis / resolve path — needs binary rows:** `get_tags` (`tags.rs:23`, called
  from `reader.rs:248`) keeps returning text rows for `TagInput`; a **new sibling
  query** returns binary rows (`key, rowid, length(value_blob) WHERE value_blob IS
  NOT NULL`) for `BinaryTagInput`. `HeaderCache::resolve` loads both, plus
  `structural_blocks` for FLAC (or the §1 front-read fallback when empty).
- `replace_tags` (`tags.rs:5`) writes both text and binary rows (binary rows carry
  `value=''` + `value_blob`); unchanged in shape.

`mapping.rs` splits the two row sets accordingly: text rows → `TagInput`; binary
rows → `BinaryTagInput`. The DB `Tag` struct stays text-only.

### 7. FLAC resolve simplification (perf win)

Today `reader.rs:257-260` re-reads `audio_offset` bytes of the backing file on every
FLAC cache-miss resolve (`read_front` → `flac::read_metadata`), which **includes the
embedded cover art** — then discards those art bytes (art is served from the DB).
With `STREAMINFO`/`SEEKTABLE` in `structural_blocks` and `APPLICATION`/`CUESHEET` in
`tags`, FLAC resolve does **zero** backing-file reads for rescanned tracks, exactly
like MP3 ("MP3 needs no front read"). This removes a per-resolve read that scales
with embedded-art size. (Legacy FLAC tracks not yet rescanned fall back to the front
read per §1 until their first rescan.)

Deletion/refactor fallout the plan must enumerate:

- The `Format::Flac` arm in `reader.rs` `build` (~248–260) stops constructing a
  `FlacScan` from `read_front`; `audio_offset`/`audio_length` come from the `tracks`
  row (as MP3 already does). The front read survives only as the legacy fallback (§1).
- `read_front` itself is **not** removed (WAV `reader.rs:296`, Ogg `:308` still call
  it).
- `flac::synthesize_layout`'s signature changes (FlacScan → structural blocks +
  binary inputs); this ripples to every caller and to all tests that construct a
  `FlacScan`. The plan must enumerate those tests explicitly (test-fallout
  enumeration has been under-scoped before — e.g. SP4).

## Error handling

- Oversize binary payloads: logged-and-skipped at scan (`MAX_BINARY_TAG_BYTES`),
  consistent with art. Never a hard scan failure.
- Zero-length payloads: skipped at scan (would fail `RegionLayout::validate` as an
  empty segment).
- Synthesis size-field overflow: the existing per-format guards (ID3 28-bit, FLAC
  24-bit, MP4 u32) now account for binary payload lengths and return the existing
  `TooLarge`/`InvalidLayout` errors rather than emitting a corrupt file.
- A binary frame whose payload the parser cannot extract is skipped at scan (the
  data is unrecoverable; better a clean text-tag set than a scan abort), matching
  the current tolerant scan posture.
- **No new `CoreError`/`FormatError` variant is introduced.** `EmbeddedBinaryTag` is
  a parser return struct (not an error); `read_binary_tag_chunk` reuses
  `read_art_chunk`'s short-read error. So the FUSE `errno()` exhaustive match
  (`musefs-fuse/src/lib.rs:64`) needs no new arm. If the plan finds a new variant
  unavoidable, it must add the corresponding `errno()` arm in the same change.

## Testing

Mirrors the existing per-format test surface (CLAUDE.md "Adding a format" checklist):

- **Round-trip proptest** per format (`proptest_<fmt>.rs`): parse → store →
  synthesize → re-parse yields byte-identical binary payloads and correctly
  promoted `rating`/`playcount`/`musicbrainz_trackid`. For FLAC, assert
  payload/round-trip fidelity **only** — not byte-identical block ordering (the
  canonical reorder in §5 is intentional).
- **Promoted + opaque `UFID` coexistence**: a file with both a MusicBrainz `UFID`
  (owner `http://musicbrainz.org` → promoted to `musicbrainz_trackid`) and a
  non-MusicBrainz `UFID` (→ opaque) re-synthesizes to two distinct `UFID` frames;
  assert owner-uniqueness holds (promoted owner = `http://musicbrainz.org`, opaque
  owner ≠ it) so ID3v2.4's distinct-owner rule isn't violated.
- **Query-split correctness**: a track with a binary frame (e.g. `PRIV`) renders the
  **same** tree path as one without — binary rows must not leak into `tags_to_fields`.
  Asserts the `value_blob IS NULL` filter holds in `tags_grouped`/`tags_for_tracks`.
- **Byte-identical invariant** proptest extended so fixtures carry binary frames,
  confirming audio bytes remain untouched with binary tags present.
- **Fuzz** targets `mp3`/`mp4` seeded with binary frames; a FLAC round-trip test
  locks the migrated `APPLICATION`/`CUESHEET` behavior and the structural store.
- **Interop (Property 5, `interop_emit.rs` + `tests/interop`)** — the real-world
  proof: fixtures gain `POPM`/`UFID`/`PRIV`/`GEOB` (ID3) and a `----` atom (MP4),
  and the independent mutagen reader asserts they survive the mount (semantic fields
  as readable tags, opaque frames byte-for-byte).
- **Migration test**: V1→V2 upgrade is idempotent and preserves existing rows;
  `value_blob` defaults NULL on existing tags.
- **Legacy-FLAC migration/backfill**: a V1 DB with a FLAC track, migrated to V2 and
  **not** rescanned, still resolves and serves correctly via the §1 front-read
  fallback (empty `structural_blocks`); and after a `scan --revalidate`,
  `structural_blocks`/`value_blob` are backfilled and the front read no longer fires.

## Implementation phasing (for the plan)

The pieces are separable and should land incrementally; each step compiles green:

1. Schema V2 + `Segment::BinaryTag` + `read_binary_tag_chunk` + `BinaryTagInput`,
   **plus the `value_blob IS NULL` filter on `tags_grouped`/`tags_for_tracks`** (the
   shared foundation; no format behavior change yet). The query-split filter is a
   *correctness* requirement — it must be in place before any step writes a binary
   row, not deferred — so it lands here.
2. ID3 (MP3 + WAV): opaque passthrough + `POPM`/`UFID` promotion + the new
   synthesis-path binary-row query. (Primary gap.)
3. MP4 `----` opaque passthrough (incl. the `build_udta` segment-list refactor, §5).
4. FLAC: `APPLICATION`/`CUESHEET` → DB-backed; `STREAMINFO`/`SEEKTABLE` →
   structural store; resolve re-read elimination. **Gated on the §1 backfill/fallback
   story** (front-read fallback when `structural_blocks` empty + `revalidate`
   backfill) so it does not regress existing FLAC libraries.
5. Test-surface expansion: interop (mutagen) fixtures, fuzz seeds, proptests, and the
   query-split/legacy-FLAC migration tests (§Testing).
