# FLAC

How musefs scans and synthesizes native FLAC files (`.flac`). FLAC inside an
Ogg container is a different beast — see [Ogg](ogg.md). For the segment
model these layouts plug into, see
[the segment model](../architecture/serving.md#the-segment-model).

## What round-trips

- **All text tags.** Canonical keys (`title`, `artist`, `albumartist`,
  `date`, `tracknumber`, …) map to their conventional Vorbis field names via
  the shared vocabulary (`musefs-format/src/tagmap.rs`); any other field
  round-trips verbatim by its own name. Multi-value fields keep their order.
  User-defined keys that are not legal Vorbis field names (empty, containing `=`,
  control characters, or non-ASCII bytes — i.e. outside ASCII `0x20`–`0x7D` minus
  `=`) are dropped on synthesis and logged; they cannot round-trip by name.
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

## Leading ID3 tags

FLAC does not define an ID3 tag, but files that put one (or several) in front
of the `fLaC` marker exist in the wild — usually a blank header left behind by
a converter. musefs scans them rather than skipping them:

- The tag run is stepped over to find the `fLaC` marker, and the recorded audio
  offset counts it, so the audio segment still reads the right range out of the
  backing file. A tag is stepped over by its **declared size**, whatever its
  version: the ID3v2 header has the same shape in every version, and the spec's
  own rule for a version a reader does not understand is to ignore the tag. A
  header musefs cannot parse the *frames* of is therefore still skipped
  correctly — only its contents go unread. What is required is that the header
  matches the spec's detection pattern (`$49 44 33 yy yy xx zz zz zz zz`, both
  version bytes below `$FF`, every size byte below `$80`) and that its declared
  length lands on the marker. A header that fails either test contradicts the
  file it is in, and the file is skipped rather than guessed at.
- The tag's **text frames and `APIC` pictures are ingested as a fallback**: the
  FLAC's own `VORBIS_COMMENT` / `PICTURE` blocks win, and the ID3 tag only
  supplies keys the FLAC does not define. The rule is all-or-nothing per key, so
  a multi-value field never mixes values from both sources. A file whose tags
  live *only* in the ID3 header is therefore still ingested with its tags.
- A 128-byte trailing ID3v1 tag is trimmed from the audio length, so it is not
  served as audio. This check runs only for files that carry a leading ID3v2
  tag: a stock FLAC pays no extra read, and audio that happens to spell `TAG`
  128 bytes from the end is never mistaken for a trailer.
- Neither tag survives into the synthesized file — see the lossy edge below.

## Lossy edges

- A leading ID3v2 tag is **not preserved**: it is metadata, and the synthesized
  file is regenerated metadata in front of untouched audio. Its contents are
  ingested first (above), so they come back as Vorbis comments and `PICTURE`
  blocks — the served file is a stock FLAC that starts at `fLaC`. This matches
  how MP3's original ID3v2 tag is treated.
- A trailing ID3v1 tag is **dropped without being read**. Unlike the leading
  tag, its fields are never parsed into tags — it is only excluded from the
  audio length so its 128 bytes are not served as audio. This mirrors MP3, where
  ID3v1 is likewise not read; populate those tags through the store instead.
- `PADDING` blocks are dropped — the synthesized file carries no padding.
- Metadata blocks of unknown/reserved types are dropped at scan time.
- A `PICTURE` block whose picture type falls outside the standard `0`–`20`
  range is clamped to `0` (`Other`) at scan time, matching the store's
  `track_art.picture_type` `CHECK`. This shared `PICTURE` parser also serves
  FLAC-in-Ogg, so the same clamp applies there.
- The `VORBIS_COMMENT` vendor string is replaced with musefs's own.
- Vorbis field names are case-insensitive by spec; musefs re-emits canonical
  keys under their conventional uppercase names and upper-cases unknown
  field names. A field stored as `MixedCase` comes back as `MIXEDCASE` —
  same field to a conforming reader, different bytes.

## How synthesis works

`flac::synthesize_layout` (`musefs-format/src/flac.rs`) builds the layout in
this order — an inline metadata region, DB-streamed payloads, then the
untouched audio:

```text
 offset 0
 ┌──────────────────────────────────────────────┐ ┐
 │ █ "fLaC" marker                      (Inline) │ │
 │ █ STREAMINFO / SEEKTABLE, bit-exact  (Inline) │ │ generated
 │ █ VORBIS_COMMENT rebuilt from DB     (Inline) │ │ metadata
 │ ▒ APPLICATION / CUESHEET bodies   (BinaryTag) │ │ region
 │ █ PICTURE framing + ▒ image bytes  (ArtImage) │ │
 ├──────────────────────────────────────────────┤ ┘
 │ ░ audio frames, verbatim       (BackingAudio) │
 └──────────────────────────────────────────────┘
 EOF     █ inline-generated   ▒ DB-streamed   ░ untouched backing
```

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
- The parser now rejects (at scan and synthesis) any FLAC whose metadata does
  not begin with exactly one 34-byte STREAMINFO block; a crafted store
  providing malformed structural rows fails synthesis with a controlled error
  rather than emitting decoder-rejected output.
