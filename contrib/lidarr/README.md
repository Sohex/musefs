# lidarr-musefs

A [Lidarr](https://lidarr.audio/) integration that syncs Lidarr's metadata
into a [musefs](../../README.md) SQLite store, so a live musefs mount shows a
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
  MusicBrainz ids, genres), runs `musefs scan` on the files to create/refresh
  their track rows (the structural columns only musefs can compute), and
  writes the tags into the store.

musefs's auto-refresh surfaces each sync at the mount with no remount. Both
scripts build on the shared [`python-musefs`](../python-musefs/README.md)
store-contract library.

## Install

Install the package — with its `python-musefs` dependency — into the
environment Lidarr uses to run custom scripts, so both scripts are on
Lidarr's `PATH`:

```bash
pip install lidarr-musefs
```

Or, from this repository (the working-tree library first — it is the local
source of the `python-musefs` dependency):

```bash
pip install -e contrib/python-musefs
pip install -e contrib/lidarr
```

You also need the `musefs` binary reachable by the sync script (see
`MUSEFS_BIN` below) and a musefs store/mount of your own — see the
[main README](../../README.md).

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
  [external-writer contract](../../ARCHITECTURE.md#the-external-writer-contract)).
- **No art sync:** the integration writes text tags only; any art `musefs scan`
  ingested from embedded pictures is preserved.
- **Schema version:** the sync refuses to run if the DB's `user_version`
  differs from the version it targets — rebuild the store after upgrading
  musefs.
- **CI coverage:** a fast smoke (real Lidarr exec path + mocked API) gates PRs,
  and a full real-instance download-client import e2e gates the Python
  releases — see
  [CONTRIBUTING.md](../../CONTRIBUTING.md#python-plugins-contrib).

## Tests

```bash
cd contrib/lidarr
python -m venv .venv && source .venv/bin/activate
pip install -e ../python-musefs    # shared library (unpublished in-tree; install first)
pip install -e ".[test]"

python -m pytest                   # unit + integration (no Rust binary)
python -m pytest -m musefs_bin     # path-matching gate vs the real `musefs` binary
```

The `musefs_bin` gate shells out to the real `musefs` binary, so build it first
from the repo root (`cargo build`). It is deselected from the default run and
skips cleanly if the binary is absent.
