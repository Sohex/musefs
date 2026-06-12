<p align="center">
  <img src="assets/banner.svg" alt="musefs — tag your library without duplicating a single byte" width="100%">
</p>

# musefs

[![CI](https://github.com/Sohex/musefs/actions/workflows/ci.yml/badge.svg)](https://github.com/Sohex/musefs/actions/workflows/ci.yml)
[![Coverage](https://codecov.io/gh/Sohex/musefs/branch/main/graph/badge.svg)](https://codecov.io/gh/Sohex/musefs)
[![Release](https://img.shields.io/github/v/release/Sohex/musefs?sort=semver)](https://github.com/Sohex/musefs/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

A read-only FUSE filesystem that presents a re-tagged, reorganized view of
your music library — without modifying or duplicating a single byte of the
original audio. Fix tags, art, and folder structure in a SQLite store; the
mount shows a clean library while your files stay exactly as they are.

## Quick start

```bash
cargo install musefs    # compiles from source — needs a Rust toolchain,
                        # libfuse3-dev and pkg-config; prebuilt binaries
                        # and container images: see Installing

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
  or via the [beets plugin](contrib/beets/README.md),
  [Picard plugin](contrib/picard/README.md), or
  [Lidarr integration](contrib/lidarr/README.md)) and the mounted view
  updates live — no remount, no rewrite, no re-rip anxiety.
- **Lossless-by-construction experimentation.** Try a retag, a different
  organization scheme, new cover art — the originals are physically
  read-only to the mount.

## Installing

Three ways to get musefs: a [prebuilt binary](#prebuilt-binaries) (no
toolchain needed), [building from source](#building-from-source), or a
[container image](#container-images).
Whichever you pick, mounting needs a FUSE-capable OS — see
[Platform support](#platform-support).

### Prebuilt binaries

Each tagged release attaches static/portable Linux binaries for four targets:

| Target | libc | Notes |
| --- | --- | --- |
| `x86_64-unknown-linux-gnu`  | glibc | Pinned to glibc 2.17 — runs on essentially any current distro. |
| `aarch64-unknown-linux-gnu` | glibc | glibc 2.17 floor, ARM64. |
| `x86_64-unknown-linux-musl`  | musl | Fully static — runs on Alpine / scratch containers. |
| `aarch64-unknown-linux-musl` | musl | Fully static, ARM64. |

Download the tarball for your target from the
[latest release](https://github.com/Sohex/musefs/releases/latest), verify it,
and extract:

```bash
sha256sum -c musefs-<version>-<target>.tar.gz.sha256
tar -xzf musefs-<version>-<target>.tar.gz   # yields ./musefs
```

**Runtime requirements:** the binaries mount via FUSE's `fusermount3` helper, so
the target needs the FUSE userspace tools and `/dev/fuse`:

- Debian/Ubuntu: `apt-get install fuse3`
- Alpine: `apk add fuse3`

No glibc/libfuse install is needed for the musl binaries beyond `fuse3`.

### Building from source

`cargo install musefs` compiles the latest release; building needs a stable
Rust toolchain (2024 edition) plus the FUSE headers (`libfuse3-dev`) and
`pkg-config`. To install the latest development version instead:

```bash
cargo install --git https://github.com/Sohex/musefs musefs
```

The same `fuse3` runtime requirement as the prebuilt binaries applies.

### Container images

Each tagged release also publishes multi-arch images to the GitHub Container
Registry:

| Image | libc | Platforms |
| --- | --- | --- |
| `ghcr.io/sohex/musefs:<version>`, `ghcr.io/sohex/musefs:latest` | glibc | amd64, arm64 |
| `ghcr.io/sohex/musefs:<version>-musl`, `ghcr.io/sohex/musefs:musl` | musl | amd64, arm64 |

`docker pull` selects the CPU architecture automatically. Use the `-musl` /
`:musl` tags when slotting musefs into an Alpine-based stack; the default
(glibc) tags suit everything else. Floating `:latest` / `:musl` track the most
recent stable release only — prereleases publish only version-pinned tags.

**Running musefs on the host is the simplest, best-supported option** — it is an
ordinary FUSE daemon and the image exists mainly to colocate musefs with
containerized media managers (e.g. Lidarr). If you do containerize, mind the
gotchas below.

#### Required flags

musefs mounts via FUSE, so the container needs `/dev/fuse` and the matching
capability:

```bash
docker run --rm \
  --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined \
  -v /path/to/library:/library:ro \
  -v /path/to/store:/store \
  ghcr.io/sohex/musefs:latest scan /library --db /store/musefs.db
```

Without `--device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined`
the mount cannot be established.

Note that `CAP_SYS_ADMIN` is a broadly privileged capability — it grants far more
than FUSE mounting (mounting arbitrary filesystems, and more). It is unavoidable
for an in-container FUSE mount, but it is another reason to prefer running musefs
on the host, which needs no such capability.

#### Runs as a non-root user

The images run as a dedicated unprivileged user (default uid/gid 1000), not
root — musefs mounts via the setuid `fusermount3` helper and needs no root of
its own. Consequences for the commands above:

- The bind-mounted **store** volume must be writable by that uid. Either
  `chown 1000:1000 /path/to/store` on the host, or add `--user $(id -u):$(id -g)`
  to run as your own uid. The **library** volume is mounted `:ro`, so its
  ownership does not matter.
- To bake an image whose user matches your host account (so no `chown` or
  `--user` is needed), build from source with
  `--build-arg MUSEFS_UID=$(id -u) --build-arg MUSEFS_GID=$(id -g)`.
- The images include `user_allow_other` in `/etc/fuse.conf`, so a non-root
  `--allow-other` / `--owner` / `--group` mount (used by the pod pattern below)
  passes musefs's pre-flight check. See
  [Ownership and permissions](#ownership-and-permissions).

#### The mount-visibility gotcha (read this before sharing the mount)

A FUSE mount made **inside** a container lives in that container's mount
namespace. By default neither the host nor other containers can see it, so
pointing a second container (your media manager) at musefs's output does not
work out of the box. Two ways to share it:

- **Prefer Podman in a shared pod** — Docker has no first-class pod primitive,
  but Podman does, so this is the cleanest path: put musefs and the consumer in
  the same pod so they share namespaces and the mount is directly reachable:

  ```bash
  podman pod create --name media
  podman run -d --pod media \
    --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined \
    -v /path/to/library:/library:ro -v /path/to/store:/store \
    ghcr.io/sohex/musefs:latest mount /mnt/musefs --db /store/musefs.db
  # the consumer container, in the same pod, reads /mnt/musefs
  ```

- Or bind-mount the mount point with **`rshared` propagation** so the mount
  escapes the container's namespace to the host (`--mount type=bind,...,bind-propagation=rshared`).
  This is fiddlier than a shared pod, which is why the pod approach is
  recommended.

Both the glibc and musl images carry the `fuse3` userspace tools; pick `:musl`
if your other containers are Alpine-based, otherwise the default tags are fine.

### Platform support

| Platform | FUSE | Kernel passthrough (StructureOnly) | Notes |
| --- | --- | --- | --- |
| Linux | Yes (`/dev/fuse` + `fusermount3`, from the `fuse3` package) | Yes (6.9+, falls back to daemon serving otherwise) | Full support. |
| FreeBSD | Yes (pure-rust `/dev/fuse` backend; `fusefs` kernel module, no libfuse) | No | Full FUSE support. |
| macOS (FUSE-T) | Best-effort | No | Compiles and runs unit tests with `macos-no-mount`; mounted e2e is not yet validated. |

On platforms without kernel passthrough, `--mode structure-only` still serves
the original bytes, just through the daemon instead of the kernel.

## Usage

### Scan

```bash
musefs scan /path/to/music --db library.db            # ingest (dirs recurse)
musefs scan /path/to/music --db library.db --revalidate
```

`scan` probes each audio file (FLAC, MP3, M4A/M4B, Ogg, WAV), recording its
audio byte range, tags, and embedded art in the store. It takes one or more
files or directories, and `--jobs N` controls probe parallelism.
`--follow-symlinks` walks symlinked files and directories (off by default, so
symlinks are logged and skipped). `--quiet`
(`-q`) suppresses the per-target summary for scripting; scan failures still
surface on stderr (raise detail with `RUST_LOG=info`).

The per-target summary reads `scanned N: … skipped X, failed Y`. `skipped`
counts every file that isn't a supported audio format — cover art, `.cue` /
`.log` / `.nfo` sidecars, and anything else non-audio — so a large `skipped`
number (hundreds or thousands on a big library) is expected, not an error.
`failed` is the one to watch: those are audio files musefs recognised by
extension but could not parse.

`--revalidate` is the maintenance pass: it skips unchanged files —
**preserving any tag edits you made in the store** — prunes tracks whose
backing file is gone, and garbage-collects orphaned art.

### Mount

```bash
musefs mount /path/to/mountpoint --db library.db \
    --template '$albumartist/$album/$title' \
    --default-fallback Unknown \
    --fallback albumartist='Unknown Artist' \
    --mode synthesis        # or: structure-only
```

`mount` blocks until the filesystem is unmounted (`fusermount -u`, or
Ctrl-C).

> **Mounting at an arbitrary path may be denied by AppArmor.** On distros that
> ship an AppArmor profile for `fusermount3` (Ubuntu 24.04+ / libfuse ≥ 3.17),
> unprivileged FUSE mounts are only allowed when the mountpoint is under a
> whitelisted prefix — the shipped profile permits `$HOME/**`, `/mnt`, `/media`,
> `/tmp`, `/cvmfs`, `$XDG_RUNTIME_DIR`, plus flatpak dirs. Mounting elsewhere
> (e.g. a data volume at `/data/...`) fails with `fusermount3: mount failed:
> Permission denied`, and the kernel audit log shows
> `apparmor="DENIED" operation="mount" … profile="fusermount3"`. The mountpoint's
> own ownership is irrelevant — AppArmor rejects the `mount()` syscall first. Fix
> it by mounting under a permitted prefix, or by whitelisting your prefix in
> `/etc/apparmor.d/local/fusermount3` (the shipped profile ends with
> `include if exists <local/fusermount3>`).

Two modes:

- **`synthesis`** (default) — files carry metadata freshly generated from
  the store, spliced ahead of the original audio bytes.
- **`structure-only`** — files are served byte-for-byte as they are on disk;
  only the directory tree is virtual.

Edit tags or art in the database while mounted (another `scan`, a
beets/Picard/Lidarr sync, raw SQL) and the view refreshes automatically.

Run `musefs <command> --help` for the full flag list.

#### Path templates

Paths come from a beets-style template (matched case-insensitively;
any tag key in the store works):

- `$field` / `${field}` — substitute a tag field (e.g. `$artist`, `$album`,
  `$title`, `$tracknumber`, `$date`, `$genre`).
- `${albumartist|artist}` — **fallback chain**: the first present field wins,
  before the `--default-fallback` value (default `Unknown`) is used.
- A missing field resolves in order: the field's value, then a **per-field
  fallback** from `--fallback FIELD=VALUE` (repeatable, e.g. `--fallback
  albumartist='Unknown Artist'`), then `--default-fallback`. Per-field
  fallbacks let one field default differently from the rest.
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
default template is `$albumartist/$album/$title`.

### Tuning

The defaults are sensible for most setups. On slow backing storage (HDD, NFS) the one
flag worth changing is `--keep-cache`; the read-ahead / background knobs have little
measurable effect on musefs (see [BENCHMARKS.md](BENCHMARKS.md#storage-tunables)
for the methodology and numbers).

| Flag | Default | What it does |
| ---- | ------- | ------------ |
| `--poll-interval-ms` | `1000` | Debounce window for detecting external DB edits. |
| `--keep-cache` | disabled | Keep the kernel page cache across opens. **Worth enabling on HDD/NFS:** repeat opens of a file are then served from cache instead of re-read over slow storage (~3× faster reopen in our benches). External re-tags auto-invalidate the affected files, so cached bytes never go stale. |
| `--attr-ttl-ms` | `1000` | How long the kernel may trust cached entry/attr lookups. Higher cuts `lookup`/`getattr` traffic — useful for metadata-heavy clients (library scanners) over high-latency backing — but bounds how fast external edits become visible. |
| `--max-readahead-kib` | `512` | Kernel read-ahead window (clamped to the kernel maximum). In practice this does **not** speed up musefs streaming: reads reach the daemon in fixed FUSE-sized chunks and a single stream is served serially, so a larger window doesn't reduce per-read latency. On HDD, values well above the default can even hurt. Leave at the default unless your own profiling shows otherwise. |
| `--max-background` | `64` | Max outstanding background (read-ahead/async) requests the kernel keeps in flight. Does **not** bound foreground reads (those scale with client concurrency), so it has little effect on read throughput; left for completeness. |
| `--case-insensitive <true\|false>` | OS default | Compare filenames case-insensitively. Case-variant directories merge into one (first-seen casing wins) and case-variant files get a numeric suffix (e.g. `Song (2)`). Defaults to `true` on macOS and `false` on Linux/FreeBSD; case-insensitive mounts refresh via a full rebuild rather than the incremental fast path. |

### Ownership and permissions

By default the mount presents the launching process's uid/gid and read-only
permission bits (`555` dirs, `444` files), and is reachable only by the user who
performed the mount (and root).

To present a different owner — e.g. a media-server service account — and let that
account actually reach the mount, pass `--owner`/`--group` (or `--allow-other`).
Either makes musefs mount with `allow_other` and `default_permissions`: other
users can traverse the mount, and the kernel enforces the presented owner/mode
bits instead of ignoring them.

| Flag | Default | What it does |
| ---- | ------- | ------------ |
| `--owner <NAME\|UID>` | process uid | User presented as the owner of every entry. Accepts a username or a numeric uid. Implies `--allow-other`. |
| `--group <NAME\|GID>` | process gid | Group presented for every entry. Accepts a group name or a numeric gid. Implies `--allow-other`. |
| `--allow-other` | off | Mount with `allow_other` + `default_permissions` so accounts other than the mounting user can reach the mount and the owner/mode bits are enforced. Implied by `--owner`/`--group`. |
| `--file-mode <OCTAL>` | `444` | Permission bits for regular files, in octal. The mount is read-only, so write bits are advertised but writes still fail with `EROFS`. |
| `--dir-mode <OCTAL>` | `555` | Permission bits for directories, in octal. |

The default `444`/`555` bits are world-readable, so any account can read once
`allow_other` is on. To restrict the mount to the presented owner/group, drop the
world bits (e.g. `--file-mode 440 --dir-mode 550`) — only then does
`--owner`/`--group` gate access rather than merely label it.

**Non-root mounts need `user_allow_other`.** When you are not root, libfuse
refuses an `allow_other` mount unless `/etc/fuse.conf` contains a line
`user_allow_other`. musefs checks this before mounting and fails with an
explanatory error if it is missing; add the line to `/etc/fuse.conf`, or run
musefs as root. (This is libfuse/system policy, not a musefs restriction.) The
published container images already include this line, so non-root `allow_other`
mounts work out of the box there.

### Configuring with environment variables

Every scalar `mount` and `scan` flag can also be set with a `MUSEFS_*`
environment variable — uppercase the long flag and turn dashes into
underscores (e.g. `--poll-interval-ms` → `MUSEFS_POLL_INTERVAL_MS`, the
`mount` mountpoint → `MUSEFS_MOUNTPOINT`). An explicit flag always overrides
its env var, which overrides the default. The repeatable `--fallback` and the
`scan` targets are command-line only. See
[`contrib/systemd/musefs.conf.example`](contrib/systemd/musefs.conf.example)
for the full, canonical list.

These variables are read the same way no matter how musefs is launched:
exported into the shell before running the binary directly
(`MUSEFS_DB=… musefs mount`), set via a systemd `EnvironmentFile=` or
`Environment=` directive, or passed into a container with `-e`/`--env-file`.
The configuration surface is identical across all three; the sections below
just show the per-deployment wiring.

### Running as a systemd user service

To run musefs on the host at login, drop-in units live in
[`contrib/systemd/`](contrib/systemd/): a `musefs.service` mount daemon, an
optional `musefs-scan.timer` for periodic re-scans, and a commented
`musefs.conf.example` holding every `MUSEFS_*` setting. Copy the units to
`~/.config/systemd/user/`, copy the config to `~/.config/musefs/musefs.conf`,
edit `MUSEFS_MOUNTPOINT` and `MUSEFS_DB`, then
`systemctl --user enable --now musefs.service`. See
[`contrib/systemd/README.md`](contrib/systemd/README.md) for the full walkthrough
and the `PATH` / linger gotchas.

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
plugins, the [Lidarr](contrib/lidarr/README.md) integration, or with plain
SQL — the schema is a documented, stable contract
(see [ARCHITECTURE.md](ARCHITECTURE.md#the-sqlite-store)).

**Do edits show up without remounting?**
Yes. The mount polls the database (debounced) and picks up external commits
automatically, with stable inodes across refreshes — even files held open
keep working.

**Can I write through the mount?**
No — and it's not planned. Out-of-band editing against the store *is* the
design: it's what guarantees your originals can never be corrupted.

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

## Status

All five formats ship with embedded cover art and binary-tag preservation.
The serve path has been through a performance/concurrency hardening pass for
real-world player and media-manager access against large libraries on
HDD/SSD/NFS, and the parsers are continuously fuzzed. beets, Picard, and
Lidarr plugins ship in [`contrib/`](contrib/). See the
[CHANGELOG](CHANGELOG.md) for history.

Deeper reading: [ARCHITECTURE.md](ARCHITECTURE.md) for how it works,
[CONTRIBUTING.md](CONTRIBUTING.md) for the development workflow.

## License

Licensed under the [MIT License](LICENSE).

## Acknowledgements

The Lidarr real-instance end-to-end test plays a real album through a real
Lidarr. With thanks to **[Komiku](https://loyaltyfreakmusic.com/)** (Loyalty
Freak Music), whose track *"The calling"* (from *The Adventure Goes On, Vol. 1*)
is dedicated to the public domain under [CC0 1.0](https://creativecommons.org/publicdomain/zero/1.0/)
and vendored as that test's fixture — thank you for releasing music freely.
