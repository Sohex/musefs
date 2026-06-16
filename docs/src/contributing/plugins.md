# Python plugins

## Python plugins (contrib)

The four packages share one drift-guarded contract; see
[the contrib ecosystem](../architecture/tree-scanning.md#the-contrib-ecosystem) for the layout and
the [integration pages](../integrations/overview.md) for plugin-specific setup.

```bash
# python-musefs: self-contained
cd contrib/python-musefs && python -m pytest && ruff check . && ruff format --check .

# beets: install the local python-musefs first so the suite tests the working
# tree, not the PyPI release (see the beets integration page for the venv flow)
cd contrib/beets && pip install -e ../python-musefs && pip install -e ".[test]" && python -m pytest tests

# picard: no install needed (vendored + pythonpath=".")
cd contrib/picard && python -m pytest tests

# lidarr: install the local python-musefs first so the suite tests the working
# tree, not the PyPI release (see the lidarr integration page for the env flow)
cd contrib/lidarr && pip install -e ../python-musefs && pip install -e ".[test]" && python -m pytest tests
```

Gotchas that have bitten before:

- On PEP 668 "externally managed" systems, bare `pip install` fails — use a
  venv for the beets suite.
- The real-Picard tests `importorskip` Picard and Qt: without an importable
  Picard (e.g. the system package on `PYTHONPATH`), they **silently skip**.
  When touching the Picard plugin, make sure they actually ran.
- The Lidarr integration is gated by two automated tiers, both deterministic
  and network-free (Lidarr's metadata server is mocked too):
  - **PR check — `.github/workflows/lidarr-smoke.yml`** (`scripts/lidarr-smoke.sh`):
    a fast smoke that proves the Custom Script exec path on a real Lidarr (its
    Test event) and runs the content leg (`musefs-lidarr-sync` tag-writes,
    `musefs-lidarr-import` symlink, served-mount tags, unchanged bytes) against a
    local mock Lidarr API. Runs on PRs touching the Lidarr surface.
  - **Release gate — `.github/workflows/lidarr-e2e.yml`** (`scripts/lidarr-e2e/run-e2e.sh`):
    the full real-instance e2e. A real Lidarr, driven by local
    metadata/indexer/qBittorrent mocks, performs a genuine **download-client
    import** of a real CC0 album as a `NewDownload`, firing `OnReleaseImport`,
    which execs the real musefs scripts; the served mount is then asserted to
    carry Lidarr-supplied metadata the backing file lacked, bytes unchanged. This
    **gates the Python `py-v*` publish** and closes what used to be the manual
    download-client gap. The vendored CC0 fixture is `scripts/lidarr-e2e/fixtures/`.
- `musefs_common/schema.py` is **generated** from `musefs-db/src/schema.rs`.
  After a schema change:
  `MUSEFS_REGEN_SCHEMA_PY=1 cargo test -p musefs-db schema_py`, then
  re-vendor Picard's copy with
  `python contrib/python-musefs/vendor_to_picard.py`. Drift is enforced by a
  `musefs-db` unit test and the Picard vendor-sync test.
- `MAX_ART_BYTES` in `contrib/python-musefs/src/musefs_common/constants.py`
  is **hand-mirrored** from `musefs-core/src/scan.rs` — update both sides
  together.
