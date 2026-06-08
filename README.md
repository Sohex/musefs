# musefs

[![CI](https://github.com/Sohex/musefs/actions/workflows/ci.yml/badge.svg)](https://github.com/Sohex/musefs/actions/workflows/ci.yml)

A read-only FUSE filesystem that presents a re-tagged, reorganized view of
your music library — without modifying or duplicating a single byte of the
original audio. Fix tags, art, and folder structure in a SQLite store; the
mount shows a clean library while your files stay exactly as they are.

## Quick start

```bash
cargo install musefs    # compiles from source: needs a Rust toolchain,
                        # libfuse3-dev and pkg-config — see Requirements

musefs scan ~/Music --db library.db        # ingest your library
mkdir -p ~/mnt/music
musefs mount ~/mnt/music --db library.db \
    --template '$albumartist/$album/$title'
# mount blocks until unmounted: fusermount -u ~/mnt/music (or Ctrl-C)
```

`~/mnt/music` now serves your library as
`Album Artist/Album/Title.flac` — with each file's metadata generated fresh
from the database, spliced in front of your original, untouched audio.

## What it's for

- **A clean view of a messy library.** Your files keep their on-disk chaos;
  the mount presents one consistent, template-driven tree for players and
  media managers.
- **Tag editing without touching files.** Edit the SQLite store (directly,
  or via the [beets plugin](contrib/beets/README.md) or
  [Picard plugin](contrib/picard/README.md)) and the mounted view updates
  live — no remount, no rewrite, no re-rip anxiety.
- **Lossless-by-construction experimentation.** Try a retag, a different
  organization scheme, new cover art — the originals are physically
  read-only to the mount.

## Usage

### Scan

```bash
musefs scan /path/to/music --db library.db            # ingest (dirs recurse)
musefs scan /path/to/music --db library.db --revalidate
```

`scan` probes each audio file (FLAC, MP3, M4A/M4B, Ogg, WAV), recording its
audio byte range, tags, and embedded art in the store. It takes one or more
files or directories, and `--jobs N` controls probe parallelism. `--quiet`
(`-q`) suppresses the per-target summary for scripting; scan failures still
surface on stderr (raise detail with `RUST_LOG=info`).
`--revalidate` is the maintenance pass: it skips unchanged files —
**preserving any tag edits you made in the store** — prunes tracks whose
backing file is gone, and garbage-collects orphaned art.

### Mount

```bash
musefs mount /path/to/mountpoint --db library.db \
    --template '$albumartist/$album/$title' \
    --default-fallback Unknown \
    --mode synthesis        # or: structure-only
```

`mount` blocks until the filesystem is unmounted (`fusermount -u`, or
Ctrl-C). Paths come from a beets-style template (matched case-insensitively;
any tag key in the store works):

- `$field` / `${field}` — substitute a tag field (e.g. `$artist`, `$album`,
  `$title`, `$tracknumber`, `$date`, `$genre`).
- `${albumartist|artist}` — **fallback chain**: the first present field wins,
  before the `--default-fallback` value (default `Unknown`) is used.
- `[ … ]` — **conditional section**: the bracketed text is emitted only when at
  least one field inside it is present. So `$album[ - CD $disc]` yields
  `Album - CD 2`, or just `Album` on a single-disc release. Write `$[` / `$]`
  for literal brackets.
- `$!{field}` — **path field**: the value's `/` are kept as directory
  separators (each segment sanitized; empty/`.`/`..` dropped). Lets an external
  tool precompute a whole relative path into one tag and mount it as
  `--template '$!{beets_path}'`.

Anything else is literal. Name collisions get a deterministic `(2)`, `(3)`, …
suffix. Every rendered component is capped at 255 bytes (NAME_MAX, truncated on
a UTF-8 boundary, extension preserved), and a plain field whose value is
exactly `.` or `..` is dropped rather than creating an unusable directory. The
default template is `$artist/$title`.

Two modes:

- **`synthesis`** (default) — files carry metadata freshly generated from
  the store, spliced ahead of the original audio bytes.
- **`structure-only`** — files are served byte-for-byte as they are on disk;
  only the directory tree is virtual.

Edit tags or art in the database while mounted (another `scan`, a
beets/Picard sync, raw SQL) and the view refreshes automatically.

Run `musefs <command> --help` for the full flag list.

### Tuning

All tuning flags have sensible defaults; adjust them to your backing store:

| Flag | Default | What it does |
| ---- | ------- | ------------ |
| `--poll-interval-ms` | `1000` | Debounce window for detecting external DB edits. |
| `--attr-ttl-ms` | `1000` | How long the kernel may trust cached entry/attr lookups. Higher cuts `lookup`/`getattr` traffic; bounds how fast external edits become visible. |
| `--max-readahead-kib` | `512` | Kernel read-ahead window. Larger hides HDD/NFS latency during sequential playback (clamped to the kernel maximum). |
| `--max-background` | `64` | Max outstanding background (read-ahead/async) requests the kernel keeps in flight. |
| `--keep-cache` | disabled | Keep the kernel page cache across opens. External re-tags auto-invalidate the affected files, so cached bytes never go stale. |
| `--case-insensitive <true\|false>` | OS default | Compare filenames case-insensitively. Case-variant directories merge into one (first-seen casing wins) and case-variant files get a numeric suffix (e.g. `Song (2)`). Defaults to `true` on macOS and `false` on Linux/FreeBSD; case-insensitive mounts refresh via a full rebuild rather than the incremental fast path. |

## Supported formats

| Format | Extensions | What synthesis does | Details |
| ------ | ---------- | ------------------- | ------- |
| FLAC | `.flac` | Regenerates the metadata blocks; preserves `STREAMINFO`/`SEEKTABLE` bit-exact | [docs/FLAC.md](docs/FLAC.md) |
| MP3 | `.mp3` | Regenerates the ID3v2.4 tag; the audio frames (incl. Xing/LAME) are untouched | [docs/MP3.md](docs/MP3.md) |
| M4A | `.m4a`, `.m4b` | Rebuilds the `moov` atom, patching chunk offsets; `mdat` served verbatim | [docs/M4A.md](docs/M4A.md) |
| Ogg | `.ogg`, `.oga`, `.opus` | Regenerates header pages (Opus/Vorbis/FLAC-in-Ogg); audio pages served verbatim, only page seq numbers/CRCs patched in place | [docs/OGG.md](docs/OGG.md) |
| WAV | `.wav` | Regenerates the RIFF front (`LIST`/`INFO` + embedded ID3v2); `data` payload verbatim | [docs/WAV.md](docs/WAV.md) |

Text tags round-trip losslessly through a shared canonical vocabulary (so
`$albumartist`, `$date`, etc. work the same regardless of source format),
and binary metadata (ratings, embedded blocks, opaque frames) is preserved
where the format allows. Each format has a handful of well-defined lossy
edges — see its doc for the exact list.

## FAQ

**Does musefs ever write to my audio files?**
No. The mount is read-only and the scanner only reads. The served files are
assembled on the fly: generated metadata plus positioned reads of your
originals. Nothing is ever copied or rewritten.

**Where do my edited tags live?**
In the SQLite store (`--db`). Edit it with the
[beets](contrib/beets/README.md) or [Picard](contrib/picard/README.md)
plugins, or with plain SQL — the schema is a documented, stable contract
(see [ARCHITECTURE.md](ARCHITECTURE.md#the-sqlite-store)).

**Do edits show up without remounting?**
Yes. The mount polls the database (debounced) and picks up external commits
automatically, with stable inodes across refreshes — even files held open
keep working.

**Can I write through the mount?**
No — and it's not planned. Out-of-band editing against the store *is* the
design: it's what guarantees your originals can never be corrupted.

**What platforms?**
See [Platform support](#platform-support).

**Is it fast enough for a big library on a NAS?**
That's the design target: synthesized headers are cached, blocking reads run
on a worker pool so a slow disk never stalls the filesystem, and read-ahead,
cache TTLs, and poll intervals are all [tunable](#tuning). In
`structure-only` mode on kernel 6.9+, reads can bypass the daemon entirely
via FUSE passthrough (needs `CAP_SYS_ADMIN`).

**A file in the mount won't open / reads error — why?**
The most common cause is a backing file that changed since its last scan
(musefs refuses to serve a file whose size or mtime drifted, rather than
splice at stale offsets). Run `musefs scan --revalidate` to re-probe it.

## Requirements

- Rust (2024 edition) and Cargo to build/install.
- A supported OS with FUSE to mount — Linux (`/dev/fuse` + libfuse) or FreeBSD
  (`/dev/fuse` + the `fusefs` kernel module; no libfuse). macOS (FUSE-T) is
  best-effort. See [Platform support](#platform-support) for details.

## Platform support

| Platform | FUSE | Kernel passthrough (StructureOnly) | Notes |
| --- | --- | --- | --- |
| Linux | Yes | Yes (6.9+, falls back to daemon serving otherwise) | Full support. |
| FreeBSD | Yes (pure-rust `/dev/fuse` backend; `fusefs` kernel module) | No | Full FUSE support. |
| macOS (FUSE-T) | Best-effort | No | Compiles and runs unit tests with `macos-no-mount`; mounted e2e is not yet validated. |

On platforms without kernel passthrough, `--mode structure-only` still serves
the original bytes, just through the daemon instead of the kernel.

`cargo install` compiles from source, so the same prerequisites as a local
build apply: a Rust toolchain plus FUSE headers (`libfuse3-dev`) and
`pkg-config`. To install the latest development version:

```bash
cargo install --git https://github.com/Sohex/musefs musefs
```

## Status

All five formats ship with embedded cover art and binary-tag preservation.
The serve path has been through a performance/concurrency hardening pass for
real-world player and media-manager access against large libraries on
HDD/SSD/NFS, and the parsers are continuously fuzzed. beets and Picard
plugins ship in [`contrib/`](contrib/). See the
[CHANGELOG](CHANGELOG.md) for history.

Deeper reading: [ARCHITECTURE.md](ARCHITECTURE.md) for how it works,
[CONTRIBUTING.md](CONTRIBUTING.md) for the development workflow.

## License

Licensed under the [MIT License](LICENSE).
