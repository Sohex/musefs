# M4A

How musefs scans and synthesizes MP4-container audio (`.m4a`, `.m4b`). Only
unfragmented files with exactly one track, and that track audio (`soun`), are
accepted; anything else is skipped at scan time. For the segment model these
layouts plug into, see [ARCHITECTURE.md](../ARCHITECTURE.md#the-segment-model).

## What round-trips

- **Canonical text tags** map to their standard `ilst` atoms (`©nam`,
  `©ART`, `aART`, `©alb`, `©day`, …) via the shared vocabulary
  (`musefs-format/src/tagmap.rs`).
- **Vocabulary freeform keys** (ReplayGain fields, MusicBrainz album/artist
  ids, `ISRC`, `COPYRIGHT`, …) round-trip through `----` freeform atoms under
  the `com.apple.iTunes` mean, matched case-insensitively.
- **Other text freeform atoms** round-trip keyed by their verbatim `name`,
  original casing preserved.
- **Track and disc numbers**: the binary `trkn`/`disk` atoms are decoded
  positionally to `tracknumber`/`discnumber` and rebuilt as binary atoms.
- **Opaque binary freeform atoms, byte-exact**: a `----` atom whose payload
  is binary-typed is captured verbatim under the key `----:<mean>:<name>`
  (so the mean survives) and re-emitted streamed from the DB (`BinaryTag`
  segment).
- **Cover art**: every `data` child of a `covr` atom (the iTunes
  multiple-artwork convention) is ingested; synthesis emits one `covr` atom
  with one `data` child per stored art row, in order, image bytes streamed.

## Lossy edges

- **Track/disc totals are dropped.** Only the number itself is read from
  `trkn`/`disk` (the "x of N" total is not), and synthesis writes the total
  as zero.
- A *text* freeform atom under a mean other than `com.apple.iTunes` is
  re-emitted with the `com.apple.iTunes` mean (the scan keys text freeform
  by name only). Binary freeform atoms keep their mean via the
  `----:<mean>:<name>` key.
- **Multi-value atoms round-trip only their first value**: synthesis writes
  one `data` sub-box per value, but the scan reads only the first `data`
  sub-box of each atom.
- Binary `ilst` atoms other than `trkn`/`disk` and `----` (e.g. `tmpo`,
  `cpil`, `pgap`) are dropped at scan time.
- `covr` ingestion accepts only JPEG (type 13) and PNG (type 14) artwork;
  other type codes are skipped. MP4 has no picture-type or description
  fields: scanned art becomes "front cover" with an empty description, and
  any non-PNG stored art is emitted with the JPEG type code.
- A `covr` image or binary `----` value larger than its size cap is skipped
  at scan time — before the image is materialized out of a potentially large
  `moov` — and logged (a `warn` line on stderr) so the lossy drop is explained
  rather than silent.

## How synthesis works

`mp4::synthesize_layout` (`musefs-format/src/mp4.rs`) regenerates the `moov`
box and serves `[ftyp][regenerated moov][mdat header][mdat payload]`:

```text
 offset 0
 ┌──────────────────────────────────────────────┐ ┐
 │ █ ftyp, copied verbatim              (Inline) │ │
 │ █ moov: kept structural children,    (Inline) │ │ regenerated
 │ █   stco/co64 offset values += Δ              │ │ front
 │ █ fresh udta/meta/ilst framing       (Inline) │ │
 │ █ ---- framing + ▒ freeform body  (BinaryTag) │ │
 │ █ covr framing + ▒ image bytes     (ArtImage) │ │
 │ █ mdat header                        (Inline) │ │
 ├──────────────────────────────────────────────┤ ┘
 │ ░ mdat payload, verbatim       (BackingAudio) │
 └──────────────────────────────────────────────┘
 EOF     █ inline-generated   ▒ DB-streamed   ░ untouched backing
         Δ = new mdat payload offset − old
```

1. The scan keeps `moov`'s structural children and drops its old `udta`. A
   fresh `udta`/`meta`/`ilst` is built from the DB: inline box framing, with
   each opaque `----` value and each cover image spliced in as streamed
   `BinaryTag`/`ArtImage` segments. Every enclosing box size accounts for
   the streamed lengths, so the spliced bytes land exactly where the sizes
   say.
2. The `mdat` payload is served verbatim (`BackingAudio`), merely relocated:
   every chunk offset in `stco` (32-bit) or `co64` (64-bit) shifts by one
   constant delta. Only offset *values* are patched, never box sizes, so the
   new `moov` size is computable before the delta — no circular dependency.
   A 32-bit `stco` offset that would overflow fails synthesis rather than
   corrupt.
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
  (`musefs-core/tests/interop_emit.rs`).
