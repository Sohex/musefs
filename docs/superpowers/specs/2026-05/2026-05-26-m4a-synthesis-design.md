# M4A/MP4 full-synthesis support — design

**Date:** 2026-05-26
**Status:** Approved for planning
**Scope:** Add a third audio format (MP4/M4A) to musefs with **full metadata
synthesis** (re-tagged headers, like FLAC/MP3), so a mixed beets library is no
longer silently incomplete at the mount. AAC and ALAC, audio-only, non-fragmented.

---

## 1. Why this is different from FLAC/MP3

FLAC and MP3 fit musefs's splice model because metadata is a prefix and the audio
frames are one contiguous run served verbatim after it. MP4 does not:

- It is an atom/box container (`size` + 4-char type + payload, recursive).
- Audio samples live in an `mdat` box, indexed by **absolute file offsets** in the
  `moov` sample tables (`stco` 32-bit / `co64` 64-bit chunk offsets).
- `moov` may sit **before or after** `mdat`.

So regenerating metadata changes `moov`'s size, which shifts `mdat`, which
invalidates every chunk offset. Synthesis therefore must **rewrite the chunk
offsets**, not just prepend a new header. The audio samples themselves are still
served byte-for-byte (the cardinal invariant holds).

## 2. Accepted shape (strict; reject everything else)

The parser accepts only files it fully understands and **rejects anything else**
(`locate_audio` returns `None` → scan skips; `resolve` errors → that file reads as
an IO error). It never emits partial/corrupt output. Required shape:

- top-level boxes contain `ftyp`, **exactly one** `moov`, **exactly one** `mdat`;
- `moov` before or after `mdat` (both handled);
- **exactly one audio track, no video track, no fragmentation** (`moof`/`mvex`
  present → reject);
- chunk offsets in `stco` or `co64` (both handled);
- 32-bit and 64-bit box sizes handled (`size==1` → 64-bit `largesize`); `size==0`
  (box-to-EOF) is accepted only for a trailing `mdat`.

Codec (AAC vs ALAC) is irrelevant — samples are never touched, so one code path
serves both. Extensions: **`.m4a` and `.m4b`**. `.mp4` is excluded (usually video);
revisit later.

## 3. The serving layout (core mechanism)

A synthesized M4A is the ordered segment list:

```
[ftyp]            Inline   — copied verbatim from the original
[regenerated moov] Inline  — mvhd + trak/stbl preserved except stco/co64 values;
                             udta/meta/ilst regenerated from the DB
                             (cover image streamed via an ArtImage split, see §5)
[mdat box header]  Inline   — original 8/16-byte [size]['mdat'] reused
[mdat payload]     BackingAudio { offset, len }  — original samples, verbatim
```

**Offset patch (the crux):** every chunk offset shifts by the same constant

```
delta = new_mdat_payload_pos − old_mdat_payload_pos
new_mdat_payload_pos = len(ftyp) + len(new moov) + len(mdat header)
```

because the `mdat` payload is served verbatim, merely relocated. Patching changes
only offset *values* (4 bytes for `stco`, 8 for `co64`), never box sizes, so the
new `moov` size is computable *before* the delta and there is no circular
dependency. Guard: if a file uses 32-bit `stco` and a patched offset would exceed
`u32::MAX` (effectively impossible for music), **error** rather than overflow.

`mdat` payload length is unchanged, so the original `mdat` box header is reused
as-is and `audio_offset`/`audio_length` (existing track columns) store the `mdat`
payload offset/len — reusing the reader's existing size/mtime + bounds validation.

## 4. moov regeneration

Build the new `moov` from the original `moov` bytes:

1. Walk `moov`'s direct children; keep all **except** any existing `udta`
   (typically `mvhd` + one `trak`). Concatenate the kept children.
2. Within the kept children, locate the `trak/mdia/minf/stbl/{stco|co64}` entries
   and patch each by `delta` (computed in §3).
