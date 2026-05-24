# musefs — Design

**Date:** 2026-05-24
**Status:** Approved design, ready for implementation planning

## Overview

musefs is a **read-only FUSE filesystem** that presents a virtual, well-organized,
correctly-tagged view of music whose real bytes live untouched elsewhere on disk
(for example, files being seeded as torrents). Organization (folder structure and
filenames) and metadata (in-file tags and album art) are driven entirely by a
backing SQLite store. The real audio is never modified or duplicated.

### Motivating use case

You have an album (e.g. a torrent) with incomplete or wrong track metadata and a
folder layout you dislike. You cannot edit the files — that would make them
unseedable. musefs lets you mount a virtual filesystem that reads the real audio
frames from the original files but presents them with corrected tags, embedded
art, and an organized directory layout, all sourced from SQLite. Music players,
`beets`, `picard`, etc. see clean, correct files; the originals stay byte-identical.

### Operating modes (per mount)

- **Synthesis mode (core):** virtual paths *and* freshly generated in-file tags
  (text + art) spliced onto the real audio frames.
- **Structure-only mode:** virtual paths only; file bytes are passed through
  verbatim with no tag rewriting.

### Scope (MVP)

- Formats: **FLAC and MP3 only.** (Ogg/Opus deferred — comment changes can force
  page re-pagination + per-page CRC recompute, which is disproportionate effort
  for its rarity relative to FLAC/MP3. MP4 also deferred.)
- **Read-only** mount.
- **Beets-style path templates** for the virtual layout.
- **Full album-art management** (store/replace art, synthesize picture blocks).
- **SQLite is the source of truth and the integration contract.** External taggers
  (beets/picard) writing to the schema are the happy path for editing; a no-frills
  **built-in scanner** bootstraps the store and works standalone.

### Explicitly out of MVP

- Writable mount / intercepting inbound tag writes.
- Manual per-track path overrides (clean only with a writable FS).
- beets/picard plugins as shipped artifacts (the *contract* they target is in
  scope; the plugins themselves are follow-on deliverables).
- Ogg/Opus, MP4/M4A.

## Architecture

Rust workspace, `fuser` crate for the FUSE binding.

### Crates

- **`musefs-db`** — SQLite schema, migrations, triggers, and the typed read/write
  API. The contract that everything else (and external plugins) targets.
- **`musefs-format`** — per-format parsing & synthesis. Locates audio-frame
  boundaries in backing files; generates metadata regions (Vorbis comment +
  PICTURE for FLAC; ID3v2 + APIC, ID3v1 handling for MP3). Pure, heavily
  unit-tested, no FUSE/DB dependencies.
- **`musefs-core`** — synthesis engine and virtual-tree logic: path-template
  evaluation, inode/tree model, the lazy generate-and-measure header cache,
  segment/offset mapping for reads, and change detection.
- **`musefs-fuse`** — the `fuser` filesystem implementation (lookup, getattr,
  readdir, open, read, release), translating VFS calls into `musefs-core`
  operations.
- **`musefs-cli`** — the `musefs` binary: `mount`, `scan`, `refresh`.

### Data flow

1. **Ingest:** the scanner or an external tagger writes track rows (backing path,
   format, audio offsets, validation stamps), tag rows, and art into SQLite.
2. **Mount:** `musefs-core` builds the virtual tree by evaluating the path
   template against each track's tags.
3. **Read:** on `open`, the header version is pinned; on `read`, the file is served
   as ordered segments — `[synthesized metadata region]` (text/framing generated,
   art streamed from the art store) followed by `[backing audio frames]` — with
   `st_size` from the measured (then cached) total length.
4. **Live changes:** `musefs-core` detects external DB writes and lazily rebuilds
   the tree / invalidates header caches.

## Synthesis strategy

**Lazy, generate-and-measure, art-streamed**, with version-pinned opens.

Rationale (chosen over persisted header blobs and full retagged-file caches):

- **Always fresh.** The synthesized region is derived from current DB state, keyed
  by a per-track content version. When external taggers edit tag rows directly,
  the next access regenerates — nothing to invalidate manually. This is the
  decisive property given that external tools owning edits is the happy path.
