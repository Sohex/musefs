# MP3

How musefs scans and synthesizes MP3 files (`.mp3`) and their ID3v2 metadata.
For the segment model these layouts plug into, see
[the segment model](../architecture/serving.md#the-segment-model). The ID3v2 builder
described here is shared with WAV's embedded `id3 ` chunk — see
[WAV](wav.md).

## What round-trips

- **Canonical text tags** (`title`, `artist`, `albumartist`, `date`,
  `tracknumber`, …) map to their standard ID3v2 text frames (`TIT2`, `TPE1`,
  `TPE2`, `TDRC`, `TRCK`, …) via the shared vocabulary
  (`musefs-format/src/tagmap.rs`). NUL-separated multi-value frames yield one
  tag row per value and are re-emitted NUL-separated in a single frame.
- **Vocabulary `TXXX` keys** (ReplayGain fields, MusicBrainz album/artist
  ids) round-trip through `TXXX` frames with their fixed, exact-case
  descriptions (e.g. `MusicBrainz Album Id`).
- **Unmapped standard text frames** round-trip keyed by their own frame id: a
  `TSSE` (or a legacy v2.3 `TYER`) comes back as the same frame inside the
  synthesized tag.
- **Other user-defined keys** round-trip as `TXXX` frames keyed by their own
  description, original casing preserved.
- **Comments and lyrics** (`COMM`/`USLT`): the text content, one tag row per
  frame.
- **Ratings and play counts**: a `POPM` frame is promoted at scan time to
  `rating` (the raw 0–255 byte) and `playcount` (omitted when 0) text tags,
  and rebuilt as a `POPM` frame on synthesis.
- **MusicBrainz track id**: a `UFID` frame with the `http://musicbrainz.org`
  owner is promoted to `musicbrainz_trackid` and rebuilt with the same owner.
- **Opaque binary frames, byte-exact**: `PRIV`, `GEOB`, `SYLT`, `MCDI`,
  URL (`W***`) frames, non-MusicBrainz `UFID`s, and unknown frames are
  captured verbatim (frame id + raw body) and re-emitted streamed from the DB
  (`BinaryTag` segments) — never held in memory.
- **Embedded pictures** (`APIC`): MIME type, picture type, and description
  round-trip; image bytes are stored content-addressed and streamed.

## Lossy edges

- The synthesized tag is always **ID3v2.4**, regardless of the source tag's
  version (v2.2/v2.3 tags are parsed but never re-emitted as such).
- `COMM`/`USLT` language codes and descriptions are not preserved: every
  comment/lyric is written back with language `XXX` and an empty description.
  The *text* of multiple comment frames survives (one `COMM` per value), but
  frames distinguished only by language/description become indistinguishable.
- `POPM`: the owner ("email to user") field is dropped by design. Multiple
  `POPM` frames collapse to one (first rating wins, last parseable play
  count wins); counters above `u32::MAX` clamp to 4 bytes.
- **ID3v1 is not read.** A file whose only tag is ID3v1 scans with no tags
  (populate the DB via beets/Picard instead). A trailing ID3v1 tag is also
  excluded from the audio region, so the synthesized file does not carry it.
- The audio locator validates the ID3v2 major version (2–4) and rejects
  synchsafe size bytes with the high bit set, producing a controlled
  `Malformed` error rather than mask-decoding an invalid offset. Tags using
  unsynchronisation or an extended header still scan — their declared size
  already covers the audio boundary.
- Scan-time tag extraction is skipped entirely — by a deliberate
  denial-of-service guard, see below — for tags using unsynchronisation, an
  extended header, non-zero frame flags (compression/encryption), malformed
  synchsafe size fields, or containing `CHAP`/`CTOC` chapter frames. Such
  files still mount and serve; they just contribute no scanned tags.
- ID3v2.2 binary frames are not extracted (3-char ids; text and art still
  parse). `APIC` width/height are not recorded at scan time.

## How synthesis works

`mp3::synthesize_layout` (`musefs-format/src/mp3.rs`) emits a fresh ID3v2.4
tag followed by the untouched audio:

```text
 offset 0
 ┌──────────────────────────────────────────────┐ ┐
 │ █ ID3v2.4 header (10 bytes)          (Inline) │ │
 │ █ text / TXXX / COMM / USLT frames   (Inline) │ │ generated
 │ █ rebuilt POPM / UFID frames         (Inline) │ │ ID3v2.4
 │ █ frame header + ▒ opaque body    (BinaryTag) │ │ tag
 │ █ APIC framing + ▒ image bytes     (ArtImage) │ │
 ├──────────────────────────────────────────────┤ ┘
 │ ░ MPEG audio incl. Xing/LAME,  (BackingAudio) │
 │ ░ verbatim                                    │
 └──────────────────────────────────────────────┘
 EOF     █ inline-generated   ▒ DB-streamed   ░ untouched backing
```

1. `Inline` — the 10-byte tag header, all text/`TXXX`/`COMM`/`USLT` frames,
   and the rebuilt `POPM`/`UFID` frames. Frame sizes are synchsafe-bounded;
   oversized frames fail synthesis rather than emit a corrupt tag.
2. Per picture: inline `APIC` framing + an `ArtImage` segment streaming the
   image bytes.
3. Per opaque binary frame: an inline frame header + a `BinaryTag` segment
   streaming the body from the DB (empty payloads are skipped — they would
   fail layout validation).
4. `BackingAudio` — the audio region located at scan time: everything after
   the leading ID3v2 tag and before a trailing ID3v1 tag, anchored by an
   MPEG frame-sync check. The Xing/LAME info frame is an MPEG frame, so it
   travels with the audio untouched.

## Quirks & invariants

- **The OOM guard** (`id3v2_alloc_safe`): the `id3` parser crate eagerly
  allocates a frame's declared size (v2.3 sizes are plain 32-bit — up to
  4 GiB), so musefs validates every frame bound itself before handing a
  buffer to the crate, and refuses tags it cannot validate. Found and locked
  in by the `mp3` fuzz target; the conservative skips listed under "Lossy
  edges" are this guard.
- Byte-identical audio and tag round-trip stability are asserted by
  `musefs-format/tests/proptest_mp3.rs` and the mutagen interop suite
  (`musefs-core/tests/interop_emit.rs`).
