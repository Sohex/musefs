# Plugin deletion pruning (issue #422)

## Problem

None of the source integrations prune store rows when the source tool reports a
deletion, so deleting content at the source leaves stale rows and the mount keeps
presenting files the source no longer tracks.

- **beets** (`contrib/beets/beetsplug/musefs.py`) registers no
  `item_removed`/`album_removed` listeners. `_reconcile_pending` already runs a
  full existence-based `prune_missing` at `cli_exit`, but it early-returns unless
  something populated `_pending` (an `after_write`/`item_imported`/
  `album_imported`). A bare `beet remove`/`remove -d` populates nothing, so it
  prunes nothing.
- **Lidarr** (`contrib/lidarr/src/musefs_lidarr/events.py`) recognizes only
  `Test`/`AlbumDownload`/`Rename`/`TrackRetag`; deletion events map to
  `UNSUPPORTED` and are skipped. The rename path (`sync_rename_prune`) is the only
  prune today.
- **Picard** never calls `prune_missing` — and correctly so (see Scope).

## Scope

Plugins only. No `musefs-core`/scan changes, no schema/`user_version` change. The
deferred "refind-before-prune" `revalidate` work mentioned in the
backing-file-checksums spec is a separate concern and out of scope here.

**Picard is out of scope.** The Picard plugin is an on-demand interactive action:
the user selects files and syncs them. It has no deletion event in its model, so
there is nothing to wire.

## Key finding: the two integrations need different mechanisms

The choice of pruning mechanism is forced by each source's file topology, verified
against the Lidarr source tree (`/home/cfutro/git/Lidarr`, `develop`).

### beets manages files in place → existence-based

beets owns the audio files directly; `beet remove -d` deletes the backing file
synchronously within the command. So by `cli_exit` the on-disk state is final and
the existing existence-based `prune_missing` (delete rows whose `backing_path` no
longer exists) is exactly right:

- `beet remove` (library only, file kept on disk) → row **retained** (musefs can
  still serve the bytes; the store contract is keyed on backing files).
- `beet remove -d` (file deleted) → backing gone → row **pruned**.

### Lidarr keeps backing files untouched → existence-based is structurally wrong

Per `contrib/lidarr/README.md` (lines 145–147), `musefs scan` points at the
**backing directory, not Lidarr's symlink tree**, with `--follow-symlinks` off:
the store keys off the real downloaded files, and Lidarr's destination tree is
"just its own tracking view" of symlinks (or hardlinks) pointing at those backing
files.

A Lidarr deletion only removes entries from *its symlink tree*; it never touches
the backing directory. So `os.path.exists(backing_path)` stays `True` and an
existence-based prune can **never** fire for a Lidarr deletion, in either link
mode. Three facts from the Lidarr source make existence-based and API-based
approaches unworkable, and force an intent-based mapping:

1. **No per-file delete event.** Lidarr's CustomScript emits only `ArtistDeleted`
   and `AlbumDeleted` (`CustomScript.cs` `OnArtistDelete`/`OnAlbumDelete`). There
   is no `TrackFileDelete` script event. Neither event carries per-file paths.
2. **The record is already gone from Lidarr's DB.** `AlbumService.DeleteAlbum`
   deletes the album row *before* publishing `AlbumDeletedEvent`, so an API
   enumeration (`GET /api/v1/trackfile?albumId=`) at script time queries a
   deleted album. API-based mapping is unreliable.
3. **File/symlink deletion is asynchronous, after our script.**
   `EventAggregator.PublishEvent` runs synchronous `IHandle` handlers first and
   blocks on them; `MediaFileDeletionService` deletes files via `IHandleAsync`,
   dispatched to a background task afterward. `NotificationService` (which runs
   our custom script) is a synchronous `IHandle`, so our script runs **before**
   anything is unlinked. Any filesystem observation at event time sees the
   pre-deletion state.

What the events *do* carry is the MusicBrainz identity, which we already store as
tags: `AlbumDeleted` → `Lidarr_Album_MBId`, `ArtistDeleted` → `Lidarr_Artist_MBId`;
the sync writes `musicbrainz_albumid` / `musicbrainz_artistid`
(`mapping.py` `build_pairs`). So we map the delete event to rows by MBID and delete
them outright — intent-based, race-free, API-free, no schema change.

