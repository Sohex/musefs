# WAV

How musefs scans and synthesizes RIFF/WAVE files (`.wav`). WAV has no single
native tag standard, so musefs writes metadata twice: a broad-compatibility
`LIST`/`INFO` chunk and a full-fidelity embedded `id3 ` chunk. For the
segment model these layouts plug into, see
[ARCHITECTURE.md](../ARCHITECTURE.md#the-segment-model). The ID3v2 tag inside
the `id3 ` chunk is built by the same code as MP3's — [MP3.md](MP3.md)'s
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
  output, `COMM`/`USLT` language/description reset, `POPM` owner dropped,
  ID3v1 ignored, the OOM-guard skips.

## How synthesis works

`wav::synthesize_layout` (`musefs-format/src/wav.rs`) regenerates the entire
RIFF front, then serves the untouched payload:

1. `Inline` — `RIFF`/`WAVE` framing, the preserved `fmt ` (and `fact`)
   chunks, the rebuilt `LIST`/`INFO` chunk, and the embedded `id3 ` chunk's
   text frames. Every chunk length is known up front, so the `RIFF` size and
   each chunk size field are byte-exact — no placeholder sizes.
2. Inside the `id3 ` chunk: `APIC` framing inline with `ArtImage` segments
   streaming image bytes, and `BinaryTag` segments streaming opaque ID3
   frame bodies, exactly as in MP3 synthesis.
3. `BackingAudio` — the original `data` chunk payload, served verbatim by
   positioned reads.

## Quirks & invariants

- A file must have both a `fmt ` chunk and a `data` chunk to scan; the
  declared `data` size must lie within the file.
- The ID3-in-WAV path inherits MP3's allocation-bomb guard
  (`id3v2_alloc_safe`): a crafted `id3 ` chunk cannot OOM the scanner — this
  exact vector was found by the `wav` fuzz target.
- Byte-identical audio and front re-parseability are asserted by
  `musefs-format/tests/proptest_wav.rs` and the mutagen interop suite
  (`musefs-core/tests/interop_emit.rs`).
