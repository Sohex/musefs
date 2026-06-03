# python-musefs

The shared store-contract library behind the [beets](../beets/README.md) and
[Picard](../picard/README.md) musefs plugins. It is the single source of truth
for how a plugin writes the musefs SQLite store: the schema-version check, the
`tags` / `art` / `track_art` writes, sha256 art content-addressing, the
`realpath_key` path normalization, the `musefs scan` shell-out (`run_scan`), and
the per-file sync write-loop (`Record` / `sync_files`).

Field mapping stays in each plugin — beets expands multi-valued
`genres`/`composers` into one tag each, Picard takes the first value — so this
library deliberately does not own it.

## Consumers

- **beets** depends on this package via pip (`contrib/beets/pyproject.toml`).
- **Picard** cannot pip-install plugin dependencies, so the package is
  **vendored** into `contrib/picard/musefs/_common/` by
  `vendor_to_picard.py`. After any change here, re-run:

  ```bash
  python contrib/python-musefs/vendor_to_picard.py
  ```

  The Picard test `tests/test_vendor_sync.py` fails if the committed copy drifts.

## Schema coupling

`EXPECTED_USER_VERSION` (in `constants.py`) mirrors the Rust `schema.rs`
MIGRATIONS length. When the Rust schema bumps, change it here once; both plugins
inherit it (Picard after a re-vendor). This is independent of the package's own
`__version__` (its release SemVer).

## Tests

```bash
cd contrib/python-musefs
python -m venv .venv && source .venv/bin/activate
pip install -e ".[test]"
python -m pytest -v
ruff check . && ruff format --check .
```
