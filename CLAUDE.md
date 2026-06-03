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

# Property tests (proptest): byte-identical invariant + tag round-trip. The
# format-layer proptests need the `fuzzing` feature; `cargo test --workspace`
# also runs them via feature unification.
cargo test -p musefs-format --features fuzzing
cargo test -p musefs-core --test proptest_read_fidelity

# Coverage-guided fuzzing (needs nightly + cargo-fuzz; the fuzz/ crate is
# excluded from the workspace):
cargo +nightly fuzz run <flac|mp3|mp4|ogg|wav|ogg_page|b64|vorbiscomment>
cargo +nightly fuzz coverage <target>     # confirm coverage reaches the parser
cargo run --manifest-path fuzz/Cargo.toml --bin generate_seeds  # (re)build seeds

# Independent-reader (mutagen) interop — Property 5:
MUSEFS_INTEROP_DIR=/tmp/i cargo test -p musefs-core --test interop_emit -- --ignored emit_interop_fixtures
MUSEFS_INTEROP_DIR=/tmp/i python -m pytest tests/interop

# Run the CLI (binary is `musefs`):
cargo run -p musefs -- scan <backing_dir> --db <db_path> [--revalidate]
cargo run -p musefs -- mount <mountpoint> --db <db_path> \
    [--template '$albumartist/$album/$title'] [--default-fallback Unknown] \
    [--mode synthesis|structure-only]
```

The FUSE end-to-end tests in `musefs-fuse` (e.g. `end_to_end_read_through_mount`)
are `#[ignore]`d because they perform real mounts; they only run with `--ignored`
and require `/dev/fuse`.

### contrib Python plugins (beets / Picard)

The `contrib/` plugins share one library, `python-musefs` (import package
`musefs_common`, in `contrib/python-musefs/`): beets depends on it via pip,
Picard vendors a committed copy into `musefs/_common/` (re-vendor with
`python contrib/python-musefs/vendor_to_picard.py`; a drift-guard test enforces
freshness). Mirror these constants when the Rust schema changes:
`EXPECTED_USER_VERSION` (= `MIGRATIONS` length in `musefs-db/src/schema.rs`) and
`MAX_ART_BYTES` (mirrors `musefs-core/src/scan.rs`) in
`contrib/python-musefs/src/musefs_common/constants.py`.

```bash
# python-musefs is self-contained (its tests use pythonpath=src):
cd contrib/python-musefs && python -m pytest && ruff check . && ruff format --check .

# beets declares python-musefs but it's UNPUBLISHED and has no [tool.uv.sources],
# so install the local lib FIRST (a bare `uv run`/pip install of beets alone
# fails resolving python-musefs from PyPI):
cd contrib/beets && pip install -e ../python-musefs && pip install -e ".[test]" && python -m pytest tests

# Picard needs no install (vendored + pythonpath="."); pytest-qt needs a Qt
# binding or it errors at collection. Real-Picard tests importorskip if Picard is absent:
cd contrib/picard && python -m pytest tests
```

## Crate layout and dependency direction

A strict layered workspace; dependencies point one way only:

```
musefs-db   ─┐                 SQLite store + schema/migrations (source of truth)
musefs-format┘← (db)           format byte-surgery: FLAC/MP3/MP4/Ogg/WAV metadata synthesis + layout
        ↑
musefs-core ← (db, format)     orchestration: virtual tree, resolution, scanning
        ↑
musefs-fuse ← (core)           thin FUSE adapter (fuser)
        ↑
musefs-cli  ← (core, fuse, db) clap commands library (scan/mount logic)
musefs      ← (cli)            thin binary entrypoint; published as `musefs`
```

`musefs-core` is the integration layer — most cross-cutting logic lives here.
`musefs-fuse`, `musefs-cli`, and the `musefs` binary crate are deliberately thin.

## The central mechanism (read this before touching read/synthesis paths)

A synthesized virtual file is a `RegionLayout`: an ordered list of `Segment`s
(`musefs-format/src/layout.rs`):

- `Inline(Vec<u8>)` — generated framing/text bytes (e.g. an ID3v2 tag or FLAC
  metadata blocks), fully materialized.
- `ArtImage { art_id, len }` — only the *length* is known here; image bytes are
  streamed from the DB blob at read time, never held in memory.
- `BackingAudio { offset, len }` — a run of the **original** file's audio frames.
- `OggAudio { offset, len, seq_delta }` — original Ogg audio pages served with each
  page's sequence number shifted by `seq_delta` and its CRC recomputed in place (a
  resized header changes the page count); the served byte length is unchanged.
- `OggArtSlice { art_id, offset, len, base64, art_total }` — an Ogg cover-art window
  served lazily from the blob store, base64-encoded incrementally at read time.

`reader::read_at` walks the segments and serves a byte range by splicing: inline
bytes are copied, art is read in chunks via `db.read_art_chunk`, and backing
audio is served with positioned `read_exact_at` against the original file (Ogg
audio pages are renumbered and CRC-patched in place, never recopied). This is
how "no audio bytes copied" holds end to end.

Two mount **modes** (`musefs_core::Mode`):
- `Synthesis` (default) — generate a fresh metadata region from the DB and splice
  it before the backing audio. FLAC re-reads the file's front for preserved
  structural blocks; MP3 regenerates the ID3v2 tag entirely from the DB (the
  Xing/LAME info frame travels with the audio); M4A rebuilds the `moov` atom and
  patches `stco`/`co64` chunk offsets; Ogg renumbers audio pages and recomputes
  per-page CRCs; WAV regenerates the RIFF front (a native `LIST`/`INFO` chunk plus
  an embedded `id3 ` chunk for full ID3v2 + art) ahead of the verbatim `data`
  payload.
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
  prunes the header/size caches to the live track set (unchanged entries stay
  warm; a changed track self-invalidates lazily via `content_version`), then
  commits the new stamp **only after** a successful rebuild. The FUSE layer fires
  `poll_refresh` on metadata ops (e.g. `lookup`, `readdir`) off the dispatch
  thread, so external edits appear **without remounting**. Polling is debounced
  (`--poll-interval-ms`) and rebuilds are single-flighted, so a metadata-op storm
  costs at most one rebuild per interval.

Inodes are **stable across rebuilds**: a persistent path→inode allocator
(`tree.rs`) reuses an unchanged rendered path's inode and never recycles a retired
one, so a descriptor held open across a refresh keeps resolving to the same node
(a path that vanished degrades to `ENOENT`, bounded by the entry/attr TTL). When
mounted with `--keep-cache`, `poll_refresh_notify` reports the inodes whose
`content_version` rose and the FUSE layer drops their kernel page cache
(`inval_inode`), so a re-tagged file never serves stale cached bytes.

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
- Internal error paths do not discard diagnostics: no `Result<_, ()>`, and no
  `.map_err(|_| …)` that drops a source. Each error variant carries its source
  (`#[from]`) or a static reason describing the broken invariant.
- Adding a format: implement probe + `synthesize_layout` in `musefs-format`
  (mirror an existing module — `flac.rs`, `mp3.rs`, `mp4.rs`, `ogg/`, `wav.rs`),
  returning a `RegionLayout`; add the variant to `musefs-db`'s `Format` enum, then
  wire it into the `match track.format` arms in `reader::HeaderCache::resolve` and
  into `scan.rs`. Then extend the test surface: add a
  `fuzz_check::fixtures::<fmt>()` minimal file, a `fuzz/fuzz_targets/<fmt>.rs`
  target with a seed in `generate_seeds`, a `musefs-format/tests/proptest_<fmt>.rs`,
  and a manifest row in `musefs-core/tests/interop_emit.rs`.
