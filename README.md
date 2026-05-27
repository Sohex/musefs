# musefs

A read-only passthrough FUSE filesystem that presents a virtually reorganized,
re-tagged view of a music library — without modifying or duplicating a single
byte of the original audio.

Point musefs at a directory of FLAC, MP3, M4A, Ogg (Opus / Vorbis /
FLAC-in-Ogg), or WAV files, edit tags and organization in a SQLite store (directly, or
out-of-band via tools like beets/picard), and mount a clean
`$albumartist/$album/$title` tree whose files carry the corrected metadata. The
audio frames are served straight from your original files; only the
metadata/header region is synthesized on the fly.

> **Status:** MVP complete and extended. FLAC, MP3, M4A, Ogg
> (Opus / Vorbis / FLAC-in-Ogg), and WAV are supported, with embedded cover art, and the
> filesystem has been through a performance/concurrency pass hardening it for
> real-world player/media-manager access and large libraries on HDD/SSD/NFS. See
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

- **FLAC, MP3, M4A, Ogg (Opus / Vorbis / FLAC-in-Ogg), and WAV** — metadata
  synthesized from the DB and spliced in front of byte-identical backing audio. M4A
  rebuilds the `moov` atom (patching chunk offsets); Ogg renumbers audio pages and
  recomputes their CRCs so the audio frames stay untouched; WAV regenerates the
  RIFF front (a native `LIST`/`INFO` chunk plus an embedded `id3 ` chunk for full
  ID3v2 + art) ahead of the verbatim `data` payload. Multiplexed/chained Ogg is
  detected and skipped.
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
- **Concurrent & cache-friendly** — blocking reads run on a worker pool, so a slow
  backing read (NFS, spun-down HDD) never stalls metadata operations; synthesized
  layouts, file sizes, and headers are cached and invalidated lazily on external
  edits, and inodes stay stable across refreshes. Kernel read-ahead, background
  depth, the entry/attr cache TTL, the refresh poll interval, and page-cache
  retention are all tunable per backing store (see [Tuning](#tuning)). With
  `--keep-cache`, an external re-tag automatically drops the affected kernel page
  cache, so cached bytes never go stale.

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

### Tuning

`mount` accepts optional performance flags, all with sensible defaults — tune them
to your backing store:

| Flag | Default | What it does |
| ---- | ------- | ------------ |
| `--poll-interval-ms` | `1000` | Debounce window for detecting external DB edits. |
| `--attr-ttl-ms` | `1000` | How long the kernel may trust cached entry/attr lookups before re-validating. Higher cuts `lookup`/`getattr` traffic; bounds how fast external edits become visible. |
| `--max-readahead-kib` | `512` | Kernel read-ahead window. Larger hides HDD/NFS latency during sequential playback. |
| `--max-background` | `64` | Max outstanding background (read-ahead/async) requests the kernel keeps in flight. |
| `--keep-cache` | off | Keep the kernel page cache across opens. External re-tags auto-invalidate the affected inodes on refresh, so cached bytes are dropped when content changes. |

## Project layout

A layered Cargo workspace:

| Crate           | Responsibility                                              |
| --------------- | ----------------------------------------------------------- |
| `musefs-db`     | SQLite store: schema, migrations, tracks/tags/art access    |
| `musefs-format` | FLAC/MP3/MP4/Ogg/WAV byte surgery: metadata synthesis + layout |
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

The FUSE end-to-end tests perform real mounts and are `#[ignore]`d by default; run
them with `--ignored` on a host that has `/dev/fuse`.

A pre-commit hook (`.githooks/pre-commit`) runs `cargo fmt --check` and
`cargo clippy --all-targets -- -D warnings`. The lint policy — `clippy::pedantic`
minus a few intentional/noisy groups — lives in the root `Cargo.toml` under
`[workspace.lints]`. Enable the hook in a fresh clone with:

```bash
git config core.hooksPath .githooks
```

## License

Licensed under the [MIT License](LICENSE).
