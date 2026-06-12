# Parser strictness hardening (#295 #296 #298 #299)

## Summary

Four independent parser-strictness fixes in `musefs-format` (one touching
`musefs-core` read-path math), all from the same adversarial audit. Each parser
currently accepts malformed *container geometry* that should fail closed with a
controlled `FormatError`, and three of them also accept hostile DB-sourced
structural data at synthesis time. The cardinal invariant is unaffected
throughout: no change copies or mutates backing audio bytes — these fixes only
turn "silently accept malformed geometry" into "reject with a controlled error",
plus one read-path availability fix.

- **#295 FLAC** — validate STREAMINFO structure at scan and synthesis.
- **#296 MP3** — validate the ID3v2 header in the audio locator.
- **#298 Ogg** — wrap page sequence renumbering mod 2³² instead of failing reads.
- **#299 WAV** — enforce the RIFF form size before accepting chunk bounds.

### Threat model

Per `SECURITY.md`, the threat surface is parsing untrusted media *and* a crafted
database (`--db` is trusted "only as far as the documented contract"). So both
surfaces are in scope:

- **Scan-time**: a crafted `.flac`/`.mp3`/`.wav`/`.ogg` file must fail with a
  controlled `FormatError`, not scan into the store with malformed geometry.
- **Synthesis-time**: hostile structural rows from a crafted DB must be rejected
  by `synthesize_layout` rather than emitted into output a downstream decoder
  rejects. (Confirmed: emitting bad structural rows cannot mutate audio bytes —
  the served audio is still a positioned read — so this is defense-in-depth that
  returns a controlled error instead of decoder-rejected output.)

### Approach

Per-format inline hardening: add the validation directly inside each format
module, with a small shared helper *within* a module where its scan and synthesis
paths overlap. No cross-format abstraction — FLAC metadata blocks, ID3 frames,
RIFF chunks, and Ogg pages share a theme but no structure, so a common
"container validator" would be a forced fit. One commit per issue, each green
(the pre-commit hook runs the full workspace test suite).

---

## #295 — FLAC STREAMINFO validation

### Current state

(Code is referenced by function + file; the codebase moves and line numbers go
stale, so they are omitted deliberately.)

`parse_blocks` (`musefs-format/src/flac.rs`) and its bounded twin
`read_metadata_bounded` walk metadata blocks until the first last-block flag,
recording `STREAMINFO`/`APPLICATION`/`SEEKTABLE`/`CUESHEET` bodies. Neither
enforces the FLAC structural rule that STREAMINFO is the first metadata block,
appears exactly once, and has a 34-byte body. `synthesize_layout` sorts stored
structural blocks by type (`sort_by_key(block_type)`; `BLOCK_STREAMINFO == 0`, so
STREAMINFO sorts to index 0) and emits them before the regenerated
VORBIS_COMMENT, assuming the rows describe a valid FLAC front — but neither
scanner rows nor hostile DB rows are required to contain exactly one valid
STREAMINFO.

`synthesize_layout` is invoked from `musefs-core/src/reader.rs` (`HeaderCache::build`)
with structural data from one of two sources: DB rows filtered through
`flac::structural_block_type` (the crafted-DB surface), or — when no structural
rows exist — a fallback re-read `flac::read_metadata(&front)?.preserved`.

Two existing unit tests intentionally characterize the laxness:
`parse_blocks_accepts_header_flush_with_end` (accepts a single empty STREAMINFO)
and `bounded_is_last_flag_continues_past_nonlast_block` (accepts two STREAMINFO).

### Change

Add a shared structural-validation check used by both `parse_blocks` and
`read_metadata_bounded`. The FLAC structural rule to enforce:

1. The **first** metadata block must be `STREAMINFO`.
2. STREAMINFO appears **exactly once**.
3. Its body length is **exactly 34** bytes.

Any violation → `FormatError::Malformed` (the `fLaC` marker is present; the
structure is malformed). In `read_metadata_bounded`, the first block's
header+body may not yet be in the prefix — return `NeedMore { up_to }` as today
until enough bytes are present to validate, then apply the check.

In `synthesize_layout`, reject any `structural` slice that does not contain
exactly one STREAMINFO with a 34-byte body → `FormatError::Malformed`. This check
runs **first**, before the existing `TooLarge` size guards (validate inputs
before doing size arithmetic). APPLICATION/CUESHEET continue to ride through
`binary_tags`, not `structural`; SEEKTABLE remains allowed in `structural`.

Consequence of the fallback re-read: because `read_metadata` → `parse_blocks`
now enforces the rule, a file scanned under the old lax rules with malformed
STREAMINFO and *no* structural DB rows will fail at serve time via the
`reader.rs` fallback (`Err(Malformed)` propagates out of `HeaderCache::build`).
This is intended — serving decoder-rejected FLAC is worse than a controlled
failure — and is distinct from the "no forced re-scan" out-of-scope item (that
concerns the scanner, not this serve-time re-parse).

