# Ogg Invariant

Original Ogg packet payload bytes are preserved during synthesis. Ogg page
sequence numbers and CRCs may be patched intentionally.

## Rationale

Ogg synthesis regenerates the logical bitstream headers (page granule positions,
packet boundaries) to embed synthesized tags and cover art. Audio packets are
passed through unchanged. This means:

- Audio payload bytes are always preserved (tested via property tests and
  mutagen interop).
- Page sequence numbers are re-numbered to account for the changed header page
  count.
- CRCs are recomputed for any page whose content changed (headers only).
- VorbisComments are rebuilt from the DB tag store, with cover art injected as
  `METADATA_BLOCK_PICTURE` base64 values (Opus/Vorbis) or native FLAC PICTURE
  blocks (OggFLAC).

## Verified By

- `proptest_ogg` property tests (crate feature `fuzzing`)
- `read_at` integration tests that compare source and synthesized audio payloads
- Mutagen interop tests that verify ecosystem readers can parse the output
