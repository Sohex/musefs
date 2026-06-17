# lidarr-musefs

A [Lidarr](https://lidarr.audio/) integration that syncs Lidarr's metadata
into a [musefs](../introduction.md) SQLite store, so a live musefs mount shows a
re-tagged view of your library without Lidarr ever copying, moving, or
rewriting backing audio bytes.

Lidarr stays the downloader, matcher, and metadata source; its destination
tree becomes a placeholder of symlinks that exists only so Lidarr can track
files. Point Navidrome, Plex, Jellyfin, or other consumers at the musefs
mount instead.

## How it fits together

The package installs two console scripts that plug into Lidarr's hooks:

- **`musefs-lidarr-import`** (Import Using Script) — replaces Lidarr's own
  copy/move when it imports a download: it creates the destination entry as a
  **symlink** (or hardlink) to the downloaded file and **fails closed** — it
  never falls back to copying bytes.
- **`musefs-lidarr-sync`** (Custom Script notification) — fires after an
  import or rename: it queries Lidarr's API for the affected tracks' metadata
  (title, artist/albumartist, album, track/disc numbers, release date,
  MusicBrainz ids, genres) plus each album's cover art, runs `musefs scan` on
  the files to create/refresh their track rows (the structural columns only
  musefs can compute), and writes the tags and art into the store. Transient
  API failures (network errors, timeouts, 5xx) are retried with backoff so a
  blip or a Lidarr restart mid-import doesn't silently drop the sync.

musefs's auto-refresh surfaces each sync at the mount with no remount. Both
scripts build on the shared [`python-musefs`](python-musefs.md)
store-contract library.

## Install

Install the package — with its `python-musefs` dependency — into the
environment Lidarr uses to run custom scripts, so both scripts are on
Lidarr's `PATH`:

```bash
pip install lidarr-musefs
```

This pulls in the shared [`python-musefs`](python-musefs.md) dependency
from PyPI automatically. To install from a checkout instead (e.g. for
development), install both editable so imports resolve to the local source:

```bash
pip install -e contrib/python-musefs
pip install -e contrib/lidarr
```

You also need the `musefs` binary reachable by the sync script (see
`MUSEFS_BIN` below) and a musefs store/mount of your own — see the
[main README](../introduction.md).

## Required Lidarr settings

- Settings -> Media Management -> Import Using Script: enabled.
- Import Script Path: `musefs-lidarr-import`.
- Metadata Provider -> Write Audio Tags: `Never`.
- File Date: `None`.
- Linux permission management: disabled.

Do not rely on Lidarr's built-in "Use Hardlinks instead of Copy" for this
workflow. Lidarr uses a hardlink-or-copy transfer mode internally, so a hardlink
failure can copy bytes. `musefs-lidarr-import` creates the destination entry
itself and fails closed.

`musefs-lidarr-sync --doctor` verifies these settings over the API (see
[Doctor](#doctor)).

## Lidarr Custom Script

Configure a Custom Script notification (Settings -> Connect):

- On Release Import: enabled.
- On Rename: enabled.
- On Album Delete: enabled.
- On Artist Delete: enabled.
- Path: `musefs-lidarr-sync`.

Test events exit successfully without touching files or the database.
`TrackRetag` events are skipped with a warning because they fire after Lidarr
writes tags.

## Environment

Both scripts are configured through environment variables, set in the
environment Lidarr launches scripts with.

Import script:

```bash
MUSEFS_LIDARR_LINK_MODE=symlink   # default; use hardlink only if symlinks are unsuitable
```

Sync script:

```bash
MUSEFS_DB=/path/to/musefs.db      # the musefs SQLite store (required)
MUSEFS_BIN=musefs                 # musefs executable; full path if not on PATH
MUSEFS_LIDARR_URL=http://localhost:8686
MUSEFS_LIDARR_API_KEY=your-api-key
MUSEFS_LIDARR_AUTOSCAN=1          # default; runs `musefs scan` before each sync
```

API keys are redacted from logs and errors.

## Manual backfill

To sync every track file Lidarr already knows about (e.g. on first setup):

```bash
musefs-lidarr-sync --all
```

Manual backfill requires `MUSEFS_LIDARR_URL` and `MUSEFS_LIDARR_API_KEY`. It
runs the doctor preflight first (skip with `--skip-lidarr-preflight`), then
queries all Lidarr artists and syncs their known track files into the musefs
DB.

## Migrating an existing Lidarr library

The forward path above (new import → import script symlink → sync) works
cleanly on a fresh import. Re-homing a **pre-existing** Lidarr library onto the
musefs symlink tree runs into several Lidarr behaviors; this is the working
order (observed on Lidarr v1, lsio image). None of it is a musefs bug — these
are Lidarr quirks an integrator only hits here.

1. **Reassign the artists to the new (musefs) root folder.**
2. **Clear the stale trackfile records before re-importing.** If the artists'
   existing trackfiles still reference the *old* root, re-import fails with
   `NotParentException` (`/old/root/... is not a child of /new/root`) —
   Lidarr's `RemoveExistingTrackFiles` chokes computing the relative path.
   Delete the stale trackfile records first.
   - **The empty-root deletion guard:** Lidarr blocks trackfile deletion while
     the new root folder is empty ("Artist's root folder is empty", a
     mass-deletion safety guard) — a chicken-and-egg with the symlinks not
     existing yet. Drop a placeholder file in the root until the first symlinks
     land, then remove it.
   - **Batch the bulk delete:** `DELETE /api/v1/trackfile/bulk` returns 500 on
     large batches (~200 ids); send ~25 ids per call.
3. **Re-import.** The import script creates the destination symlinks.
4. **Backfill the store:** `musefs-lidarr-sync --all`.

Point `musefs scan` at the **backing directory, not the symlink tree.** The
default (`--follow-symlinks` off) is exactly right here: the store should key
off the real files, while Lidarr's symlink tree is just its own tracking view.

## Doctor

To verify your Lidarr settings are musefs-safe:

```bash
musefs-lidarr-sync --doctor
```

The doctor checks Lidarr's API for:

- `writeAudioTags = no`
- `fileDate = none`
- `setPermissionsLinux = false`

If `MUSEFS_LIDARR_URL` and `MUSEFS_LIDARR_API_KEY` are not configured, `doctor`
and sync fail because the integration cannot verify safe settings or build
complete per-track metadata.

`--doctor` is a **runtime / post-deploy** check, not an offline one: it makes a
live Lidarr API call, so it needs `MUSEFS_LIDARR_URL` + `MUSEFS_LIDARR_API_KEY`
and a reachable Lidarr instance. Run it after deployment, not at container
**build** time — offline it fails with connection-refused even when the
toolchain itself is wired up correctly. There is no offline "are the binary and
plugins installed/wired" check; to confirm installation at build time, test that
the `musefs-lidarr-import` / `musefs-lidarr-sync` scripts and the `musefs`
binary are importable/on `PATH`.

## Smoke test

1. Build and install musefs.
2. Install `python-musefs` and `lidarr-musefs` into the environment Lidarr uses
   for custom scripts.
3. Configure Import Using Script and Custom Script as described above.
4. Import a small album.
5. Confirm Lidarr's destination entry is a symlink by default.
6. Run `musefs mount /tmp/mnt --db "$MUSEFS_DB"`.
7. Confirm the mount shows Lidarr metadata.
8. Confirm the source file's bytes and mtime did not change.

## Notes

- **Tags are fully replaced** with Lidarr's view on every sync (scanner-written
  binary tags always survive — see the
  [external-writer contract](../architecture/store.md#the-external-writer-contract)).
- **Cover art:** each album's Lidarr cover is fetched and written as the front
  cover, replacing the track's art rows on every sync (an over-cap or
  unreachable cover is skipped, leaving any scanner-ingested art in place).
- **Schema version:** the sync refuses to run if the DB's `user_version`
  differs from the version it targets — rebuild the store after upgrading
  musefs.
- **Deletions prune by MusicBrainz id, scoped to rows this plugin owns.** On an
  Album/Artist delete, the sync removes the matching store rows
  (`musicbrainz_albumid` / `musicbrainz_artistid`) so the mount stops presenting
  them. The backing audio is never touched — pruning only drops the store rows,
  not the files Lidarr keeps in the backing directory. A delete event for a
  release with no MusicBrainz id cannot be mapped and is logged and skipped.
- **Ownership marker.** Every track the sync writes is stamped with a
  `musefs_lidarr_managed=1` tag, and a delete only removes rows carrying that
  marker. Without it, a `musicbrainz_albumid` the *scanner* seeded from a file's
  own native tags is indistinguishable from one Lidarr wrote, so an unrelated
  Lidarr delete could drop an unmanaged track's metadata. The marker is a normal
  text tag, so it **does appear in served files** (e.g. as a
  `MUSEFS_LIDARR_MANAGED` Vorbis comment / a `TXXX` frame / an iTunes freeform
  atom). A track imported under an older plugin version (before the marker
  existed) is treated as unmanaged and is left in place on delete — re-sync it to
  stamp the marker.
- **CI coverage:** a fast smoke (real Lidarr exec path + mocked API) gates PRs,
  and a full real-instance download-client import e2e gates the Python
  releases — see
  [the Python plugins guide](../contributing/plugins.md#python-plugins-contrib).

## Tests

```bash
cd contrib/lidarr
python -m venv .venv && source .venv/bin/activate
pip install -e ../python-musefs    # shared library (editable, from the working tree)
pip install -e ".[test]"

python -m pytest                   # unit + integration (no Rust binary)
python -m pytest -m musefs_bin     # path-matching gate vs the real `musefs` binary
```

The `musefs_bin` gate shells out to the real `musefs` binary, so build it first
from the repo root (`cargo build`). It is deselected from the default run and
skips cleanly if the binary is absent.