### Test churn

- Flip `parse_blocks_accepts_header_flush_with_end` and
  `bounded_is_last_flag_continues_past_nonlast_block` to expect
  `FormatError::Malformed`.
- **Ordering hazard — these pass `&[]` (STREAMINFO-less) `structural` and expect
  `TooLarge`; with the new check running first they now return `Malformed`, so
  each must be updated to pass a valid 34-byte STREAMINFO block:**
  - `flac.rs` unit tests `synthesize_layout_picture_block_size_boundary_is_inclusive`,
    `synthesize_layout_vorbis_comment_block_size_boundary_is_inclusive`,
    `synthesize_layout_binary_tag_block_size_boundary_is_inclusive`,
    `synthesize_layout_checked_picture_len_rejects_overflow`.
  - `synthesize_art.rs::synthesize_errors_on_oversized_picture`
    (`synthesize_layout(&[], …, &[art])` expecting `TooLarge`).
- `roundtrip.rs`/`synthesize_art.rs`/`synthesize_tags.rs` tests that build
  structural via `streaminfo_body()` (a valid 34-byte body) already pass a valid
  STREAMINFO and stay green — do not "fix" them.
- `proptest_flac.rs` stays green: it feeds `synthesize_layout` only the output of
  a successful `locate_audio` on `fixtures::flac()`, which emits a valid 34-byte
  STREAMINFO first; the new `parse_blocks` rule accepts that fixture unchanged.

### New tests (from the audit)

- `locate_audio` rejects `fLaC` + last PADDING/VORBIS_COMMENT with no STREAMINFO.
- `locate_audio` rejects a STREAMINFO whose body length is not 34.
- `locate_audio` and `read_metadata_bounded` reject duplicate STREAMINFO.
- `synthesize_layout` rejects hostile structural input lacking a valid
  STREAMINFO (crafted-DB path).

---

## #296 — MP3 ID3v2 header validation in the locator

### Current state

`locate_audio` and `locate_audio_bounded` (`musefs-format/src/mp3.rs`) use a
lightweight ID3v2 skip: they `synchsafe_decode` the size bytes (masking the high
bit of each), honor the footer flag, and never validate the ID3 major version or
reject high-bit size bytes. The stricter `id3v2_alloc_safe` rejects exactly those
shapes (its `data[6] | data[7] | data[8] | data[9] >= 0x80` guard). The
divergence lets a malformed ID3-looking 10-byte header
(`49 44 33 04 00 00 00 00 00 80` — `"ID3"`, version 4, flags 0, size bytes
`00 00 00 80` with the high bit set in the last byte) mask-decode to
`audio_offset = 10`; if bytes 10–11 satisfy the MPEG sync check (`0xff`,
`0xfb`) the file scans as MP3 with an audio window starting inside malformed
metadata.

### Change

Extract a shared ID3v2 header validator — the **intersection** of the three
checks, *not* a wholesale extraction of `id3v2_alloc_safe`. Given the 10-byte
header it requires:

1. `ID3` magic.
2. Major version in `2..=4`.
3. High-bit-clear synchsafe size bytes (`data[6..10]`, each `< 0x80`).

On success it returns the decoded tag length (the locators add the 10-byte footer
when the footer flag is set, as today); on violation the locators return
`FormatError::Malformed`. `id3v2_alloc_safe` calls this helper for the header
portion and keeps its additional, stricter checks layered on top
(extended-header/unsync flag rejection, frame walking) — the plan must not push
those flag checks down into the locator (see Decided, below).

**Decided:** the locator does **not** reject unsynchronization or extended-header
flags. The declared tag size already accounts for an extended header, and
unsynchronization only affects frame *content*, not the size field — so neither
shifts the audio offset, and rejecting them would gratuitously fail *valid*
MP3s. `id3v2_alloc_safe` keeps its stricter skip-for-OOM behavior (those files
still scan as MP3 but lose scan-time tag extraction, which is the existing,
intentional behavior; tags come from the DB).

### New tests (from the audit)

- `locate_audio` and `locate_audio_bounded` reject high-bit size bytes even when
  the masked offset lands on `0xff, 0xfb`.
- `locate_audio` rejects unsupported ID3 major versions at offset 0.

---

## #298 — Ogg page sequence-number wrap

### Current state

`ogg::synthesize_layout` (`musefs-format/src/ogg/mod.rs`) computes
`seq_delta = i64::from(seq) - i64::from(header.header_pages)` (synthesized header
page count minus original header page count). At read time `serve_ogg_window`
(`musefs-core/src/ogg_index.rs`) applies it with checked signed arithmetic:

```rust
let new_seq = u32::try_from(i64::from(old_seq) + seq_delta)
    .map_err(|_| musefs_format::FormatError::Malformed)?;
```