### `DeletedFiles` is ignored

`AlbumDeleteMessage`/`ArtistDeleteMessage` expose a `DeletedFiles` bool — the
"also delete the files from disk" checkbox in Lidarr's delete dialog
(`true` = "Album removed and all files were deleted", `false` = "Album removed,
files were not deleted"). It is emitted as `Lidarr_Artist_DeletedFiles` for
**both** events (there is no `Lidarr_Album_DeletedFiles`). In musefs's topology the
"files" are Lidarr's own symlink entries, never the backing directory, so the prune
decision ("Lidarr stopped tracking this album/artist → drop the rows") is identical
either way. We ignore it.

## Design

### 1. Shared store layer (`python-musefs`)

Two additions to `musefs_common/store.py`, mirroring `prune_missing`'s style, and
the intent-based counterpart to it:

- `track_ids_by_tag(conn, key, value)` — return a `list[int]` (possibly empty,
  order unspecified) of track-ids whose text tag `(key, value)` matches (e.g.
  `("musicbrainz_albumid", mbid)`). Scoped to plugin-owned text rows
  (`value_blob IS NULL`).
- `delete_tracks(conn, track_ids)` — unconditional `DELETE FROM tracks WHERE id = ?`
  per id; returns the count of rows **actually deleted** (sum of per-statement
  `rowcount`, so an already-gone id contributes 0). Relies on the same FK
  `ON DELETE CASCADE` that `prune_missing` already depends on (and the
  `foreign_keys = ON` pragma `connect()` sets), so tags and art rows are removed
  with the track.

Both are exported from `musefs_common/__init__.py`. Lidarr picks up the new
exports through its `python-musefs` pip dependency (it does not vendor). Picard
**does** vendor `musefs_common`, so the byte-identical drift gate
(`vendor_to_picard.py` / `test_vendor_sync.py`) requires re-running
`vendor_to_picard.py` and committing the regenerated Picard `store.py` /
`__init__.py` even though Picard does not call the new functions. The
`python-musefs` public-API test is updated for the two new names.

### 2. Lidarr — intent-based prune by MBID

- **`events.py`**: add `ARTIST_DELETED = "ArtistDeleted"` and
  `ALBUM_DELETED = "AlbumDeleted"` to `EventType`. `parse_event` extracts
  `Lidarr_Album_MBId` / `Lidarr_Artist_MBId` into new `LidarrEvent` fields
  (`album_mbid`, `artist_mbid`).
- **`sync.py`**: add `prune_deleted(*, config: SyncConfig, event) -> int` that
  opens the DB (`config.db_path`) and deletes tracks by identity:
  - `AlbumDeleted` → `delete_tracks(conn, track_ids_by_tag(conn, "musicbrainz_albumid", event.album_mbid))`
  - `ArtistDeleted` → `delete_tracks(conn, track_ids_by_tag(conn, "musicbrainz_artistid", event.artist_mbid))`

  Commit/rollback like `sync_rename_prune`. It needs only `SyncConfig` (the
  DB path) — never `LidarrConfig`/`client_factory`.
- **`cli_sync.py`**: dispatch the two new event types to `prune_deleted` **before**
  the `config.enabled` / doctor-preflight / API block — deletion is a purely local
  DB operation needing neither the Lidarr API nor a scan. `config_from_env(env)`
  (which raises `ConfigError` if `MUSEFS_DB` is unset) is called for delete events
  **inside the existing `try`** so the error maps to the exit-1 path; no
  `LidarrClient` is ever constructed. Print a summary
  (`musefs-lidarr-sync: pruned N rows`).
- **No-MBID events**: a delete event with an empty/missing MBID (`album_mbid` /
  `artist_mbid` parsed as `None`) cannot be mapped. Log it to stderr (`… delete
  event carried no MusicBrainz id; cannot prune, leaving rows for the next
  scan/reconcile`), open no DB connection, and return exit 0 — never fail
  Lidarr's hook. These rows linger until a manual rescan/reconcile; accepted (rare
  for Lidarr-managed libraries).
