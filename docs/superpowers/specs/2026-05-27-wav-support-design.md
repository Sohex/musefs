# WAV synthesis support — design

**Date:** 2026-05-27
**Status:** Approved for planning
**Scope:** Add WAV (RIFF/WAVE) as a synthesized format alongside FLAC, MP3, M4A,
and Ogg. The `data` chunk payload is served byte-for-byte as `BackingAudio`; a
fresh RIFF header carrying both a native `LIST`/`INFO` chunk and an embedded
`id3 ` chunk (full ID3v2 + art) is spliced in front. Standard 32-bit RIFF only;
codec-agnostic. RF64/BW64 (>4 GiB) is out of scope.

---

## 1. How WAV fits the splice model

WAV fits musefs's prefix-splice model cleanly, like FLAC/MP3 and unlike MP4:

- A WAV file is a RIFF container: `RIFF <LE size> WAVE`, followed by chunks. Each
  chunk is `FourCC (4) + LE size (4) + payload`, payload padded to an even byte.
- The audio is the payload of the `data` chunk — a single contiguous run served
  verbatim as a `BackingAudio { offset, len }` segment. The cardinal invariant
  ("original audio bytes are never copied or modified") holds exactly as for the
  other formats.
- Required structural metadata (`fmt `, and `fact` for non-PCM codecs) is small
  and lives before `data`. Regenerating our metadata chunks only changes the
  front of the file; no audio offset inside the served stream needs patching
  (unlike MP4's `stco`/`co64`).

The codec inside `data` (PCM or any `wFormatTag`) is irrelevant — samples are
never touched, so one code path serves all WAV. Extension: **`.wav`**.

## 2. Accepted shape (reject everything else)

`wav::locate_audio` accepts only files it fully understands and rejects the rest
(`locate_audio`/`read_structure` errors → scan skips the file; `resolve` errors →
that file reads as an IO error). It never emits partial/corrupt output.

Required shape:

- top-level RIFF form type is `WAVE`;
- a `fmt ` chunk is present (captured for synthesis);
- a `data` chunk is present (its payload bounds become the served audio);
- standard 32-bit RIFF chunk sizes only. **RF64/BW64** (the 64-bit extension for
  files >4 GiB; `RF64`/`BW64` form, `ds64` chunk, `data` size sentinel
  `0xFFFFFFFF`) is **rejected** — ~6.5 h of CD-quality audio, vanishingly rare in
  a music library.

## 3. The serving layout (core mechanism)

`wav::synthesize_layout(&scan, &inputs, &art_inputs) -> RegionLayout` produces a
deterministic, fully-sized RIFF. Every length is known up front (art lengths come
from `ArtImage` metadata), so the `RIFF` total size and every chunk size are
exact — there is no second pass.

```
RIFF <total_size> WAVE          Inline; total_size computed from all chunks below
fmt  <n> <bytes>                Inline; preserved verbatim from WavScan
fact <n> <bytes>                Inline; only if present in the original (non-PCM)
LIST <n> INFO <subchunks>       Inline; synthesized from DB tags (INAM/IART/IPRD/…)
id3  <n> <ID3v2 tag>            Inline header + build_id3v2_segments() output
                                  (text frames Inline, picture bytes as ArtImage)
data <data_len>                 Inline 8-byte chunk header
<backing audio>                 BackingAudio { offset, len } — untouched data payload
[pad 0x00]                      Inline; only when data_len is odd (RIFF word-align)
```

- `total_size = 4 + Σ(8 + payload_len + pad)` over every chunk **including**
  `data` (`payload_len` = `data_len`). All terms are known, so the header is
  byte-exact.
- Every chunk obeys RIFF even-byte padding: an odd-length payload is followed by a
  single `0x00` pad byte that is **not** counted in that chunk's size field but
  **is** counted in the enclosing `RIFF` size.
- The `id3 ` chunk's payload is a complete ID3v2.4 tag produced by the shared
  builder (§5). Its chunk size is `10 (ID3 header) + frames_len`, word-padded.
  Picture bytes are never materialized — they remain `ArtImage` segments streamed
  from the DB at read time.
- `LIST`/`INFO` maps the DB's canonical fields to INFO FourCCs (e.g. `INAM`=title,
  `IART`=artist, `IPRD`=album, `ICRD`=date, `IGNR`=genre, `ICMT`=comment). INFO
  values are NUL-terminated, word-padded. Fields with no INFO equivalent
  (albumartist, disc/track beyond informal codes, MusicBrainz IDs) are carried by
  the `id3 ` chunk only; INFO is the broad-compatibility surface, not the
  full-fidelity one.

## 4. Reading existing WAV metadata (scan only)

