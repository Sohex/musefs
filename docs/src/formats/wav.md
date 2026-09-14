# WAV

How musefs scans and synthesizes RIFF/WAVE files (`.wav`), including their
big-endian RIFX twin. WAV has no single
native tag standard, so musefs writes metadata twice: a broad-compatibility
`LIST`/`INFO` chunk and a full-fidelity embedded `id3 ` chunk. For the
segment model these layouts plug into, see
[the segment model](../architecture/serving.md#the-segment-model). The ID3v2 tag inside
the `id3 ` chunk is built by the same code as MP3's — [MP3](mp3.md)'s
round-trip and lossy-edge rules apply to it wholesale.

## Supported surface

Only files named `.wav` are probed. Of those:

| Container | Outcome |
| --------- | ------- |
| `RIFF`/`WAVE`, one top-level `data` chunk | Served. |
| `RIFX`/`WAVE` (big-endian) | Served, as RIFX (below). |
| Waveform stored as `LIST('wavl')` | Refused as **unsupported**, by name. |
| `RF64` / `BW64` | Refused as unparseable: out of scope. |

### RIFX

RIFX is WAVE with every integer big-endian. musefs reads its form size, chunk
sizes and `LIST`/`INFO` subchunk sizes big-endian, and serves it as RIFX: the
synthesized form, `fmt `/`fact`, `LIST`/`INFO` subchunk, `id3 ` and `data`
sizes are all written big-endian, as libsndfile writes RIFX. The `fmt ` and
`fact` payloads are carried verbatim and nothing in musefs interprets them, so
a big-endian `fmt ` is never wrapped in a container that claims little-endian.
The ID3v2 tag inside the `id3 ` chunk has no byte order of its own.

Nothing about the byte order is stored. The serve path already re-reads the
structural chunks from the front of the backing file (`[0, audio_offset)`),
and the `RIFF`/`RIFX` magic is its first four bytes.

Reader support for RIFX is uneven, for the source file as much as for the
served one:

- **libsndfile** and **sox** decode it correctly; libsndfile also reads the
  `LIST`/`INFO` tags. The interop suite reads musefs's served RIFX through
  libsndfile.
- **FFmpeg** opens it and reads the `id3 ` chunk's tags, but decodes RIFX PCM
  byte-swapped (it picks a little-endian PCM codec) and reads `INFO` subchunk
  sizes little-endian, so it drops the `INFO` chunk with "too big INFO
  subchunk". Little-endian `INFO` sizes would fix FFmpeg and break
  libsndfile, which follows the RIFX rule; musefs follows the rule, and the
  `id3 ` chunk carries every tag to FFmpeg regardless.
- **mutagen** and **GStreamer** refuse RIFX outright.

### `LIST('wavl')`

RIFF also lets a waveform be a `LIST` of type `wavl`: an ordered run of `data`
and `slnt` (silence) chunks in place of one `data` chunk. No mainstream reader
plays one. FFmpeg, libsndfile, GStreamer and sox all fail with no `data` chunk
found, and mutagen opens it with a length of zero. musefs refuses such a file
rather than serve a layout nothing can play: the scan logs `skipping <path>:
WAVE waveform stored as LIST('wavl')` and counts it as `unsupported`.

That makes it a refusal of the file's shape, as for a chained Ogg: a stored WAV
later rewritten as `LIST('wavl')` fails every `revalidate`, and
`revalidate --prune` removes its row
(see [maintenance](../guide/maintenance.md)). A file that has a top-level
`data` chunk is served from it, whatever lists it also carries.

### RF64 and BW64

RF64 and BW64 hold their sizes in a `ds64` chunk as 64-bit values, with the
32-bit fields set to `0xFFFFFFFF`, so that a waveform can exceed 4 GiB. musefs
reads and writes 32-bit sizes only: the magic is not accepted, and synthesis
refuses a served file whose size would not fit (`TooLarge`).

### Other refusals

A file fails to parse, and is counted `unparseable`, when:

- it has no `fmt ` chunk, or no `data` chunk (and no `LIST('wavl')`), inside
  its declared form;
- the declared form runs past the end of the file, or the `data` payload runs
  past the form (see [form-size enforcement](#riff-form-size-enforcement));
- its form size is a streaming sentinel (`0` or `0xFFFFFFFF`).

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
precedence** and INFO filling gaps. Because tag chunks may trail the `data`
payload, the probe reads the whole file up to the 64 MiB probe ceiling; only a
larger file falls back to bounds taken from the `data` chunk header (see the
lossy edge below).

## Lossy edges

- **Non-structural chunks are dropped.** The synthesized front carries only
  `fmt `, `fact`, the new `LIST`/`INFO`, and the new `id3 ` chunk: cue
  points (`cue `), broadcast-wave metadata (`bext`), sampler loops (`smpl`),
  and any other chunk from the original front are not reproduced.
- The INFO chunk carries only the seven-field vocabulary above, and only the
  first value of each key; readers that understand *only* INFO see just those.
  When no tag maps to an INFO field, the `LIST` chunk is omitted entirely.
  Everything still rides in the `id3 ` chunk.
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

1. `Inline` — `RIFF`/`WAVE` framing (`RIFX` for a big-endian source), the preserved `fmt ` (and `fact`)
   chunks, the rebuilt `LIST`/`INFO` chunk, and the embedded `id3 ` chunk's
   text frames. Every chunk length is known up front, so the `RIFF` size and
   each chunk size field are byte-exact — no placeholder sizes. A pad byte
   follows any odd-length chunk, keeping chunks word-aligned.
2. Inside the `id3 ` chunk: `BinaryTag` segments streaming opaque ID3 frame
   bodies, and `APIC` framing inline with `ArtImage` segments streaming image
   bytes, exactly as in MP3 synthesis.
3. The `data` chunk header, inline, then `BackingAudio` — the original `data`
   chunk payload, served verbatim by positioned reads — and an inline pad byte
   when the payload length is odd.

## RIFF form-size enforcement

Every RIFF/WAVE file declares a form size at bytes 4..8 (`riff_size`,
big-endian in RIFX).
The form covers bytes 8 through `8 + riff_size` and must encompass all
top-level chunks (`fmt `, `data`, `LIST`, `id3 `, …). musefs enforces
this at parse time:

- `riff_wave_start` parses the RIFF size and returns `form_end = 8 + riff_size`.
- `locate_audio` and `locate_audio_at_ceiling` reject any file where
  `form_end` exceeds the physical file **or** where the `data` chunk
  payload extends past `form_end`.
- Streaming or concatenated WAVs that write `riff_size = 0` or
  `0xFFFFFFFF` are rejected, but only incidentally: there is no explicit
  sentinel check. `riff_size = 0` yields `form_end = 8`, before the first
  chunk header at byte 12, so the form-bounded chunk walk finds no `fmt ` or
  `data` chunk and the file fails as not-WAV. `0xFFFFFFFF` yields a
  `form_end` larger than any real file, which fails the bounds check above.
  Detecting and honouring those sentinels explicitly is a deferred follow-up.

## Quirks & invariants

- A file must have both a `fmt ` chunk and a top-level `data` chunk to scan;
  the declared `data` size must lie within the file. A `LIST('wavl')` standing
  in for `data` is refused by name (see [the supported surface](#supported-surface)).
- The served file keeps the source's byte order: `RIFX` in, `RIFX` out.
- The ID3-in-WAV path inherits MP3's allocation-bomb guard
  (`id3v2_alloc_safe`): a crafted `id3 ` chunk cannot OOM the scanner — this
  exact vector was found by the `wav` fuzz target.
- Byte-identical audio and front re-parseability are asserted by
  `musefs-format/tests/proptest_wav.rs` and the mutagen interop suite
  (`musefs-core/tests/interop_emit.rs`).
