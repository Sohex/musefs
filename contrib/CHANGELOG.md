# Python packages changelog

Changelog for the musefs `contrib/` Python packages — `python-musefs`,
`beets-musefs`, `lidarr-musefs`, and the (unpublished) Picard plugin. These
share a single version, released on `py-v*` tags and decoupled from the Rust
crate version tracked in the [root CHANGELOG](../CHANGELOG.md).

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and these packages adhere to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

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
