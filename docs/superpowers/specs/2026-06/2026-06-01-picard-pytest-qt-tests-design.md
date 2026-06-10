# pytest-qt test suite for the Picard plugin

Closes issue #63. Date: 2026-06-01.

## Problem

The Picard plugin (`contrib/picard/`) is split into two layers:

- `musefs/_core.py` — pure logic (config resolution, field mapping, front-cover
  extraction, scan invocation, DB sync). **Already fully unit-tested** with
  fakes, Qt-free.
- `musefs/__init__.py` — the thin Picard adapter: the `MusefsSync` context-menu
  action, the `MusefsOptionsPage` Qt widget, and the module-level registration
  of both with Picard's UI. This layer is guarded by a `try/except ImportError`
  that disables the entire `if _PICARD:` block when Picard is absent, so today it
  is **untested** — the README verifies it only by a manual smoke test.

Issue #63 asks for a `pytest-qt` suite covering the adapter:

1. the plugin loads without error,
2. a DB write round-trip (Picard tag edits land in the SQLite store),
3. a smoke test that the plugin registers itself with Picard's UI event system.

There is also no Picard CI job at all today (the existing `_core` tests never run
in CI, unlike the sibling `beets` plugin).

## Approach

Test the **real** adapter against **real Picard 2.13.3 + real PyQt5**, run
headless (`QT_QPA_PLATFORM=offscreen`). No stubbing of `picard.*`.

Picard is not pip-installable as a clean wheel — its source build compiles
translations (`msgfmt`/gettext), appdata, Qt resources, and a C extension, so
building from source drags in a full Qt/gettext toolchain. Instead we use the
distro package (`apt-get install picard`), which provides Picard's modules at
`/usr/lib/picard` plus a system PyQt5, and bind a uv venv to the **system**
Python that package targets.

The `_core` unit tests are unchanged and continue to run Qt-free. The new
real-Picard tests use `pytest.importorskip("picard")` so they **skip cleanly** on
a machine without Picard, while `_core` tests still run there.

### Why not the alternatives

- **Stub `picard.*` entirely** — lighter, runs anywhere, but the widget/registration
  round-trips go through stubs and test almost nothing; it isn't really pytest-qt.
- **Build Picard from source via uv** — matches Picard's documented *dev* setup but
  needs gettext + Qt build tools and clones/builds the repo; strictly heavier than
  the apt package, not cleaner.
- **apt Picard on system Python with `--break-system-packages`** — works, but the
  uv `--system-site-packages` venv achieves the same with a clean, isolated install.

## Environment recipe (identical local and CI)

```bash
apt-get install -y picard                                   # Picard modules at /usr/lib/picard + system PyQt5
uv venv --system-site-packages --python "$(which python3)"  # MUST be the system python apt's Picard C ext was built for
uv pip install -e 'contrib/picard[test]' ruff               # test extra includes pytest-qt; no --break-system-packages
PYTHONPATH=/usr/lib/picard QT_QPA_PLATFORM=offscreen \
  .venv/bin/python -m pytest contrib/picard/tests
```

Notes / known wrinkles:

- **The CI runner's `python3` is the only interpreter that matters.** apt's
  Picard ships its C extension (`picard/util/_astrcmp.cpython-3XX...so`) built for
  the runner image's system Python; the uv venv must bind to that same interpreter
  (`--python "$(which python3)"`). So CI must **not** use `actions/setup-python`,
  which would shim a different Python the apt `.so` can't load. The plugin's
  `requires-python = ">=3.8"` is its *runtime* floor for end users and is
  unrelated to the CI test interpreter. To keep the apt-Picard / system-Python
  pairing stable, **pin the runner image** (`runs-on: ubuntu-24.04`, not
  `ubuntu-latest`); whatever `picard` and `python3` that image's apt provides is
  the tested pairing. (Validated during design on a Python 3.14 / PyQt5 5.15.11 /
  Picard 2.13.3 host — those exact versions are illustrative, not pins.)
- **`PYTHONPATH=/usr/lib/picard`** is required because apt installs Picard outside
  site-packages; `--system-site-packages` exposes PyQt5 but not the Picard package
  itself.