- **Zero write amplification.** Editing a tag touches only that row.
- **No audio duplication**, which is the entire point of the project.
- **Bounded memory + cheap stat**, achieved by two refinements:
  1. **Generate-and-measure, then cache the measured length.** The variable-length,
     drift-prone parts (Vorbis comment framing, ID3v2 syncsafe sizes) are generated
     and measured rather than predicted analytically, so `st_size` is correct by
     construction. The measured total is cached per content version, so `stat` is
     cheap after first touch.
  2. **Art is streamed, not cached in the header.** The header cache holds only the
     small text/framing bytes; image bytes are spliced from the art store at read
     time, so memory stays small regardless of art size.

### Known failure modes & mitigations

- **Audio-boundary mis-detection** (inherent to any splicing design; MP3 is the
  trap: syncsafe ID3v2 size, ID3v1 trailer, APEv2, extended headers, Xing/LAME
  info frame). Off by one byte → corrupt file. Mitigated by dedicated, heavily
  tested per-format parsers and round-trip validation.
- **`st_size` must be byte-perfect** — `cp`/rsync/players read the reported size
  then expect exactly that many bytes. Mitigated by generate-and-measure +
  caching the measured length (never analytic prediction).
- **Concurrent edit during an open read** — version bump mid-stream would change
  size and corrupt the consumer's view. Mitigated by **pinning the content version
  at `open()`** and serving that immutable snapshot for the fd's lifetime; new
  opens pick up the new version.
- **Cache invalidation needs a reliable change signal** — solved by DB triggers
  that bump `content_version` automatically (external writers need not remember to
  signal), plus `PRAGMA data_version` polling to notice any external commit.
- **Backing file changes underneath us** (moved/truncated/still-downloading/
  recheck-rewritten) — stored offsets go stale. Mitigated by `size`/`mtime`
  validation at `open` (→ `EIO`) and `musefs scan --revalidate` to repair.
- **Cold-scan thundering herd** — a client indexing the whole library generates
  many headers quickly. Bounded by concurrency, not catalog size; accepted.

## Data model (SQLite — the integration contract)

```
tracks
  id              INTEGER PRIMARY KEY
  backing_path    TEXT NOT NULL UNIQUE       -- absolute path to real file
  format          TEXT NOT NULL              -- 'flac' | 'mp3'
  audio_offset    INTEGER NOT NULL           -- byte where audio frames begin
  audio_length    INTEGER NOT NULL           -- bytes of audio region to splice
  backing_size    INTEGER NOT NULL           -- validation: detect backing changes
  backing_mtime   INTEGER NOT NULL           -- validation
  content_version INTEGER NOT NULL DEFAULT 0 -- bumped by triggers on tag/art change
  updated_at      INTEGER NOT NULL

tags
  track_id  INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE
  key       TEXT NOT NULL                    -- canonical name: artist, albumartist,
                                             --   album, title, tracknumber, date, genre, ...
  value     TEXT NOT NULL
  ordinal   INTEGER NOT NULL DEFAULT 0        -- multi-valued tags keep order
  PRIMARY KEY (track_id, key, ordinal)

art
  id       INTEGER PRIMARY KEY
  sha256   TEXT NOT NULL UNIQUE               -- content-addressed dedup
  mime     TEXT NOT NULL
  width    INTEGER, height INTEGER            -- for PICTURE block fields
  byte_len INTEGER NOT NULL
  data     BLOB NOT NULL                      -- streamed via incremental blob I/O

track_art
  track_id     INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE
  art_id       INTEGER NOT NULL REFERENCES art(id)
  picture_type INTEGER NOT NULL DEFAULT 3     -- 3 = front cover (ID3/FLAC enum)
  description  TEXT NOT NULL DEFAULT ''
  ordinal      INTEGER NOT NULL DEFAULT 0
  PRIMARY KEY (track_id, ordinal)
```

### Rationale

- **Key/value `tags` (multi-valued)** mirror how Vorbis comments and ID3 frames
  actually work, and feed both synthesis and templating. A canonical tag vocabulary
  lives in `musefs-format`, mapping each canonical key → Vorbis comment name and →
  ID3 frame id.
- **Art as deduplicated BLOBs** keeps the store a single portable file. Reads use
  SQLite **incremental blob I/O**, streaming art into the spliced region without
  loading whole images into memory. Dedup is enforced by the `sha256 UNIQUE`
  constraint; the writer hashes each image and does
  `INSERT … ON CONFLICT(sha256) DO NOTHING`, then links the existing `art.id`.
  (Alternative: content-addressed files on disk — easy to switch later since access
  is funneled through `musefs-db`.)
