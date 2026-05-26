# musefs

A read-only passthrough FUSE filesystem that presents a virtually reorganized,
re-tagged view of a music library — without modifying or duplicating a single
byte of the original audio.

Point musefs at a directory of FLAC, MP3, M4A, or Ogg (Opus / Vorbis /
FLAC-in-Ogg) files, edit tags and organization in a SQLite store (directly, or
out-of-band via tools like beets/picard), and mount a clean
`$albumartist/$album/$title` tree whose files carry the corrected metadata. The
audio frames are served straight from your original files; only the
metadata/header region is synthesized on the fly.

> **Status:** MVP complete and extended. FLAC, MP3, M4A, and Ogg
> (Opus / Vorbis / FLAC-in-Ogg) are supported, with embedded cover art. See
> [`docs/ROADMAP.md`](docs/ROADMAP.md) for what's in scope and what's explicitly
> deferred (writable mounts, a shipped picard plugin).

## How it works

The original files are never copied or rewritten. Each served file is assembled
on demand from an ordered list of segments:

- **Inline** — generated framing/text bytes (a fresh ID3v2 tag, FLAC metadata
  blocks) materialized from the database.
- **Art** — embedded cover art streamed from the store's content-addressed,
  deduplicated blob table (never buffered whole in memory); for Ogg the base64
  `METADATA_BLOCK_PICTURE` is encoded incrementally at read time.
- **Backing audio** — positioned reads of the untouched audio frames in your
  original file.
- **Renumbered audio (Ogg)** — original Ogg pages served verbatim with only their
  page sequence numbers and CRCs patched in place, so a resized header never
  requires rewriting the audio.

A SQLite database is the source of truth for tags, art, and the audio byte ranges
within each file. Editing happens there; the mounted view reflects it.

## Features

- **FLAC, MP3, M4A, and Ogg (Opus / Vorbis / FLAC-in-Ogg)** — metadata synthesized
  from the DB and spliced in front of byte-identical backing audio. M4A rebuilds
  the `moov` atom (patching chunk offsets); Ogg renumbers audio pages and
  recomputes their CRCs so the audio frames stay untouched. Multiplexed/chained
  Ogg is detected and skipped.
- **Embedded art** — re-embedded into the served file and streamed from the
  content-addressed, deduplicated blob store (never buffered whole in memory),
  including Ogg cover art served as incremental base64.
- **Virtual tree** — beets-style `$field` / `${field}` path templates with
  fallbacks and deterministic collision disambiguation.
- **Two mount modes** — `synthesis` (default, re-tagged view) and
  `structure-only` (pure passthrough: original bytes served verbatim under the
  templated tree).
- **Auto-refresh** — external DB edits (a `scan`, a beets/picard retag on another
  connection) are picked up automatically; no remount required.
- **Maintenance** — `scan --revalidate` skips unchanged files (preserving
  external tag edits), prunes tracks whose backing file is gone, and garbage-
  collects orphaned art.

## Requirements

- Rust (2021 edition) and Cargo.
- Linux with FUSE (`/dev/fuse` and libfuse) to mount.

## Build

```bash
cargo build --release
```

The binary is `musefs` (the `musefs-cli` crate).

## Usage

Ingest a backing directory into a SQLite store:

```bash
musefs scan /path/to/music --db library.db
```

Mount a read-only view:

```bash
musefs mount /path/to/mountpoint --db library.db \
    --template '$albumartist/$album/$title' \
    --default-fallback Unknown \
    --mode synthesis        # or: structure-only
```

`mount` blocks until the filesystem is unmounted. Edit tags/art in `library.db`
(or run another `scan`) while mounted and the view refreshes automatically.

Re-scan to pick up changes on disk while preserving external tag edits:

```bash
musefs scan /path/to/music --db library.db --revalidate
```

Run `musefs <command> --help` for the full flag list.

## Project layout

A layered Cargo workspace:

| Crate           | Responsibility                                              |
| --------------- | ----------------------------------------------------------- |
| `musefs-db`     | SQLite store: schema, migrations, tracks/tags/art access    |
| `musefs-format` | FLAC/MP3/MP4/Ogg byte surgery: metadata synthesis + layout    |
| `musefs-core`   | Orchestration: virtual tree, file resolution, scanning      |
| `musefs-fuse`   | Thin FUSE adapter (fuser)                                   |
| `musefs-cli`    | `musefs` command-line entrypoint (clap)                     |

See [`CLAUDE.md`](CLAUDE.md) for the architecture in depth.

## Development

```bash
cargo test                               # all crates
cargo test -p musefs-core read_at        # tests matching a substring
cargo test -p musefs-fuse -- --ignored   # FUSE end-to-end (needs /dev/fuse)
cargo clippy --all-targets
cargo fmt
```

The FUSE end-to-end test performs a real mount and is `#[ignore]`d by default; run
it with `--ignored` on a host that has `/dev/fuse`.

## License

Licensed under the [MIT License](LICENSE).
