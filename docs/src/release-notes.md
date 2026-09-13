# Release notes

Curated, upgrade-focused notes for each release. For the exhaustive,
per-change list see the [Changelog](changelog.md); for the external-writer
`contrib/` packages (which version independently) see the
[contrib changelog](integrations/overview.md#contrib-changelog).

## v2.0.0 (unreleased)

A major release, being assembled on the `v2.0.0` branch; each change adds its
upgrade steps here as it lands. See the [Changelog](changelog.md#unreleased) for
the full list so far.

### Upgrading from v1.3.0

**Scan flags removed.** Both changes fail loudly rather than quietly doing
something different, so a script or unit that needs updating will tell you:

- **`scan --revalidate` is gone**, with its `MUSEFS_REVALIDATE` variable
  ([#707]). It has been a deprecated alias since v1.2.0. Run
  `musefs revalidate` instead — the alias never pruned, so neither does the
  replacement unless you add `--prune`. The flag is now a usage error (exit `2`).
- **`scan --fast` and `--strict` are replaced by `--match`** ([#709]):
  `--fast` becomes `--match=fast` and `--strict` becomes `--match=strict`. If you
  passed neither, there is nothing to change: `--match=auto` is the default and
  behaves exactly as before. The variables follow: `MUSEFS_FAST=true` becomes
  `MUSEFS_MATCH=fast`, `MUSEFS_STRICT=true` becomes `MUSEFS_MATCH=strict`. The old
  flags are usage errors (exit `2`).

**Retired variables stop `scan`.** An environment variable that no flag reads
any more would otherwise be ignored in silence — turning a revalidate into a full
scan, or quietly weakening how a moved file is confirmed. So `scan` refuses to
start (exit `1`) while `MUSEFS_REVALIDATE`, `MUSEFS_FAST` or `MUSEFS_STRICT` is
set, and names the replacement. That includes a value of `false`: delete the
line from a systemd `EnvironmentFile=` or container environment rather than
switching it off. An empty value is treated as unset. `mount` does not read
these variables and is unaffected, so an environment file shared by the mount
and scan units only matters to the scan.

**External writers.** No update is needed for these changes: the `contrib/`
packages have called the `revalidate` subcommand since their 1.2.0 and pass
neither `--fast` nor `--strict`.

**Rust crate API.** This only affects code depending on the musefs crates
directly.

- `musefs_cli::run_scan` no longer takes `revalidate`, and takes a
  `musefs_cli::MatchMode` in place of `fast`/`strict`.
- The public enums a caller matches on — the error types, `Format`, the scan and
  mount option enums, and `musefs-cli`'s `Command` and value enums — are
  `#[non_exhaustive]` ([#708]). A `match` on one outside its crate needs a
  wildcard arm; in exchange, a new audio format or error case is no longer a
  breaking change. The changelog lists which enums, and which were deliberately
  left exhaustive.
- Test scaffolding is no longer public ([#710]):
  `musefs_core::scan_directory_full_oracle`, the `*_for_test` methods on `Musefs`
  and `Db`, and `musefs_format::ogg::page_test_support`. No production code
  called any of them.
- The configuration and result structs follow the enums ([#743]):
  `ScanOptions`, `MountConfig`, `FuseConfig`, `musefs-cli`'s argument structs and
  the crates' result types are `#[non_exhaustive]`, so outside their crate they
  can no longer be built with a struct literal, `..Default::default()` included.
  Start from `default()` — new for `MountConfig`, matching a bare `musefs mount`
  — and assign the fields you change. The store-row and synthesis input structs
  (`NewTrack`, `TrackArt`, `ArtInput` and the like) are unchanged, so a new store
  column is still a breaking change for code that writes rows.

[#707]: https://github.com/Sohex/musefs/issues/707
[#708]: https://github.com/Sohex/musefs/issues/708
[#709]: https://github.com/Sohex/musefs/issues/709
[#710]: https://github.com/Sohex/musefs/issues/710
[#743]: https://github.com/Sohex/musefs/issues/743

## v1.3.0

A compatibility and maintenance release. The headline change makes musefs
ingest **FLAC files that carry a leading ID3 tag** instead of rejecting them,
and the dependency tree is clear of open RUSTSEC advisories. The on-disk schema
is unchanged (still version 2) and no CLI flag or default moved, so upgrading is
a drop-in — but a re-scan is worth running, see
[Upgrading from v1.2.0](#upgrading-from-v120).

### Highlights

- **FLAC with a leading ID3 tag now scans** ([#602]). Some FLAC files put one or
  more ID3v2 tags in front of the `fLaC` marker — non-standard, but common in
  the wild, often a blank header left behind by a converter. musefs used to
  reject these with `no parseable audio metadata`, counting them `failed`; they
  now parse normally.
- **Their ID3 tags come with them.** Text frames and embedded cover art in the
  ID3 header are ingested as a *fallback*: the FLAC's own `VORBIS_COMMENT` and
  `PICTURE` blocks win, and the ID3 tag only supplies what the FLAC itself does
  not carry. A FLAC whose tags live only in its ID3 header therefore arrives
  tagged rather than blank. The served file is a stock FLAC with no ID3 tag —
  the tag is metadata, and musefs regenerates metadata rather than copying it.
- **No open security advisories.** `musefs-core`'s persistent collections moved
  from the archived `im` crate to the maintained `imbl` fork, clearing two
  unsoundness advisories along with the unmaintained ones, and a
  `crossbeam-epoch` vulnerability was patched. Both lockfiles were refreshed.

See the [Changelog](changelog.md#130---2026-08-19) for the full list.

### Upgrading from v1.2.0

**No schema migration.** The store stays at `user_version` 2, and you can roll
back to v1.2.0 without touching it.

**Re-scan to pick up previously-rejected files.** FLACs with a leading ID3 tag
were never added to the store, so a bare `musefs scan <dir>` ingests them now —
no `--force` needed, and files already tracked are left alone. If a scripted
pipeline was exiting **2** because of these files (`failed Y` with `Y > 0`), it
will now succeed.

**`musefs-core` API.** `VirtualTree::children` returns an opaque
`impl ExactSizeIterator<Item = (&str, u64)>` instead of borrowing the backing
`OrdMap`. This only affects code depending on the `musefs-core` crate directly;
callers that iterated are unaffected apart from the item type, and by-name
lookups have always had `VirtualTree::lookup`. The change takes the
persistent-collection crate out of the public API, so swapping it again cannot
break a downstream consumer.

**External writers.** The `contrib/` packages are unchanged this cycle; no
update is needed alongside musefs.

[#602]: https://github.com/Sohex/musefs/issues/602

## v1.2.0

A behavior-focused release: the headline change makes `scan` **non-destructive
by default**. The on-disk schema is unchanged (still version 2), so there is no
migration — but a default that previously overwrote curated metadata has been
removed, so read [Upgrading from v1.1.0](#upgrading-from-v110) before you update
any automation that calls `scan`.

### Highlights

- **`scan` is now additive.** A bare `musefs scan` ingests only files not already
  in the store and **never overwrites curated tags or art**. Re-running it to
  pick up newly-added files is safe; the old full-reimport-from-disk behavior is
  now opt-in via `scan --force`. This closes a footgun where a routine re-scan
  silently clobbered tags written by Picard / beets / the store.
- **`revalidate` is its own subcommand.** Promoted from the `scan --revalidate`
  flag, it refreshes the structural/serving facts (audio bounds, checksums, FLAC
  structural blocks) of in-store files whose backing bytes changed, while
  **preserving curated tags, art, and binary tags**. Files not yet in the store
  are ignored — that is `scan`'s job.
- **Deletion is opt-in.** `revalidate` no longer prunes by default; pass
  `revalidate --prune` to drop rows whose backing file is gone and
  garbage-collect orphaned art. No bare command deletes or overwrites store data.
- **Data-loss fix.** Revalidating a changed file no longer clobbers curated tags,
  art, or binary tags — previously the structural/checksum backfill routed
  through a full re-seed from disk.

See the [Changelog](changelog.md#120---2026-06-18) for the full list.

### Upgrading from v1.1.0

**No schema migration.** The store stays at `user_version` 2; nothing about the
on-disk format changes, and you can roll back to v1.1.0 without touching it.

**Behavior changes to check:**

- **Bare `scan` no longer re-imports existing tracks.** Automation that runs
  `musefs scan <dir>` expecting it to refresh tags on files already in the store
  now **skips** them (reported as `already present`). To re-seed curated metadata
  from disk, use `scan --force`. The common case — re-running `scan` to pick up
  newly-added files — is unchanged.
- **`scan --revalidate` is deprecated.** It still works but prints a warning and
  forwards to the non-pruning `revalidate` path; it will be removed next release.
  Switch to the `musefs revalidate` subcommand — and note it **no longer prunes**,
  so add `--prune` for the old delete-gone-rows-and-GC behavior.
- **`revalidate` does not prune by default.** Scripted maintenance that relied on
  `scan --revalidate` to remove rows for deleted files must now pass
  `revalidate --prune` explicitly.

**External writers.** The `contrib/` packages are bumped to 1.2.0; the beets and
Picard plugins are updated for the new CLI — the beets plugin's autoscan now
resets the store with `scan --force`, and `beet musefs --revalidate` maps to
`revalidate --prune` (both transparent to you). Update these packages alongside
the binary; older copies invoke the deprecated `scan --revalidate` alias, which
still works for this release.

## v1.1.0

A feature-and-hardening release on top of the v1.0.0 stable line. No CLI flags
or store columns were removed, but the on-disk schema steps to **version 2** and
a few defaults change observable behavior — read [Upgrading from
v1.0.0](#upgrading-from-v100) before you update an existing store.

### Highlights

- **Runtime telemetry.** An opt-in `--expose-metrics` (env
  `MUSEFS_EXPOSE_METRICS`) surfaces a synthetic `.musefs-metrics/` directory at
  the mount root whose `metrics` file renders Prometheus-format counters for
  getattr/read/open activity, backing read-ahead behavior, and (with the
  jemalloc build) allocator stats. Off by default. See
  [Tuning & metrics](guide/tuning.md#metrics).
- **Scan progress indicator.** `scan` and `scan --revalidate` render a live
  progress bar on an interactive terminal and fall back to periodic
  `ingested N/M (P%)` lines when output is redirected. A new `--quiet`/`-q`
  suppresses it.
- **`--skip-on-missing` template flag.** Opt-in (env `MUSEFS_SKIP_ON_MISSING`):
  drops a track from the mount when a top-level template field stays unresolved,
  instead of substituting `--default-fallback`. The motivating case is
  `--template '$!{beets_path}' --skip-on-missing`, hiding tracks beets left
  without a `beets_path` rather than collapsing them into an `Unknown` bucket.
- **`--read-ahead-prefetch` flag.** Opt-in background prefetch threads layered on
  read amplification, default off — benchmarks found amplification alone
  delivers the read-ahead win, so enable this only when profiling a backend where
  a single large read does not self-pipeline.
- **riscv64 release platform.** Prebuilt `riscv64gc-unknown-linux-{gnu,musl}`
  binaries and `linux/riscv64` Docker images now ship with each tagged release.
  Container bases moved to current stable (Debian trixie, Alpine 3.23).
- **`statfs` reply.** The mount now reports a synthetic non-zero capacity with
  ample free space, so `df` no longer shows a 0-byte filesystem and
  capacity-checking importers (Lidarr et al.) no longer balk.
- **Per-extension skip breakdown.** End-of-scan summary breaks the `skipped`
  count down by lowercased extension (e.g. `skipped 42: jpg=20, cue=10, log=8`)
  so a large skip count is diagnosable. Log-only; the counters are unchanged.
- **`musefs vacuum`.** A maintenance command that compacts the SQLite store —
  reclaiming the free pages that prunes, orphan-art GC, and the migration leave
  behind — and reports the space reclaimed. Run it while unmounted. See
  [Maintenance](guide/maintenance.md).

Plus a substantial round of correctness and robustness fixes across the read
fast path (rowid-reuse consistency for art segments), the MP4/QuickTime
metadata walk, ID3 synthesis, and the prune/delete paths — see the
[Changelog](changelog.md#110---2026-06-17) for the full list.

### Upgrading from v1.0.0

**1. Back up your store.** The schema migration below is one-way. While no scan
or external writer is touching the database, copy `musefs.db` (and its `-wal` /
`-shm` sidecars if present). A v1.0.0 binary has no guard against a newer store
and may misread one that has been migrated, so keep the backup if you might roll
back. From v1.1.0 onward a binary instead **refuses** to open a store whose
schema is newer than it understands, with a clear error.

**2. Automatic schema migration (`user_version` 1 → 2).** The first time a
v1.1.0 binary opens the store — for example `musefs scan` — it migrates in a
single transaction. The migration:

- Adds scanner-owned `tracks.fingerprint` and `tracks.content_hash` columns
  (nullable SHA-256 hex, non-unique by design) plus a `fingerprint` index. They
  start `NULL` and are populated on the next scan; external writers do not set
  them.
- Rebuilds the `tags` table so the 256 KiB `value` cap counts bytes rather than
  characters (the v1 `CHECK` was up to ~4× looser for multibyte text). Any row
  that was already over the byte cap is dropped in the rebuild (this only reaches
  genuinely pathological data — a single tag value larger than 256 KiB of bytes,
  which a real library never has, and such rows were already unreadable under the
  byte-counting read guard anyway; in practice no store is affected).

The migration applies automatically the first time a v1.1.0 binary opens the
store, but you should still run `musefs scan --db <store>` once after upgrading:
that is what populates the new `fingerprint` / `content_hash` columns, which the
scanner's content-identity refind logic relies on. Then remount. See
[The SQLite store](architecture/store.md) for the full schema contract.

**3. Behavior changes to check.**

- **`scan` exit code.** `scan`/`scan --revalidate` now exit `2` when any file
  fails to parse or ingest (previously always `0` on a non-fatal run). A clean
  scan still exits `0`; a hard error still exits `1`. Pipelines that key off the
  exit status — e.g. `musefs scan … && musefs mount …` — will now correctly stop
  on a partial-ingest failure; update any script that assumed `0`.
- **`--fallback` keys are case-insensitive.** A per-field `--fallback
  AlbumArtist=…` (or any non-lowercase key) is now matched against the template
  field instead of silently never applying. If you worked around the old bug by
  lowercasing keys, no change is needed; uppercase keys now take effect.
- **`df` on the mount** now shows a synthetic capacity instead of zeros.
- **Extended attributes** (`getxattr`/`setxattr`/…) now return `ENOTSUP`
  explicitly on the read-only mount; the caller-visible result is unchanged, but
  the per-probe `[Not Implemented]` warning is gone.

**4. External writers** (beets, Picard, Lidarr, `python-musefs`) version
independently and need no change for this upgrade: the new `fingerprint` /
`content_hash` columns are scanner-owned and nullable, so the external-writer
contract is unchanged. Update those packages on their own cadence.

## Earlier releases

For v1.0.0 and earlier, see the [Changelog](changelog.md).
