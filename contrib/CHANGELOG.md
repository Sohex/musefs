# Python packages changelog

Changelog for the musefs `contrib/` Python packages — `python-musefs`,
`beets-musefs`, `lidarr-musefs`, and the (unpublished) Picard plugin. These
share a single version, released on `py-v*` tags and decoupled from the Rust
crate version tracked in the [root CHANGELOG](../CHANGELOG.md).

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and these packages adhere to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **A text-tag sync can no longer collide with a scanner-written binary tag.**
  Schema v4 replaces `tags`' primary key with a unique index that folds the
  row class in as a fourth column, so the text rows these helpers rewrite and
  the binary rows they deliberately preserve get independent ordinal spaces per
  key. `tags_for_track`, which reads both classes at once, keeps using an index
  for the lookup.
  `replace_tags` scopes its `DELETE` to `value_blob IS NULL` precisely so
  scanner-written payloads survive a sync, and that is the shape that could
  previously fail with `UNIQUE constraint failed: tags.track_id, tags.key,
  tags.ordinal` against a key a binary row already used. No API change.

- **Tag and art-link ownership is immutable.** `UPDATE tags SET track_id = ...`
  (and the same on `track_art`) is now refused by a trigger. Neither helper does
  this — both replace by delete-then-insert — so nothing here changes; it is
  noted because a third-party writer doing it will now get an abort rather than
  silently leaving the old track serving stale bytes.

- **`backing_path` is bytes.** Schema v4 changes the column from `TEXT` to
  `BLOB`, because a filesystem path is a byte string. SQLite never compares a
  `TEXT` value equal to a `BLOB`, so every query that matches on a path had to
  move with it — and the failure mode if it does not is silence, not an error:
  the lookup simply matches no row. `track_id_for_path`, `track_ids_for_paths`
  and `prune_missing` encode and decode at the boundary, so their callers are
  unaffected and keep passing and receiving `str`.

  **A writer that queries `tracks` directly must do the same.** The two helpers
  doing it are exported for that: `path_param(key)` encodes on the way in and
  `path_value(raw)` decodes on the way out, both `surrogateescape` so the round
  trip is lossless.

### Added

- **`musefs_common.path_param` / `musefs_common.path_value`** — the
  `backing_path` boundary encode and decode. See the change above.
- **`musefs_common.ScanResult`** — what a completed `run_scan` did. Carries
  `binary`, `target`, `verb`, `returncode`, `partial` and `stderr`, and renders
  the shared non-fatal message via `.warning()` (`None` for a clean run). See
  the `run_scan` change below.
- **`prune_missing`'s `unreadable` keyword** — pass a list to collect
  `(track_id, backing_path, message)` for every row kept because its backing
  path could not be stat'd, so a pass that pruned nothing can be told from one
  that could not look. Existing callers are unaffected: the return value is
  still the count actually pruned. See the `prune_missing` fix below.
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
- The store schema is now at `user_version` 4 (musefs #644 widens the
  `tags.value` and `track_art.description` caps; musefs #691 retires every
  stored `tracks.fingerprint`, whose value now includes sampled audio).
  `EXPECTED_USER_VERSION` tracks it automatically; no plugin change is needed,
  but a store must be migrated by `musefs scan`/`musefs mount` from a build
  carrying those migrations before these packages will open it. Neither
  migration touches a column these packages write: `fingerprint` is
  scanner-owned and was never part of the tag contract.

### Fixed

- **`prune_missing` no longer treats an unstattable path as a deletion**
  (musefs #692). It decided with `os.path.exists`, which answers `False` both
  for "absent" and for "there, but I could not stat it" — so a permissions
  change on a parent directory, or a network or removable mount that was
  momentarily unreachable, deleted the track row and cascaded away exactly the
  plugin-written `tags` and `track_art` rows these packages exist to preserve.
  It now deletes only on a `FileNotFoundError` and keeps the row for every other
  `OSError`, mirroring `musefs revalidate --prune`. The scoped
  `prune_missing(track_ids=…)` form gets the same rule. #538 fixed the beets
  blast radius; this fixes the shared helper every consumer calls.
- **Lidarr rename pruning says when it could not look.** `sync_rename_prune`
  now logs each store row it kept because the old path could not be stat'd, so
  a rename that pruned nothing because the mount was unreachable is
  distinguishable from one with nothing to prune.

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
