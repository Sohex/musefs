# musefs

A read-only passthrough FUSE filesystem that presents a virtually reorganized,
re-tagged view of a music library — without modifying or duplicating a single
byte of the original audio.

Point musefs at a directory of FLAC/MP3 files, edit tags and organization in a
SQLite store (directly, or out-of-band via tools like beets/picard), and mount a
clean `$albumartist/$album/$title` tree whose files carry the corrected metadata.
The audio frames are served straight from your original files; only the
metadata/header region is synthesized on the fly.

> **Status:** v0.1.0 — MVP complete. See [`docs/ROADMAP.md`](docs/ROADMAP.md) for
> what's in scope and what's explicitly deferred (Ogg/Opus, MP4/M4A, writable
> mounts, shipped beets/picard plugins).

## How it works

The original files are never copied or rewritten. Each served file is assembled
on demand from an ordered list of segments:

- **Inline** — generated framing/text bytes (a fresh ID3v2 tag, FLAC metadata
  blocks) materialized from the database.
- **Art** — embedded cover art streamed from the store's content-addressed,
  deduplicated blob table (never buffered whole in memory).
- **Backing audio** — positioned reads of the untouched audio frames in your
  original file.

A SQLite database is the source of truth for tags, art, and the audio byte ranges
within each file. Editing happens there; the mounted view reflects it.

## Features

- **FLAC and MP3** — metadata synthesized from the DB and spliced in front of the
  byte-identical backing audio.
- **Embedded art** — synthesized into the served file and streamed; content-
  addressed and deduplicated in the store.
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
| `musefs-format` | FLAC/MP3 byte surgery: metadata synthesis and segment layout|
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
