# Python packages changelog

Changelog for the musefs `contrib/` Python packages — `python-musefs`,
`beets-musefs`, `lidarr-musefs`, and the (unpublished) Picard plugin. These
share a single version, released on `py-v*` tags and decoupled from the Rust
crate version tracked in the [root CHANGELOG](../CHANGELOG.md).

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and these packages adhere to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [2.0.0] - 2026-09-14

### Added

- **Synced art states its dimensions** (musefs #737). `sync_files` linked a
  plugin's art with `width` and `height` unset, so a FLAC whose picture a plugin
  replaced served a block declaring no dimensions until the file was rescanned.
  It now reads them from each image's own header — PNG's `IHDR`, a JPEG's
  start-of-frame — with the new `image_dimensions(data)`, which decodes nothing
  and needs no dependency. `replace_track_art` accepts
  `(art_id, picture_type, description, mime, width, height)` rows alongside the
  four-field ones, which still leave them unset. A WebP, an unreadable header or
  a zero dimension stays `NULL`, and bit depth and colour count are still not
  written.

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

- **`replace_track_art` takes a fourth element per row: the mime.** Rows are now
  `(art_id, picture_type, description, mime)`. From schema v4 the MIME type
  lives on the `track_art` link rather than on the deduplicated `art` row,
  because it describes one file's picture block and not the bytes every file
  shares — and it is the value musefs writes into the synthesized picture block,
  so a link stored without one produces art whose declared type is the empty
  string. `sync_files` passes each `ArtImage`'s own mime, so callers using it are
  unaffected; a caller driving `replace_track_art` directly must add the field.

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

- **Three more store rules a direct writer must follow** (musefs #758, #761,
  #762). None changes what these helpers do, since they already comply, but a
  third-party writer that breaks one now gets an abort:
  - `tracks.backing_path` is at most 64 KiB, as bytes.
  - A digest is 64 lowercase hex characters. That covers `art.sha256`, which
    `upsert_art` already writes in that form, and the scanner-owned
    `fingerprint` and `content_hash`.
  - A track's `id` cannot be updated once assigned.

- **`backing_path` is bytes.** Schema v4 changes the column from `TEXT` to
  `BLOB`, because a filesystem path is a byte string. SQLite never compares a
  `TEXT` value equal to a `BLOB`, so every query that matches on a path had to
  move with it — and the failure mode if it does not is silence, not an error:
  the lookup simply matches no row. `track_id_for_path`, `track_ids_for_paths`
  and `prune_missing` encode and decode at the boundary, so their callers are
  unaffected and keep passing and receiving `str`.

  **A writer that queries `tracks` directly must do the same.** The two helpers
  doing it are exported for that: `path_param(key)` encodes on the way in and
  `path_value(raw)` decodes on the way out, via `os.fsencode`/`os.fsdecode` so
  the round trip is lossless.

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
  plugin) or predates the plugin (upgrade musefs and run `musefs migrate`, which
  upgrades it in place). This string is what Picard and beets surface verbatim.
- The store schema is now at `user_version` 4. V3 widens the `tags.value` and
  `track_art.description` caps (musefs #644); V4 rebuilds the core tables — the
  path, art and ownership changes above — and retires every stored
  `tracks.fingerprint`, whose value now includes sampled audio (musefs #691).
  `EXPECTED_USER_VERSION` tracks it automatically, and a store must be upgraded
  with `musefs migrate` from a build carrying those migrations before these
  packages will open it. `fingerprint` is scanner-owned and was never part of
  the tag contract.

### Removed

- **`upsert_art` no longer takes a mime.** It is `upsert_art(conn, data)` now.
  The argument was already being ignored whenever the image had been seen before
  — the insert is `ON CONFLICT(sha256) DO NOTHING`, so the stored row won — and
  from schema v4 there is no column on `art` for it to write.
  `replace_track_art` is where the mime goes now — see its entry under
  *Changed*, which is the release that put it there. `sync_files` callers are
  unaffected; a caller driving `upsert_art` directly must drop the argument, and
  a caller that was relying on it to record the mime must pass one to
  `replace_track_art`.

### Fixed

- **`upsert_art` refuses an `art` row whose digest names other bytes**
  (musefs #724). On a `sha256` conflict it returned the row already filed under
  the digest without looking at it, so a crafted or corrupt store holding a row
  whose `sha256` does not match its `data` handed that row's image to every
  track syncing the real one. It now compares the conflicting row's bytes with
  the incoming image and raises the new `ArtDigestMismatch` on a mismatch. That
  exception is a `sqlite3.IntegrityError`, so `sync_one` skips the one record
  and counts it under `skipped_invalid`, exactly as it does for a constraint
  violation, rather than aborting the sync. `musefs scan` does the same on the
  Rust side.

- **A backing path that is not valid UTF-8 now resolves to the right track, and
  two such files stay two.** `realpath_key` used to normalize undecodable bytes
  to `U+FFFD`, reproducing what the scanner itself stored back when it wrote a
  lossy conversion. Both sides agreed, and both were wrong: that mapping is not
  injective, so two files differing only in such a byte collapsed onto one key —
  and onto one row. From musefs 2.0.0 the scanner stores the real bytes, so the
  old form matched nothing at all and a plugin skipped those files silently.

  The key now resolves on bytes and decodes with `os.fsdecode`, so
  `os.fsencode(realpath_key(p))` is exactly the path on disk. `path_param` and
  `path_value` use `os.fsencode`/`os.fsdecode` rather than a hardcoded `utf-8`,
  which keeps both directions on one codec and makes the round trip hold
  whatever the filesystem encoding is.

  **This changes what `realpath_key` returns** for a non-UTF-8 path: the byte
  itself, as the filesystem encoding spells it (a surrogate such as `\udc80`
  under UTF-8), where it used to give `U+FFFD`. A caller that stores or compares
  keys across the upgrade should recompute them. Callers that pass the key
  straight to `sync_files` need no change.

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

## [1.2.0] - 2026-06-18

Recorded after the fact: this release shipped without a section here.

### Changed

- **`run_scan` drives the `musefs revalidate` subcommand.** With
  `revalidate=True` it runs `musefs revalidate <targets…>`, adding `--prune`
  when `prune=True`, instead of `musefs scan --revalidate`, which musefs 1.2.0
  deprecated. `force=True` adds `--force` to a plain scan. `beet musefs
  --revalidate` forwards to `musefs revalidate --prune`, so it still prunes rows
  whose backing file is gone.

### Fixed

- **`run_scan` rejects flag combinations it would otherwise drop.** `force`
  with `revalidate`, `prune` without `revalidate`, and an empty target list each
  raise `ValueError`, instead of silently losing a flag or failing with an
  `IndexError`.

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
