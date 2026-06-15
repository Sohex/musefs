# python-musefs

The shared store-contract library behind the [beets](beets.md),
[Picard](picard.md), and [Lidarr](lidarr.md) musefs
plugins. It is the single source of truth for how a plugin writes the musefs
SQLite store: the schema-version check, the `tags` / `art` / `track_art`
writes, sha256 art content-addressing, the `realpath_key` path normalization,
the `musefs scan` shell-out (`run_scan`), and the per-file sync write-loop
(`Record` / `sync_files`).

Field mapping stays in each plugin — beets expands multi-valued
`genres`/`composers` into one tag each, Picard takes the first value — so this
library deliberately does not own it.

## Writing a plugin

A plugin turns host metadata (a beets item, a Picard track, a Lidarr release)
into musefs store writes. This library owns every store-touching step except the
field mapping: you supply the per-file tag and art values, and it handles the
schema check, the scan shell-out, content-addressing, and the write loop.

### The write flow

The canonical order is **connect → check_schema_version → run_scan → build
`Record`s → sync_files → commit → prune_missing**. The caller owns the
transaction — nothing here commits for you.

```python
from musefs_common import (
    SCAN_TIMEOUT_SECONDS,
    ArtImage,
    Record,
    check_schema_version,
    connect,
    prune_missing,
    realpath_key,
    run_scan,
    sync_files,
)


def sync(db_path, files, *, musefs_bin="musefs"):
    # `run_scan` creates the DB if absent and fills the structural columns a
    # plugin cannot compute (format, audio offset/length, backing size/mtime).
    # On a brand-new store it must precede `connect`, which has nothing to open
    # until the scan has created the file.
    run_scan(musefs_bin, db_path, files, timeout=SCAN_TIMEOUT_SECONDS)

    conn = connect(db_path)
    try:
        check_schema_version(conn)  # raises SchemaMismatch on a version skew

        records = [
            Record(
                key=realpath_key(path),  # MUST equal the scanned row's backing_path
                pairs=[("artist", artist), ("title", title)],
                art=[ArtImage(data=cover, mime="image/jpeg")] if cover else None,
            )
            for path, artist, title, cover in host_metadata(files)
        ]

        stats = sync_files(conn, records)  # full-replace of plugin text tags
        conn.commit()  # the caller commits

        prune_missing(conn)  # drop rows whose backing file vanished
        conn.commit()
        return stats
    finally:
        conn.close()
```

For a dry run, pass `dry_run=True` to `sync_files` and `conn.rollback()` instead
of committing — `SyncStats` still reports what *would* change.

`run_scan` raises `ScanError` (`kind` ∈ `{"not_found", "timeout", "failed"}`)
and `check_schema_version` raises `SchemaMismatch`; a host adapter formats its
own user-facing message from the exception attributes (see the beets plugin's
`_scan_user_error`).

### The `Record` shape

One `Record` per file is your primary output. Its fields:

| field | type | meaning |
| ----- | ---- | ------- |
| `key` | `str` | The file's identity in the store. **Must** be `realpath_key(path)` — the canonicalized absolute path the scanner stored as `backing_path`. A `key` that matches no scanned row is silently counted in `SyncStats.skipped`, not written. |
| `pairs` | `list[tuple[str, str]]` | Ordered `(tag_key, value)` text tags. Duplicate keys are allowed and get contiguous ordinals (multi-valued tags). |
| `art` | `list[ArtImage] \| None` | Embedded pictures, already resolved to bytes. `None`/`[]` leaves existing art untouched. |
| `delete_keys` | `list[str] \| None` | Merge mode only: keys to clear without rewriting (see below). Ignored in replace mode. |

`ArtImage(data, mime, picture_type=3, description="")` is one picture: `data` is
raw bytes, `picture_type` is the ID3/FLAC type (3 = front cover). Images larger
than `MAX_ART_BYTES` are dropped and counted in `SyncStats.skipped_art`.

If every record lands in `skipped`, the `key`s and the scan target disagree —
both must canonicalize the same way, so scan the *real* files (not a symlink
farm) and build keys with `realpath_key`.

### Merge vs. replace, and sticky deletes