- **`QT_QPA_PLATFORM=offscreen`** lets Qt widgets instantiate without a display.
- **Config must be initialized.** `config.setting` is `None` until
  `config.setup_config(app, ini_path)` runs; tests need a `QApplication`
  (from pytest-qt's `qapp`/`qtbot`) and a temp ini. The plugin's options
  (`musefs_db`/`musefs_bin`/`musefs_autoscan`/`musefs_fields`) are *declared* by
  the `config.TextOption(...)`/`BoolOption(...)` entries in the
  `MusefsOptionsPage.options` class body, which run at import time — so importing
  the plugin after `setup_config` is sufficient for `load()`/`save()` to work; no
  separate registration step is needed. Re-importing the plugin within one process
  re-runs those declarations and logs a harmless idempotent "Option ... already
  declared" message.

### Test selection and the existing `addopts`

`pyproject.toml` sets `addopts = "-m 'not musefs_bin'"`, deselecting the opt-in
Rust-binary gate by default. The new real-Picard tests carry **no pytest marker**,
so they are *not* `musefs_bin` and run by default under the recipe above. They
gate on availability instead: each real-Picard test module begins with
`pytest.importorskip("picard")` (or uses a shared `requires_picard` fixture), so
the suite skips cleanly — never errors — where Picard is not importable.

## Components

### conftest.py additions

Reuse the existing `db_path`, `make_track`, `FakeMetadata`, `FakeImage` fixtures.
Add:

- `picard_config` fixture — initializes Picard config headless via
  `config.setup_config(qapp, str(tmp_path / "picard.ini"))` so `config.setting`
  is live and writable. Depends on pytest-qt's `qapp`.
- A `FakeFile` helper — exposes `.filename` and `.metadata`, to build the
  `{realpath_key: file}` dict that `_do_sync` consumes.
- A skip guard (`pytest.importorskip("picard")` at module scope in the
  real-Picard test files, or a shared `requires_picard` fixture) so the suite
  degrades gracefully where Picard is unavailable.

### Test scope

"Three bullets + obvious glue." Deep error-path matrices are intentionally
excluded — they are already covered against `_core`. New test modules:

- `test_plugin_loads.py` — importing the plugin against real Picard succeeds;
  `_PICARD is True`; `MusefsSync` and `MusefsOptionsPage` are defined. *(bullet 1)*
- `test_registration.py` — assert the plugin **calls Picard's public
  registration API**, rather than inspecting Picard's internal registry
  attributes (which are undocumented and vary across the 2.x range the plugin
  targets). Approach, verified during design: `monkeypatch.setattr` the public
  functions `register_file_action`/`register_track_action`/`register_album_action`/
  `register_cluster_action` on `picard.ui.itemviews` and `register_options_page`
  on `picard.ui.options` to record their arguments, then force a fresh import of
  the plugin (`sys.modules.pop("musefs", None)` + `importlib.import_module`) so the
  module-level registration re-runs against the spies. Assert each action register
  is called exactly once with the *same* `MusefsSync` instance, and that
  `register_options_page` receives `MusefsOptionsPage`. (These five function names
  are the documented plugin API the adapter already imports, so the test is
  version-stable.) *(bullet 3)*
- `test_sync_roundtrip.py` — call `_do_sync(opts, files)` with autoscan **off**
  against a seeded temp DB (`make_track`) + a `FakeMetadata` carrying tags and a
  `FakeImage` front cover; assert the tags and art land in the store and
  `SyncStats` reports the expected counts. *(bullet 2)*
- `test_options_page.py` — `qtbot`-driven: instantiate `MusefsOptionsPage`,
  assert `load()` reflects `config.setting`, then edit a field, `save()`, and
  assert the value is written back. (Importing the plugin under `picard_config`
  declares the options, so no extra setup is needed — see the config note above.)
  *(glue)*
- `test_callback_flow.py` — monkeypatch `picard.util.thread.run_task` with a
  synchronous stand-in matching its real signature
  `run_task(func, next_func=None, priority=0, thread_pool=None, traceback=True)`
  that calls `func()` and then `next_func(result=...)` inline (mirroring how
  Picard invokes the completion callback with a `result=`/`error=` keyword). Drive
  `MusefsSync.callback` over a selection of `FakeFile`s; assert `_do_sync` ran
  (tags landed) and `_done` logged the success summary. *(glue)*

### Packaging & docs

- `contrib/picard/pyproject.toml` — add `pytest-qt` to the `[project.optional-dependencies] test` extra.
- `contrib/picard/requirements.txt` — add `pytest-qt`.
- `contrib/picard/README.md` — extend the Tests section with the env recipe above
  (apt Picard, uv `--system-site-packages` venv, `PYTHONPATH`/offscreen run) and
  note that the real-Picard tests skip cleanly without Picard installed.

### CI

Add a `picard:` job to `.github/workflows/ci.yml`. It follows the `beets` job's
*structure* (checkout → lint → install → test) but **deliberately diverges** on
one point: it does **not** use `actions/setup-python`, because the venv must bind
to the runner's apt-provided system Python (see the Python-matching wrinkle
above). Steps:

1. `runs-on: ubuntu-24.04` (pinned, not `ubuntu-latest`, so the apt-Picard /
   system-Python pairing is stable),
2. checkout with `persist-credentials: false` (repo convention),
3. `sudo apt-get update && sudo apt-get install -y picard`,
4. install uv via `astral-sh/setup-uv`, **pinned to a commit SHA** (every action
   in `ci.yml` is SHA-pinned — match that convention; do not use a floating tag),
5. `uv venv --system-site-packages --python "$(which python3)"`,
6. `uv pip install -e 'contrib/picard[test]' ruff`,
7. `ruff check contrib/picard/` and `ruff format --check contrib/picard/`,
8. `PYTHONPATH=/usr/lib/picard QT_QPA_PLATFORM=offscreen .venv/bin/python -m pytest contrib/picard/tests -v`.

The same invocation runs the existing `_core` tests; the new real-Picard tests
execute here (Picard present) and skip on contributor machines without it.

## Out of scope

- Deep adapter error-path/edge-case matrices (covered against `_core`).
- The opt-in `musefs_bin` gate that shells out to the real Rust binary — unchanged.
- Any change to plugin runtime behavior; this is test/CI infrastructure only.

## Success criteria

- `contrib/picard/tests` passes under the env recipe, exercising real Picard +
  PyQt5 headless, covering the three issue bullets plus the OptionsPage and
  callback glue.
- The suite skips cleanly (no errors) where Picard is not importable.
- A new CI job runs lint + the full Picard suite on every push, closing the
  current CI gap.
