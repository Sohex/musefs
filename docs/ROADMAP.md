# musefs Roadmap

## Status: v0.1.0 — MVP complete

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

---

## Post-MVP (explicitly deferred)

These are intentionally **out of scope for v0.1.0**. They are recorded here so the
boundary stays explicit; none are half-built in the codebase.

### Formats

- **Ogg/Opus** — deferred. Comment changes can force whole-file rewrites for some
  containers, so the synthesis-and-splice model needs design work here.
- **MP4 / M4A** — deferred (atom-based container; non-trivial to splice).

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

*This roadmap reflects the project state as of the v0.1.0 tag. The original design
spec lives at `docs/superpowers/specs/2026-05-24-musefs-design.md`; per-milestone
implementation plans are under `docs/superpowers/plans/`.*
