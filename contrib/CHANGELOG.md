# Python packages changelog

Changelog for the musefs `contrib/` Python packages — `python-musefs`,
`beets-musefs`, `lidarr-musefs`, and the (unpublished) Picard plugin. These
share a single version, released on `py-v*` tags and decoupled from the Rust
crate version tracked in the [root CHANGELOG](../CHANGELOG.md).

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and these packages adhere to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
