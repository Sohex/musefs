# Issue #141: Lidarr integration

**Issue:** https://github.com/Sohex/musefs/issues/141
**Date:** 2026-06-07
**Status:** Approved for planning
**Scope:** Add a Lidarr integration under `contrib/lidarr` that routes Lidarr
import, rename, and metadata events into the musefs SQLite store without letting
Lidarr copy or rewrite backing audio bytes.

## Goal

Lidarr users need a path equivalent to the beets and Picard integrations: keep
Lidarr as the downloader, matcher, and metadata authority, but project its tag
view through musefs instead of mutating the original audio files.

The intended deployment is explicit:

- Lidarr may maintain a destination tree so its own database and UI are happy.
- That destination tree contains only symlink placeholders by default, or
  hardlinks when the user opts into them.
- Music consumers such as Navidrome, Plex, Jellyfin, or file browsers should
  point at the musefs FUSE mount, not the Lidarr destination tree.
- The original downloaded audio bytes are not copied, rewritten, retagged,
  chmodded, or timestamp-touched by the supported workflow.

## Lidarr source findings

The local Lidarr source shows a viable strict path:

- Lidarr has a media-management setting `UseScriptImport` and script path
  `ScriptImportPath` (`MediaManagementConfigResource` exposes both).
- During import, Lidarr invokes the script with `Lidarr_SourcePath` and
  `Lidarr_DestinationPath` (`ScriptImportDecider.TryImport`).
- If the script exits `0`, Lidarr treats the transfer as complete and skips its
  internal file transfer (`ScriptImportDecision.MoveComplete`).
- Lidarr's normal built-in hardlink setting calls `TransferMode.HardLinkOrCopy`,
  so a hardlink failure can fall back to a byte copy. The musefs integration must
  not rely on that path as the primary invariant-preserving mode.
- After import, Lidarr records the destination path and fires `AlbumDownload`;
  the Custom Script notification exposes `Lidarr_AddedTrackPaths`.
