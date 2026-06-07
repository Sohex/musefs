# Ogg (Opus / Vorbis / FLAC-in-Ogg)

How musefs scans and synthesizes Ogg files (`.ogg`, `.oga`, `.opus`) carrying
an Opus, Vorbis, or FLAC logical bitstream. Multiplexed and chained Ogg is
detected and skipped at scan time: within the header region every page must
share the first page's serial, and only the first page may carry
beginning-of-stream. For the segment model these layouts plug into, see
[ARCHITECTURE.md](../ARCHITECTURE.md#the-segment-model). Native FLAC files
are covered by [FLAC.md](FLAC.md).

## The Ogg invariant

Original Ogg **packet payload bytes are preserved** during synthesis; page
sequence numbers and CRCs may be patched intentionally. Synthesis regenerates
the logical bitstream's header pages (to embed fresh tags and art), which
changes the header page count; the audio pages that follow are served
verbatim except that each page header's sequence number is shifted by a
constant delta and its CRC recomputed in place. The served audio byte length
is unchanged — renumbering patches, never recopies.

Verified by `musefs-format/tests/proptest_ogg.rs` (crate feature `fuzzing`),
`read_at` integration tests comparing source and synthesized audio payloads
(`musefs-core/src/reader.rs` test modules), and the mutagen interop suite
(`musefs-core/tests/interop_emit.rs`).

## What round-trips

- **All text tags.** VorbisComments are rebuilt from the DB through the same
  builder as FLAC: canonical keys map to their conventional field names via
  the shared vocabulary (`musefs-format/src/tagmap.rs`); any other field
  round-trips verbatim by its own name, in order, multi-values included.
- **Embedded pictures**, with MIME type, picture type, description, and
  dimensions — in both art encodings (see below).
- **Codec headers.** The identification packet (`OpusHead`, Vorbis
  identification, the OggFLAC `STREAMINFO` carrier) and any trailing header
  packets (e.g. the Vorbis setup packet) are preserved; only the comment
  metadata is regenerated.

## Lossy edges

- The VorbisComment vendor string is replaced with musefs's own.
- Vorbis field names are case-insensitive by spec; canonical keys come back
  under their conventional uppercase names and unknown field names are
  upper-cased on synthesis.
- Ogg carries no binary-tag slot: only text comments and pictures exist, so
  there is nothing else to preserve.

## How synthesis works

`ogg::synthesize_layout` (`musefs-format/src/ogg/mod.rs`) produces:

```text
 offset 0
 ┌──────────────────────────────────────────────┐ ┐
 │ █ identification page, preserved     (Inline) │ │ regenerated
 │ █ comment page(s) rebuilt from DB    (Inline) │ │ header
 │ ▒   art windows, base64/raw    (OggArtSlice)  │ │ pages
 │ █ trailing header pages, preserved   (Inline) │ │ (repaginated)
 ├──────────────────────────────────────────────┤ ┘
 │ ░ audio pages: payload verbatim,   (OggAudio) │
 │ ░ page seq += Δ, CRC repatched in place       │
 └──────────────────────────────────────────────┘
 EOF     █ inline-generated   ▒ DB-streamed
         ░ backing pages (headers patched in place, payload untouched)
         Δ = synthesized header page count − original
```

1. `Inline` — the regenerated header pages: the preserved identification
   packet, a comment packet rebuilt from the DB, and the preserved trailing
   header packets, repaginated with correct CRCs.
2. **The art split.** Opus and Vorbis embed art as base64
   `METADATA_BLOCK_PICTURE` comments (the decoded bytes are a FLAC PICTURE
   block body): each image is an `OggArtSlice` run — a window of
   `base64(image)` encoded **incrementally at read time** from the blob
   store, never materialized whole. FLAC-in-Ogg instead carries one native
   FLAC `PICTURE` block packet per image (raw `OggArtSlice` runs, no
   base64); the last metadata packet's last-block flag and packet 0's
   16-bit following-packet count are recomputed to match.
3. `OggAudio` — one compact segment covering all original audio pages, with
   the page-count delta to apply to every sequence number.

At read time there is **no in-memory page index**: the page containing a
requested offset is found by a bounded backward scan (CRC-validated), then
pages are walked forward with each header patched algebraically and payload
bytes served by exact positioned reads. A one-page memo on the resolved file
short-circuits the scan for sequential reads. A page walk that overruns the
scanned audio bounds is a hard `Malformed` error — corrupt or misaligned
data is refused, not served.

## Quirks & invariants

- Page and header sizes are bounded at parse and serve time
  (`MAX_OGG_PAGE_BYTES`, `MAX_OGG_HEADER_BYTES` in
  `musefs-core/src/ogg_index.rs`); a crafted file cannot force unbounded
  allocation. The `ogg`, `ogg_page`, `b64`, and `vorbiscomment` fuzz targets
  hammer these paths.
- The incremental base64 encoder is windowed by output offset: any byte
  range of the encoded form can be produced from the corresponding slice of
  raw image bytes (`musefs-format/src/ogg/b64.rs`).
- The serve path's determinism does not depend on the memo: a content change
  rebuilds the resolved file and starts with a fresh, empty memo.