`wav::read_tags` / `wav::read_pictures` run **only at scan time** (`scan.rs::probe`).
The FUSE read path never calls them — `reader.rs` uses DB tags and reads only the
front of the file for structural bytes (§6).

They walk the RIFF chunk list over the already-materialized `&[u8]` (the scanner's
existing `std::fs::read`; see §7), skipping past the large `data` payload by its
size and reading only the small metadata chunk payloads. Both a `LIST`/`INFO`
chunk and an embedded `id3 ` chunk are read (either may appear before or after
`data`):

- the `id3 ` chunk is parsed with the existing `mp3::read_tags` / `mp3::read_pictures`;
- INFO subchunks are mapped back to canonical keys;
- results are **merged per field with the `id3 ` chunk taking precedence and INFO
  filling gaps**. Pictures come from the `id3 ` chunk only (INFO has no picture
  mechanism).

## 5. ID3v2 reuse refactor (`musefs-format/src/mp3.rs`)

The tag-building body of `mp3::synthesize_layout` is extracted into a reusable
helper:

```
fn build_id3v2_segments(tags: &[TagInput], arts: &[ArtInput])
    -> Result<(Vec<Segment>, u64 /* tag_len */)>
```

It returns the inline ID3v2.4 header + frame segments interleaved with `ArtImage`
segments, plus the total tag length. `mp3::synthesize_layout` keeps its exact
behavior by calling this helper and then appending the `BackingAudio` segment;
`wav::synthesize_layout` calls the same helper to fill the `id3 ` chunk. This is a
pure refactor: **MP3 synthesized output is byte-identical** (guarded by a
regression test, §8).

## 6. Wiring

- **`musefs-db` (`models.rs`):** add `Format::Wav` (`as_str` → `"wav"`,
  `parse("wav")`). No schema/migration change — `format` is a text column.
- **`musefs-format` (`src/wav.rs`, new):** RIFF chunk walker plus
  `locate_audio`, `read_structure` (capture `fmt `/`fact` for synthesis),
  `read_tags`, `read_pictures`, `synthesize_layout`. Mirrors the shape of
  `mp3.rs`/`mp4.rs`; declared in `lib.rs`.
- **`musefs-core` (`scan.rs`):** add `.wav` to `is_supported_audio`; add a
  `probe` arm → `Format::Wav` using `wav::locate_audio` + `wav::read_tags` +
  `wav::read_pictures`.
- **`musefs-core` (`reader.rs`):** add a `Format::Wav` arm to the
  `match track.format` in `HeaderCache::resolve` — `read_front(path,
  audio_offset)`, `wav::read_structure(&front)` for `fmt `/`fact`, then
  `wav::synthesize_layout(...)`.
- **Structure-only mode:** no work — it already emits a single whole-file
  `BackingAudio` for any format, so WAV is served verbatim there automatically.

## 7. Scan buffering decision

`wav::read_tags`/`read_pictures` take `&[u8]`, matching every other format. The
scanner (`scan_directory`/`revalidate`) already slurps the whole file with
`std::fs::read` for all formats today, so WAV introduces no new scan I/O pattern;
its scan-time memory cost equals today's M4A/FLAC scanning. The walker still skips
the `data` payload internally. A seek-based scan reader (bounding memory for large
files) was considered and rejected as an asymmetric, out-of-scope optimization
(§9).

## 8. Testing

Mirror the existing per-format test files:

- `wav::locate_audio`: correct `data` bounds; `fmt `/`data` presence required;
  RF64/BW64 rejected.
- chunk walker: correctly skips a large `data` payload to reach trailing chunks.
- `read_tags`/`read_pictures`: INFO-only, id3-only, both-present (merge precedence:
  id3 wins, INFO fills), neither.
- `synthesize_layout`: byte-exact output; `RIFF`/chunk size fields correct;
  word-alignment (odd `data_len` pad byte) correct; `fact` carried only when
  present.
- oracle: parse the synthesized output back with a reference reader (and `id3` for
  the embedded tag) and confirm tags + art round-trip.
- `musefs-fuse`: extend the existing `#[ignore]`d end-to-end read-through-mount
  test to cover a WAV file.
- **regression:** assert the §5 refactor leaves MP3 `synthesize_layout` output
  byte-identical.

## 9. Out of scope (record in `docs/ROADMAP.md`)

- **RF64/BW64** (>4 GiB / 64-bit RIFF).
- **Preserving non-essential chunks** (`bext`, `cue `, `smpl`, etc.): synthesis
  preserves `fmt ` (+`fact`), regenerates `LIST`/`INFO` and `id3 `, and drops
  everything else. Faithful for music-library WAV; discards DAW/broadcast metadata.
- **Seek-based scanning** for large files (a broader, all-formats optimization).
