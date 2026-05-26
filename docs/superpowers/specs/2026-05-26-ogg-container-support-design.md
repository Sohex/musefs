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
- **CRC:** recomputed per page over the patched page bytes. Ogg's CRC-32
  (polynomial `0x04c11db7`, init `0`, no input/output reflection, no final XOR) is
  recomputed while each page's bytes sit in the buffered scan described below —
  over the page header with its patched sequence number plus the unchanged payload
  — and the resulting 4-byte value is stored in the page index. No payload byte is
  ever copied into the served output or modified; payloads are served only from
  `BackingAudio`.

Building this index needs each page's boundaries and a fresh CRC. Rather than seek
past every payload to touch headers only — one syscall per page, thousands per
track, pathological on an HDD or NAS — the index is built with a single **buffered
sequential pass** over the audio region: each page's bytes pass through the read
buffer once, its new CRC is computed there, and only the per-page
`{offset, length, new_crc}` is kept. The pass is **deferred to the first `read()`
and cached** — it never runs at `open()`/`stat` (see "Layout sizing, lazy
indexing, and cache bounds").

All multi-byte Ogg page-header fields (granule, serial, sequence, CRC) are
little-endian; the CRC patch operates on the raw header byte buffer.

## Scope

**In:** single-logical-bitstream Ogg carrying Opus, Vorbis, or FLAC, in
`synthesis` mode, with re-tagged VorbisComments and embedded cover art; audio
byte-identical.

**Out (v1):**
- Chained or multiplexed Ogg (more than one logical bitstream — e.g. video+audio,
  or concatenated streams). Detected at probe and **not ingested**.

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

1. **`page`** — parse a page header + segment table; build/lace a packet stream
   into pages (precise lacing rules in Synthesis §a); a page walker that hops
   header-to-header.
2. **`crc`** — Ogg CRC-32 (table-driven; polynomial `0x04c11db7`, init `0`, no
   reflection, no final XOR). Unit-tested against known page vectors.
3. **codec detection + header-packet extraction** — locate and reassemble the
   header packets (and, where needed, preserve them).
4. **per-codec header-region builders** (see below).
5. **`locate_audio`** — returns `audio_offset` + codec (used by `scan.rs`).
6. **`read_tags` / `read_pictures`** — extract existing metadata at scan time.
7. **`synthesize_layout`** — assemble the `RegionLayout`.
8. **page index + sizing** — analytic `st_size` (no page walk) and the lazily
   built, cached audio-page index that backs the `OggAudio` segment.
9. **streaming art encoder** — on-the-fly base64 (Opus/Vorbis) or raw (OggFLAC)
   art emission from the SQLite blob at read time, with base64-quantum page
   alignment.

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

These few pages have their CRCs computed at build time (the art bytes must exist
to CRC the page — see "Embedded art" below).

#### Page lacing rules

Once an art-bearing comment/metadata packet is megabytes, lacing across pages is
the common case, not an edge case, so the page builder must implement it exactly:

- A page's segment table holds at most **255** lacing values, so a page carries
  at most **255 × 255 = 65 025** payload bytes; larger packets span many pages.
- A packet of length `L` laces as `⌊L/255⌋` values of `255` followed by one value
  of `L mod 255`. If `L` is an exact multiple of 255, a **terminating `0` lacing
  value** is required to signal packet end — omitting it silently corrupts the
  stream.
- Header-type flags: **BOS (0x02)** only on the logical bitstream's first page;
  **continued (0x01)** set on every page that begins mid-packet (i.e. all but the
  first page of a multi-page packet); **EOS (0x04)** only on the last page (audio
  region). Header-page granule = 0.

#### Embedded art

Art is included via a **streaming** segment so it is never held in the cached
layout:

- At resolve, the art's base64 (Opus/Vorbis) or raw bytes (OggFLAC) are
  materialized **transiently** only to compute the enclosing page CRCs, then
  dropped. Page CRCs can't be shared across tracks (each file's per-stream serial
  number differs), but the materialized art bytes can be **`Arc`-interned by
  `art_id`** so concurrent resolves of an album share one copy.
- Page payload boundaries within the art region are aligned to the base64 quantum
  (4 output chars / 3 source bytes) — and the `METADATA_BLOCK_PICTURE` structure
  prefix is padded to a 3-byte multiple — so each page's chunk encodes
  independently and `read()` can serve an arbitrary sub-range by mapping output
  offsets to 3-byte source groups read incrementally from the DB blob.
- The cached layout stores only the page framing + precomputed CRCs plus the
  streaming-art segment (a new `Segment` variant, `OggArt { art_id, encoding }`,
  where `encoding` is base64 for Opus/Vorbis or raw for OggFLAC); the image bytes
  live only in the DB and are re-encoded per read.

### (b) Renumbered audio region

