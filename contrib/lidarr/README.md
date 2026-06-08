# lidarr-musefs

A Lidarr integration that lets Lidarr import into a placeholder library tree
while musefs serves the real consumer-facing, re-tagged FUSE view.

The supported workflow keeps Lidarr as the downloader, matcher, and metadata
source, but prevents Lidarr from copying, moving, or rewriting backing audio
bytes. Lidarr's destination tree exists so Lidarr can track files. Point
Navidrome, Plex, Jellyfin, or other consumers at the musefs mount instead.

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

## Environment

Import script:

```bash
MUSEFS_LIDARR_LINK_MODE=symlink   # default; use hardlink only if symlinks are unsuitable
```

Sync script:

```bash
MUSEFS_DB=/path/to/musefs.db
MUSEFS_BIN=musefs
MUSEFS_LIDARR_URL=http://localhost:8686
MUSEFS_LIDARR_API_KEY=your-api-key
MUSEFS_LIDARR_AUTOSCAN=1
```

API keys are redacted from logs and errors.

## Lidarr Custom Script

Configure a Custom Script notification:

- On Release Import: enabled.
- On Rename: enabled.
- Path: `musefs-lidarr-sync`.

Test events exit successfully without touching files or the database.
`TrackRetag` events are skipped with a warning because they fire after Lidarr
writes tags.

## Manual backfill

Run:

```bash
musefs-lidarr-sync --all
```

Manual backfill requires `MUSEFS_LIDARR_URL` and `MUSEFS_LIDARR_API_KEY`. It
queries all Lidarr artists and syncs their known track files into the musefs DB.

## Doctor

Run:

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
