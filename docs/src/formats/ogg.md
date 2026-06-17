# Ogg (Opus / Vorbis / FLAC-in-Ogg)

How musefs scans and synthesizes Ogg files (`.ogg`, `.oga`, `.opus`) carrying
an Opus, Vorbis, or FLAC logical bitstream. Multiplexed and chained Ogg is
detected and skipped at scan time: within the header region every page must
share the first page's serial, and only the first page may carry
beginning-of-stream. For the segment model these layouts plug into, see
[the segment model](../architecture/serving.md#the-segment-model). Native FLAC files
are covered by [FLAC](flac.md).

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
  User-defined keys outside the Vorbis field-name grammar (empty, containing `=`,
  control characters, or non-ASCII — outside ASCII `0x20`–`0x7D` minus `=`) are
  dropped on synthesis and logged.
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
- **Embedded picture descriptions are right-padded with up to two trailing
  spaces.** The FLAC PICTURE block is built with its description padded so the
  *prefix* length — `32 + mime.len() + description.len()`, i.e. everything
  before the image bytes — is a multiple of 3 (`picture_prefix`,
  `musefs-format/src/ogg/mod.rs`), which is what makes
  `base64(prefix ++ image) == base64(prefix) ++ base64(image)` and lets the
  image's base64 be served as an independent, incrementally-streamable
  substring (the [art split](#how-synthesis-works) above). Padding the
  description is the safe place to do it — the MIME type must stay a valid
  type. So a synthesized picture's description can differ from the original by
  up to two trailing spaces; this applies to Opus/Vorbis and OggFLAC alike,
  since both build the block body the same way.

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
   store, never materialized whole. Artwork is streamed at synthesis time:
   page CRCs are computed from page-bounded `ArtSource` windows, and the
   full image and its base64 copy are never materialized. FLAC-in-Ogg
   instead carries one native FLAC `PICTURE` block packet per image (raw
   `OggArtSlice` runs, no base64); the last metadata packet's last-block
   flag and packet 0's 16-bit following-packet count are recomputed to
   match. Art exceeding `MAX_ART_BYTES` (16 MiB − 64 KiB) is rejected by the
   store's `CHECK`, with a resolve-time cap backstopping a writer that
   disables check enforcement.
3. `OggAudio` — one compact segment covering all original audio pages, with
   the page-count delta to apply to every sequence number.

At read time there is **no in-memory page index**: the page containing a
requested offset is found by a bounded backward scan (CRC-validated), then
pages are walked forward with each header patched algebraically and payload
bytes served by exact positioned reads. A one-page memo on the resolved file
short-circuits the scan for sequential reads. A page walk that overruns the
scanned audio bounds is a hard `Malformed` error — corrupt or misaligned
data is refused, not served. Synthesized page sequence numbers wrap modulo
2³² (matching Ogg's `u32` sequence field), so files whose audio pages have
very high sequence numbers serve correctly rather than failing the read.

The forward page-walk reads (`serve_ogg_window`) flow through the shared backing
read-ahead buffer (`BackingReader`, see
[backing read-ahead](../architecture/serving.md#backing-read-ahead)) just like PCM
`BackingAudio` reads, so a sequential Ogg stream amortizes backing latency the
same way. The read-ahead cache holds *raw backing bytes keyed by absolute
offset*, so it is orthogonal to header patching: the algebraic CRC/sequence
rewrite happens on the bytes after they are read, and the cache never sees a
patched page. (The backward `find_page_start` scan and its CRC check stay on the
raw fd — they are short, non-sequential probes that the forward-streaming window
would not help.)

## CRC patching: the linear-CRC trick

This is the neatest thing in the Ogg path. Every Ogg page carries a CRC-32 over
its *entire* contents — header **and** payload, with the 4-byte CRC field
treated as zero during the computation (`musefs-format/src/ogg/crc.rs`).
Renumbering shifts every audio page's sequence number by Δ, which changes 4
header bytes (offsets 18..22). Naively, repairing the CRC means re-checksumming
the whole page — including the up-to-64 KB payload that musefs has gone out of
its way *never* to pull into memory.

It doesn't have to. The Ogg CRC uses init 0, no input/output reflection, and no
final XOR, which makes it **linear over GF(2)**: for two equal-length messages,
`crc32(A ⊕ B) == crc32(A) ⊕ crc32(B)`. Take `A` = the original page and `B` = a
*delta* page the same length as the original but all zeros except bytes 18..22,
which hold `old_seq ⊕ new_seq`. Then `A ⊕ B` is exactly the renumbered page, so:

```text
new_crc = old_crc ⊕ crc32(DELTA)
```

and the payload — identical in `A` and `A ⊕ B` — cancels out entirely. The
patched CRC depends only on the old CRC (already in the header) and the 4-byte
sequence delta. The payload is never read.

Computing `crc32(DELTA)` also avoids walking the page. The 18 leading zero bytes
leave the running CRC at 0 (`TABLE[0] = 0`, so each step is a no-op), so the
computation starts directly from the 4-byte seq delta, then only has to "advance
the CRC over" the trailing zeros (the rest of the header plus the whole payload
length, read straight from the segment table). That advance is `crc_shift_zeros`
— the CRC-32 of *appending n zero bytes*. Appending one zero byte is a fixed
linear map on the 32-bit CRC state, so appending `n` of them is that 32×32 GF(2)
matrix raised to the `n`-th power by repeated squaring: **O(log n)**, independent
of page size. Small, typical pages take a cheaper per-byte loop; only a huge
single packet laced into max-size pages crosses the matrix threshold.

The net effect is that `patch_page_header_algebraic`
(`musefs-format/src/ogg/page.rs`) repairs each served audio page's header from
just its `27 + seg_count` header bytes, in work bounded independent of payload
size — and the audio payload stays untouched on disk, spliced in verbatim by
positioned reads. That is what lets the [Ogg invariant](#the-ogg-invariant)
("renumbering patches, never recopies") hold at serve time without a per-page
in-memory index.

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
