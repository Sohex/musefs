# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

musefs is a **read-only passthrough FUSE filesystem** that presents a virtually
reorganized, re-tagged view of a music library backed by a SQLite store. The
cardinal invariant: **original audio bytes are never copied or modified.** A
served file is assembled on the fly by splicing freshly-generated metadata in
front of positioned reads of the untouched backing file.

Project state and explicit scope boundaries live in `docs/ROADMAP.md`. The
original design spec is `docs/superpowers/specs/2026-05-24-musefs-design.md`;
per-milestone plans are under `docs/superpowers/plans/`.

## Commands

```bash
cargo build                              # build the workspace
cargo test                               # all crates (excludes FUSE e2e — see below)
cargo test -p musefs-core                # one crate
cargo test -p musefs-core read_at        # tests matching a substring
cargo test -p musefs-fuse -- --ignored   # FUSE end-to-end; needs /dev/fuse + libfuse
cargo clippy --all-targets               # lint
cargo fmt                                # format

# Run the CLI (binary is `musefs`):
cargo run -p musefs-cli -- scan <backing_dir> --db <db_path> [--revalidate]
cargo run -p musefs-cli -- mount <mountpoint> --db <db_path> \
    [--template '$albumartist/$album/$title'] [--default-fallback Unknown] \
    [--mode synthesis|structure-only]
```

The `end_to_end_read_through_mount` test in `musefs-fuse` is `#[ignore]`d because
it performs a real mount; it only runs with `--ignored` and requires `/dev/fuse`.

## Crate layout and dependency direction

A strict layered workspace; dependencies point one way only:

```
musefs-db   ─┐                 SQLite store + schema/migrations (source of truth)
musefs-format┘← (db)           format byte-surgery: FLAC/MP3 metadata synthesis + layout
        ↑
musefs-core ← (db, format)     orchestration: virtual tree, resolution, scanning
        ↑
musefs-fuse ← (core)           thin FUSE adapter (fuser)
        ↑
musefs-cli  ← (core, fuse, db) clap entrypoint; binary `musefs`
```

`musefs-core` is the integration layer — most cross-cutting logic lives here.
`musefs-fuse` and `musefs-cli` are deliberately thin.

## The central mechanism (read this before touching read/synthesis paths)

A synthesized virtual file is a `RegionLayout`: an ordered list of `Segment`s
(`musefs-format/src/layout.rs`):

- `Inline(Vec<u8>)` — generated framing/text bytes (e.g. an ID3v2 tag or FLAC
  metadata blocks), fully materialized.
- `ArtImage { art_id, len }` — only the *length* is known here; image bytes are
  streamed from the DB blob at read time, never held in memory.
- `BackingAudio { offset, len }` — a run of the **original** file's audio frames.

`reader::read_at` walks the segments and serves a byte range by splicing: inline
bytes are copied, art is read in chunks via `db.read_art_chunk`, and backing
audio is served with positioned `read_exact_at` against the original file. This is
how "no audio bytes copied" holds end to end.

Two mount **modes** (`musefs_core::Mode`):
- `Synthesis` (default) — generate a fresh metadata region from the DB and splice
  it before the backing audio. FLAC re-reads the file's front for preserved
  structural blocks; MP3 regenerates the ID3v2 tag entirely from the DB (the
  Xing/LAME info frame travels with the audio).
- `StructureOnly` — a single whole-file `BackingAudio` segment; the original bytes
  are served verbatim under the templated tree. Stored audio bounds are not
  validated in this mode because the whole file is served.

## SQLite store is the contract

`musefs-db/src/schema.rs` (`MIGRATION_V1`) defines the schema and is the
**interface external tools write to** (the roadmap targets beets/picard writing
here out-of-band). Tables: `tracks`, `tags`, `art` (content-addressed by sha256,
deduplicated), `track_art`. Migrations are append-only in `MIGRATIONS`; bump
`user_version` accordingly.

Two version counters drive correctness and freshness — keep them distinct:

- **`content_version`** (per-track column). DB triggers increment it (and
  `updated_at`) on any `tags`/`track_art` insert/update/delete. `HeaderCache`
  (`reader.rs`) keys its cached `ResolvedFile` on it: a mismatch rebuilds the
  layout. Every resolve also re-validates the backing file's size+mtime and
  errors with `BackingChanged` if they drifted.
- **`data_version`** (`PRAGMA data_version`, whole-DB). `Musefs::poll_refresh`
  compares it to `last_data_version`; on a change it rebuilds the virtual tree and
  clears the header cache, then commits the new stamp **only after** a successful
  rebuild. The FUSE layer calls `poll_refresh` on metadata ops (e.g. `lookup`,
  `readdir`), so external edits appear **without remounting**.

A tree rebuild **reassigns inodes**, so a descriptor held open across a refresh
may resolve to a different node or none; this is bounded by the FUSE entry/attr
TTL and degrades to `ENOENT`. Refreshes are rare (only on external commits), so
this is acceptable for a read-only mount.

## Virtual tree and templates

`VirtualTree::build` (`tree.rs`) materializes inode → node mappings from rendered
paths. Paths come from beets-style `$field` / `${field}` templates (`template.rs`)
with per-field fallbacks and a `default_fallback`; `tree.rs::disambiguate`
deterministically resolves path collisions. `mapping.rs` bridges DB tag rows to
the format layer's `TagInput`/`ArtInput` and to template fields (order and
multi-value semantics matter — see `mapping.rs` tests).

## Scanning

`scan.rs`: `scan_directory` ingests a backing dir (probe format → extract audio
offset/length + tags + pictures → upsert track/tags/art). `revalidate` is the
maintenance pass: skip unchanged files (preserving external tag edits), prune
tracks whose backing file is gone, and GC orphaned art. `--revalidate` selects it.

## Conventions

- Errors: each crate has its own `error.rs` with a `thiserror` enum; `core` wraps
  lower layers in `CoreError`. The CLI is the only `anyhow` consumer.
- Adding a format: implement probe + `synthesize_layout` in `musefs-format`
  (mirror `flac.rs`/`mp3.rs`), returning a `RegionLayout`; wire it into the
  `match track.format` arms in `reader::HeaderCache::resolve` and into `scan.rs`.
