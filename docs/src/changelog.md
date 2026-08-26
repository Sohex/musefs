# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> The `contrib/` Python packages have their own decoupled version and changelog:
> see [the contrib changelog](integrations/overview.md#contrib-changelog).

For curated, upgrade-focused notes (highlights and per-version migration steps),
see the [Release notes](release-notes.md).

## [Unreleased]

### Added

- `musefs_dir_handle_rejections_total` ([#626](https://github.com/Sohex/musefs/issues/626)), a monotonic counter of
  `opendir` calls that could not be given a cached directory snapshot. The
  existing `musefs_dir_handles` gauge cannot stand in for it: saturation is
  bursty, and a walk that produced 7,525 rejections never showed a gauge sample
  above 593. It is also the signal that the stateless path from
  [#616](https://github.com/Sohex/musefs/issues/616) is in use and directories are being rebuilt on every
  `readdir`.

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
- The virtual tree interns each node name into one shared `Arc<str>` rather than
  storing it separately in `Node.name`, `Node.rendered_name`, the `children` key
  and both `rendered_children` keys ([#617](https://github.com/Sohex/musefs/issues/617)). Measured over 200,000
  tracks / 222,001 nodes: ~1713 to ~1284 bytes per track (~84 MiB, -25%), with
  tree build 10-15% faster from the removed allocations. A case-insensitive
  mount saves more, since `folded_children` held a sixth copy. The tuning guide
  gained a "Memory footprint" section. The full rendered paths were still stored
  twice at this point; the next entry shares those as well.
- Each entry's rendered path is stored once and shared between the inode
  allocator's map key and `TrackRenderState.path`, rather than allocated
  independently by each ([#629](https://github.com/Sohex/musefs/issues/629)). Measured over 200,000 tracks with
  41-byte paths: 1150 to 1078 bytes per track (-6%); with 137-byte paths the
  saving is -14%. What it removes is exactly one whole path per track, so the
  gain tracks path length and template depth rather than track count — a flat
  `--template` gains little. Sharing is conditional: a disambiguated leaf keeps
  its own allocation, since keying it on the path its bare-named sibling already
  interned would collapse two nodes onto one inode.
- Full tree rebuilds and the head of every scan read projected columns instead
  of materializing a whole `Track` per row ([#621](https://github.com/Sohex/musefs/issues/621)) — roughly 40 MB of
  transient allocation on a 200,000-track store, on a path already holding a
  pool connection.
- `readdir`'s unknown-`fh` fallback runs on the worker pool instead of inline on
  the fuser dispatch thread ([#623](https://github.com/Sohex/musefs/issues/623)), matching the offload every other
  blocking operation already used. This matters more now that over-cap `opendir`
  makes that fallback the normal path for large directories.
- Scan failures are now broken down by reason and their per-file warnings capped
  ([#651](https://github.com/Sohex/musefs/issues/651)). A scan that ends
  `failed 37` also logs `failed 37: unparseable=30, io=5, oversize=2` (and
  `walk errors N: …` for directories the walk could not read), so the number
  that drives the exit-2 partial-failure signal explains itself instead of
  having to be reconstructed from N individual lines. The per-file messages
  themselves are capped at ten per reason per scan, the rest dropping to
  `debug` — an unreadable subtree or a share that went away mid-scan no longer
  emits one warning per file. The existing per-extension skip breakdown moves
  from `warn` to `info` (so it now needs `-v` / `RUST_LOG=info`): cover art and
  `.cue` sidecars are the normal contents of a music library, and a warning on
  every healthy scan only teaches operators to tune warnings out. The `skipped`
  count itself is unchanged and still printed in the per-target summary.

### Fixed

- Scan log records and the progress bar no longer clobber each other on an
  interactive terminal ([#648](https://github.com/Sohex/musefs/issues/648)).
  `ScanReporter` renders an `indicatif` bar on stderr and the `log` facade
  writes to the same stderr, with nothing coordinating the two: a record was
  emitted at whatever column the last bar frame left the cursor on, and the
  next 120 ms tick issued a clear-line that ate part of it. Both the warning
  and the bar came out mangled. This was reachable on essentially the first
  interactive scan of any real library, because the end-of-walk skip tally
  ([#341](https://github.com/Sohex/musefs/issues/341)) warns about `cover.jpg`
  / `.cue` / `.log` / `.nfo` sidecars, and every unparseable file warns from
  inside the pipeline. The CLI now owns a single process-wide stderr draw
  target: the scan bar draws through it, and the binary's `env_logger` is
  installed wrapped in a sink that emits each record while that target is
  suspended — bar cleared, record written, bar redrawn below it. The per-target
  summary line (`scanned N: …`, on stdout) is suspended the same way; it used
  to be glued onto a bar frame whenever no log record happened to precede it.
  Off a terminal the draw target is hidden and suspending is a no-op, so the
  `--quiet` and piped `ingested N/M (P%)` paths are byte-for-byte unchanged, as
  is the verbosity policy (`-v`/`-vv`/`-vvv`, `RUST_LOG` taking precedence),
  which stays in the binary.
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
- Over-cap `opendir` degrades to a stateless directory handle instead of
  replying `ENFILE` ([#616](https://github.com/Sohex/musefs/issues/616)). The 1024-handle cap was assumed to sit
  well above any real client, but `bfs` — an ordinary parallel `find`, and the
  default `find` in several distributions — exceeded it immediately: 7,525
  rejections over a 200,000-track mount, 17,910 files never enumerated. The
  rejection surfaced to the operator as "Too many open files in system", which
  points at the kernel rather than the mount, and an indexer that logs and
  continues would simply present an incomplete library. Serving over-cap
  directories through the existing stateless fallback keeps listings complete
  and preserves the memory bound the cap was added for, at the cost of an O(N)
  rebuild per `readdir`.
- Unmount helpers are resolved against `/usr/bin`, `/bin` and `/usr/local/bin`
  before falling back to a bare-name `PATH` lookup ([#620](https://github.com/Sohex/musefs/issues/620)). The
  mounting guide steers operators toward running as root for kernel passthrough
  in `StructureOnly` mode, and nothing sanitizes `PATH` there, so a writable
  `PATH` entry meant attacker-chosen code executed as root on `SIGTERM`.
- A failed batch commit during a scan winds the pipeline down before the error
  propagates ([#618](https://github.com/Sohex/musefs/issues/618)). `ByteBudget` gained a `close()` that wakes
  waiters, so a worker parked in `acquire` on a condvar only `flush` ever
  signalled is no longer stranded. This was benign for the CLI, where process
  exit reaps everything, but `scan_directory_with` / `revalidate_with` are
  public API and an embedder that caught the error accumulated leaked threads
  and their in-flight art bytes. The two state-mutex `unwrap()`s adopted the
  daemon's `lock_recover` policy at the same time, retiring the open note in
  `lock.rs`.
- `find_page_start` bounds the number of candidate pages it CRC-validates per
  call ([#619](https://github.com/Sohex/musefs/issues/619)). The header pre-filter almost never admits a false
  `OggS` on real audio, but a file whose audio region is deliberately packed
  with `OggS\x00\x00` cleared it at every offset, allowing up to ~65,000 CRC
  validations — each with its own positioned reads — for a single seeking read.
  Hardening rather than a live vulnerability: the attacker is whoever can place
  a file in the scanned library.
- `access` is implemented and replies `ok` ([#624](https://github.com/Sohex/musefs/issues/624)), so fuser's default
  no longer logs `[Not Implemented]` per mount. The mount carries `RO` and, with
  `allow_other`, `DefaultPermissions`, so the kernel already enforces the
  presented mode bits.

### Internal

- The `ogg_page` fuzz target round-trips the page machinery the serve path
  actually depends on — `verify_page_crc` and `patch_page_header_algebraic` —
  instead of only decoding a header ([#625](https://github.com/Sohex/musefs/issues/625)). Coverage against the
  committed seed rose from 51 edges / 69 features to 129 / 267.
- The read-ahead budget invariant restored by #536 now has a concurrent
  regression test that the ASan and TSan CI legs actually reach
  ([#628](https://github.com/Sohex/musefs/issues/628)). The sanitizer legs previously ran a test that builds
  `ReadAheadPool::new(0)`, leaving the pool disabled throughout. No live bug was
  found; this closes the coverage gap.
- `musefs-core/tests/tree_footprint.rs` gates the virtual tree's per-track
  resident cost ([#629](https://github.com/Sohex/musefs/issues/629)), turning #617's throwaway probe into a
  committed ceiling. It samples `VmRSS` either side of `VirtualTree::build_with`
  with the rendered paths materialized beforehand, so the delta is the tree's
  own marginal cost. The ceiling is deliberately loose — a gate that catches
  only a large regression is worth more than one that reddens on a loaded
  runner — and a floor assertion keeps a broken measurement from passing
  vacuously.
- `deny.toml`'s allow and ignore lists can no longer rot ([#622](https://github.com/Sohex/musefs/issues/622)). A
  stale `RUSTSEC-2025-0167` ignore and an unmatched `ISC` license allowance both
  warned and exited 0, so neither surfaced on a PR — which is how an entry ends
  up silently pre-exempting the next real advisory for the same crate. Both were
  dropped, and `advisory-not-detected` / `license-not-encountered` are now
  errors in the `deny` job. The promotion is scoped to the root graph, which is
  what the lists are authored against; the fuzz-lockfile scan in `audit.yml`
  allows the advisory diagnostic explicitly, since off that graph it is noise.
- Issue, pull-request and `CODEOWNERS` templates ([#627](https://github.com/Sohex/musefs/issues/627)). The PR
  checklist covers the steps that are easy to forget and silently break
  something: the pre-commit hook, `cargo +nightly fuzz build` after a
  format-layer API change, regenerating the Python schema mirror after a
  `musefs-db` change, and a changelog entry.

## [1.3.0] - 2026-08-19

### Added

- FLAC files carrying one or more ID3v2 tags in front of the `fLaC` marker are
  now scanned instead of being skipped with "no parseable audio metadata"
  ([#602](https://github.com/Sohex/musefs/issues/602)). The tag run is stepped over to reach the FLAC stream, and its text
  frames and `APIC` pictures are ingested as a fallback beneath the file's own
  `VORBIS_COMMENT` / `PICTURE` blocks — so a FLAC whose tags live only in the
  ID3 header lands in the store with its tags. A trailing 128-byte ID3v1 tag is
  trimmed from the audio length (checked only for files with a leading ID3v2
  tag, so a stock FLAC pays no extra read). Neither tag survives into the
  synthesized file, which is a stock FLAC starting at `fLaC`.

### Changed

- `musefs-core` now builds its persistent virtual-tree collections on
  [`imbl`](https://crates.io/crates/imbl) 7 instead of the archived `im` 15.
  Clears RUSTSEC-2026-0248 / RUSTSEC-2023-0126 (`im`) and
  RUSTSEC-2026-0251 / RUSTSEC-2026-0255 (`sized-chunks`).
- `VirtualTree::children` now yields an opaque
  `impl ExactSizeIterator<Item = (&str, u64)>` instead of borrowing the backing
  `OrdMap`, taking the persistent-collection crate out of `musefs-core`'s
  public API so swapping it stays an internal detail. In-tree the only caller
  is `readdir`, which iterates; name lookups have always had
  `VirtualTree::lookup`.

### Fixed

- Bumped `crossbeam-epoch` 0.9.18 -> 0.9.20 (RUSTSEC-2026-0204, invalid
  pointer dereference in the `fmt::Pointer` impls) and `num-bigint`
  0.4.7 -> 0.4.8 (0.4.7 was yanked). Both are dev-dependency-only paths
  (criterion -> rayon, mp4 -> num-rational).

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
- **`musefs vacuum` command:** compact the SQLite store, reclaiming free pages
  left by prunes, orphan-art GC, and the schema migration. Runs `VACUUM` + a WAL
  checkpoint and reports the space reclaimed; run it while unmounted (#566).

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

[Unreleased]: https://github.com/Sohex/musefs/compare/v1.3.0...HEAD
[1.3.0]: https://github.com/Sohex/musefs/releases/tag/v1.3.0
[1.2.0]: https://github.com/Sohex/musefs/releases/tag/v1.2.0
[1.1.0]: https://github.com/Sohex/musefs/releases/tag/v1.1.0
[1.0.0]: https://github.com/Sohex/musefs/releases/tag/v1.0.0
[0.2.0]: https://github.com/Sohex/musefs/releases/tag/v0.2.0
[0.1.0]: https://github.com/Sohex/musefs/releases/tag/v0.1.0