3. Build a fresh `udta` (§4.1) from the DB tags + art, placed **last** in `moov`.
4. Wrap: `moov` size = 8 + len(kept children) + len(new `udta`).

`mvhd`, `trak`, and all `stbl` sub-tables (`stsd`, `stts`, `stsc`, `stsz`,
`stco`/`co64`, …) are preserved byte-for-byte except the patched offset values, so
sample timing/sizing/codec config are untouched.

### 4.1 udta / meta / ilst structure and tag mapping

```
udta
└─ meta            (FullBox: 1-byte version=0 + 3-byte flags=0, then children)
   ├─ hdlr         (handler_type 'mdir', 'appl' marker)  — required by iTunes readers
   └─ ilst
      ├─ ©nam/©ART/aART/©alb/©gen/©day/©wrt   (text atoms)
      ├─ trkn / disk                          (binary atoms)
      └─ covr                                 (image atom; LAST — see §5)
```

Each `ilst` atom contains a `data` sub-atom: `[size]['data'][4-byte type][4-byte
locale=0][value]`. Type codes: **1 = UTF-8 text, 13 = JPEG, 14 = PNG, 0 = binary**.

Canonical DB key → ilst atom (the DB stores Vorbis-style lowercase keys; the beets
plugin already writes these):

| DB key | ilst atom | encoding |
|---|---|---|
| `title` | `©nam` | UTF-8 text |
| `artist` | `©ART` | UTF-8 text |
| `albumartist` | `aART` | UTF-8 text |
| `album` | `©alb` | UTF-8 text |
| `genre` | `©gen` | UTF-8 text (freeform; **not** the `gnre` enum) |
| `date` | `©day` | UTF-8 text |
| `composer` | `©wrt` | UTF-8 text |
| `tracknumber` | `trkn` | binary: 8 bytes `00 00 | track(2) | total(2) | 00 00` |
| `discnumber` | `disk` | binary: 6 bytes `00 00 | disc(2) | total(2)` |
| (cover art) | `covr` | image bytes, type 13/14 by mime |

