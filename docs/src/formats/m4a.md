# M4A

How musefs scans and synthesizes MP4-container audio (`.m4a`, `.m4b`). Only
unfragmented files with exactly one audio (`soun`) track are accepted; chapter
tracks (`text`, `sbtl`) may accompany it, and any other track — video above all
— is skipped at scan time with an error naming the handler types found. For the
segment model these layouts plug into, see
[the segment model](../architecture/serving.md#the-segment-model).

## What round-trips

- **Canonical text tags** map to their standard `ilst` atoms (`©nam`,
  `©ART`, `aART`, `©alb`, `©day`, …) via the shared vocabulary
  (`musefs-format/src/tagmap.rs`).
- **Vocabulary freeform keys** (ReplayGain fields, MusicBrainz album/artist
  ids, `ISRC`, `COPYRIGHT`, …) round-trip through `----` freeform atoms under
  the `com.apple.iTunes` mean, matched case-insensitively.
- **Other text freeform atoms** round-trip keyed by their verbatim `name`,
  original casing preserved.
- **Track and disc numbers, with totals**: the binary `trkn`/`disk` atoms are
  decoded to `tracknumber`/`discnumber` as `"N"` or `"N/M"` (the "N of M" total,
  matching ID3 `TRCK`/`TPOS`) and rebuilt as binary atoms with the total filled
  in.
- **Integer atoms**: `tmpo`/`cpil`/`pgap` map to the canonical `bpm`/
  `compilation`/`gapless` keys (shared with ID3 `TBPM`/`TCMP` and Vorbis) and
  are rebuilt as type-21 integer atoms.
- **Multi-value atoms**: every `data` sub-box of an atom is read (the iTunes
  multiple-`data` convention), so a multi-valued atom round-trips all its
  values, not just the first.
- **Opaque binary freeform atoms, byte-exact**: a `----` atom whose payload
  is binary-typed is captured verbatim under the key `----:<mean>:<name>`
  (so the mean survives) and re-emitted streamed from the DB (`BinaryTag`
  segment).
- **Cover art**: every `data` child of a `covr` atom (the iTunes
  multiple-artwork convention) is ingested; synthesis emits one `covr` atom
  with one `data` child per stored art row, in order, image bytes streamed.
- **Chapters, both conventions.** A QuickTime chapter track — the second
  `text`/`sbtl` track that is the reason `.m4b` exists — is kept verbatim like
  any other structural `moov` child, and its chunk offsets are relocated
  alongside the audio track's. A Nero chapter list (`moov/udta/chpl`) is copied
  through byte-for-byte into the regenerated `udta`; it holds timestamps and
  titles, never a file offset, so relocating `mdat` cannot invalidate it.
  ffmpeg writes both by default (`-movflags +disable_chpl` suppresses the
  `chpl`), and players differ on which form they read, so both are preserved.

## Lossy edges

- A *text* freeform atom under a mean other than `com.apple.iTunes` is
  re-emitted with the `com.apple.iTunes` mean (the scan keys text freeform
  by name only). Binary freeform atoms keep their mean via the
  `----:<mean>:<name>` key.
- `ilst` atoms outside the handled set are dropped at scan time, since they
  are not re-emitted on synthesis: text atoms not in the shared vocabulary, and
  binary atoms other than `trkn`/`disk`, the `tmpo`/`cpil`/`pgap` integer
  atoms, and `----` freeform.
- `covr` ingestion accepts only JPEG (type 13) and PNG (type 14) artwork;
  other type codes are skipped. MP4 has no picture-type or description
  fields: scanned art becomes "front cover" with an empty description, and
  any non-PNG stored art is emitted with the JPEG type code.
- A `covr` image or binary `----` value larger than its size cap fails the
  whole file at scan time, counted under reason `oversize`. The size is checked
  before the payload is copied out of a potentially large `moov`, so the
  oversized item is never materialized.

## QuickTime keyed metadata

Besides the iTunes `ilst`, an MP4 can carry QuickTime's own metadata model
([Metadata atoms and types](https://developer.apple.com/documentation/quicktime-file-format/metadata_atoms_and_types)):
a `meta` whose `hdlr` names the `mdta` handler, holding a `keys` table and an
`ilst` whose item atoms are typed by a 1-based index into that table rather
than by a FourCC — `com.apple.quicktime.artist` instead of `©ART`. Apple
software writes it, and so does ffmpeg's `-movflags use_metadata_tags`
([#771](https://github.com/Sohex/musefs/issues/771)).

### Where the scan reads it

`mp4::read_tags` and `mp4::read_pictures` read every `mdta` `meta` in these
places, in this order, on both the whole-file and the bounded `moov` probe
paths:

1. `moov/meta`, the movie level, where Apple writes it.
2. `moov/udta/meta`, where ffmpeg writes it, beside or instead of the iTunes
   `meta`. The iTunes `ilst` is read from the first `udta/meta` that is *not*
   `mdta`.
3. `trak/meta`, then `trak/mdia/meta`, per track: the two track-level locations
   the QuickTime File Format allows. Apple devices write `player.movie.audio.*`
   settings in the audio track.

A `meta` counts as keyed only when its `hdlr` says `mdta`, which is also the
gate ffmpeg's reader uses. A `keys` entry outside the `mdta` namespace, or
whose name is not UTF-8, holds its index without resolving; an item whose index
is 0 or past the table is skipped. As everywhere in the metadata readers, a
malformed box ends only its own sibling list, and nothing is allocated from a
declared count.

### Keys, values and precedence

- **Mapping.** `com.apple.quicktime.title`, `.artist`, `.album`, `.genre`,
  `.comment` and `.copyright` map to the canonical key of the same name, and
  `com.apple.quicktime.year` to `date`. ffmpeg writes its own metadata names as
  keys: those already canonical (`title`, `artist`, `composer`, …) need no
  mapping, and `album_artist`, `track` and `disc` map to `albumartist`,
  `tracknumber` and `discnumber`. Matching is case-insensitive
  (`musefs-format/src/tagmap.rs`).
- **Unknown keys** keep their verbatim name as the tag key, as an unknown `----`
  name does: `com.apple.quicktime.location.ISO6709`, `encoder`.
- **Precedence.** The iTunes `ilst` wins outright: a key it carries takes
  nothing from keyed metadata, so a tagger's edit to `©ART` is never undercut by
  a recorder's keyed `artist`. Keyed metadata fills in only the keys the `ilst`
  lacks, and among keyed items the first to yield a value wins, in the reading
  order above. Both rules are all-or-nothing per key, compared
  case-insensitively.
- **One value per item.** Several `data` boxes in one item are alternative
  representations of one datum, by locale or storage type, ordered
  most-specific first
  ([Data ordering](https://developer.apple.com/documentation/quicktime-file-format/data_ordering)).
  They are not the multiple values an iTunes atom carries. The first
  default-locale (locale `0`) value is taken; with none, the last one musefs can
  decode.
- **Value types**
  ([well-known types](https://developer.apple.com/documentation/quicktime-file-format/well-known_types)):
  UTF-8 (1) and UTF-16BE (2, a byte-order mark dropped) as text; the
  variable-width big-endian integers (21 signed, 22 unsigned, 1–8 bytes) and the
  fixed-width ones (65/66/67/74 signed, 75/76/77/78 unsigned) as decimal; finite
  float32 (23) and float64 (24) as their shortest decimal form.
- **Artwork.** `com.apple.quicktime.artwork` holding a JPEG (13) or PNG (14)
  becomes a front-cover picture, as `covr` does, under the same size cap. It is
  consulted only when the `covr` atoms yield nothing at all, so an oversize
  `covr` still fails the file rather than giving way to a different image. One
  artwork is taken, from the first artwork item that holds an image.

### What synthesis drops

The served file carries one metadata system, built from the store. So
`synthesize_layout` does not pass through a `meta` with the `mdta` handler at the
movie, track or media level; the `udta` copy goes with the rebuilt `udta`.
Otherwise an edit to `artist` would be served as the store's `©ART` beside the
file's original keyed `artist`, and which one a player showed would depend on the
player. Those are the only `meta` boxes dropped. A `meta` under any other handler
(ID3-in-MP4's `ID32`, for example), and a `meta` nested anywhere else, are boxes
the store does not model and pass through untouched.

Removing a `meta` from a track shrinks its `mdia` and `trak`, and both are
re-emitted with their new size, in the header width they were written with
(8-byte or 64-bit largesize). That happens before the new `moov` size is summed,
so the constant chunk-offset delta below is computed from the final layout, and
patching still changes only offset values.

### What keyed metadata loses

- Keyed metadata is read, never written. Mapped keys come back as their iTunes
  atoms, and unknown keys as `----` text atoms under the `com.apple.iTunes` mean.
- Keyed values of other types are dropped at scan time: sort-only strings (4, 5),
  S/JIS (3), BMP (27), nested metadata (28), the geometry types, and non-finite
  floats, along with integers whose length does not fit their type. Localized
  alternatives collapse to the one value chosen above.

### Stores scanned before this change

A store scanned by an older build never ingested keyed values, and a revalidate
does not add them, because a revalidate never rewrites tags. Nothing picks them
up automatically: telling which stored files carry keyed metadata means reading
every M4A again, and filling the gaps in on a later pass would bring back tags a
curator deliberately removed. `musefs scan --force <file>` re-seeds one file's
tags and art from what it embeds, keyed metadata included, and replaces that
file's curated tags and art in doing so. `ffprobe -show_entries format_tags
<file>` lists a file's `com.apple.quicktime.*` keys; keys written by ffmpeg use
plain names like `artist`, which look the same there as iTunes tags.

## How synthesis works

`mp4::synthesize_layout` (`musefs-format/src/mp4.rs`) regenerates the `moov`
box and serves `[ftyp][regenerated moov][mdat header][mdat payload]`:

```text
 offset 0
 ┌──────────────────────────────────────────────┐ ┐
 │ █ ftyp, copied verbatim              (Inline) │ │
 │ █ moov: kept structural children,    (Inline) │ │ regenerated
 │ █   every track's stco/co64 += Δ              │ │ front
 │ █ fresh udta/meta/ilst framing       (Inline) │ │
 │ █ ---- framing + ▒ freeform body  (BinaryTag) │ │
 │ █ covr framing + ▒ image bytes     (ArtImage) │ │
 │ █ chpl, copied from the old udta     (Inline) │ │
 │ █ mdat header                        (Inline) │ │
 ├──────────────────────────────────────────────┤ ┘
 │ ░ mdat payload, verbatim       (BackingAudio) │
 └──────────────────────────────────────────────┘
 EOF     █ inline-generated   ▒ DB-streamed   ░ untouched backing
         Δ = new mdat payload offset − old
```

1. Synthesis (`mp4::synthesize_layout`) keeps `moov`'s structural children
   and drops its old `udta`, save for a `chpl` chapter list, which is carried
   through. A fresh
   `udta`/`meta`/`ilst` is built from the DB: inline box framing, with
   each opaque `----` value and each cover image spliced in as streamed
   `BinaryTag`/`ArtImage` segments. Every enclosing box size accounts for
   the streamed lengths, so the spliced bytes land exactly where the sizes
   say.
2. The `mdat` payload is served verbatim (`BackingAudio`), merely relocated:
   every chunk offset in `stco` (32-bit) or `co64` (64-bit), in *every* track,
   shifts by one constant delta. One delta suffices because a file has a single
   `mdat`, so a chapter track's chunks relocate exactly as the audio track's do.
   Only offset *values* are patched, never box sizes, so the new `moov` size is
   computable before the delta — no circular dependency. A 32-bit `stco` offset
   that would overflow fails synthesis rather than corrupt, and a track with
   neither `stco` nor `co64` fails it too rather than being left unpatched.
3. A `moov` that sits after `mdat` (common for faststart-less files) is
   handled by a streaming reader that skips the mdat payload — the
   potentially hundreds-of-MB payload is never read at resolve time.

## Quirks & invariants

- The structural metadata read at resolve time is capped
  (`MAX_MP4_METADATA_BYTES`, 256 MiB); a file declaring more is refused with
  a controlled error instead of ballooning memory.
- MP4 box sizes are 32-bit: oversized synthesized metadata (e.g. enormous
  art) fails with `TooLarge` at the format boundary rather than emitting a
  truncated size field.
- Byte-identical audio and structural validity are asserted by
  `musefs-format/tests/proptest_mp4.rs`, an offset-patching oracle test
  (`mp4_oracle.rs`), and the mutagen interop suite
  (`musefs-core/tests/interop_emit.rs`). With keyed metadata in the source, the
  `mp4` fuzz target and the property tests also check that the served file holds
  no keyed `meta` and that every chunk offset moved by exactly the relocation
  delta (`fuzz_check::assert_mp4_single_metadata_system`), and the interop suite
  checks with mutagen, and with ffprobe where installed, that no reader sees an
  old keyed value.
