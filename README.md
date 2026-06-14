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
- **Lossless-by-construction experimentation.** Change your tags, try a different
  organization scheme, new cover art — the originals are physically
  read-only to the mount. Backing up a current library is as easy as copying the db file.
- **Hash-stable by construction.** The mount never rewrites a byte, so each
  backing file's checksum is exactly what it was the day it arrived — anything
  verified by hash keeps verifying, and anything you're seeding keeps seeding,
  however aggressively you retag and reorganize the view on top.

> [!NOTE]
> This project was built with AI. The general workflow was to use the [superpowers](https://github.com/obra/superpowers) skills to provide a framework. Claude Opus was used to write plans and specs which were then implemented by another model, primarily MiMo v2.5.
>
> One of my goals in building this project was to "vibe code" something that was decisively not slop. I believe I've realized that objective and I hope that you take the project on its merits.
>
> If you disagree, please let me know! I'd love to know where I came up short so I can improve things. 

## Installing

Three ways to get musefs: a [prebuilt binary](#prebuilt-binaries) (no
toolchain needed), [building from source](#building-from-source), or a
[container image](#container-images).
Whichever you pick, mounting needs a 64-bit FUSE-capable OS (Linux, FreeBSD, macOS) — see
[Platform support](#platform-support).

> [!IMPORTANT]
> Linux and FreeBSD are E2E tested. I don't have anything running macOS to test on, if you run this on one let me know if it works, or especially if it doesn't!
>
> At present AMD64, AARCH64, and RISC-V 64 are supported. If you'd like 32-bit support please open an issue.

### Prebuilt binaries

Each tagged release attaches static/portable Linux binaries for six targets:

| Target | libc | Notes |
| --- | --- | --- |
| `x86_64-unknown-linux-gnu`  | glibc | Pinned to glibc 2.17 — runs on essentially any current distro. |
| `aarch64-unknown-linux-gnu` | glibc | glibc 2.17 floor, ARM64. |
| `x86_64-unknown-linux-musl`  | musl | Fully static — runs on Alpine / scratch containers. |
| `aarch64-unknown-linux-musl` | musl | Fully static, ARM64. |
| `riscv64gc-unknown-linux-gnu` | glibc | glibc 2.27 floor, RISC-V 64. |
| `riscv64gc-unknown-linux-musl` | musl | Fully static, RISC-V 64. |

The `*-musl` build is statically linked, so it runs on **any** Linux host of
that architecture regardless of libc — glibc distros (Debian/Ubuntu/Fedora)
included, not just Alpine/musl. For mixed or containerized deployments it is the
simplest choice: one binary you can drop onto a glibc host and an Alpine image
alike.

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

The binary uses **jemalloc** as its global allocator by default (it bounds
resident memory for the long-lived mount daemon under heavy concurrent reads).
Distribution packagers or anyone debugging memory with valgrind/heaptrack can
build against the system allocator instead with
`cargo build -p musefs --no-default-features` (or `cargo install musefs
--no-default-features`).

### Container images

Each tagged release also publishes multi-arch images to the GitHub Container
Registry:

| Image | libc | Platforms |
| --- | --- | --- |
| `ghcr.io/sohex/musefs:<version>`, `ghcr.io/sohex/musefs:latest` | glibc | amd64, arm64, riscv64 |
| `ghcr.io/sohex/musefs:<version>-musl`, `ghcr.io/sohex/musefs:musl` | musl | amd64, arm64, riscv64 |

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

> [!NOTE]
> The apparmor flag may or may not be necessary depending on how your system is configured.

Note that `CAP_SYS_ADMIN` is a broadly privileged capability — it grants far more
than FUSE mounting (mounting arbitrary filesystems, and more). It is unavoidable
for an in-container FUSE mount — even rootless Podman cannot drop it; without
`--cap-add SYS_ADMIN` the mount fails with `fusermount3: mount failed: Permission
denied`. Under rootless Podman the capability is confined to the container's user
namespace rather than the host, so its blast radius is smaller, but it is still
required. Running musefs on the host needs no such capability at all.

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
  `--allow-other` / `--owner` / `--group` mount (needed to share the mount across
  containers or users, below) passes musefs's pre-flight check. See
  [Ownership and permissions](#ownership-and-permissions).

#### The mount-visibility gotcha (read this before sharing the mount)

A FUSE mount made inside a container lives in that container's mount namespace.
By default neither the host nor other containers can see it, so pointing a second
container (your media manager) at musefs's output does not work out of the box.
To share the mount you propagate it between containers through a host directory:
musefs binds that directory with `rshared` and mounts itself there, and the
consumer binds the same directory with `rslave` so the mount propagates in. The
host directory must itself be a shared mount.

```bash
# A host directory both containers bind to, marked shared so mounts propagate.
mkdir -p /srv/musefs-mnt
mount --bind /srv/musefs-mnt /srv/musefs-mnt
mount --make-rshared /srv/musefs-mnt

# A named volume for the store, writable by the image's unprivileged user.
podman volume create musefs-store

# musefs container: bind rshared, mount musefs there with --allow-other.
podman run -d --name musefs \
  --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined \
  -v /path/to/library:/library:ro -v musefs-store:/store \
  --mount type=bind,source=/srv/musefs-mnt,destination=/mnt/musefs,bind-propagation=rshared \
  ghcr.io/sohex/musefs:latest mount /mnt/musefs --db /store/musefs.db --allow-other

# consumer container: bind the same host path rslave; the mount propagates in.
podman run -d --name player \
  --mount type=bind,source=/srv/musefs-mnt,destination=/music,bind-propagation=rslave \
  ghcr.io/sohex/yourmediamanager:latest
```

Use a named volume (or an already-writable host path) for the store: a bind from
a root-owned host directory is read-only to the image's unprivileged user and
musefs aborts before mounting. `--allow-other` is required because the consumer
container runs as a different uid than the musefs container; without it the
consumer gets `Permission denied` on the mount. See
[Ownership and permissions](#ownership-and-permissions).

> [!NOTE]
> Some hardened kernels block cross-uid access to an unprivileged user's FUSE
> mount even with `--allow-other` — for example when the fuse module's
> `allow_sys_admin_access` parameter is `N`, or unprivileged user namespaces are
> restricted. If the consumer still gets `Permission denied`, set
> `/sys/module/fuse/parameters/allow_sys_admin_access` to `Y`, or run musefs and
> the consumer under the same uid.

Both the glibc and musl images carry the `fuse3` userspace tools; pick `:musl`
if your other containers are Alpine-based, otherwise the default tags are fine.

#### Sharing a host mount into a container

Running musefs on the host instead of in a container is simpler and needs no
`CAP_SYS_ADMIN`. Mark the mount point as shared and mount musefs there with
`--allow-other`, then bind it into the consumer container with `rslave` so the
host's musefs mount propagates in:

```bash
# On the host: mark the mount point shared, then mount musefs with --allow-other.
mkdir -p /srv/musefs-mnt
mount --bind /srv/musefs-mnt /srv/musefs-mnt
mount --make-rshared /srv/musefs-mnt
musefs mount /srv/musefs-mnt --db /store/musefs.db --allow-other &

podman run -d \
  --mount type=bind,source=/srv/musefs-mnt,destination=/music,bind-propagation=rslave \
  ghcr.io/sohex/yourmediamanager:latest
# the container reads the re-tagged view at /music, byte-for-byte live
```

`rslave` is what keeps this working across restarts: a plain bind only captures
whatever is mounted when the container starts, so it shows an empty directory if
musefs mounts later and a stale view after a musefs restart.

### Platform support

| Platform | FUSE | Kernel passthrough (StructureOnly) | Notes |
| --- | --- | --- | --- |
| Linux | Yes (`/dev/fuse` + `fusermount3`, from the `fuse3` package) | Yes (6.9+, falls back to daemon serving otherwise) | Full support. |
| FreeBSD | Yes (pure-rust `/dev/fuse` backend; `fusefs` kernel module, no libfuse) | No | Full FUSE support. |
| macOS (FUSE-T) | Best-effort | No | Compiles and runs unit tests with `macos-no-mount`; mounted e2e is not yet validated. |

On platforms without kernel passthrough, `--mode structure-only` still serves
the original bytes, just through the daemon instead of the kernel.

## Usage

`musefs --version` (or `-V`) prints the build version; `--help` on the root or
any subcommand lists its flags.

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

`scan` and `scan --revalidate` show a live progress indicator: on an interactive
terminal, a discovery spinner followed by a determinate bar (position, percent,
ETA, current file); on a non-interactive stderr (piped or logged), throttled
`ingested N/M (P%)` lines. `--quiet` (`-q`) suppresses the progress indicator
and the per-target summary. Each summary line ends with the elapsed time.

The per-target summary reads `scanned N: … skipped X, failed Y`. `skipped`
counts every file that isn't a supported audio format — cover art, `.cue` /
`.log` / `.nfo` sidecars, and anything else non-audio — so a large `skipped`
number (hundreds or thousands on a big library) is expected, not an error.
A per-extension breakdown of the skip count is logged at end of scan (e.g.
`skipped 42: jpg=20, cue=10, log=8, <none>=4`), so you can tell expected
sidecars from anything genuinely unexpected. `failed` is the one
to watch: those are audio files musefs recognised by extension but could not
parse. Format dispatch is by **extension only** —
there is no content sniffing and no fallback to another parser, so a file
whose contents don't match its extension (e.g. a FLAC named `.mp3`) is handed
to the wrong parser, fails, and is counted here rather than retried. Renaming
files across formats makes them vanish from the mount; fix the extension and
rescan.

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

> **`mount` never creates the store** — unlike `scan`, it requires a populated
> DB to already exist and exits non-zero otherwise. Interactively this is
> invisible (the `scan` → `mount` quick start always seeds it first), but it
> bites automation: a `mount` started at boot before anything has scanned
> hard-fails (and crash-loops under `Restart=`). Seed the store with an initial
> `scan`, or order the mount after it — see
> [`contrib/systemd`](contrib/systemd/README.md).

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
- `--skip-on-missing` — drop a track from the mount entirely when a **top-level**
  template field stays unresolved, instead of substituting `--default-fallback`.
  Per-field `--fallback` chains and `[ … ]` sections are unaffected (a field
  resolved via its fallback counts as present, and section fields stay optional).
  Handy when an external tool tags only some tracks, e.g.
  `--template '$!{beets_path}' --skip-on-missing` hides tracks beets left without
  a `beets_path` (such as deduplicated albums).
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

The defaults are sensible for most setups. On slow or high-latency backing (NFS, remote,
HDD), the two flags worth knowing are `--read-ahead-budget-mib` — the daemon-level backing
read-ahead that hides per-read latency, the single biggest win for NFS/remote — and
`--keep-cache`. The *kernel*-level read-ahead / background knobs have little measurable
effect (see [BENCHMARKS.md](BENCHMARKS.md#storage-tunables) for the methodology and numbers).

| Flag | Default | What it does |
| ---- | ------- | ------------ |
| `--poll-interval-ms` | `1000` | Debounce window for detecting external DB edits. |
| `--read-ahead-budget-mib` | `64` | Per-mount RAM budget (MiB) for **backing read-ahead**: the daemon coalesces a stream's small FUSE reads into one large positioned read, so the backing client can pipeline/parallelize them. **The biggest lever for slow/high-latency backing** — ~5–6× single-stream throughput over a 200 ms-RTT NFS mount; neutral on local disk. Shared across all active streams with LRU eviction; `0` disables it. |
| `--read-ahead-prefetch` | disabled | Advanced: add background prefetch threads on top of read amplification. Off by default — benchmarks found amplification alone delivers the entire read-ahead win, while the threads add ~10% overhead with no measured benefit. Enable only when profiling a backend where a single large read does not self-pipeline. |
| `--keep-cache` | disabled | Keep the kernel page cache across opens. **Worth enabling on HDD/NFS:** repeat opens of a file are then served from cache instead of re-read over slow storage (~3× faster reopen in our benches). External re-tags auto-invalidate the affected files, so cached bytes never go stale. |
| `--attr-ttl-ms` | `1000` | How long the kernel may trust cached entry/attr lookups. Higher cuts `lookup`/`getattr` traffic — useful for metadata-heavy clients (library scanners) over high-latency backing — but bounds how fast external edits become visible. |
| `--max-readahead-kib` | `512` | *Kernel* read-ahead window (clamped to the kernel maximum). Distinct from `--read-ahead-budget-mib` (the daemon-level read-ahead, which is the effective one): this kernel knob does **not** speed up musefs streaming, since reads reach the daemon in fixed FUSE-sized chunks regardless. On HDD, values well above the default can even hurt. Leave at the default unless your own profiling shows otherwise. |
| `--max-background` | `64` | Max outstanding background (read-ahead/async) requests the kernel keeps in flight. Does **not** bound foreground reads (those scale with client concurrency), so it has little effect on read throughput; left for completeness. |
| `--case-insensitive <true\|false>` | OS default | Compare filenames case-insensitively. Case-variant directories merge into one (first-seen casing wins) and case-variant files get a numeric suffix (e.g. `Song (2)`). Defaults to `true` on macOS and `false` on Linux/FreeBSD; case-insensitive mounts refresh via a full rebuild rather than the incremental fast path. |

### Metrics

`musefs mount` optionally exposes runtime telemetry through a synthetic
`.musefs-metrics/` directory at the mount root:

```bash
musefs mount /mnt/music --db library.db --expose-metrics   # or: MUSEFS_EXPOSE_METRICS=1
cat /mnt/music/.musefs-metrics/metrics
```

```text
# HELP musefs_uptime_seconds Seconds since the mount started.
# TYPE musefs_uptime_seconds gauge
musefs_uptime_seconds 60
# HELP musefs_handles_open Open file handles in the core slab.
# TYPE musefs_handles_open gauge
musefs_handles_open 3
# HELP musefs_cache_header_hits_total Raw header-cache key hits; a hit may still trigger a content-version rebuild.
# TYPE musefs_cache_header_hits_total counter
musefs_cache_header_hits_total 100
```

`--expose-metrics` (default off) is a **runtime** flag that gates the virtual
file; it is unrelated to the compile-time `metrics` cargo feature, which adds
syscall counters (opens, preads, etc.) to the output. The jemalloc allocator
stats require a build with the `jemalloc` feature, which is the default.

The `metrics` file advertises `st_size == 0` (like `/proc`), so use an
EOF-aware reader — `cat`, `head -c`, or the Prometheus textfile collector —
not a stat-and-`read`-by-size approach.

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

**`--allow-other` grants other users — but not root.** A FUSE mount made with
`allow_other` (not `allow_root`) is reachable by other unprivileged users, yet
**root specifically cannot traverse or stat it** when it is owned by another
user. This surprises root-run tooling (Ansible, boot scripts):

- `mountpoint -q <mnt>` / `stat <mnt>` run as root report it as *not a
  mountpoint* — they try to stat *through* the mount and get EACCES. Detect the
  mount from root with `findmnt <mnt>` or `/proc/mounts` instead, which read the
  mount table rather than the filesystem.
- Don't have root manage the mountpoint **directory** while it is mounted: a
  root task that re-asserts the directory (e.g. Ansible `file: state=directory`)
  fails with EACCES/EEXIST on every run after the first. Create the directory
  before mounting, or run such tasks as the mounting user.

### Configuring with environment variables

Every scalar `mount` and `scan` flag can also be set with a `MUSEFS_*`
environment variable — uppercase the long flag and turn dashes into
underscores (e.g. `--poll-interval-ms` → `MUSEFS_POLL_INTERVAL_MS`, the
`mount` mountpoint → `MUSEFS_MOUNTPOINT`). An explicit flag always overrides
its env var, which overrides the default. Boolean flags (`MUSEFS_KEEP_CACHE`,
`MUSEFS_REVALIDATE`, `MUSEFS_FOLLOW_SYMLINKS`, `MUSEFS_QUIET`,
`MUSEFS_ALLOW_OTHER`, `MUSEFS_CASE_INSENSITIVE`, `MUSEFS_EXPOSE_METRICS`) accept a case-insensitive
boolish value — `true`/`false`, `yes`/`no`, `on`/`off`, `1`/`0` — and reject
anything else. The repeatable `--fallback` and the
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