Multi-valued keys (e.g. several `genre` rows from the plugin's `genres` expansion)
are emitted as repeated `data` sub-atoms within one ilst atom (the iTunes
convention for multiple values). Non-numeric `trkn`/`disk` values that don't parse
are omitted. DB keys with no mapping are dropped in v1 (no `----` freeform atoms).

## 5. Cover art — streamed, not materialized

Art bytes must not be held in memory (the project invariant). MP4 art lives inside
`moov`, but we control regeneration, so we place `covr` **last** (`covr` last in
`ilst`, `ilst` last in `meta`, `meta`+`hdlr` the only `udta` children, `udta` last
in `moov`). The image bytes are then the final bytes of `moov`, and the layout
splits:

```
Inline( ftyp + new moov header + kept children + udta…through the covr/data header )
ArtImage { art_id, len }            — the image bytes (streamed from the DB at read time)
Inline( mdat box header )
BackingAudio { offset, len }
```

All enclosing box sizes (`data`, `covr`, `ilst`, `meta`, `udta`, `moov`) are
computed up front from the known `art.byte_len`, so the inline prefix carries
correct sizes without ever holding the image. No art → `moov` is fully inline
(single `Inline`), then `mdat`. Only the first `track_art` row (front cover) is
emitted in v1, matching what the plugin syncs.

## 6. Module layout & data flow

New **`musefs-format/src/mp4.rs`** (hand-rolled box layer; no MP4 runtime
dependency), exposing functions that mirror `flac.rs`/`mp3.rs`:

- `locate_audio(&[u8]) -> Result<Mp4Bounds>` — validate the §2 shape; return the
  `mdat` payload offset/len. Used by `scan::probe`.
- `read_tags(&[u8]) -> Vec<(String, String)>` and
  `read_pictures(&[u8]) -> Vec<EmbeddedPicture>` — walk `…/ilst` for scan seeding.
- `read_structure(&[u8]) -> Result<Mp4Scan>` — parse `ftyp` bytes, `moov` bytes,
  `mdat` header bytes, and `mdat` payload offset/len, for synthesis.
- `synthesize_layout(&Mp4Scan, &[TagInput], &[ArtInput]) -> Result<RegionLayout>`
  — the §3–§5 regeneration. Same shape as `flac::synthesize_layout`.

**`resolve` data flow (`reader.rs`, `Format::M4a` arm):** because `moov` may be at
the file's end, the reader reads the whole backing file's bytes
(`std::fs::read`), calls `mp4::read_structure(&bytes)`, then
`mp4::synthesize_layout(&scan, &inputs, &art_inputs)`. This whole-file read is a
transient, parse-time cost on a resolve **cache miss only** (resolve is cached per
`content_version`); the `mdat` payload is still served lazily via `BackingAudio`,
and only `ftyp` + the regenerated `moov` + the `mdat` header are retained in the
cached layout. (`mp4.rs` stays a pure `&[u8]` module; box-aware partial reading is
a noted future optimization.)

## 7. Wiring

- **`musefs-db`** (`models.rs`, `tracks.rs`): `Format` enum gains `M4a`; the
  `format` TEXT round-trip gains `"m4a"`.
- **`musefs-core/scan.rs`**: `collect_audio` accepts `.m4a`/`.m4b`; `probe` adds an
  arm using `mp4::locate_audio` + `read_tags` + `read_pictures`.
- **`musefs-core/reader.rs`**: `resolve` adds the `Format::M4a` synthesis arm
  (§6). `StructureOnly` mode already serves any file whole, so M4A works there with
  no change.
- **beets plugin**: format-agnostic (keys on path); m4a tracks sync with no plugin
  change once `scan` recognises `.m4a`.

## 8. Error handling

- Unsupported/odd files: `locate_audio` → `None` → scan skips (counted), nothing
  stored.
- Structural problems in `resolve`, or the §3 stco-overflow guard → `FormatError`
  (wrapped as `CoreError`), surfaced like any resolve failure; the FUSE layer maps
  it to an IO error for that one file. No partial/corrupt output is ever produced —
  we splice a fully valid MP4 or refuse.
- `mdat` payload bounds are guarded by the existing `resolve` checks
  (size/mtime + `audio_offset`+`audio_length` ≤ file size).

## 9. Testing

- **Unit (`mp4.rs`):** box walker (32/64-bit sizes, nesting, `size==0` trailing
  `mdat`); `locate_audio` accept cases (moov-before-mdat and moov-after-mdat) and
  reject cases (fragmented, video track, multi-`mdat`, no audio); ilst text +
  `trkn`/`disk` + `covr` read round-trips; the `delta` patch math (incl. the
  stco-overflow guard).
- **Independent oracle (dev-dependency `mp4`):** materialize a synthesized layout
  to bytes, parse with the `mp4` crate (an implementation sharing no code with
  ours), and assert it reads **samples through our patched tables byte-identical to
  the originals** — the definitive check on the offset surgery. `mp4ameta`
  optionally cross-checks ilst tags. Both are dev-only (no runtime/distribution
  cost), mirroring `metaflac`'s role for FLAC.
- **e2e tier (existing `e2e` marker):** add an `.m4a` (ffmpeg AAC) to the
  generate→import→retag→`beet musefs`→FUSE-mount flow, and assert ffprobe reads
  beets' tags from the mounted file and `ffmpeg -map 0:a -f md5` of the mounted
  file equals the backing file — ffmpeg as the authoritative external oracle, end
  to end (incl. cover art).

## 10. Non-goals (v1)

- `.mp4` / video-bearing / fragmented (`moof`) / multi-`mdat` / multi-track files
  (rejected, not handled).
- `----` freeform iTunes atoms for unmapped keys (dropped).
- Multiple embedded images (only the front-cover `covr` is emitted).
- Upgrading `stco`→`co64` when offsets overflow (errors instead; not a real case
  for music).
- Box-aware partial reads in `resolve` (whole-file read for now; noted optimization).
