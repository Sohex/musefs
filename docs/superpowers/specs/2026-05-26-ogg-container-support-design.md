# Ogg container support (Opus, Vorbis, FLAC-in-Ogg) — design

Date: 2026-05-26
Status: approved (pre-implementation)

## Goal

Bring the Ogg container into musefs's synthesis model with full parity to the
existing FLAC/MP3/M4A formats: present a re-tagged view — re-written
VorbisComments **and embedded cover art** — while serving the original audio
bytes untouched. Three codecs ship together:

- **Opus** (`.opus`) — `OpusHead` / `OpusTags`.
- **Vorbis** (`.ogg`) — Vorbis ID / comment / setup headers.
- **FLAC-in-Ogg** (OggFLAC) — native FLAC metadata blocks carried one-per-Ogg-packet.

`structure-only` mode already serves any Ogg verbatim via the generic whole-file
`BackingAudio` path; it needs no Ogg-specific code. This design concerns
`synthesis` mode.

## Background: why Ogg needs new machinery

A synthesized virtual file is a `RegionLayout` of `Segment`s
(`musefs-format/src/layout.rs`): `Inline(Vec<u8>)`, `ArtImage { art_id, len }`
(streamed at read time), and `BackingAudio { offset, len }` (a run of the
original file). For FLAC/MP3/M4A the metadata region can be regenerated and
spliced in front of byte-identical audio because the audio bytes carry no
position-dependent framing (M4A only needs its `stco`/`co64` offset table
patched).

Ogg breaks that assumption. Audio is split into **pages**; every page header
carries a **page sequence number** (monotonic across the logical bitstream) and
a **CRC-32 over the whole page**. The first audio page's sequence number is
fixed by how many header pages precede it. Embedded cover art is base64-encoded
into a `METADATA_BLOCK_PICTURE` comment (Opus/Vorbis) or a native PICTURE block
(OggFLAC), which makes the regenerated metadata header larger than the original
and therefore occupy a different number of pages. That shifts every subsequent
audio page's sequence number, so the audio pages must be **renumbered** — and
because the sequence number sits inside the CRC-covered region, each renumbered
page's CRC must be updated too.

### Renumbering without touching audio bytes

The audio invariant is preserved by patching only the ~27-byte page *headers*,
never the payloads:

- **Sequence number:** `new_seq = old_seq + delta`, where
  `delta = new_header_page_count − original_header_page_count` (constant for all
  audio pages).
- **CRC:** patched algebraically, never recomputed over payload. Ogg's CRC-32
  (polynomial `0x04c11db7`, init `0`, no input/output reflection, no final XOR)
  is a **linear map over GF(2)**, so for a fixed-length page:

  ```
  CRC(page_new) = CRC(page_old) XOR CRC(diff)
  ```

  `CRC(page_old)` is already stored in the page header (read for free while
  parsing). `diff` is a page-length buffer that is zero except for the 4
  sequence-number bytes (old XOR new) at offset 18; its CRC depends only on those
  4 bytes plus the page length (trailing zeros "roll the register forward" via a
  precomputed `x^(8·n) mod poly` zero-advance operator). No payload byte is read,
  copied, or modified.

Consequently the audio region is built by walking page **headers** only — each
header's segment table gives the payload length, so we hop header-to-header. I/O
is O(number of pages); CPU is O(pages) with the zero-advance operator.

All multi-byte Ogg page-header fields (granule, serial, sequence, CRC) are
little-endian; the CRC patch operates on the raw header byte buffer.

## Scope

**In:** single-logical-bitstream Ogg carrying Opus, Vorbis, or FLAC, in
`synthesis` mode, with re-tagged VorbisComments and embedded cover art; audio
byte-identical.

**Out (v1):**
- Chained or multiplexed Ogg (more than one logical bitstream — e.g. video+audio,
  or concatenated streams). Detected at probe and **not ingested**.
- A streaming (non-materialized) art segment for Ogg (see Tradeoffs).

## Data model and detection

- **`Format` enum** (`musefs-db/src/models.rs`): add three variants —
  `Opus` (`"opus"`), `Vorbis` (`"vorbis"`), `OggFlac` (`"oggflac"`). Native FLAC
  remains `Flac`. Flat-string convention matches the existing enum;
  `parse`/`as_str` updated.
- **No DB schema migration.** Existing `audio_offset`/`audio_length` bounds are
  reused: `audio_offset` = byte offset of the first audio page; the header region
  `[0, audio_offset)` is re-read at resolve to reconstruct preserved structures.
  Tags and art live in the existing `tags` / `art` / `track_art` tables.
- **`probe`** (`scan.rs`): `OggS` capture pattern at offset 0, then inspect the
  first packet to discriminate codec:
  - `OpusHead` → `Opus`
  - `\x01vorbis` → `Vorbis`
  - `\x7FFLAC` → `OggFlac`
  Return `None` (skip ingest) if a second logical bitstream (different serial /
  another BOS page) appears before audio.

## Module layout

A new `musefs-format/src/ogg.rs`, hand-rolled like `flac.rs`/`mp3.rs` (no decoder
dependency). Internally decomposed into small, independently testable units:

1. **`page`** — parse a page header + segment table; build a page (lace a packet
   stream into pages with correct segment tables, BOS/continuation/EOS flags,
   granule, serial); a page walker that hops header-to-header.
2. **`crc`** — Ogg CRC-32, the linear sequence-number CRC patch
   (`patch_seq_crc`), and the zero-advance operator. Unit-tested against known
   page vectors.
3. **codec detection + header-packet extraction** — locate and reassemble the
   header packets (and, where needed, preserve them).