`sync_files(..., merge=False)` (the default) **replaces** every plugin-owned
text tag on each track: it clears all `value_blob IS NULL` rows and rewrites
them from `record.pairs`. Scanner-written binary tags always survive.

`sync_files(..., merge=True)` **merges**: only the keys named in `record.pairs`
and `record.delete_keys` are touched; other scan-seeded text tags stay. Use
merge when your plugin owns a *subset* of the tags and must not clobber the
rest. The store does not remember which keys you manage — **you** track your
managed-key set out of band (the contract is explicit that the store is not the
place for plugin state).

Merge-mode key matching is **case-insensitive** (`lower(key) = lower(?)`): Vorbis
keys render case-insensitively, so a scan that seeds a tag in the file's native
case (e.g. `LABEL`) is correctly replaced when your plugin canonicalizes to
lowercase (`label`), rather than leaving the original row behind as a duplicate.

When the user removes a tag in the host, merge mode needs to delete the
now-orphaned store row. The beets plugin solves this with an **accumulating
managed-key set** (the `musefs_managed` pattern), worth copying:

- Persist, per file, the set of keys you have *ever* written (beets uses a
  flexattr; any per-file host metadata works).
- On each sync, `delete_keys = previous_managed − keys_written_now`, and the new
  persisted set is `previous_managed ∪ keys_written_now`.
- A key you stop writing becomes a tombstone: it keeps getting deleted on every
  sync until you write it again. Persist the managed set **only after** the store
  commit succeeds, so a failed sync doesn't lose the record of what you owe.

See `contrib/beets/beetsplug/_core.py` (`build_records` / `persist_managed`) for
the reference implementation.

### Store invariants you must respect

The full external-writer contract is in
[ARCHITECTURE.md](../architecture/store.md#the-external-writer-contract). The rules
that bite plugin authors:

- **Write only `tags`, `art`, and `track_art`.** The scanner owns the structural
  columns of `tracks` and all of `structural_blocks`; never compute them — run
  `musefs scan` (i.e. `run_scan`). `CHECK` constraints reject malformed
  structural shapes at commit, so you cannot persist them anyway.
- **Binary tags survive a sync.** `merge_tags` / `replace_tags` scope their
  deletes to text rows (`value_blob IS NULL`), so the write loop never wipes
  scanner-written binary tags. You may write binary tags yourself too — a binary
  row carries its payload in `value_blob` and must leave `value` empty (the only
  `CHECK` on the row).
- **Content-address art** through `upsert_art` (sha256 de-dup) rather than
  inserting `art` rows by hand; `sync_files` does this for you.
- **Art rows are immutable.** A trigger rejects in-place updates of an
  `art` row's content columns (`data`, `sha256`, `mime`, `byte_len`, `width`,
  `height`). To change a track's art, insert a new content-addressed row via
  `upsert_art` and relink it via `replace_track_art`.
- **Path layout is just a tag.** To drive a reorganized mount, write your
  computed relative path into a custom tag (e.g. `beets_path`) and mount with
  `--template '$!{beets_path}'`. musefs sanitizes each path segment, so a writer
  cannot inject traversal.

## API reference

Everything in `__all__`, imported from the top-level `musefs_common` package.

**Connection & schema**

- `connect(db_path)` → `sqlite3.Connection` — open with a 5s busy timeout and
  `foreign_keys = ON`.
- `check_schema_version(conn)` — raise `SchemaMismatch` unless the store's
  `user_version` equals `EXPECTED_USER_VERSION`.

**Scanning**

- `run_scan(binary, db_path, target, *, timeout=None)` — shell out to `musefs
  scan`; `target` is one path or an iterable, all scanned under one process.
  Creates the DB if absent. Raises `ScanError`.

**Building records**

- `Record(key, pairs=[], art=None, delete_keys=None)` — one file's sync inputs
  (see *The `Record` shape*).
- `ArtImage(data, mime, picture_type=3, description="")` — one embedded picture.
- `realpath_key(path)` — canonical path string matching the scanner's
  `backing_path`; accepts `str`/`bytes`, returns `str`.

**Writing**

