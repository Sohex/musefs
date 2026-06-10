# python-musefs

The shared store-contract library behind the [beets](../beets/README.md),
[Picard](../picard/README.md), and [Lidarr](../lidarr/README.md) musefs
plugins. It is the single source of truth for how a plugin writes the musefs
SQLite store: the schema-version check, the `tags` / `art` / `track_art`
writes, sha256 art content-addressing, the `realpath_key` path normalization,
the `musefs scan` shell-out (`run_scan`), and the per-file sync write-loop
(`Record` / `sync_files`).

Field mapping stays in each plugin — beets expands multi-valued
`genres`/`composers` into one tag each, Picard takes the first value — so this
library deliberately does not own it.

- `merge_tags(conn, track_id, managed_pairs, delete_keys)` — per-key replacement
  of plugin-managed text tags. Unlike `replace_tags` (which clears all text rows),
  `merge_tags` clears only the keys it rewrites plus `delete_keys`, leaving other
  scan-seeded text tags intact. Scanner-written binary tags survive either way.

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