Represented as a single compact segment, `OggAudio { offset, len, seq_delta }`
(new `Segment` variant) — **not** ~2 entries per page. At `read()` it is backed
by the lazily built, cached page index: payload bytes are served straight from
the backing file, and only the 8 changed header bytes per page (sequence number
at offset 18, patched CRC at offset 22) are overlaid. Granule, serial, and EOS
flag are preserved unchanged.

## Layout sizing, lazy indexing, and cache bounds

- **`st_size` is analytic — no page walk at `open()`.** Renumbering patches bytes
  in place and never changes a page's size, so the audio region's byte length is
  exactly the unchanged `audio_length`. `st_size = header_region_len +
  audio_length`. `open()`/`stat`/`getattr` therefore do **no** audio I/O.
- **Lazy, cached page index.** The first `read()` that touches the `OggAudio`
  segment triggers one buffered sequential pass over the audio region, recording
  per-page `{offset, length, new_crc}` (the CRC recomputed over the patched page).
  It is cached on the `ResolvedFile`, so the
  pass runs at most once per file and is invalidated with the rest of the cache on
  `BackingChanged` / `content_version` change. A reader faults those bytes in
  anyway; a pure metadata scan never pays for it.
- **Concurrency guard.** `fuser` services reads on multiple threads, and tools
  like `cp`/`rsync` fan out concurrent reads at different offsets immediately
  after `open()` — so several threads will fault on the empty index at once. The
  index must be guarded so exactly **one** thread runs the pass while the others
  block until it is populated (no thundering herd of duplicate disk passes). The
  build is fallible (I/O), so the guard needs **fallible init** —
  `once_cell::sync::OnceCell::get_or_try_init`, or a `Mutex<Option<Arc<PageIndex>>>`
  populated under the lock. Plain `std::sync::OnceLock::get_or_init` is unsuitable:
  its closure cannot return `Result`, and a cached error must not poison the file
  permanently — a transient failure must leave the slot unset for the next `read()`
  to retry.
- **Byte-size-bounded `HeaderCache`.** Eviction must be by **total cached bytes**
  (LRU), not entry count. Once any format embeds materialized data, counting
  entries is unsafe; with art streamed (above) the cached layout is small, but the
  byte bound is the general safety net.

## Wiring

- **`reader.rs`** resolve: add `Format::Opus | Format::Vorbis | Format::OggFlac =>
  ogg::synthesize_layout(...)` arms. Like FLAC, this re-reads `[0, audio_offset)`
  to reconstruct preserved header structures and builds the regenerated header
  pages; it computes `st_size` analytically and emits the compact `OggAudio`
  segment **without** walking the audio pages (that happens lazily on first
  `read()`).
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

- **Unit (`ogg.rs` / `crc`):** CRC test vectors against known good pages; page
  lacing round-trips (packet → pages → packet), **including a packet whose length
  is an exact multiple of 255** (terminating-zero case). For each codec, synthesize
  on a tiny fixture and assert:
  1. regenerated tags and art decode correctly;
  2. **every** output page CRC validates and sequence numbers are gapless;
  3. audio-payload bytes are byte-identical to the source.
- **Multi-page art:** a **>100 KB** art fixture to force multi-page lacing of the
  metadata packet; assert the streamed base64 round-trips and page CRCs validate.
- **No-I/O `stat`:** assert `open()`/`getattr` returns the correct `st_size`
  without reading the audio region (the page index stays unbuilt until a `read()`).
- **Cache bound:** opening a many-track album with embedded art stays under the
  configured `HeaderCache` byte budget (art not duplicated per track).
- **e2e (`musefs-fuse`, `#[ignore]`):** mount → read a synthesized `.opus`,
  `.ogg`, and OggFLAC file → validate with a demuxer. Add dev-dependencies (the
  `ogg` crate for page/CRC validation; optionally `lewton`/`symphonia` to confirm
  full decodability). Extend the beets e2e art coverage to the Ogg codecs.
- Fixtures generated at test time via `ffmpeg`/`opusenc`/`flac` when available,
  else committed as tiny files.

## Tradeoffs and limitations

- **Art is never held in the cached layout.** It is materialized only transiently
  at resolve to CRC its pages (deduped across an album via `Arc`-interning), then
  streamed from the DB per read. This preserves the FLAC/MP3 "art not resident"
  property at the cost of re-encoding base64 on each read.
- **First read pays a one-time sequential index pass** over the audio region (read
  once, sequentially, to recompute per-page CRCs; payloads are not retained);
  subsequent reads use the cached index. `open()`/`stat` do no audio I/O.

## Out of scope / future

- Chained / multiplexed Ogg.
- FLAC-in-Ogg with a non-standard mapping version (only the `0x7F "FLAC"` 1.x
  mapping is handled).

## Documentation fixes (incidental)

- `docs/ROADMAP.md` lists Ogg/Opus and MP4/M4A as deferred, but M4A synthesis is
  in fact implemented (`musefs-format/src/mp4.rs`,
  `docs/superpowers/specs/2026-05-26-m4a-synthesis-design.md`). Correct the M4A
  note and move Ogg into delivered scope when this work lands.