- **`audio_offset`/`audio_length`** exclude any backing trailers (ID3v1, APEv2);
  legacy ID3v1 values are folded into the DB at scan time.

### Change detection

- **Triggers** on `tags` and `track_art` (insert/update/delete) bump the parent
  `tracks.content_version` (and `updated_at`). External tools never have to
  remember to signal a change.
- musefs cheaply polls **`PRAGMA data_version`** to notice any external commit,
  then reconciles per-track `content_version` to invalidate exactly the affected
  header caches and tree nodes.
- `musefs refresh` / SIGHUP force an immediate full rebuild as a manual fallback.

## Path templates

Presentation concern; lives in per-mount config, not the DB.

- MVP supports `$field` / `${field}` substitution, path-illegal-character
  sanitization, and configurable fallbacks for missing fields (e.g.
  `Unknown Artist`, `Untitled`). The extension is appended from `format`.
- Multi-valued fields collapse to their first value for path purposes.
- Example: `$albumartist/$album/$track - $title`
- **Path collisions** (two tracks resolving to the same virtual path) receive a
  deterministic disambiguation suffix (logged).
- Heavier beets-isms (conditionals, format functions) are explicitly deferred.

### Per-mount config (TOML, CLI-overridable)

`db path`, `mount point`, `mode` (`synthesis` | `structure-only`), `path template`,
`field fallbacks`, `art on/off`.

## Synthesis engine (`musefs-format`)

Pure, no DB/FUSE dependencies. Each per-format handler exposes two operations.

### `locate_audio(file) -> AudioBounds` (scan time)

Finds where audio frames begin/end.

- **FLAC:** parse metadata blocks; audio starts after the last one. STREAMINFO and
  other structural blocks (SEEKTABLE/CUESHEET/APPLICATION) are *preserved* and
  re-read cheaply from the backing file at synth time (tiny, at the front, backing
  file validated immutable). VORBIS_COMMENT and PICTURE blocks are *regenerated*
  from the DB.
- **MP3:** audio starts after any ID3v2; `audio_length` excludes a trailing ID3v1.
  A Xing/LAME header lives inside the first audio frame and is preserved
  automatically. Legacy ID3v1/ID3v2 values are folded into the DB at scan time,
  then a fresh ID3v2 is synthesized.

### `synthesize_layout(preserved, tags, art_refs) -> RegionLayout` (synth time)

Returns an **ordered segment list**, not a flat buffer:

- `Inline(bytes)` — generated framing + text blocks (Vorbis comment block, ID3v2
  frames, picture-block headers). The variable-length, drift-prone parts — so these
  are **generated and measured**.
- `ArtImage(art_id, image_len)` — raw image bytes, *not* materialized; `image_len`
  comes exactly from `art.byte_len`.
- Trailed by `BackingAudio(audio_offset, audio_length)`.

`header_len = Σ inline + Σ image_len`; `st_size = header_len + audio_length`.
Correct by construction: the only un-generated lengths are exact image byte counts.
The layout + total length are cached keyed by `(track_id, content_version, mode)`.

The `RegionLayout` type includes `ArtImage` segments **from the start** (M1), so art
support lands later without reworking the model.

## FUSE serving layer (`musefs-fuse`)

- **Inode/tree:** in-memory tree built at mount by evaluating the template per
  track. `inode → Dir(name→inode) | File(track_id)`. Held in an `arc-swap` so reads
  are lock-free and a refresh atomically swaps in a rebuilt tree. Inodes are kept
  stable across rebuilds by virtual path (best-effort) to avoid disrupting open
  handles.
- **`getattr`:** for files, `st_size` from the cached total length; computes the
  layout on first need (the bounded cold-scan cost). Dirs `0555`, files read-only
  (`0444`); `mtime = max(backing_mtime, tag updated_at)` so taggers notice changes.
- **`open`:** allocates a file handle that **snapshots**
  `(track_id, content_version, resolved layout, total_len)` and validates the
  backing file (`size`/`mtime`). The snapshot is an immutable `Arc`, so reads stay
  consistent even if the DB changes mid-stream; the new version is picked up on the
  next `open`.
- **`read(fh, offset, size):`** walks the snapshot's segments for the requested
  range, gathering from: inline buffers (memcpy), art (SQLite incremental blob read
  at the computed image offset), and backing audio (positioned `pread` at
  `audio_offset + (offset − header_len)`). Handles ranges spanning multiple segments
  and arbitrary seeks.
