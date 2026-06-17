# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> The `contrib/` Python packages have their own decoupled version and changelog:
> see [contrib/CHANGELOG.md](contrib/CHANGELOG.md).

> The full, detailed changelog (including internal changes) lives in the
> documentation site: <https://sohex.github.io/musefs/changelog.html>.

## [Unreleased]

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