- `sync_files(conn, records, *, dry_run=False, stats=None, merge=False)` →
  `SyncStats` — the write loop; caller owns the transaction. Pass `stats` to
  accumulate into a caller-seeded instance.
- `sync_one(conn, record, stats, *, dry_run=False, merge=False)` — sync a single
  record into a caller-supplied `SyncStats`.
- `SyncStats` — `synced` / `skipped` / `art_linked` / `skipped_art` /
  `skipped_invalid` counters, plus `.summary()`. A record whose tags or art
  violate a store CHECK constraint is rolled back and skipped (not raised),
  bumping `skipped_invalid` and appending `(record.key, message)` to the
  `invalid` list — one malformed record never aborts the batch.

**Lower-level store helpers** (called for you by `sync_files`; use directly only
for a custom write loop)

- `track_id_for_path(conn, key)` → track id or `None`.
- `merge_tags(conn, track_id, managed_pairs, delete_keys)` — per-key replace of
  plugin-managed text tags, leaving unmanaged text rows intact.
- `replace_tags(conn, track_id, pairs)` — replace all plugin-owned text tags.
- `upsert_art(conn, data, mime)` → art id — content-address `data` by sha256,
  inserting only if new.
- `replace_track_art(conn, track_id, arts)` — replace a track's `track_art`
  rows; `arts` is `[(art_id, picture_type, description), …]`.
- `sniff_mime(data, path)` — image mime from magic bytes, falling back to file
  extension.
- `prune_missing(conn, track_ids=None)` → count — delete tracks whose backing
  file no longer exists (every track, or just `track_ids`).

**Reading**

- `track_ids_for_paths(conn, keys)` → `{key: id}` — bulk `backing_path` → track
  id; keys with no matching row are omitted. Chunked under SQLite's parameter
  cap, so arbitrarily large lookups are safe (the bulk `track_id_for_path`).
- `tags_for_track(conn, track_id)` → `[TagRow, …]` ordered by key then ordinal,
  covering both plugin-owned text tags and scanner-written binary tags.
- `TagRow(key, value, value_blob)` — one read-back tag row. Text tags have
  `value_blob is None`; binary tags have `value == ""` and `value_blob` bytes.

**Constants**

- `EXPECTED_USER_VERSION` — schema `user_version` this library targets.
- `MAX_ART_BYTES` — per-image art cap; larger images are skipped.
- `SCAN_TIMEOUT_SECONDS` — default wall-clock cap for one `run_scan`.

**Exceptions**

- `SchemaMismatch(found)` — schema-version skew; `.found` is the DB's version.
- `ScanError(kind, *, binary, target, …)` — a `musefs scan` failure; `.kind` ∈
  `{"not_found", "timeout", "failed"}`, with context attributes for messaging.

## Consumers

- **beets** depends on this package via pip (`contrib/beets/pyproject.toml`).
- **Picard** cannot pip-install plugin dependencies, so the package is
  **vendored** into `contrib/picard/musefs/_common/` by
  `vendor_to_picard.py`. After any change here, re-run:

  ```bash
  python contrib/python-musefs/vendor_to_picard.py
  ```

  The Picard test `tests/test_vendor_sync.py` fails if the committed copy drifts.
- **Lidarr** depends on this package via pip (`contrib/lidarr/pyproject.toml`).

## Schema coupling

`musefs_common/schema.py` (`SCHEMA_SQL`, `USER_VERSION`) is **generated** from
the Rust migrations in `musefs-db/src/schema.rs` — do not edit it by hand.
`EXPECTED_USER_VERSION` (in `constants.py`) derives from it. When the Rust
schema bumps, regenerate and re-vendor:

```bash
MUSEFS_REGEN_SCHEMA_PY=1 cargo test -p musefs-db schema_py
python contrib/python-musefs/vendor_to_picard.py
```

A `musefs-db` unit test fails if the generated file drifts. This is all
independent of the package's own `__version__` (its release SemVer).

## Tests

```bash
cd contrib/python-musefs
python -m venv .venv && source .venv/bin/activate
pip install -e ".[test]"
python -m pytest -v
ruff check . && ruff format --check .
```
