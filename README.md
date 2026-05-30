# musefs

[![CI](https://github.com/Sohex/musefs/actions/workflows/ci.yml/badge.svg)](https://github.com/Sohex/musefs/actions/workflows/ci.yml)

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

## Tag handling

musefs preserves the tags it reads from a backing file when it synthesizes the
served file (always in the same format — it never converts between formats).

**Round-trips losslessly:**

- All text tags. Common fields use a shared canonical vocabulary (so
  `$albumartist`, `$date`, etc. work the same regardless of source format);
  everything else round-trips through the format's extension slot — ID3 `TXXX`,
  MP4 `----` freeform, or a raw Vorbis field — keyed by its own name. Unmapped
  standard ID3 text frames round-trip by their frame id.
- Comments and lyrics (text content).
- User-defined keys keep their original casing (e.g. `MusicBrainz Album Id`).

**Known limitations (lossy edges):**

- All ID3v2.x tags are normalized to **ID3v2.4** on synthesis. Legacy date
  frames (`TYER`, `TDAT`) fold to `date` and are re-emitted as `TDRC`.
- ID3 `COMM`/`USLT` language code and short description are not preserved; they
  are written back with language `XXX` and an empty description. Multiple
  comments/lyrics distinguished only by those collapse to one.
- MP4 `----` `mean` is normalized to `com.apple.iTunes` on write.
- Binary / extended frames are **not** round-tripped and are dropped on scan:
  ID3 `POPM` (ratings), `UFID`, and other non-text frames; MP4 binary atoms
  beyond `trkn`/`disk` (e.g. `tmpo`, `cpil`). Embedded cover art is handled by a
  separate dedicated path, not the tag path.
- Multi-value MP4 `----` freeform tags round-trip only their first value.
- If several source tags map to one canonical key (e.g. a `TXXX` whose
  description is `comment` alongside a real `COMM` frame), they merge into a
  single multi-value tag and are re-emitted via that key's native slot.

## Requirements

- Rust (2021 edition) and Cargo.
- Linux with FUSE (`/dev/fuse` and libfuse) to mount.

## Install

Install the `musefs` binary from crates.io:

```bash
cargo install musefs
```

`cargo install` compiles from source, so the same prerequisites as a local
build apply: a Rust toolchain plus FUSE (`libfuse3` / `libfuse3-dev`) and
`pkg-config` on Linux.

Or install the latest from the repository:

```bash
cargo install --git https://github.com/Sohex/musefs musefs
```

## Build

```bash
cargo build --release
```

The binary is `musefs` (the `musefs` crate).

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

### Fuzzing & property tests

Property-based tests (`proptest`) assert the byte-identical audio invariant and
tag round-trips, and run as part of `cargo test`. Coverage-guided fuzzing
(`cargo-fuzz`, requires a nightly toolchain) hammers every format parser and the
byte-level primitives for panics, hangs, and OOM:

```bash
cargo test -p musefs-format --features fuzzing   # format-layer property tests
cargo install cargo-fuzz                         # one-time
cargo +nightly fuzz run flac                     # or mp3|mp4|ogg|wav|ogg_page|b64|vorbiscomment
cargo +nightly fuzz coverage flac                # confirm coverage reaches the parser
```

An independent-reader interop test confirms the wider ecosystem (`mutagen`) reads
the tags musefs synthesizes, across all five formats:

```bash
pip install -r tests/interop/requirements.txt
MUSEFS_INTEROP_DIR=/tmp/i cargo test -p musefs-core --test interop_emit -- --ignored emit_interop_fixtures
MUSEFS_INTEROP_DIR=/tmp/i python -m pytest tests/interop
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

## Limitations

### MP4/M4A Cover Art

MP4/M4A synthesis embeds only the first cover-art input when multiple images
are available. This is an intentional current limitation: the MP4 container
format stores cover art in the `covr` metadata atom, which maps to a single
`data` item. If multiple cover images are present in the database, only the
first (earliest `track_art.ordinal`) is embedded.

Tests lock this behavior — see `tests/mp4_oracle.rs`. Future work may
support multiple covers via additional metadata atoms or a gallery layout.

## License

Licensed under the [MIT License](LICENSE).
