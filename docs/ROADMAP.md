# musefs Roadmap

## Status: MVP complete; formats and integrations extended since

musefs is a read-only passthrough FUSE filesystem that presents a virtually
reorganized, re-tagged view of a music library backed by a SQLite store, without
modifying or duplicating the original audio bytes.

### Delivered in v0.1.0

- **Formats:** FLAC and MP3 — metadata synthesized on the fly from the DB and
  spliced in front of the byte-identical backing audio (no audio bytes copied).
- **Embedded art:** synthesized into the served file and streamed (never
  materialized in memory), content-addressed and deduplicated in the store.
- **Virtual tree:** beets-style `$field` / `${field}` path templates with
  fallbacks and deterministic collision disambiguation.
- **Mount modes:** `synthesis` (default — re-tagged view) and `structure-only`
  (pure passthrough: original backing bytes served verbatim under the templated
  tree).
- **Auto-refresh:** external DB edits (a `scan`, a beets/picard retag on another
  connection) are picked up automatically via `PRAGMA data_version` polling on
  metadata operations — no remount required.
- **Maintenance:** `scan` ingests a backing directory; `scan --revalidate` skips
  unchanged files (preserving external tag edits), prunes tracks whose backing
  file is gone, and garbage-collects orphaned art.
- **CLI:** `musefs scan` and `musefs mount` with `--mode`, `--template`,
  `--default-fallback`, and `--revalidate`.

### Delivered since v0.1.0

- **beets plugin** (`contrib/beets/`) — syncs beets' canonical tags and cover
  art into the SQLite store, keyed by each file's real path, so a live mount
  presents the re-tagged view with no remount and no audio rewrite. It
  auto-scans via the `musefs` binary (no separate `scan` step; `musefs scan`
  also gained single-file support), reconciles file moves/renames (pruning rows
  whose backing file is gone), and runs both as a `beet musefs` command and via
  import/write hooks. Verified end-to-end (beets import + retag + FUSE mount)
  with byte-identical audio.
- **M4A / M4B (MP4):** metadata synthesized by rebuilding the `moov` atom and
  patching `stco`/`co64` chunk offsets so the `mdat` audio is served
  byte-identically. Embedded art included.
- **Ogg container — Opus, Vorbis, and FLAC-in-Ogg:** re-tagged VorbisComments
  (Opus `OpusTags`, Vorbis comment header, OggFLAC native blocks) plus embedded
  cover art, with audio served byte-identically. Because a resized metadata
  header changes the Ogg page count, the original audio pages are served verbatim
  except for their page sequence numbers and CRCs, which are patched in place; the
  per-page index is built lazily on first read (constant-memory, cached) so
  `open()`/`stat` do no audio I/O. Cover art is re-embedded without ever holding
  the image in the cached layout — read serves any base64 window from a bounded
  input range, so a full-library scan stays cheap on SSD/HDD/NFS. Multiplexed or
  chained Ogg (more than one logical bitstream) is detected at scan and skipped.
  Verified end-to-end (real FUSE mount + independent demux) for all three codecs,
  including byte-identical cover-art round-trips.

---

## Post-MVP (explicitly deferred)

These are intentionally **out of scope for v0.1.0**. They are recorded here so the
boundary stays explicit; none are half-built in the codebase.

### Formats

- All currently targeted formats (FLAC, MP3, M4A/M4B, and Ogg Opus/Vorbis/
  FLAC-in-Ogg) are delivered — see above. Remaining edges: FLAC-in-Ogg only
  handles the standard `0x7F "FLAC"` 1.x mapping, and chained/multiplexed Ogg is
  intentionally skipped rather than synthesized.

### Editing / writability

- **Writable mount** — intercepting inbound tag writes through the FUSE layer.
  The MVP is read-only; editing happens out-of-band against the SQLite schema
  (e.g. via beets/picard), which auto-refresh then surfaces.
- **Manual per-track path overrides** — only clean with a writable filesystem;
  follows the writable-mount work.

### Distribution / integration

- **picard plugin as a shipped artifact** — the SQLite *contract* is a target
  for picard too, but only the **beets** plugin ships today (see "Delivered
  since v0.1.0"). A picard plugin is not yet built.

### Operations

- **Explicit `musefs refresh` / SIGHUP command** — a manual "rebuild now"
  fallback. Deferred because automatic `data_version` polling already covers
  external edits without remounting, and signal handling inside fuser's blocking
  session loop was not worth the complexity for the MVP. Revisit only if a
  forced, synchronous rebuild proves necessary in practice.

---

*The original design spec lives at
`docs/superpowers/specs/2026-05-24-musefs-design.md`; the Ogg container work is
specced at `docs/superpowers/specs/2026-05-26-ogg-container-support-design.md`.
Per-milestone implementation plans are under `docs/superpowers/plans/`.*