- **Release-group granularity (accepted limitation)**: Lidarr models an "album" as
  a MusicBrainz **release group** and `Lidarr_Album_MBId` is its release-group id;
  the sync stores that same value in `musicbrainz_albumid`. So the album-delete
  prune removes every stored row carrying that release-group id. Within one Lidarr
  instance this is exact (Lidarr keys albums uniquely by `ForeignAlbumId`, so one
  id ↔ one album's track files). The only over-prune risk is a **mixed store**
  where non-Lidarr rows happen to carry the same `musicbrainz_albumid` value;
  accepted as a limitation (rare; the backing bytes are untouched and the rows
  rebuild on the next scan/sync). We do not store the numeric Lidarr album id, so
  tighter scoping is out of scope here.

### 3. beets — existence-based prune on removal

- **`__init__`**: register `item_removed` and `album_removed` listeners.
- The handlers set a new `_saw_removal` flag (they do **not** feed the prune).
  Their job is solely to make `_reconcile_pending` run on a removals-only command;
  removed items are deliberately not scanned or synced.
- **`_reconcile_pending`**: change the early-return guard to run when `_pending`
  is non-empty **or** `_saw_removal` is set. The prune step is the **existing
  unscoped** `prune_missing(db_path)` (full-DB existence sweep, `items=None`) —
  it already covers removed-and-deleted files *and* moved-away backing files in
  one pass, so the removed items themselves never need to be threaded into the
  prune. Written items (from `_pending`) still go through scan + sync as today.
  Because beets deletes files synchronously within the command, the on-disk state
  is final by `cli_exit`: `remove -d` → row pruned; `remove` (file kept) → row
  retained.
- Stays best-effort: a passive `cli_exit` hook never aborts the beets command
  (existing warning / `print_` degradation preserved).

### 4. Picard

No change.

## Error handling

- **beets** — unchanged passive-hook posture: environmental failures (locked DB,
  vanished file, wedged scan, read-only DB) degrade to a warning / `print_`; an
  unexpected exception still propagates so a real bug surfaces.
- **Lidarr** — `prune_deleted` failures surface as the existing
  `MusefsLidarrError`/`LidarrApiError` → stderr + exit 1 pattern; the no-MBID case
  is a benign skip (exit 0).

## Testing

- **Store** (`python-musefs/tests`): `track_ids_by_tag` matches the right rows
  (set comparison, order unspecified) and ignores binary tags; `delete_tracks`
  removes the track and cascades to its tags and art, and returns the count of
  rows actually deleted (0 for an already-gone id); public-API test updated for
  the two new exports.
- **Lidarr**: `events.py` parse tests for `ArtistDeleted`/`AlbumDeleted` (MBID
  extraction, including the empty/missing-MBID → `None` case); `prune_deleted`
  integration test — album delete prunes the rows whose `musicbrainz_albumid`
  matches and leaves a different album's rows intact, artist delete prunes all the
  artist's rows, and **the backing files are left on disk** in every case;
  no-MBID delete event → returns exit 0, prints the caveat to stderr, and opens
  **no DB connection**; a `cli_sync` dispatch test asserting **no `LidarrClient`
  is constructed** for delete events.
- **beets**: `item_removed` with file deleted → pruned; `item_removed` without
  delete (file present) → retained; a removal-only command (no writes) still
  triggers the reconcile/prune.

All commits stay green under the pre-commit full workspace + contrib suites (the
beets venv and the vendored-tree re-vendor step apply).

## Docs

- **`contrib/lidarr/README.md`**: add "On Album Delete" / "On Artist Delete" to the
  Custom Script settings, and a note that deletions prune by MusicBrainz id while
  the backing bytes stay untouched (and the no-MBID caveat).
- **`contrib/beets/README.md`**: note that `remove -d` prunes the store row while a
  bare `remove` (file kept) retains it.
- **`ARCHITECTURE.md`** external-writer-contract section: a line distinguishing
  existence-based pruning (in-place writers like beets) from intent-based pruning
  (link-tree writers like Lidarr).

## Out of scope / non-goals

- Picard deletion handling (no deletion concept in its model).
- `revalidate` refind-before-prune ("#422 second pass").
- Any `musefs-core`/scan or schema change.
- Recovering deletions for Lidarr releases with no MusicBrainz id (left to a
  manual rescan/reconcile).
- Release-scoped (vs release-group-scoped) Lidarr album deletion, and avoiding
  over-prune in a mixed store sharing `musicbrainz_albumid` values — would require
  storing the numeric Lidarr album id.