- `WriteAudioTags = No` prevents Lidarr's post-import tag write; `FileDate =
  None` and disabled Linux permission management avoid extra file mutations.
- `UpgradeMediaFileService` still calls Lidarr's tag writer after transfer, so
  `WriteAudioTags = No` is not merely recommended; it is required to preserve
  the no-rewrite invariant. `FileDate = None` and disabled Linux permissions are
  likewise required because Lidarr can set file mtimes and permissions after the
  script import succeeds.

This means the integration can make Lidarr's import succeed without Lidarr
copying bytes: the import script creates the destination entry itself and exits
`0`.

## Architecture

Add `contrib/lidarr` as a Python package built on `contrib/python-musefs`,
matching the existing contrib layering:

- `python-musefs` remains the shared store-contract library: schema check,
  `musefs scan` shell-out, `Record`, `sync_files`, tag/art writes, and
  `realpath_key`.
- `contrib/lidarr` owns Lidarr-specific behavior: environment parsing, link
  creation, Lidarr API calls, Lidarr metadata mapping, CLI entry points, tests,
  and documentation.
- No Rust data-path changes are part of this feature. The core invariant remains
  unchanged: served audio bytes come from the backing file; tag bytes are
  synthesized from the store.

Ship two executable entry points:

- `musefs-lidarr-import`: configured as Lidarr's **Import Using Script** path.
  It creates the destination symlink or hardlink and exits with the code Lidarr
  expects.
- `musefs-lidarr-sync`: configured as a Lidarr **Custom Script** for `On Release
  Import` and `On Rename`, and callable manually for backfill and debugging.

The package should be installable in the same environment that runs Lidarr's
custom scripts. It can depend on `python-musefs`; unlike Picard, no vendored copy
is needed unless a later packaging constraint demands it.

## Import data flow

On a new download:

1. Lidarr completes matching/import and calls `musefs-lidarr-import` with
   `Lidarr_SourcePath` and `Lidarr_DestinationPath`.
2. The import script creates the destination parent directory.
3. The import script creates either:
   - a symlink at `DestinationPath` pointing to `SourcePath`, by default; or
   - a hardlink when `MUSEFS_LIDARR_LINK_MODE=hardlink`.
4. The script exits `0`. Lidarr records the destination path and skips its
   built-in `HardLinkOrCopy` transfer.
5. Lidarr emits `AlbumDownload`.
6. `musefs-lidarr-sync` reads `Lidarr_AddedTrackPaths`, runs `musefs scan --db`
   on those paths, maps Lidarr metadata into musefs tag rows, and commits the
   projection to the musefs DB.
7. musefs auto-refresh surfaces the changed tags in the FUSE mount without
   remounting and without modifying backing audio.

Symlink mode is the default because it creates no extra inode alias for the
audio. `musefs scan` canonicalizes paths, so scanning a symlink destination still
stores the real backing path and preserves musefs's path-key contract.

Hardlink mode is an explicit fallback for users whose environment dislikes
symlinks. It must fail closed if `os.link` fails; it must never fall back to a
copy.

## Rename and backfill flows

On Lidarr rename:

- Lidarr renames the placeholder entries in its destination tree.
- `musefs-lidarr-sync` handles `Rename` using `Lidarr_TrackFile_Paths` and
  `Lidarr_TrackFile_PreviousPaths`.
- The sync scans the new paths and writes refreshed metadata.
- Symlink mode must **not** prune musefs rows based on previous placeholder
  paths. `musefs scan` canonicalizes symlinks to the real backing file, so the
  old and new placeholder names point at the same canonical `backing_path`.
- Hardlink mode may prune rows for previous paths when those paths no longer
  exist, because the hardlink destination path itself can be the scanned
  `backing_path`.

Manual backfill:

- Provide a manual mode such as `musefs-lidarr-sync --all`.
- It requires Lidarr API configuration, queries every known artist, then queries
  each artist's track files and tracks so every known file can be synced into the
  musefs store.
- This is useful for an existing Lidarr library or for recovering from missed
  Custom Script events.

Test events:

- Lidarr `Test` events exit `0` without touching files or the DB.

Unsupported events:

- Events that do not include useful paths are ignored with a concise log message
  and exit `0`.
- `TrackRetag` is not the primary workflow because it fires after Lidarr rewrites
  tags. v1 must skip `TrackRetag` with a warning and exit `0`; enabling Lidarr
  retagging violates the intended setup and must not be treated as a supported
  sync source.

## Configuration

Supported Lidarr settings:

- **Media Management -> Import Using Script:** enabled.
- **Import Script Path:** `musefs-lidarr-import`.
- **Metadata Provider -> Write Audio Tags:** `Never`.
- **File Date:** `None`.
- **Linux permission management:** disabled.
- **Rename Tracks:** allowed, but understood as renaming placeholder entries in
  Lidarr's destination tree. Consumers still use the musefs mount.

Environment variables:

Shared:

- `MUSEFS_LIDARR_LINK_MODE`: `symlink` default, `hardlink` optional.

Import script:

- No DB or Lidarr API configuration is required. It consumes Lidarr's
  `Lidarr_SourcePath` and `Lidarr_DestinationPath` environment variables and
  creates the configured link.

Sync script:

- `MUSEFS_DB`: required.
- `MUSEFS_BIN`: optional, default `musefs`.
- `MUSEFS_LIDARR_URL`: required for sync metadata and config preflight.
- `MUSEFS_LIDARR_API_KEY`: required for sync metadata and config preflight.
- `MUSEFS_LIDARR_AUTOSCAN`: optional, default enabled.

The sync command should support CLI flags mirroring the environment where that
is useful for manual runs. Environment variables remain the natural interface for
Lidarr script execution.

## Metadata mapping

First version mapping should be intentionally pragmatic:

- Required tags: `title`, `artist`, `albumartist`, `album`, `tracknumber`,
  `discnumber`, `date`.
- MusicBrainz IDs where Lidarr exposes them cleanly: artist, release group,
  release, recording, and track IDs when available.
- Genres from Lidarr artist or album metadata map to repeated `genre` rows.
- Quality, release group, custom formats, and Lidarr IDs can be considered later
  as optional extra fields, but they are not required for v1.

Data sources, in order:

1. Custom Script environment variables for event type, affected paths, and
   coarse artist/album IDs.
2. Lidarr API for per-track, per-album, and per-artist metadata.

For v1, all non-test, non-unsupported sync modes require `MUSEFS_LIDARR_URL` and
`MUSEFS_LIDARR_API_KEY`. There is no env-only metadata sync fallback because
Custom Script environment variables cannot reliably produce a complete
per-track tag set.

Tags are fully replaced for each synced track, preserving scanner-written binary
tags according to the existing `python-musefs` behavior.

### API path-to-track matching

`AlbumDownload` gives added paths, not a complete per-track tag payload. When
Lidarr API configuration is present, sync must match paths to Lidarr track-file
records before building `Record`s:

1. Split `Lidarr_AddedTrackPaths` or `Lidarr_TrackFile_Paths`.
2. Normalize each affected path with `realpath_key`.
3. Query Lidarr for candidate track files for the event's artist and album when
   those IDs are present; otherwise query the track-file endpoint broadly enough
   to find the affected paths.
4. Normalize each candidate track-file path with `realpath_key`.
5. Require exactly one candidate for each affected path.
6. Fetch or use the linked track, album, and artist data for the matched
   track-file.
7. For artist-scoped rename or manual backfill payloads, group candidate
   track-files by each file's own `albumId` and `artistId`; never apply one
   album payload to every file in a multi-album event.

Zero matches are counted as skipped and logged with the path. Multiple matches
are an error for that path because syncing ambiguous metadata is worse than
skipping. Multi-track files are supported only when Lidarr exposes all linked
tracks clearly; v1 emits repeated track-related rows in Lidarr's order. If the
API payload does not expose linked tracks cleanly, the file is skipped with an
explicit "multi-track metadata unavailable" reason.

## Safety and error handling

Import script behavior:

- Refuse to overwrite an existing destination unless it already represents the
  same source in the same configured mode.
- In symlink mode, verify an existing symlink target before treating the import
  as idempotent.
- In hardlink mode, verify same inode before treating the import as idempotent.
- Missing source, destination conflict, permission failure, and hardlink failure
  exit nonzero so Lidarr marks import failed rather than silently copying.
- The script must not return Lidarr's "defer move" exit code in the supported
  workflow, because that allows Lidarr's internal transfer path.

Sync behavior:

- Missing DB with autoscan enabled is handled by `musefs scan`.
- Missing DB with autoscan disabled is a user-facing error.
- Schema mismatch fails through `python-musefs`'s existing `SchemaMismatch`.
- Path mismatch or no matching track row after scan increments `skipped`, like
  beets and Picard.
- Lidarr API failures should be explicit and should distinguish configuration
  errors from transient HTTP errors.
- Missing Lidarr API configuration is a user-facing error for `AlbumDownload`,
  `Rename`, and `--all`; it is not an env-only fallback path.
- `TrackRetag` logs that it is skipped because it fires after Lidarr writes tags
  and exits `0` without scanning or writing the DB.
- Sync should write one batch transaction and commit only on success.
- API keys and authorization values must never be printed in logs, errors, test
  snapshots, or command output. Redact them as `<redacted>` whenever reporting
  resolved configuration.

No operation in this integration should open an audio file for write. The only
filesystem write in the default import path is creating a symlink placeholder.

### Lidarr settings preflight

Because unsafe Lidarr settings can break musefs's no-mutation invariant, sync
must provide a preflight check:

- `musefs-lidarr-sync doctor` queries Lidarr's API and verifies `WriteAudioTags`
  is `No`, `FileDate` is `None`, and Linux permission management is disabled.
- Normal sync runs the same preflight before writing unless the user passes an
  explicit escape hatch such as `--skip-lidarr-preflight`.
- Unsafe settings are a hard failure by default. The failure message names the
  specific setting and the required value.
- When API configuration is absent, `doctor` and non-test sync fail with a
  concise configuration error.

## Documentation updates required

Implementation must update all relevant docs, not only add code:

- New `contrib/lidarr/README.md` covering installation, Lidarr UI settings,
  environment variables, symlink vs hardlink mode, Custom Script setup, manual
  backfill, troubleshooting, and a smoke test.
- New durable real-Lidarr smoke checklist artifact under
  `docs/superpowers/specs/` or another release-note path chosen during
  implementation. It must record whether the real-instance smoke gate is pending
  or completed, the Lidarr version, link mode, destination entry type, source
  checksum before/after, source mtime before/after, and whether mount metadata
  was verified.
- Root `README.md` integration list and workflow text, adding Lidarr beside
  beets and Picard.
- `ARCHITECTURE.md` contrib ecosystem section, adding Lidarr as a third
  external writer and documenting the placeholder-tree vs musefs-mount split.
- `CONTRIBUTING.md` Python plugin test commands, CI expectations, and any
  Lidarr-specific environment gotchas.
- `contrib/python-musefs/README.md` consumers list, adding Lidarr as a pip
  consumer.
- `.github/workflows/ci.yml` updates for linting/testing `contrib/lidarr`.
- `CHANGELOG.md` or release notes when the feature is included in a release.
- Troubleshooting notes in `contrib/lidarr/README.md` explaining that media
  servers should point at the musefs FUSE mount, not at Lidarr's placeholder
  destination tree.
- A `doctor` section documenting the exact settings checked through the Lidarr
  API and the failure behavior when API configuration is absent.

The docs must explicitly warn against these unsupported or dangerous setups:

- `Write Audio Tags` enabled.
- Allowing Lidarr's internal `HardLinkOrCopy` path as the primary workflow.
- Pointing Navidrome or another consumer at Lidarr's placeholder tree and
  expecting synthesized tags there.
- Enabling file-date or permission mutation settings that touch placeholder or
  backing paths.
- Logging API keys or command invocations that include API keys.

## Tests

Unit tests:

- Env parsing for import and sync events.
- Link mode selection and validation.
- Symlink creation, idempotency, destination conflict handling, and missing
  source failure.
- Hardlink creation, same-inode idempotency, and no copy fallback.
- Event selection: `Test`, `AlbumDownload`, `Rename`, unsupported events.
- `TrackRetag` skip behavior: warning, exit `0`, no scan, no DB writes.
- Metadata mapping from representative Lidarr API payloads.
- API path-to-track matching for exact match, zero match, multiple matches, and
  multi-track file behavior.
- Artist-scoped rename payloads spanning multiple albums, proving each file uses
  metadata from its own `albumId`.
- API client URL construction, auth header/API key behavior, and error
  classification, including API key redaction in logs and exceptions.
- Lidarr settings preflight: safe settings pass; unsafe `WriteAudioTags`,
  `FileDate`, and permission settings fail; missing API config fails for doctor
  and non-test sync.
- CLI preflight behavior: unsafe settings return `1`, `--skip-lidarr-preflight`
  allows sync to proceed, and stdout/stderr never include the API key.

Integration tests:

- Temporary musefs DB with `python-musefs` sync helpers.
- `musefs-lidarr-sync` writes expected tags and repeated genre rows.
- `musefs-lidarr-sync --all` queries Lidarr API data for all artists and syncs
  every matched known track file.
- Rename sync behavior differs by link mode: symlink mode rescans without stale
  placeholder pruning; hardlink mode prunes missing previous paths.
- Missing row after scan is counted as skipped.

Release gates:

- `musefs_bin` path-matching test against a real built `musefs` binary.
- Manual smoke test with a real Lidarr instance:
  1. Configure Import Using Script to `musefs-lidarr-import`.
  2. Configure Custom Script On Release Import and On Rename to
     `musefs-lidarr-sync`.
  3. Import a small album.
  4. Confirm Lidarr destination entries are symlinks by default.
  5. Confirm musefs mount shows Lidarr metadata.
  6. Confirm backing file bytes and mtime are unchanged.

The real-Lidarr smoke test does not need to run in normal CI, but it is required
before marking the feature release-ready because the core workflow depends on
Lidarr accepting script-created symlink destinations and firing the expected
Custom Script event.

## Non-goals

- Modifying Lidarr itself.
- Teaching musefs to mount or emulate a Lidarr library tree directly.
- Supporting a workflow where Lidarr writes audio tags and musefs merely mirrors
  the already-mutated file.
- Guaranteeing correctness for media consumers pointed at Lidarr's placeholder
  tree.
- Building a large custom field-mapping DSL in v1.