4. **per-codec header-region builders** (see below).
5. **`locate_audio`** — returns `audio_offset` + codec (used by `scan.rs`).
6. **`read_tags` / `read_pictures`** — extract existing metadata at scan time.
7. **`synthesize_layout`** — assemble the `RegionLayout`.

### Shared VorbisComment / metadata-block helpers

Three codecs now use VorbisComment semantics and two (native FLAC, OggFLAC) use
native FLAC metadata blocks. `flac.rs` already contains `vorbis_comment_body`,
`parse_vorbis_comment_body`, `push_block_header`, `picture_body_framing`, and
picture-block parsing. Extract the VorbisComment body build/parse and the
FLAC-metadata-block (header + PICTURE) build/parse into a small shared module
(e.g. `musefs-format/src/vorbis.rs` / `metablock`), consumed by both `flac.rs`
and `ogg.rs`. This avoids `ogg → flac` coupling and keeps each helper single-purpose.

## Synthesis: `synthesize_layout`

The `RegionLayout` is two regions concatenated.

### (a) Regenerated header region — `Inline` pages

Codec-specific reconstruction, then laced into fresh pages numbered `0, 1, …`:

- **Opus:** preserve the original `OpusHead` page byte-identically. Regenerate
  the `OpusTags` packet (vendor string + comments + base64 `METADATA_BLOCK_PICTURE`
  for art). Lace into pages (granule 0).
- **Vorbis:** preserve the ID-header page. Regenerate the comment packet.
  **Preserve the original setup/codebook packet byte-identically** (re-extracted
  from the backing header region at resolve — not stored in the DB). Re-lace
  `comment + setup` into fresh pages. Setup bytes are codec data, not audio
  samples; re-lacing/copying them does not touch the audio invariant.
- **OggFLAC:** preserve the mapping-header packet (`0x7F "FLAC"` + version +
  16-bit count of following metadata-block packets + native `fLaC` marker +
  STREAMINFO). Regenerate the `VORBIS_COMMENT` and `PICTURE` blocks; preserve
  other structural blocks (SEEKTABLE, CUESHEET, APPLICATION) byte-identically —
  exactly as native FLAC synthesis does. **Recompute the 16-bit following-packet
  count.** Lace one metadata block per Ogg packet, last block's
  last-metadata-block flag set.

These few pages have their CRCs computed normally. This is the **only** place art
is materialized in memory.

### (b) Renumbered audio region

For each original audio page: `Inline`(page header — 27 bytes + segment table,
with patched sequence number and patched CRC) + `BackingAudio`(payload). `delta`
and CRC patch as described in Background. Granule, serial, and EOS flag are
preserved unchanged. No new `Segment` variant is required; `ArtImage` is not used
for Ogg.

## Wiring

- **`reader.rs`** resolve: add `Format::Opus | Format::Vorbis | Format::OggFlac =>
  ogg::synthesize_layout(...)` arms. Like FLAC, this re-reads `[0, audio_offset)`
  from the backing file to reconstruct preserved header structures, then walks
  the audio page headers.
- **`scan.rs`:** probe arms + extraction (audio bounds via `locate_audio`, tags,
  pictures) → upsert, mirroring the FLAC path.

## Edge cases and failure handling

- Chained/multiplexed Ogg → `probe` returns `None`; not ingested.
- Malformed/truncated pages, bad lacing, missing header packets →
  `FormatError::Malformed`, consistent with other formats.
- Backing file changed after scan → existing `BackingChanged` path applies
  unchanged.
- A regenerated 32-bit field overflow (e.g. art too large for a sane file) →
  `FormatError::TooLarge`, consistent with M4A's offset-overflow handling.

## Testing

- **Unit (`ogg.rs` / `crc`):** CRC test vectors; the linear seq#-CRC patch matches
  a from-scratch recompute on sample pages; page lacing round-trips (packet →
  pages → packet). For each codec, synthesize on a tiny fixture and assert:
  1. regenerated tags and art decode correctly;
  2. **every** output page CRC validates and sequence numbers are gapless;
  3. audio-payload bytes are byte-identical to the source.
- **e2e (`musefs-fuse`, `#[ignore]`):** mount → read a synthesized `.opus`,
  `.ogg`, and OggFLAC file → validate with a demuxer. Add dev-dependencies (the
  `ogg` crate for page/CRC validation; optionally `lewton`/`symphonia` to confirm
  full decodability). Extend the beets e2e art coverage to the Ogg codecs.
- Fixtures generated at test time via `ffmpeg`/`opusenc`/`flac` when available,
  else committed as tiny files.

## Tradeoffs and limitations

- **Art is materialized** in the cached layout for Ogg (inside the regenerated
  header pages), unlike FLAC/MP3 streaming `ArtImage`. Bounded by image size ×
  cached entries. A streaming-art segment variant (raw bytes for OggFLAC PICTURE;
  base64 for Opus/Vorbis) is a possible later optimization; OggFLAC's native
  PICTURE block makes it the cleanest candidate.
- Resolve reads the header region plus all page **headers** (not payloads) —
  heavier than FLAC/MP3 but small I/O, and cached in `HeaderCache`.

## Out of scope / future

- Chained / multiplexed Ogg.
- Streaming (non-materialized) art segment for Ogg.

## Documentation fixes (incidental)

- `docs/ROADMAP.md` lists Ogg/Opus and MP4/M4A as deferred, but M4A synthesis is
  in fact implemented (`musefs-format/src/mp4.rs`,
  `docs/superpowers/specs/2026-05-26-m4a-synthesis-design.md`). Correct the M4A
  note and move Ogg into delivered scope when this work lands.
