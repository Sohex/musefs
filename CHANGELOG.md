# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> The `contrib/` Python packages have their own decoupled version and changelog:
> see [contrib/CHANGELOG.md](contrib/CHANGELOG.md).

> The full, detailed changelog (including internal changes) lives in the
> documentation site: <https://sohex.github.io/musefs/changelog.html>.

## [Unreleased]

### Added

- `musefs_dir_handle_rejections_total` counts `opendir` calls that could not be
  given a cached directory snapshot, so directory-handle pressure stays visible
  after a burst rather than only as a gauge that reads healthy between samples.
- `musefs_serve_warns_suppressed_total` counts the serve-path failure warnings
  the rate limiter downgraded to `debug`, so log throttling is visible to
  anything scraping metrics. Without it the count escaped only as prose inside
  the next warning that was admitted, and an operator could not tell a quiet
  serve path from one failing faster than it logs.
- `--workers` (env `MUSEFS_WORKERS`) sizes the FUSE worker pool explicitly.
  The default stays auto (2× the CPU count, oversized for I/O-bound work), but
  each worker lazily opens its own read-only SQLite connection, so steady-state
  memory scales with the pool — many-core hosts serving few concurrent readers
  can now cap that component (#631).
- `musefs_process_resident_bytes` (Linux) reports the whole-process RSS, and
  `musefs_sqlite_memory_bytes` reports what SQLite holds across all connections.
  SQLite allocates through libc, so the jemalloc `musefs_alloc_*` gauges never
  saw it — a full-library walk grew the process by hundreds of MB while the
  allocator gauges barely moved (#631). The metrics surface now answers "how
  much memory is this using" honestly.

### Changed

- The `tags.value` cap rises from 256 KiB to 16 MiB − 1, and
  `track_art.description` from 1 KiB to 8 KiB (schema `MIGRATION_V3`). The new
  tag cap is FLAC's 24-bit metadata-block ceiling — the largest tag synthesis
  could ever serve — so the store no longer refuses a tag the format itself can
  carry. Existing stores upgrade in place, and automatically, on the next
  `musefs scan` or `musefs mount`: both open the store read-write and run the
  migration, which only widens the constraints and so carries every existing row
  across. No rescan of audio is needed and nothing has to be regenerated.
- A backing file whose metadata exceeds a store limit now fails **that file**
  instead of being stored with the offending part quietly dropped. Oversize
  embedded art and binary tags were previously omitted from an otherwise-stored
  track with only a `warn` to show for it, which is easy to lose in a scan of
  ten thousand files and leaves a mount silently missing data. Such a file is
  now logged with its path, what was too big, its size and the limit, counted
  `failed`, and skipped; the rest of the directory scans normally.
- Serve-path failure warnings are rate-limited: a burst of 10 per 30-second
  window logs at warn, the rest drop to debug, and the first warn of each new
  window carries the suppressed count. A library walk over missing backing
  files previously warned once per file — 200,000 lines (tens of MB of log)
  for a single enumeration. The read load-shed (`EAGAIN`) warning shares the
  same limiter, since a saturated client retries in a tight loop. The limiter is
  now process-wide rather than FUSE-local, so the warns emitted from inside
  synthesis — a dropped Vorbis tag key, over-cap art, a failed art-blob read —
  are bounded by the same budget instead of bypassing it (#650). Log targets are
  unchanged: each warning is still attributed to the module that raised it, so
  per-crate `RUST_LOG` filters keep working.
- Worker read connections now cap their SQLite page cache at 512 KiB (the
  default is ~2 MiB). The serve path opens one connection per worker thread
  (2× CPUs), so the default multiplied into hundreds of MB of steady-state RSS
  after a full-library enumeration; the cap saved ~110 MB on a 200,000-track
  walk with 64 workers and no measured latency change (#631). The tuning guide
  now documents the post-enumeration steady state as the number to size a host
  against, and the transparent-hugepage inflation some distros' `THP=always`
  default adds on top.
- The virtual tree stores each node name in one shared allocation instead of
  five copies, cutting about a quarter of its resident cost (~1.7 KB to ~1.3 KB
  per track, measured over 200,000 tracks). The tuning guide now documents the
  footprint and how to size a host against it.
- Each entry's rendered path is stored once and shared between the inode
  allocator and the refresh snapshot instead of being allocated twice, taking a
  further ~7% off the tree's resident cost. The saving tracks path length, so a
  deeper `--template` gains proportionally more.
- Full tree rebuilds and the head of every scan read only the columns they use
  instead of materializing whole track rows.

### Fixed

- A tag larger than the store's cap no longer aborts the entire scan
  ([#644](https://github.com/Sohex/musefs/issues/644)). The scanner had no
  length check on text tags, so an over-cap value reached the DB `CHECK` inside
  a batch commit and failed the whole run with
  `CHECK constraint failed: length(CAST(value AS BLOB)) <= 262144` — naming
  neither the offending file nor what the number meant. Every cap the scanner
  can trip is now checked in one place before anything is written, and a
  violation fails only that file, with a message naming it. The same applies to
  the `tags.key`, `art.mime` and `track_art.description` caps, which had the
  identical unattributed-abort failure mode. Should a store write still fail
  fatally, the error now names the file it died on.
- A FLAC whose tags outgrow what a `VORBIS_COMMENT` block can hold is rejected
  at scan time rather than stored and then served `EIO` on every read. This is
  reachable by merging a leading ID3v2 tag's fields into a FLAC's own comments
  ([#602](https://github.com/Sohex/musefs/issues/602)): ID3v2's tag size is
  synchsafe 28-bit (256 MiB) while a FLAC metadata block is 24-bit.
- A leading ID3v2 tag on an MP3 (or a FLAC) is stepped over by its declared size
  even when its major version is not one musefs can parse the frames of, instead
  of the whole file being rejected. The ID3v2 header has the same shape in every
  version, so its size is enough to step over the tag, and the spec's rule for a
  version a reader does not understand is to ignore it. Such a tag's frames are
  still not read.
- A parallel directory walk over a large mount no longer loses entries. Once
  1024 directory handles were open, further `opendir` calls were rejected with
  `ENFILE` — `bfs` over a 200,000-track mount silently lost about 9% of the
  tree. Over-cap directories are now served without a cached snapshot instead,
  so listings stay complete.
- Unmount helpers (`fusermount3`, `fusermount`, `umount`) are resolved to an
  absolute path before being spawned, so a daemon running as root can no longer
  be made to execute an attacker-supplied binary from a writable `PATH` entry
  when it receives `SIGTERM`.
- A scan whose batch commit fails no longer leaves worker threads parked on the
  byte budget forever. They are woken and joined before the error propagates, so
  an embedder that catches the error and keeps running no longer leaks a thread
  and its in-flight art bytes per failed scan.
- Locating an Ogg page for a seeking read now bounds how many candidate pages it
  will CRC-validate, so a file whose audio region is packed with false `OggS`
  markers cannot amplify a single read into thousands of positioned reads.
- `access` is implemented, so a mount no longer logs a `[Not Implemented]`
  warning from fuser.

## [1.3.0] - 2026-08-19

### Added

- FLAC files with a leading ID3 tag are now scanned rather than skipped with
  "no parseable audio metadata". Any tags and cover art in the ID3 header are
  ingested as a fallback for what the FLAC itself does not carry, and the
  served file is a stock FLAC with no ID3 tag.

## [1.2.0] - 2026-06-18

### Changed

- Bare `scan` is now additive: it skips rows already in the DB instead of
  re-seeding them from disk. Use `scan --force` when you want the old
  full-reimport behavior.
- `revalidate` is now its own subcommand and no longer prunes by default. It
  refreshes changed rows' structural data while preserving curated tags/art;
  use `revalidate --prune` to drop rows whose backing file is gone and
  garbage-collect orphaned art.

### Deprecated

- `scan --revalidate` is a deprecated, warned alias for `revalidate` and will
  be removed next release. It does not prune; use `revalidate --prune` when you
  need deletion.

### Fixed

- Revalidating a changed file no longer clobbers curated tags, art, or binary
  tags in the DB.

## [1.1.0] - 2026-06-17

### Added

- **`-v`/`--verbose` flag:** a global verbosity flag (`-v` = info, `-vv` =
  debug, `-vvv` = trace; default `warn`) on `scan` and `mount`, so diagnosing a
  run no longer requires knowing the `RUST_LOG` env var. An explicit `RUST_LOG`
  still takes precedence.
- **`mount --dry-run`:** validate the `--template` and configuration and print a
  sample of the paths the mount would expose (with total file and directory
  counts), then exit without mounting — a way to check a template before
  committing to a mount.
- **Runtime telemetry (`.musefs-metrics`):** an opt-in `--expose-metrics` flag
  (env `MUSEFS_EXPOSE_METRICS`) surfaces a synthetic `.musefs-metrics` file at
  the mount root rendering Prometheus-format counters — getattr/read/open
  activity, backing read-ahead behavior, and (when built with jemalloc)
  allocator stats. Off by default; the file is absent unless enabled. See the
  [Metrics](https://sohex.github.io/musefs/guide/tuning.html#metrics) section (#394).
- **Scan progress indicator:** `scan` and `scan --revalidate` render a live
  progress bar (indicatif) with an elapsed-time summary on an interactive
  terminal, falling back to periodic `ingested N/M (P%)` log lines when output
  is non-interactive. A new `--quiet`/`-q` flag suppresses it (#406).
- **`--skip-on-missing` template flag:** an opt-in `--skip-on-missing` (env
  `MUSEFS_SKIP_ON_MISSING`) drops a track from the mount when a top-level
  template field stays unresolved, instead of substituting `--default-fallback`.
  Per-field `--fallback` chains and `[...]` optional sections are unaffected (a
  field resolved via its fallback counts as present). The motivating case is
  `--template '$!{beets_path}' --skip-on-missing`, which hides tracks beets left
  without a `beets_path` rather than collapsing them into an `Unknown` bucket
  (#408).
- **`--read-ahead-prefetch` flag:** opt-in background prefetch threads layered on
  top of read amplification, default off — benchmarks found amplification alone
  delivers the entire read-ahead win, while the threads add ~10% overhead with no
  measured benefit. Enable only when profiling a backend where a single large
  read does not self-pipeline (#255).
- **riscv64 release platform:** prebuilt `riscv64gc-unknown-linux-{gnu,musl}`
  binaries and `linux/riscv64` Docker images now ship with each tagged release.
  Container bases bumped to current stable: glibc Debian bookworm → trixie
  (bookworm has no riscv64 image), musl Alpine 3.20 → 3.23 (3.20 is end-of-life).
- **`statfs` reply:** the mount now reports a non-zero synthetic capacity with
  ample free space instead of fuser's all-zero default, so `df` no longer shows a
  0-byte filesystem and capacity-checking importers (Lidarr et al.) don't balk
  (#368).
- **Per-extension skip breakdown:** at end of scan, a summary line breaks the
  `skipped` count down by lowercased extension (e.g. `skipped 42: jpg=20,
  cue=10, log=8, <none>=4`), logged at `warn` so it shows by default, so a large
  skip count is diagnosable — expected sidecars versus genuinely unexpected
  files. Log-only; the `ScanStats` struct and CLI summary are unchanged (#341).
- **`musefs vacuum` command:** compact the SQLite store, reclaiming free pages
  left by prunes, orphan-art GC, and the schema migration. Runs `VACUUM` + a WAL
  checkpoint and reports the space reclaimed; run it while unmounted (#566).

### Changed

- **Declared MSRV (`rust-version = "1.95"`):** the workspace now states a
  minimum supported Rust version so a too-old toolchain fails with a clear cargo
  message instead of mid-compile. It is best-effort and tracks recent stable
  (the bundled-SQLite dependency requires it); not CI-gated.
- **Supply-chain license gate:** a `deny.toml` + `cargo deny` CI job enforces a
  permissive-license allow-list (and bans/sources), closing the gap left by the
  advisory-only `cargo audit` check.
- **Strict template validation:** an unclosed `[ … ]` section or an unterminated
  `${` / `$!{` field is now rejected at mount time with an error naming the
  problem, instead of silently folding the rest of the template into the open
  construct — which turned a typo'd bracket into a surprising directory tree.

### Fixed

- **Clearer mount errors:** a missing or non-directory mountpoint is reported
  with an actionable message before FUSE setup (previously a bare `os error 2`,
  or a misleading "Permission denied" when the path was a regular file), and
  I/O errors no longer print their OS string twice.
- **Silent mp4 oversize drops:** oversized embedded `covr` cover art and binary
  freeform (`----`) values in `.m4a`/`.m4b` files are skipped in the format layer
  before materialization (to avoid building a large image out of a large `moov`),
  which previously dropped them with nothing in the logs. The scan now emits a
  `warn` line for each, matching the logging the other formats already had (#343,
  follow-up to #284).
- **xattr log noise:** `getxattr`/`listxattr`/`setxattr`/`removexattr` now reply
  `ENOTSUP` explicitly (read-only filesystem, no extended attributes) instead of
  falling through to fuser's default, which logged a `[Not Implemented]` warn on
  every xattr probe (`ls -l`, indexers, backup tools). The caller-visible result
  is unchanged (#364).

## [1.0.0] - 2026-06-12

First stable release.

### Added

- **Lidarr integration:** a new `contrib/lidarr/` package that drives
  symlink-based placeholder imports and syncs Lidarr metadata into the musefs
  SQLite store.
- **FUSE mount-access controls:** new `--allow-other`, `--owner`, and `--group`
  flags mount with `allow_other` + `default_permissions` so accounts other than
  the mounting user can reach the view and the presented owner/group/mode bits
  are enforced; `--owner`/`--group` imply `--allow-other`. A non-root
  `allow_other` mount is pre-flight checked against `/etc/fuse.conf`
  `user_allow_other` and fails early with guidance if it is missing. See the
  [Ownership and permissions](https://sohex.github.io/musefs/guide/configuration.html#ownership-and-permissions)
  section (#293, #294).
- **Hardened deployment assets:** the container image runs as a dedicated
  unprivileged user with a build-arg-configurable UID/GID, and the
  `musefs-scan.service` systemd unit ships a strong sandbox (the FUSE-mounting
  `musefs.service` deliberately cannot be sandboxed). See
  [systemd hardening](https://sohex.github.io/musefs/integrations/systemd.html#hardening)
  (#317, #318, #319).
- **crates.io distribution:** the `musefs` binary is published to crates.io as of
  this release and installable with `cargo install musefs`. A new thin `musefs` wrapper crate
  owns the binary (`musefs-cli` is now a library crate), and a tag-triggered
  release workflow publishes all crates in dependency order.

### Changed

- **`mount --db` now requires an existing store.** Mounting against a missing
  database path is rejected before any FUSE setup instead of silently creating
  and migrating an empty store, so a mistyped `--db` fails loudly rather than
  mounting an empty view. `scan --db` still creates the store if absent (#309).

### Fixed

- **Scanner no longer drops files and embedded art silently:** embedded cover
  art over `MAX_ART_BYTES` (and binary tags over `MAX_BINARY_TAG_BYTES`) were
  filtered out at ingest with no log line, so a track whose art exceeded the cap
  appeared to simply have none — indistinguishable from a scan bug. The drop is
  now logged (`RUST_LOG=warn`). Likewise, a supported-extension file that fails
  to parse or errors mid-probe was counted `failed` with the underlying error
  discarded; the reason is now logged. Note: oversized art in `.m4a`/`.m4b`
  files is dropped earlier, inside the format layer, and is not yet logged
  (#284, #343).
- **Lidarr custom-script env var casing:** Lidarr stores custom-script
  environment variables in a .NET `StringDictionary`, which lowercases every key,
  so a Linux script actually receives `lidarr_sourcepath` / `lidarr_eventtype`
  rather than the PascalCase names Lidarr's docs list. The integration read the
  PascalCase names, so with a real Lidarr every import failed and every event
  parsed as unsupported. Lidarr env vars are now resolved case-insensitively.
  Found by the issue #141 real-instance smoke run.
- **VorbisComment parse OOM (DoS):** a crafted comment block declaring a huge
  entry count made `Vec::with_capacity` attempt a multi-gigabyte allocation; the
  pre-allocation is now bounded by the readable byte count. Found by the new
  `vorbiscomment` fuzz target.
- **MP4 box-bounds integer overflow:** an untrusted 64-bit extended box size made
  the box-bounds check (`pos + total`) overflow `usize` — a panic in debug and a
  silent wrap in release that accepted a bogus box length. The addition is now
  checked. Found by the `mp4` fuzz target.
- **ID3v2 parsing unbounded allocation (DoS):** the `id3` crate eagerly allocates
  a frame's declared size (ID3v2.3 frame sizes are plain 32-bit, up to 4 GiB), so
  a crafted tag could exhaust memory at scan time — via an MP3 or a WAV embedded
  `id3 ` chunk. Parsing is now gated on validated ID3v2 frame bounds and an
  ID3v2 tag at offset 0 (the `id3` reader scans forward). Found by the `mp3` and
  `wav` fuzz targets.
- **Scan counters now match their documented contract:** `musefs scan` reports
  every non-audio file (any unsupported or missing extension — `.jpg`, `.cue`,
  `.log`, `.nfo`, cover art, etc.) as `skipped`, and supported-extension files
  that fail to parse (e.g. a corrupt `.flac`) as `failed`. Previously malformed
  files were miscounted as `skipped` and unsupported files were not counted at
  all, so expect `skipped` to be larger than before on a real library (#301).
- **Symlink scans no longer double-count:** with `--follow-symlinks`, a file
  reached via both its real path and a symlink is ingested and counted once
  instead of inflating `scanned`; multiple hardlinks to the same inode are
  likewise collapsed to a single track (#302).
- **Stable inodes on case-insensitive mounts:** the inode allocator is now keyed
  on the case-folded path in case-insensitive mode, so an unrelated deletion that
  flips a merged directory's display casing no longer reassigns a survivor's
  inode (#305).
- **Lidarr autoscan now honors the scan timeout:** an import/release-triggered
  autoscan applies the shared 120s scan timeout, matching the beets and Picard
  integrations, so a wedged `musefs scan` fails with a controlled timeout instead
  of blocking the custom-script process indefinitely (#312).

## [0.2.0] - 2026-05-27

First public release.

### Added

- **Formats:** synthesis for M4A/M4B (MP4), Ogg (Opus, Vorbis, FLAC-in-Ogg), and
  WAV, alongside the existing FLAC and MP3 — metadata generated on the fly from
  the SQLite store and spliced in front of byte-identical backing audio.
- **Arbitrary tag support:** a single canonical tag vocabulary maps common fields
  to each format's native slot (ID3 frame / MP4 atom / Vorbis field); any other
  tag round-trips through the format's extension slot (ID3 `TXXX`, MP4 `----`
  freeform, raw Vorbis field). User-defined key casing is preserved.
- **beets plugin** (`contrib/beets/`): syncs beets' canonical tags and cover art
  into the store keyed by each file's real path, with no remount and no audio
  rewrite.
- **Performance, concurrency & caching pass:** worker-pool offload of blocking
  reads, lock-free virtual-tree swap, per-handle I/O, a bounded LRU header-layout
  cache, debounced single-flighted refresh with stable inodes, kernel/mount
  tuning flags, bounded-memory MP4 resolves, and opt-in `--keep-cache` with
  auto-invalidation.

### Notes

- Read-only mount; tag edits happen out-of-band against the SQLite store and are
  picked up automatically (`PRAGMA data_version` polling). See the
  [Supported formats](https://sohex.github.io/musefs/formats/overview.html) docs
  for round-trip limitations.

## [0.1.0]

- Initial MVP (FLAC and MP3 synthesis, virtual tree with beets-style templates,
  `synthesis` / `structure-only` mount modes, auto-refresh, `scan` /
  `scan --revalidate`). Never published publicly; superseded by 0.2.0.
