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
- The sync scans the new paths, writes refreshed metadata, and prunes stale
  musefs rows for previous paths when those paths no longer exist.

Manual backfill:

- Provide a manual mode such as `musefs-lidarr-sync --all`.
- It queries Lidarr for known track files and syncs every known file into the
  musefs store.
- This is useful for an existing Lidarr library or for recovering from missed
  Custom Script events.

Test events:

- Lidarr `Test` events exit `0` without touching files or the DB.

Unsupported events:

- Events that do not include useful paths are ignored with a concise log message
  and exit `0`.
- `TrackRetag` is not the primary workflow because it fires after Lidarr rewrites
  tags. It can be treated as best-effort sync if present, but the docs must make
  clear that enabling Lidarr retagging violates the intended setup.

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

- `MUSEFS_DB`: required for sync.
- `MUSEFS_BIN`: optional, default `musefs`.
- `MUSEFS_LIDARR_LINK_MODE`: `symlink` default, `hardlink` optional.
- `MUSEFS_LIDARR_URL`: required for API-enriched metadata.
- `MUSEFS_LIDARR_API_KEY`: required for API-enriched metadata.
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

1. Custom Script environment variables for event, affected paths, and coarse
   artist/album fields.
2. Lidarr API, when URL and API key are configured, for per-track and per-album
   metadata.
3. A minimal env-only fallback for simple tags when API configuration is absent.

The env-only fallback should be honest: it can sync basic album-level fields but
cannot produce a complete per-track tag set in every case. Rich metadata requires
the Lidarr API.

Tags are fully replaced for each synced track, preserving scanner-written binary
tags according to the existing `python-musefs` behavior.

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
- Sync should write one batch transaction and commit only on success.

No operation in this integration should open an audio file for write. The only
filesystem write in the default import path is creating a symlink placeholder.

## Documentation updates required

Implementation must update all relevant docs, not only add code:

- New `contrib/lidarr/README.md` covering installation, Lidarr UI settings,
  environment variables, symlink vs hardlink mode, Custom Script setup, manual
  backfill, troubleshooting, and a smoke test.
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

The docs must explicitly warn against these unsupported or dangerous setups:

- `Write Audio Tags` enabled.
- Allowing Lidarr's internal `HardLinkOrCopy` path as the primary workflow.
- Pointing Navidrome or another consumer at Lidarr's placeholder tree and
  expecting synthesized tags there.
- Enabling file-date or permission mutation settings that touch placeholder or
  backing paths.

## Tests

Unit tests:

- Env parsing for import and sync events.
- Link mode selection and validation.
- Symlink creation, idempotency, destination conflict handling, and missing
  source failure.
- Hardlink creation, same-inode idempotency, and no copy fallback.
- Event selection: `Test`, `AlbumDownload`, `Rename`, unsupported events.
- Metadata mapping from representative Lidarr API payloads.
- API client URL construction, auth header/API key behavior, and error
  classification.

Integration tests:

- Temporary musefs DB with `python-musefs` sync helpers.
- `musefs-lidarr-sync` writes expected tags and repeated genre rows.
- Rename sync prunes stale rows and syncs new paths.
- Missing row after scan is counted as skipped.

Optional gates:

- `musefs_bin` path-matching test against a real built `musefs` binary.
- Manual smoke test with a real Lidarr instance:
  1. Configure Import Using Script to `musefs-lidarr-import`.
  2. Configure Custom Script On Release Import and On Rename to
     `musefs-lidarr-sync`.
  3. Import a small album.
  4. Confirm Lidarr destination entries are symlinks by default.
  5. Confirm musefs mount shows Lidarr metadata.
  6. Confirm backing file bytes and mtime are unchanged.

## Non-goals

- Modifying Lidarr itself.
- Teaching musefs to mount or emulate a Lidarr library tree directly.
- Supporting a workflow where Lidarr writes audio tags and musefs merely mirrors
  the already-mutated file.
- Guaranteeing correctness for media consumers pointed at Lidarr's placeholder
  tree.
- Building a large custom field-mapping DSL in v1.
