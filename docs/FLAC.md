# FLAC

How musefs scans and synthesizes native FLAC files (`.flac`). FLAC inside an
Ogg container is a different beast — see [OGG.md](OGG.md). For the segment
model these layouts plug into, see
[ARCHITECTURE.md](../ARCHITECTURE.md#the-segment-model).

## What round-trips

- **All text tags.** Canonical keys (`title`, `artist`, `albumartist`,
  `date`, `tracknumber`, …) map to their conventional Vorbis field names via
  the shared vocabulary (`musefs-format/src/tagmap.rs`); any other field
  round-trips verbatim by its own name. Multi-value fields keep their order.
- **Binary metadata blocks.** `APPLICATION` and `CUESHEET` blocks are
  captured at scan time as binary tags (an `APPLICATION` payload includes its
  4-byte application id) and re-emitted on synthesis, streamed from the DB
  rather than held in memory.
- **Embedded pictures.** Each `PICTURE` block round-trips with its MIME type,
  picture type, description, and dimensions; image bytes are stored
  content-addressed and streamed at read time.
- **Structural blocks.** `STREAMINFO` and `SEEKTABLE` are preserved
  bit-exact. They are captured into the read-only `structural_blocks` store
  at scan time (external tools must not edit them) and re-emitted on
  synthesis.

## Lossy edges

- `PADDING` blocks are dropped — the synthesized file carries no padding.
- Metadata blocks of unknown/reserved types are dropped at scan time.
- The `VORBIS_COMMENT` vendor string is replaced with musefs's own.
- Vorbis field names are case-insensitive by spec; musefs re-emits canonical
  keys under their conventional uppercase names and upper-cases unknown
  field names. A field stored as `MixedCase` comes back as `MIXEDCASE` —
  same field to a conforming reader, different bytes.

## How synthesis works

`flac::synthesize_layout` (`musefs-format/src/flac.rs`) builds the layout in
this order:

1. `Inline` — the `fLaC` marker plus the preserved structural blocks
   (`STREAMINFO`, `SEEKTABLE`, sorted by block type) and a `VORBIS_COMMENT`
   block regenerated entirely from the DB tag rows.
2. `BinaryTag` — one segment per stored `APPLICATION`/`CUESHEET` block,
   streamed from the DB at read time.
3. `ArtImage` — one `PICTURE` block per linked art row; the block framing is
   inline, the image bytes stream from the blob store.
4. `BackingAudio` — the original audio frames, served by positioned reads at
   the stored `audio_offset`/`audio_length`.

Structural blocks normally come from the `structural_blocks` store. A
database scanned before that store existed has no rows there; synthesis then
falls back to re-reading the file's front for every preserved block
(carrying `APPLICATION`/`CUESHEET` inline and suppressing the streamed
binary tags so nothing is emitted twice). A re-scan upgrades the track to
the streamed path.

## Quirks & invariants

- The audio frames are never touched: the backing segment starts exactly at
  the scanned audio offset, and the byte-identical-audio property is asserted
  by `musefs-format/tests/proptest_flac.rs` and the mutagen interop suite
  (`musefs-core/tests/interop_emit.rs`).
- Synthesis re-parses its own inline output in tests
  (`flac_tag_roundtrip_is_stable`): the regenerated front must be a valid
  FLAC metadata region whose computed audio boundary equals the layout's
  header length.
- Block-body sizes are bounded at parse time (`MAX_BLOCK_BODY`); a crafted
  file cannot force a huge allocation.
