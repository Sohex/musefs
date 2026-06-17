# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> The `contrib/` Python packages have their own decoupled version and changelog:
> see [the contrib changelog](integrations/overview.md#contrib-changelog).

## [Unreleased]

### Added

- **Runtime telemetry (`.musefs-metrics`):** an opt-in `--expose-metrics` flag
  (env `MUSEFS_EXPOSE_METRICS`) surfaces a synthetic `.musefs-metrics` file at
  the mount root rendering Prometheus-format counters — getattr/read/open
  activity, backing read-ahead behavior, and (when built with jemalloc)
  allocator stats. Off by default; the file is absent unless enabled. See the
  README [Metrics](guide/tuning.md#metrics) section (#394).
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

### Fixed

- **Art/serve rowid-reuse consistency:** the read fast path's WAL-snapshot +
  `content_version` guard, previously gated only on binary-tag layouts, now
  covers all DB-rowid segments (art `ArtImage`/`OggArtSlice` too) via
  `RegionLayout::streams_db_rowid`, and the stateless no-fh read fallback now
  applies the same snapshot/recheck and re-validates its freshly opened backing
  fd against the resolved stamp. A concurrent external retag + `gc_orphan_art` +
  reinsert can no longer splice a wrong image or stale tag bytes mid-read (the
  audio-bytes invariant was never affected) (#502, #503).
- **Per-field `--fallback` case-insensitivity:** fallback keys are now ASCII
  lowercased to match template field names, so `--fallback AlbumArtist=…` (any
  uppercase) is honored instead of silently never matching (#504).
- **Tag value byte cap:** both the schema `CHECK` (rebuilt in the `MIGRATION_V2`
  upgrade) and the read-time `tags.value` guard now count bytes, not UTF-8
  characters, so the 256 KiB materialized-memory bound is exact rather than up to
  ~4x looser for multibyte text. The upgrade drops any pre-existing over-cap rows
  (already unreadable under the byte-counting reader guard) (#505).
- **Embedded NUL in ID3 metadata:** synthesized ID3 frames now reject a
  DB-sourced tag key, tag value, art mime, or art description containing an
  embedded NUL instead of emitting a frame a downstream parser would misread
  (#506).
- **Orphan-art GC NULL safety:** `gc_orphan_art` uses `NOT EXISTS` rather than
  `NOT IN (subquery)`, so a NULL `art_id` could not silently turn the GC into a
  no-op (#507).
- **Mount usability:** `mount` now warns when the mountpoint is non-empty (its
  contents are shadowed for the mount's lifetime), and a permission-denied mount
  (e.g. an AppArmor-restricted prefix) prints actionable guidance instead of a
  bare "Permission denied" (#508, #509).
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
- **MP4 path-to-`ilst` leniency:** the walk to `moov/udta/meta/ilst` now uses the
  same lenient box scan as the metadata extractors, so a single malformed or
  truncated sibling box anywhere on the path no longer suppresses an otherwise
  well-formed `ilst` and silently drops every tag and cover. The audio/structure
  path stays strict (#542).
- **QuickTime bare `meta` atoms:** the `meta` parser only consumes the 4-byte
  FullBox version/flags prefix when it is actually present (a zero word), so a
  QuickTime-style bare `meta` — which has no such prefix — is read instead of
  landing mid-header and dropping all tags and art (#543).
- **`scan` exit code on ingest failure:** `scan`/`scan --revalidate` now exit `2`
  when any file fails to parse/ingest (`failed > 0`), instead of always exiting
  `0`. A pipeline such as `musefs scan … && musefs mount …` can now detect a
  partial or total ingest failure; a clean scan still exits `0` and a hard error
  still exits `1` (#554).
- **Release smoke audio-bytes check:** `scripts/smoke-binary.sh` (the per-arch
  release gate) now compares the served file's encoded audio stream against the
  untouched backing file, asserting the cardinal byte-identical-audio invariant
  rather than only checking the `fLaC` magic — so a target-specific positioned-read
  or offset regression in a cross-compiled binary is caught (#547).

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
  README [Ownership and permissions](guide/configuration.md#ownership-and-permissions)
  section (#293, #294).
- **Hardened deployment assets:** the container image runs as a dedicated
  unprivileged user with a build-arg-configurable UID/GID, and the
  `musefs-scan.service` systemd unit ships a strong sandbox (the FUSE-mounting
  `musefs.service` deliberately cannot be sandboxed). See
  [the systemd hardening notes](integrations/systemd.md#hardening)
  (#317, #318, #319).
- **crates.io distribution:** the `musefs` binary is published to crates.io as of
  this release and installable with `cargo install musefs`. A new thin `musefs` wrapper crate
  owns the binary (`musefs-cli` is now a library crate), and a tag-triggered
  release workflow publishes all crates in dependency order.
- **Fuzzing & property tests:** coverage-guided `cargo-fuzz` targets for every
  format parser (FLAC, MP3, MP4, Ogg, WAV), the byte-level primitives (Ogg
  page parsing, base64 windowing, VorbisComment), and the serve path — the
  latter drives the full synthesis pipeline over hostile DB rows and binary tags
  via a fuzzing-gated `Db::with_raw_conn`. Plus `proptest` invariants —
  panic-freedom, the byte-identical audio guarantee, and tag round-trip — an
  end-to-end read-fidelity property, and a `mutagen` interop test asserting an
  independent reader sees the tags we synthesize.

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
  picked up automatically (`PRAGMA data_version` polling). See the README
  [Supported formats](formats/overview.md#supported-formats) section and the per-format
  docs for round-trip limitations.

## [0.1.0]

- Initial MVP (FLAC and MP3 synthesis, virtual tree with beets-style templates,
  `synthesis` / `structure-only` mount modes, auto-refresh, `scan` /
  `scan --revalidate`). Never published publicly; superseded by 0.2.0.

[Unreleased]: https://github.com/Sohex/musefs/compare/v1.0.0...HEAD
[1.0.0]: https://github.com/Sohex/musefs/releases/tag/v1.0.0
[0.2.0]: https://github.com/Sohex/musefs/releases/tag/v0.2.0
[0.1.0]: https://github.com/Sohex/musefs/releases/tag/v0.1.0
