# Design: cover-art coverage in the beets E2E suite

Date: 2026-05-26
Status: approved (pending spec review)

## Problem

The full end-to-end suite `contrib/beets/tests/test_e2e.py` drives the real
pipeline — `beet import` → retag → `beet musefs` (autoscan + sync) → a real FUSE
mount — and verifies that the mount shows beets' tags and serves byte-identical
audio for FLAC, MP3, and M4A. It never verifies **cover art**.

This is the only place the two art *ingestion* mechanisms meet the synthesis and
read paths together. Both deserve E2E coverage:

- **Embedded art in the audio files** is ingested by musefs's own `scan`
  (`musefs-core/src/scan.rs`, via `flac/mp3/mp4::read_pictures`) into `track_art`.
- **External cover files** are ingested by the **beets plugin**: it reads the
  album's `artpath` and links the cover via `replace_track_art`
  (`contrib/beets/beetsplug/_core.py`).

The documented precedence (`contrib/beets/README.md`) is: **beets art wins when
present; otherwise scan's embedded art is served.**

No production code is in scope. FLAC, MP3, and MP4/M4A synthesis already emit an
`ArtImage` segment (`musefs-format/src/{flac,mp3,mp4}.rs::synthesize_layout`); this
work closes the E2E verification gap only.

## Goal

Add E2E tests that prove, for every supported format, that a served virtual file
carries the correct front-cover image — covering both ingestion paths and their
precedence — with byte-exact image verification.

## Why exact-bytes verification is correct

musefs never re-encodes the image. The `ArtImage` segment streams the stored DB
blob **verbatim** at read time; each format wraps that identical payload in its own
framing (FLAC `METADATA_BLOCK_PICTURE`, MP3 `APIC`, MP4 `covr`) without altering the
image bytes. mutagen's extractors (`Picture.data`, `APIC.data`, `MP4Cover`) return
exactly that payload. So a `sha256` comparison of the served picture against the
source-of-truth image is the right strictness — mirroring the existing
`_audio_md5(served) == _audio_md5(backing)` check.

The reference image differs per path:

- **External-cover path:** the plugin stores `cover.jpg`'s bytes verbatim →
  compare the served picture to the original cover file.
- **Embedded-art path:** scan stores whatever picture is embedded in the backing
  file → embed with **mutagen** (not ffmpeg) so the embedded payload is a known,
  exact value, and compare the served picture to that known image.

## Scenarios

Three new tests, each exercising FLAC, MP3, and M4A. All three files are imported
as a single beets album (one directory, `import -A`), so the album has one cover
shared across the three formats.

### 1. Embedded art → scan path (`test_e2e_art_embedded_via_scan`)

- Generate the three source files; embed a known cover image **A** into each with
  mutagen.
- Import as-is. fetchart **off**; no external cover present → `album.artpath` stays
  unset, so the plugin does not touch art.
- `beet musefs` → autoscan ingests the embedded pictures into `track_art`.
- Mount; for each format assert the served front-cover bytes' sha256 equals
  image **A**.

### 2. External cover → plugin-sync path (`test_e2e_art_external_via_plugin`)

- Generate the three source files with **no** embedded art; place a generated
  `cover.jpg` (image **B**) in the source album directory.
- fetchart **on** (`sources: filesystem`) so import sets `album.artpath` to the
  cover.
- `beet musefs` → plugin upserts the cover and links it via `replace_track_art`.
- Mount; for each format assert the served front-cover bytes' sha256 equals
  image **B**.

### 3. Precedence — beets wins (`test_e2e_art_precedence_beets_wins`)

- Source files carry embedded image **A**; external cover image **B** (distinct
  from A) is present with fetchart **on**.
- `beet musefs` scans (ingests A) then syncs (`replace_track_art` replaces with B).
- Mount; for each format assert the served front-cover bytes' sha256 equals **B**
  and **not** A — validating the documented "beets art wins" precedence.

In every scenario the existing tag and `_audio_md5` integrity assertions are kept
(carried along so art coverage does not weaken what is already proven).

## Test infrastructure (all in `contrib/beets/tests/test_e2e.py`)

### New helpers

- `_make_cover(path, color)` — generate a small, real image via ffmpeg lavfi
  (`color=c=<color>:s=NxN`, single frame). Distinct colors produce distinct
  sha256s, required by the precedence scenario.
- `_embed_art(audio_path, cover_bytes, mime)` — embed a cover into an existing
  source file **with mutagen**: FLAC `Picture` + `add_picture`, MP3 `APIC`, MP4
  `covr`. Front-cover type. mutagen (not ffmpeg) guarantees the embedded payload
  equals `cover_bytes` exactly, keeping the scan-path reference deterministic.
- `_extract_cover(path)` — per-format extraction of the served picture's raw bytes:
  FLAC `FLAC(path).pictures[0].data`, MP3 `ID3`/`APIC.data`, MP4
  `MP4(path).tags["covr"][0]`. Returns bytes for sha256 comparison.

### Config change

- `_write_config(...)` gains an opt-in `fetchart=False` parameter. When `True`, it
  adds `fetchart` to the plugins list and configures `fetchart: { sources:
  filesystem }` (filesystem-only — no network, deterministic). Scenario 1 leaves it
  off; scenarios 2 and 3 turn it on.

### Library builder

- Generalize `_imported_library` (or add an art-aware sibling) to optionally
  (a) embed art into the generated sources before import and (b) drop a
  `cover.<ext>` into the source album dir before import, and to enable fetchart in
  the config when an external cover is used. The existing tag/move tests keep
  calling it with art off and remain unchanged.

## Risks / open items

- **fetchart under `import -A` with `import.copy: yes`.** The filesystem source
  must actually populate `album.artpath` in the isolated test config. Verify early
  in implementation. **Fallback:** if it does not behave, set `album.artpath` via
  the beets API post-import — this still exercises the plugin-sync path
  (`_album_art_path` → `_prepare_art` → `upsert_art` → `replace_track_art`).
- **Scan-vs-sync ordering in `beet musefs`.** The precedence scenario relies on
  sync running after (and overriding) scan. This is the documented contract; the
  test pins it.

## Out of scope

- The Rust `musefs-fuse/tests/mount.rs` E2E (FLAC-only) — not "per file type".
- Any change to synthesis, scan, or plugin production code.
- Multi-picture / non-front-cover picture types; only a single front cover per
  album is exercised.