A valid-or-crafted file whose first audio pages have high sequence numbers reads
fine until a positive `seq_delta` pushes a page over `u32::MAX`, at which point
reads fail — a read-path availability bug. The local reference helper
(`new_reference_region`) already uses `wrapping_add`.

### Change

Wrap mod 2³² (Ogg's `page_sequence_number` is a `u32` that naturally wraps in
long streams; decoders use it only for gap detection):

```rust
let new_seq = old_seq.wrapping_add(seq_delta as u32);
```

`(a + d) mod 2³² == (a + (d mod 2³²)) mod 2³²`, and `seq_delta as u32` is
`d mod 2³²`, so this is the correct wrapped value for both positive and negative
deltas. CRC patching already runs over `new_seq`, so it stays consistent with the
emitted header bytes. Document the wrap in `docs/OGG.md`.

### New tests (from the audit)

- First audio page `seq = u32::MAX` with `seq_delta = +1` wraps to `0` and reads
  succeed.
- Corresponding low-sequence / negative-delta case (e.g. `seq = 0`,
  `seq_delta = -1` → wraps to `u32::MAX`) wraps and reads succeed.
- A regression that specifically crosses the `u32::MAX` boundary and compares
  `serve_ogg_window` output to a wrapping oracle. (The existing
  `serve_ogg_window_whole_region_matches_reference` already compares against
  `new_reference_region` for ordinary deltas; the new test must exercise the
  *wrap*, not just any delta.)

---

## #299 — WAV RIFF form-size enforcement

### Current state

`riff_wave_start` (`musefs-format/src/wav.rs`) validates only the `RIFF`/`WAVE`
magic and ignores the size field at bytes 4..8. `walk_chunks` walks to the
physical buffer length, not the declared form end. `locate_audio` and
`locate_audio_at_ceiling` accept the first in-bounds `data` chunk regardless of
the declared form size. A crafted WAV can declare a form size that ends before
`data`, or larger than the file, and still be ingested. The ceiling fixture
documents this by writing a zero RIFF size (`// size field unused by the walk`).

### Change

- `riff_wave_start` parses the RIFF size (`u32` LE at bytes 4..8) and exposes the
  declared `form_end = 8 + riff_size` to callers (e.g. return
  `(start, form_end)`). This signature change touches every caller —
  `walk_chunks`, `locate_audio`, `locate_audio_at_ceiling`, and `read_structure`
  (which currently calls `riff_wave_start(front)?` and discards the result; it
  must be updated to compile even though it ignores `form_end`).
- `walk_chunks` walks chunks only within `min(form_end, buf.len())`.
- `locate_audio` (full file present): reject when `form_end > buf.len()` or when
  the `data` chunk's end exceeds `form_end` → `FormatError::Malformed`.
- `locate_audio_at_ceiling` (file past probe budget): validate the declared RIFF
  size against `file_len` — reject when `form_end > file_len` or the `data` end
  exceeds `form_end`. It stays best-effort only about losing trailing metadata,
  not about container geometry.
- Update the ceiling-probe fixture to write a valid RIFF size.

**Known limitation (accepted, documented):** strict enforcement rejects streaming
or concatenated WAVs that write `riff_size = 0` or `0xFFFFFFFF` as a placeholder.
These are rare in a curated music library — finished WAVs from CD rippers
(EAC, dBpoweramp, XLD) and DAW exports patch the real size; the sentinel pattern
comes from live capture to a pipe or files truncated mid-write, which rarely
land in a library and are usually malformed when they do. Rejection is graceful:
the scanner calls `locate_audio(bytes).ok()?` (in `scan.rs`'s `detect` helpers),
so an `Err(Malformed)` becomes "not recognized as WAV" → the file is skipped from
the virtual tree, never touched; re-encoding fixes it. If a real user hits this,
allowing the two sentinels is a trivial follow-up.

### New tests (from the audit)

- A RIFF header whose declared size ends before `data` is rejected.
- A RIFF header whose declared size exceeds the physical file length is rejected.
- A correctly sized RIFF with odd-size chunks and trailing metadata still parses.
- The ceiling-probe fixture uses a valid RIFF size (or explicitly tests the chosen
  best-effort exception).

---

## Documentation

- `docs/FLAC.md`: note that the synthesized front requires exactly one 34-byte
  STREAMINFO first, and that malformed STREAMINFO is rejected at scan/synthesis.
- `docs/MP3.md`: clarify the ID3v2 audio-boundary contract (locator validates
  version + synchsafe size; unsync/extended-header tags still scan).
- `docs/OGG.md`: document that page sequence numbers wrap mod 2³².
- `docs/WAV.md`: document RIFF form-size enforcement and the streaming-sentinel
  known limitation.

## Out of scope

- Re-validating files already scanned under the lax rules — the freshness path
  re-probes on change; no forced re-scan.
- The `0`/`0xFFFFFFFF` RIFF-size carve-out (deferred per above).
- Findings already tracked elsewhere (#266 Ogg art materialization, #267
  unbounded text tags, #274 aggregate length math).
