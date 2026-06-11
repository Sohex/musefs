# Ogg artwork: stream instead of materialize, and cap oversized art at resolve

Issue: #266 (Reject or stream oversized Ogg artwork instead of materializing it
during layout synthesis). Related: #284 (scanner silently drops oversized art).

## Problem

Ogg synthesis is the only format path that materializes a full artwork image —
and, for Opus/Vorbis, its base64 copy too — while building regenerated header
pages, before the final `RegionLayout` exists.

- `musefs-core/src/mapping.rs::track_art_images` reads each linked art blob whole
  via `db.read_art_chunk(..., 0, data_len)` into `Vec<Vec<u8>>`.
- `musefs-format/src/ogg/mod.rs::comment_packet_chunks` stores
  `b64_encode(art.image)` in `PayloadChunk::Art.out` (Opus/Vorbis, +1.33×).
- `musefs-format/src/ogg/mod.rs::oggflac_packets_with_art` stores
  `art.image.to_vec()` in `PayloadChunk::Art.out` (OggFLAC).

Peak transient memory is ~2.3× the image size, paid once per resolve. Every other
format (FLAC/MP3/MP4/WAV) streams art at read time via `Segment::ArtImage` and
never buffers it, so this breaks the documented invariant — "large DB payloads
are streamed at read time, not materialized whole" — for Ogg specifically.

Two consequences:

1. A hostile or careless SQLite writer can insert a large `art.data` row (the
   store is a documented external-writer contract; the scanner's ingest cap does
   not apply to direct inserts) and force a large resolve-time allocation.
2. There is no serve-time ceiling on art size for Opus/Vorbis (only a ~3 GiB u32
   VorbisComment guard) or, effectively, for MP3/MP4/WAV. Only FLAC/OggFLAC reject
   oversize art at serve, as a structural consequence of FLAC's 24-bit
   `MAX_BLOCK_BODY` (16 MiB − 1).

### Why the materialization exists

It is purely a page-CRC artifact. The layout output is already byte-free:

- Lacing and page geometry derive from `lacing_values(total)` over chunk
  `out_len()` — length-driven, no bytes needed.
- `emit_segments` already stores art as `Segment::OggArtSlice { art_id, offset,
  len, base64, art_total }` and reads no art bytes.

The single consumer of art bytes at synthesis is the CRC pass: `copy_payload`
assembles each full page into a `Vec` so `lace_chunks_to_segments` can
`crc32(&page)`. Because a packet laces into pages strictly in order, the art's
output range is walked forward once — a single sequential pass suffices, no random
access or re-reads.

## Goals

- **Stream Ogg art end-to-end.** Synthesis peak memory becomes O(one Ogg page)
  (≤ ~64 KiB), independent of art size. The "art is streamed at read time, never
  materialized whole" invariant becomes true for Ogg.
- **Enforce one art-size ceiling at resolve, for all formats.** Promote the
  scanner's `MAX_ART_BYTES` (16 MiB − 64 KiB) to a serve-time invariant; reject
  oversize art with a diagnosable log line rather than silently or opaquely.
- **Byte-output-preserving.** Served Ogg files are byte-identical to today; only
  synthesis-time memory changes.

## Non-goals

- Changing served bytes for any format.
- Adding scanner-side logging for the silent oversize-art drop — tracked
  separately as #284.
- Per-art CRC memoization (`crc_shift_zeros` / `patch_page_header_algebraic`
  combine machinery) — an optimization the layout cache already makes unnecessary.

## Design

Two independent components. Component A is separable and lands first; Component B
is the streaming rework.

### Component A — Shared resolve-time art cap (all formats)

`MAX_ART_BYTES = 16 * 1024 * 1024 - 64 * 1024` currently lives as a private const
in `musefs-core/src/scan.rs`, used only to filter scanned pictures at ingest.

- Promote it to a constant shared by `scan.rs` and `mapping.rs` (a `pub(crate)`
  const in a core module). `scan.rs` keeps using it for its ingest filter
  unchanged.
