# Release notes

Curated, upgrade-focused notes for each release. For the exhaustive,
per-change list see the [Changelog](changelog.md); for the external-writer
`contrib/` packages (which version independently) see the
[contrib changelog](integrations/overview.md#contrib-changelog).

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
