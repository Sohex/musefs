# Lidarr Integration Real-Instance Smoke Checklist

**Date:** 2026-06-07 (run 2026-06-08)
**Status:** Completed

This checklist is the durable release gate for issue #141. The automated suite
can pass without a real Lidarr instance, but the Lidarr integration is not
release-ready until this file records a completed smoke run.

## Required Observations

- Lidarr version: 3.1.0.4875 (lscr.io/linuxserver/lidarr, .NET 8.0.12, Alpine)
- Import link mode: symlink (default)
- Destination entry type: symlink — Lidarr's `ImportScriptService` invoked
  `musefs-lidarr-import`, which created symlinks for every track (Widowspeak
  10/10, Wet Leg 12/12); 0 regular files, 0 bytes copied.
- Source byte checksum before: Widowspeak aggregate
  `3dcd717c5e649d75b46bfa5ccf3f34e96fbe522384c3980b837bcad7a4e02ada`;
  Wet Leg aggregate
  `f4848d0db9d4d0e6629982b49c393c302dc2a1f74598486efc46a8bb97c266c6`
- Source byte checksum after: identical (per-file `sha256` diff empty)
- Source mtime before: captured per file (`stat -c %Y`)
- Source mtime after: unchanged (per-file diff empty)
- musefs mount metadata verified: yes — mounted at synthesis mode; served FLACs
  carry Lidarr's tags (artist/albumartist/album/date/disc/genre list/title/track
  and `musicbrainz_*` IDs matching Lidarr's foreign IDs). Decoded-audio MD5 is
  identical between the mounted file and the backing source for both a Widowspeak
  track (`74a9d300519e45740dfd5f6d9a724125`) and a Wet Leg track
  (`f01df8add27fdfcc88147474dfc7b95c`); the file sizes differ (metadata spliced),
  the audio bytes do not.
- Notes: 2026-06-08: Smoke run performed against a containerized Lidarr
  (rootless podman, isolated from the host's production instance). The run
  surfaced and fixed a blocking bug: Lidarr stores custom-script environment
  variables in a .NET `StringDictionary`, which lowercases every key, so a Linux
  script receives `lidarr_sourcepath` / `lidarr_eventtype` rather than the
  PascalCase names Lidarr's docs and source list. `import_link.py` and
  `events.py` read the PascalCase names, so every import failed
  (`Lidarr_SourcePath is required`) and every event parsed as `UNSUPPORTED`. The
  fix resolves Lidarr env vars case-insensitively (`musefs_lidarr/env.py`).
  After the fix: the import script, host-driven sync, and container-driven sync
  (invoked exactly as Lidarr's notification does — verified Lidarr executes the
  notification script via its Test event) all succeed end to end.

  Caveat: Lidarr's `On Release Import` notification only fires for
  download-client-driven imports (`NotificationService.Handle(AlbumImportedEvent)`
  early-returns when `!NewDownload`); a manual import does not raise it. The
  download-client path (indexer + grab) was not exercised. The notification
  wiring itself is confirmed (Lidarr ran the script on its Test event), and the
  sync was driven with the real `AlbumDownload` environment Lidarr would pass.