- In `musefs-core/src/mapping.rs::track_art_to_inputs` — the single art-input
  builder consumed by every format branch in `reader.rs` — after deriving
  `data_len`, reject any art whose `data_len.get()` exceeds `MAX_ART_BYTES` with a
  new `CoreError::ArtTooLarge { track_id, art_id, byte_len }`, after emitting a
  `log::warn!` naming the track, art id, byte length, and cap.

Effects:

- The check is in shared code, so the cap applies uniformly to
  FLAC/MP3/MP4/WAV/Ogg.
- Behavior on an oversize row: the track fails to resolve with `ArtTooLarge` —
  consistent with FLAC's existing `MAX_BLOCK_BODY` reject — and the cause is
  visible in the logs rather than surfacing only as an opaque read error.
- Legitimate cover art is far below 16 MiB, so this bites only
  pathological/hostile rows. The scanner's silent ingest drop is unchanged here
  (see #284).

### Component B — Stream Ogg art end-to-end

Five edits. The format layer stays pure (no DB dependency) and fuzzable.

1. **`PayloadChunk::Art` drops `out: Vec<u8>`.** It carries `{ art_id, base64,
   art_total }`. `out_len()` computes `b64_len(art_total)` when `base64`, else
   `art_total`. This removes the +1.33× base64 buffer (Opus/Vorbis) and the
   `art.image.to_vec()` (OggFLAC) — the two materializations the issue names.

2. **`OggArt` carries metadata only.** Drop `image: &[u8]`. Delete
   `musefs-core/src/mapping.rs::track_art_images` and its test; `reader.rs` stops
   pre-reading blobs into `art_images`.

3. **`ArtSource` trait — defined in the format layer, implemented in core.** A
   minimal reader over art bytes by id:

   ```rust
   pub trait ArtSource {
       fn read_window(&self, art_id: i64, offset: u64, buf: &mut [u8]) -> Result<()>;
   }
   ```

   where `Result` is `musefs-format`'s `Result<T, FormatError>`. `FormatError`
   currently `#[derive(Debug, Error, PartialEq, Eq)]` (`error.rs:3`) and is compared
   with `assert_eq!`/`matches!` across the format crate's tests, so the new variant
   **must stay `PartialEq + Eq`** — a `Box<dyn std::error::Error>` payload would
   break the derive and every comparison site. The variant is therefore
   `FormatError::ArtRead { art_id: i64 }` (an `i64` is `Eq`). The core `ArtSource`
   impl logs the underlying typed `DbError` from `read_art_chunk_into` (with art id
   and offset) at the point of failure, then returns `ArtRead { art_id }`; the
   typed error is preserved in the log, not in the enum. This keeps
   `synthesize_layout`'s return type non-generic `Result<RegionLayout, FormatError>`
   and leaves the `Eq` derive intact.

   Rejected alternative: making `ArtSource` carry an associated `Error` and
   `synthesize_layout` generic over it. It avoids a new `FormatError` variant but
   ripples a generic error parameter through `synthesize_layout` →
   `lace_chunks_to_segments` → the CRC feeder, and the read path already flattens
   every art-read failure to `CoreError` at the `reader.rs` boundary, so the typed
   error buys nothing the log doesn't already give.

   `musefs-core` implements the trait over `read_art_chunk_into` (the existing
   streaming read); format-layer tests and the `fuzz/` target implement it over an
   in-memory `art_id → &[u8]` map (infallible). `synthesize_layout` takes art
   metadata plus `&dyn ArtSource` instead of `&[OggArt]` carrying bytes. The
   `ArtRead` variant needs no conversion code: `CoreError` already takes
   `FormatError` via `#[from]` (`musefs-core/src/error.rs`), so it reaches the FUSE
   layer unchanged in shape.

4. **Incremental CRC.** Add `crc32_update(state: u32, chunk: &[u8]) -> u32` to
   `musefs-format/src/ogg/crc.rs`. The existing `crc32` starts at state 0 with no
   final inversion, so `crc32(a ++ b) == crc32_update(crc32_update(0, &a), &b)` —
   a direct extraction of the existing loop. `crc32(buf)` becomes
   `crc32_update(0, buf)`.

5. **Rewrite the CRC pass: `copy_payload` → a streaming feeder.**
   `lace_chunks_to_segments` stops assembling a full `page` `Vec`. Per page:
   - Build the 27-byte page header + lacing table with the CRC field zeroed; feed
     it to a running CRC via `crc32_update`.
   - Walk the page's payload span `[payload_pos, payload_pos + page_payload)`
     across the chunk list, feeding the CRC:
     - `Bytes` chunks: feed the overlapping slice directly (small header bytes,
       already in memory).
     - `Art` chunks: for the overlapping output span, use `b64_window` to compute
       the 3-byte-aligned raw read plan, read that window from the `ArtSource`
       into a reused buffer, base64-encode on the fly when `base64`, take exactly
       the `[skip, skip + len)` output bytes, and feed them to the CRC. For raw
       (OggFLAC) art, read and feed the raw window directly.
   - Finalize the CRC, write it into `header[22..26]`, and call `emit_segments`
     (unchanged — already byte-free) with the header.

   The per-page art span is bounded by Ogg page geometry (≤ 255 × 255 ≈ 65025
   output bytes), so the window buffer is inherently page-bounded. Total reads over
   an art equal one full forward pass — the same I/O as today; only memory
   improves. The 3-byte base64 alignment carry is exactly what `b64_window`
   already solves on the read path, so it is reused, not reinvented.

Peak synthesis memory after B: running CRC state + one page-sized raw window + its
base64 buffer — independent of art size.

#### Coupling: `art_total` now equals the art's byte length

Today `PayloadChunk::Art` carries `out` (the actual output bytes; geometry follows
`out.len()`) *and* a separate `art_total` (metadata for the read-time
`OggArtSlice`). Because `out` is self-consistent, the two are free to disagree, and
the `fuzz/fuzz_targets/ogg.rs` target and the `chunk_lacer_splits_art_across_pages_
and_crcs_validate` test in `page.rs` deliberately exercise that disagreement
(e.g. `art_total: 12345` with a 70 000-byte `out`).

After B there is no `out`: geometry is derived from `art_total` (`out_len()` =
`b64_len(art_total)` or `art_total`) and the bytes come from the `ArtSource`. So
`art_total` must equal the number of bytes the source yields — the two can no
longer diverge. This is sound in production: `musefs-db/src/schema.rs` enforces
`CHECK (byte_len = length(data))` on the `art` table (test
`v4_art_rejects_byte_len_mismatch`), and `data_len`/`art_total` derive from
`byte_len`, so the stored blob is always exactly `art_total` bytes.

Consequence for the harnesses (both must change in Component B):

- `fuzz/fuzz_targets/ogg.rs`: drop the "lengths must be free to disagree"
  decoupling; generate each image with length equal to its `data_len` (or derive
  `data_len` from the generated image) and feed it through the in-memory
  `ArtSource`. The current comment documenting the divergence is removed.
- `page.rs` `chunk_lacer_splits_art_across_pages_and_crcs_validate`: construct
  `PayloadChunk::Art { art_id, base64, art_total: N }` with a matching in-memory
  source of exactly `N` bytes (choosing `N` large enough that `b64_len(N)` still
  spans pages).

### Callers / migration surface

Signature changes ripple to a bounded, mostly-test set (verified by grep):

- `synthesize_layout` (now metadata + `&dyn ArtSource`): real caller
  `reader.rs:282`; zero-art callers passing `&[]` (`ogg_index.rs:339`,
  `proptest_ogg.rs:18`, several `ogg/mod.rs` tests) adapt trivially; with-art test
  callers in `ogg/mod.rs` (≈ lines 918, 1027, 1058, 1091, 1123, 1151, 1186, 1320)
  and `fuzz/fuzz_targets/ogg.rs:53` migrate to the in-memory `ArtSource`.
- `OggArt` (loses `image`): all ~13 construction sites are tests + `reader.rs:277`
  + `fuzz`.
- `PayloadChunk::Art` (loses `out`): producers `ogg/mod.rs:410`
  (`comment_packet_chunks`), `ogg/mod.rs:471` (`oggflac_packets_with_art`),
  `page.rs:591` (test); consumers `out_len` (`page.rs:283`), `copy_payload`
  (`page.rs:365`, rewritten), `emit_segments` (`page.rs:392`, already byte-free).
- `track_art_images`: delete the fn (`mapping.rs:95`), its test
  (`mapping.rs:413`), and the `reader.rs:273` call site.
- The `flatten`/reconstruct verify helpers (`page.rs:557`, `ogg/mod.rs` header
  region) already take an `art_id → bytes` map and become the in-memory
  `ArtSource`.

### Data flow (Ogg resolve, after)

```
reader.rs (HeaderCache::build, Ogg branch)
  → track_art_to_inputs(db, track_id)         # metadata only; enforces MAX_ART_BYTES (A)
  → synthesize_layout(header, …, art_metas, &DbArtSource)   # no blob pre-read (B)
      → build_packets_with_art → PayloadChunk::Art { art_id, base64, art_total }  # no bytes
      → lace_chunks_to_segments
          → per page: stream CRC via ArtSource windows (B); emit_segments → OggArtSlice
  → RegionLayout (OggArtSlice runs, no art bytes)  # unchanged shape
read_at (read time, unchanged)
  → OggArtSlice → b64_window → read_art_chunk → encode_b64_slice
```

## Error handling

- `CoreError::ArtTooLarge { track_id, art_id, byte_len }` is returned by
  `track_art_to_inputs`, preceded by a `log::warn!`. It propagates like other
  resolve errors (e.g. `OrphanedArt`) and makes the track unreadable until the row
  is corrected — the same outcome FLAC already produces for oversize art.
- `ArtSource::read_window` returns `Result<(), FormatError>`; the core impl wraps
  its `DbError` in the new `FormatError::ArtRead(Box<…>)` variant, which
  `synthesize_layout` surfaces without buffering. `reader.rs` already converts
  `FormatError` into `CoreError`, so the read failure reaches the FUSE layer
  unchanged in shape.
- Existing structural guards stay as defense in depth: the u32 VorbisComment
  value-length check in `build_packets_with_art`, and FLAC's `MAX_BLOCK_BODY` in
  `oggflac_packets_with_art`.

## Testing

- **Cap (A):** `mapping.rs` unit test — art at exactly `MAX_ART_BYTES` resolves;
  one byte over yields `ArtTooLarge` and logs a warning. Shared check, so it
  covers all formats.
- **Streaming (B):** existing Ogg synthesis byte-exactness tests must stay green
  unchanged — byte-identical output is the correctness bar. Add a test asserting
  the in-memory `ArtSource` is read in bounded windows (no single full-image
  read). Keep the existing verify/round-trip helper (`ogg/mod.rs`, the
  segment-reconstruction path) over the in-memory source.
- **Fuzz:** update the out-of-workspace `fuzz/` Ogg target for the
  `OggArt`/`PayloadChunk`/`ArtSource` signature change; confirm with
  `cargo +nightly fuzz build`.
- Pre-commit runs the full workspace suite, so every commit stays green.

## Commit split

1. Component A: promote `MAX_ART_BYTES`, add `CoreError::ArtTooLarge` + warn,
   enforce in `track_art_to_inputs`, cross-format test, docs.
2. Component B: `ArtSource` trait + DB impl, `crc32_update`, `PayloadChunk::Art`
   and `OggArt` signature change, streaming CRC rewrite, delete
   `track_art_images`, update format tests + fuzz target, docs.

## Docs to update

- `docs/OGG.md`: art is now streamed at synthesis (no whole-image
  materialization); note the shared serve-time `MAX_ART_BYTES` cap.
- `ARCHITECTURE.md`: the "art is streamed at read time, never materialized whole"
  invariant now holds for Ogg; mention the resolve-time `MAX_ART_BYTES` cap in the
  external-writer-contract section.
