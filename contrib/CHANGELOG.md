# Python packages changelog

Changelog for the musefs `contrib/` Python packages — `python-musefs`,
`beets-musefs`, `lidarr-musefs`, and the (unpublished) Picard plugin. These
share a single version, released on `py-v*` tags and decoupled from the Rust
crate version tracked in the [root CHANGELOG](../CHANGELOG.md).

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and these packages adhere to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **`musefs_common.ScanResult`** — what a completed `run_scan` did. Carries
  `binary`, `target`, `verb`, `returncode`, `partial` and `stderr`, and renders
  the shared non-fatal message via `.warning()` (`None` for a clean run). See
  the `run_scan` change below.
- **`musefs_common.MAX_TAG_VALUE_LEN`** — the store's byte cap on a
  `tags.value`, generated from the Rust constant into the schema mirror rather
  than hand-kept. A writer can now check a value against the contract instead of
  discovering the limit as an `IntegrityError` from the `CHECK`. The cap moved
  in the same change (musefs #644), which is exactly why it should not be a
  literal in anyone's source.

### Changed

- **`run_scan` no longer treats a partial scan as a failure** (musefs #647).
  `musefs scan` exits `2` when the batch completed and committed but some file
  could not be ingested; `run_scan` raised `ScanError` for any non-zero code, so
  a single unparseable file took down the whole sync. Picard reported
  `sync failed` and wrote **no tags at all**; the beets `cli_exit` hook skipped
  its sync silently, surfacing only as a `WARNING` beets hides at default
  verbosity; `beet musefs` and the Lidarr adapter aborted outright. `run_scan`
  now reads the three-state contract — `0` success, `2` partial, anything else a
  hard failure — and returns a `ScanResult` for the first two. All three
  adapters warn and go on to sync. Hard failures still raise `ScanError`.
  **Callers of `run_scan` that relied on it raising for exit `2` need updating.**
- **`SchemaMismatch` now names the direction of the skew and the remedy**
  (musefs #654). It reported both version numbers and said the versions "have
  diverged", leaving the user to work out which side was behind and what to do.
  It now says whether the store was written by a newer musefs (upgrade the
  plugin) or predates the plugin (upgrade musefs and run `musefs scan`, which
  migrates in place). This string is what Picard and beets surface verbatim.
- The store schema is now at `user_version` 3 (musefs #644 widens the
  `tags.value` and `track_art.description` caps). `EXPECTED_USER_VERSION`
  tracks it automatically; no plugin change is needed, but a store must be
  migrated by `musefs scan`/`musefs mount` from a build carrying that migration
  before these packages will open it.

## [1.1.0] - 2026-06-17

### Changed

- **beets: pruning is now a deliberate act.** The passive `cli_exit` reconcile
  hook no longer prunes store rows — it only syncs touched items. Previously
  every command ran an unscoped, existence-based `prune_missing` over the whole
  library, so a transient backing-storage loss (an unmounted share, an offline
  drive, a momentary realpath divergence) mass-deleted plugin-written metadata.
  Pruning rows for moved-away/deleted files now happens only on the explicit
  `beet musefs` command (or `musefs scan`); the `item_removed`/`album_removed`
  listeners are removed (#538).

### Fixed

- **Lidarr deletes no longer touch unmanaged tracks.** `prune_deleted` mapped an
  Album/Artist delete to store rows by `musicbrainz_albumid` /
  `musicbrainz_artistid`, which also matched ids the *scanner* seeded from a
  file's own native tags. Lidarr now stamps a `musefs_lidarr_managed=1` ownership
  marker on every track it writes and only deletes rows carrying it, so an
  unrelated delete can't drop an unmanaged track's metadata. The marker is a
  normal text tag and appears in served files (#546).
- **Lidarr no longer records duplicate album/artist tags for single-file
  releases.** A backing file linking multiple tracks (cue-style) emitted the
  album/artist-level fields (`artist`, `album`, `date`, the MBIDs, genres) once
  per linked track, so the store held N duplicate copies. Album/artist-level tags
  are now emitted once per file; only track-level tags repeat per track (#539).
- **Schema guard now covers the destructive prune/delete paths.** beets
  `_prune_missing`, and Lidarr `sync_rename_prune` / `prune_deleted`, ran without
  `check_schema_version`, so an out-of-date plugin could still mass-delete/prune
  a store whose schema it does not understand. These paths now refuse on a
  `user_version` mismatch (#545).

- **Duplicate rendered tags from case-only key differences:** a `musefs scan`
  seeds an unmapped tag under the backing file's native key case (e.g. Vorbis
  `LABEL`), while the beets/Picard plugins canonicalize keys to lowercase
  (`label`). `merge_tags` deleted by exact key, so the plugin's `label` insert
  never displaced the scanner's `LABEL` and both rows survived — rendering a
  duplicated value. The merge/delete key match is now case-insensitive, so a
  writer's canonical lowercase key replaces the scan-seeded native-case row;
  existing duplicates self-heal on the next sync of the affected key (#407).
- **beets reconcile failures no longer silent:** the beets `cli_exit` reconcile
  hook degraded every failure to a `_log.warning`, which beets hides at default
  verbosity — so a persistent setup failure (read-only DB, `EACCES`) became a
  silent no-op. Persistent permission/read-only failures are now surfaced loudly
  via `ui.print_` while transient failures (locked DB, vanished file) stay quiet;
  the beets operation is still never aborted (#405).

## [1.0.0] - 2026-06-12

First stable release.

### Added

- PyPI distribution: `python-musefs`, `beets-musefs`, and `lidarr-musefs` are
  published to PyPI on `py-v*` tags via a trusted-publishing release workflow.