- **Structure-only mode** reuses the same tree but files are pure passthrough:
  `st_size = backing_size`, `read = pread` on the backing file, no synthesis.

### Concurrency

`fuser` is multithreaded. The tree is shared via `arc-swap` (lock-free reads, atomic
swap on refresh). Per-fd snapshots are immutable `Arc`s. Backing reads use positioned
`pread` (no shared cursor). SQLite access is pooled; blob reads use short-lived
connections / a pool.

## Error handling

- Backing file missing/changed (fails `size`/`mtime` check) → `EIO` + log;
  `musefs scan --revalidate` repairs offsets.
- Unparseable/unsupported file at scan → skipped with a warning, never inserted.
- Missing template fields → configured fallbacks; path collisions → deterministic
  disambiguation suffix (logged).
- Referenced art blob missing → synthesize without it + warn.
- A file edited after it was opened → served from the pinned snapshot (consistent
  but stale until reopened); documented behavior.

## CLI surface (`musefs` binary)

- `musefs scan <backing-dir> --db <path> [--revalidate]` — walk the directory,
  parse each FLAC/MP3, upsert track rows (backing path, format, audio bounds,
  `size`/`mtime` stamps) and seed `tags` from existing embedded metadata (folding
  ID3v1). `--revalidate` re-checks stamps and repairs offsets for changed files.
  Auto-creates/migrates the schema if absent.
- `musefs mount <mountpoint> --db <path> [--config <toml>] [--mode synthesis|structure-only] [--template <str>] [--foreground]`
  — mount the FUSE fs. CLI flags override the TOML config.
- `musefs refresh <mountpoint>` — force an immediate rebuild of a running mount
  (locates pid, sends SIGHUP). Rarely needed thanks to automatic `data_version`
  polling; provided as a deterministic fallback.
- Unmounting via the standard `fusermount -u`.

## Testing strategy

TDD throughout; `musefs-format` is the crux.

- **Format unit tests with tiny real fixtures.** For each handler: assert
  `locate_audio` offsets, then **round-trip** — synthesize a region with known
  tags+art, concatenate with backing audio, and parse the result with an
  *independent* library to confirm the file is valid and tags/art match. This
  catches byte-layout bugs.
- **Generate-and-measure invariant:** for every fixture, assert
  `predicted header_len == bytes actually produced`, and
  `st_size == total bytes readable`.
- **Splice/seek correctness:** read at random offsets/sizes and compare against a
  reference full-file assembled in memory — exercises multi-segment and
  arbitrary-seek paths.
- **DB tests:** triggers bump `content_version`; `data_version` change detection;
  backing-change → `EIO`.
- **Open-snapshot consistency:** open a file, mutate its tags, keep reading the old
  fd, assert bytes stay stable; reopen picks up the change.
- **FUSE integration tests** (Linux/CI with `/dev/fuse`): mount temp db+fixtures,
  read through the mount, validate outputs; gated behind a CI feature flag since
  FUSE needs privileges.

## Build phasing

Each milestone is independently testable.

- **M0 — Workspace + DB:** crate scaffold; `musefs-db` schema, migrations, triggers,
  typed API + tests.
- **M1 — FLAC synthesis (pure):** `musefs-format` FLAC `locate_audio` +
  `synthesize_layout`; `RegionLayout` includes `ArtImage` segments from the start;
  generate-and-measure + round-trip validation. No FUSE yet.
- **M2 — Read-only FLAC mount:** `musefs-core` tree + header cache + segment reads;
  `musefs-fuse` lookup/getattr/readdir/open/read/release; `musefs-cli` scan+mount.
  End-to-end FLAC.
- **M3 — MP3 support:** ID3v2 synthesis, ID3v1 fold, boundary detection;
  fixtures/tests; wired into scan + synth.
- **M4 — Art management:** `art`/`track_art` population, dedup, incremental-blob
  streaming, PICTURE/APIC synthesis (segment type already present from M1). Art is
  an MVP requirement, sequenced here only because text synthesis + the mount form
  the foundation it attaches to.
- **M5 — Structure-only mode + refresh polish:** passthrough mode; `data_version`
  polling + SIGHUP refresh; `--revalidate`.

### Out of MVP (later)

beets/picard plugins, Ogg/Opus, MP4, writable mount + path overrides.
