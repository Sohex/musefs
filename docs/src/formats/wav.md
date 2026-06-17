# WAV

How musefs scans and synthesizes RIFF/WAVE files (`.wav`). WAV has no single
native tag standard, so musefs writes metadata twice: a broad-compatibility
`LIST`/`INFO` chunk and a full-fidelity embedded `id3 ` chunk. For the
segment model these layouts plug into, see
[the segment model](../architecture/serving.md#the-segment-model). The ID3v2 tag inside
the `id3 ` chunk is built by the same code as MP3's — [MP3](mp3.md)'s
round-trip and lossy-edge rules apply to it wholesale.

## What round-trips

- **All text tags**, via the embedded `id3 ` chunk (full ID3v2.4, exactly as
  for MP3: canonical frames, `TXXX` extension slot, frame-id passthrough).
- **The INFO subset, twice.** Seven canonical keys also get a native
  `LIST`/`INFO` subchunk for ID3-unaware readers: `title`→`INAM`,
  `artist`→`IART`, `album`→`IPRD`, `date`→`ICRD`, `genre`→`IGNR`,
  `comment`→`ICMT`, `tracknumber`→`ITRK`.
- **Binary ID3 frames and promoted tags** (`POPM`→`rating`/`playcount`,
  MusicBrainz `UFID`→`musicbrainz_trackid`, opaque `PRIV`/`GEOB`/… byte-exact)
  — classification identical to MP3, only the chunk extraction differs.
- **Embedded pictures**: `APIC` frames inside the `id3 ` chunk, MIME +
  picture type + description preserved, image bytes streamed.
- **Structural chunks**: `fmt ` (required) and `fact` (when present) are
  preserved from the original front.

At scan time, tags are merged per field from both surfaces with **id3 taking
precedence** and INFO filling gaps; only chunk headers are walked — the
`data` payload is never read.

## Lossy edges

- **Non-structural chunks are dropped.** The synthesized front carries only
  `fmt `, `fact`, the new `LIST`/`INFO`, and the new `id3 ` chunk: cue
  points (`cue `), broadcast-wave metadata (`bext`), sampler loops (`smpl`),
  and any other chunk from the original front are not reproduced.
- The INFO chunk carries only the seven-field vocabulary above; readers that
  understand *only* INFO see just those fields. Everything still rides in
  the `id3 ` chunk.
- All of MP3's ID3 lossy edges apply to the `id3 ` chunk: ID3v2.4-only
  output, placeholder-language `COMM`/`USLT` reset to `XXX`, `POPM` owner
  dropped, ID3v1 ignored, the OOM-guard skips (the authoritative list lives in
  [MP3's lossy edges](mp3.md#lossy-edges)).
- **Tags trailing a very large `data` payload are not seen.** When the `data`
  payload pushes any `LIST`/`INFO` or `id3 ` chunk beyond the scan probe
  ceiling (64 MiB), the file is still ingested — the `data` chunk header gives
  the audio bounds without reading the payload — but those trailing tag chunks
  are not read at scan time. Front-positioned metadata is unaffected.

## How synthesis works

`wav::synthesize_layout` (`musefs-format/src/wav.rs`) regenerates the entire
RIFF front, then serves the untouched payload:

```text
 offset 0
 ┌──────────────────────────────────────────────┐ ┐
 │ █ RIFF/WAVE framing                  (Inline) │ │
 │ █ fmt  (+ fact), preserved           (Inline) │ │ regenerated
 │ █ LIST/INFO chunk (7-field subset)   (Inline) │ │ RIFF front
 │ █ id3  chunk: ID3v2.4 text frames    (Inline) │ │ (metadata
 │ █   frame header + ▒ opaque body  (BinaryTag) │ │  written
 │ █   APIC framing + ▒ image bytes   (ArtImage) │ │  twice)
 ├──────────────────────────────────────────────┤ ┘
 │ ░ data chunk payload, verbatim (BackingAudio) │
 └──────────────────────────────────────────────┘
 EOF     █ inline-generated   ▒ DB-streamed   ░ untouched backing
```

1. `Inline` — `RIFF`/`WAVE` framing, the preserved `fmt ` (and `fact`)
   chunks, the rebuilt `LIST`/`INFO` chunk, and the embedded `id3 ` chunk's
   text frames. Every chunk length is known up front, so the `RIFF` size and
   each chunk size field are byte-exact — no placeholder sizes.
2. Inside the `id3 ` chunk: `APIC` framing inline with `ArtImage` segments
   streaming image bytes, and `BinaryTag` segments streaming opaque ID3
   frame bodies, exactly as in MP3 synthesis.
3. `BackingAudio` — the original `data` chunk payload, served verbatim by
   positioned reads.

## RIFF form-size enforcement

Every RIFF/WAVE file declares a form size at bytes 4..8 (`riff_size`).
The form covers bytes 8 through `8 + riff_size` and must encompass all
top-level chunks (`fmt `, `data`, `LIST`, `id3 `, …). musefs enforces
this at parse time:

- `riff_wave_start` parses the RIFF size and returns `form_end = 8 + riff_size`.
- `locate_audio` and `locate_audio_at_ceiling` reject any file where
  `form_end` exceeds the physical file **or** where the `data` chunk
  payload extends past `form_end`.
- Streaming or concatenated WAVs that write `riff_size = 0` or
  `0xFFFFFFFF` are rejected, but only incidentally: there is no explicit
  sentinel check. `riff_size = 0` yields `form_end = 8`, which is smaller
  than any file carrying a `data` payload, and `0xFFFFFFFF` yields a
  `form_end` larger than any real file — both fall foul of the bounds
  checks above. Detecting and honouring those sentinels explicitly is a
  deferred follow-up.

## Quirks & invariants

- A file must have both a `fmt ` chunk and a `data` chunk to scan; the
  declared `data` size must lie within the file.
- The ID3-in-WAV path inherits MP3's allocation-bomb guard
  (`id3v2_alloc_safe`): a crafted `id3 ` chunk cannot OOM the scanner — this
  exact vector was found by the `wav` fuzz target.
- Byte-identical audio and front re-parseability are asserted by
  `musefs-format/tests/proptest_wav.rs` and the mutagen interop suite
  (`musefs-core/tests/interop_emit.rs`).
