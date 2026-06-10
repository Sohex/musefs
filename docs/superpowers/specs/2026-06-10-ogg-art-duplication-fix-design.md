# Ogg cover-art duplicated on synthesis — design

Mini spec for a bug fix surfaced by the read-consistency work (#215/#214).

## Problem

Every served Ogg file (Opus/Vorbis) whose backing file carries embedded cover art
ends up with the cover **duplicated**: `read_pictures` finds two identical
pictures where the source had one.

Surfaced by `musefs-fuse/tests/ogg_read_through.rs::opus_read_through_preserves_embedded_art`,
which asserts the synthesized file carries exactly one picture matching the
source. That test had been silently skipping (its ffmpeg cover fixture never
generated), so the duplication went unnoticed. With the fixture fixed, the test
fails: `out_pics.len()` is 2, both bytes-identical to the single source picture.

## Root cause

Opus/Vorbis carry cover art as a base64 `METADATA_BLOCK_PICTURE` entry **inside the
vorbis comment**. Two readers consume the comment block:

- `ogg::read_pictures` extracts `METADATA_BLOCK_PICTURE` as art.
- `ogg::read_tags` (`musefs-format/src/ogg/mod.rs`) calls `vorbiscomment::parse`,
  which returns **every** comment field — including `METADATA_BLOCK_PICTURE` — as a
  text tag.

The scanner (`musefs-core/src/scan.rs`) stores both channels: the tags (with the
giant base64 `METADATA_BLOCK_PICTURE` text tag) **and** the pictures. Synthesis
then re-emits the art twice — once from the passed-through text tag, once as the
`OggArtSlice` built from the DB art — yielding two identical pictures.

FLAC does not have this bug because native FLAC stores art in a separate `PICTURE`
metadata block, so `flac::read_vorbis_comments` is naturally art-free. The Ogg path
is asymmetric: its art lives in the comment, and `read_tags` fails to exclude it.

## Fix

Make `ogg::read_tags` return only text metadata, mirroring FLAC: drop any comment
whose field name is `METADATA_BLOCK_PICTURE` (case-insensitive — vorbis field names
are case-insensitive, and `read_pictures` already matches it that way). Art keeps
its dedicated channel via `read_pictures`; the redundant, duplication-causing text
tag is never stored.

The filter lives in the **format layer** (`ogg::read_tags`), not the scanner, so
the responsibility boundary matches the other formats: a format's `read_tags`
returns text tags, its `read_pictures` returns art, and the two do not overlap.

### Scope / non-goals

- Only `METADATA_BLOCK_PICTURE` is filtered — the exact key `read_pictures`
  consumes, so removing it from tags cannot lose art. The legacy `COVERART` comment
  is **not** touched: `read_pictures` does not ingest it, so it is not duplicated,
  and filtering it would drop that art entirely. Out of scope.
- No change to `vorbiscomment::parse` — `read_pictures` calls it directly and needs
  `METADATA_BLOCK_PICTURE`. The filter is applied only in `read_tags`.
- No change to synthesis, the scanner, or the store schema. One format-layer
  function changes.

## Testing

- New unit test in `musefs-format` (`ogg/mod.rs` tests): `read_tags` on an
  Opus/Vorbis fixture containing a `METADATA_BLOCK_PICTURE` comment returns the
  text tags **without** it, while `read_pictures` on the same bytes still returns
  the picture. This is the hermetic regression test (no ffmpeg, no mount).
- The existing e2e `opus_read_through_preserves_embedded_art` (now un-skipped via
  the fixed cover fixture) goes green at `out_pics.len() == 1`.

## Acceptance criteria

- `ogg::read_tags` excludes `METADATA_BLOCK_PICTURE` (case-insensitive); a unit
  test proves it while `read_pictures` still returns the art.
- The un-skipped opus e2e art test passes (one picture, matching the source).
- No other format's behavior changes; full workspace suite stays green.
