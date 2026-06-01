# Implementation Plan: pytest-qt test suite for the Picard plugin

**Spec:** `docs/superpowers/specs/2026-06-01-picard-pytest-qt-tests-design.md`
**Issue:** #63
**Branch:** `picard-pytest-qt-tests`
**Date:** 2026-06-01

## Goal

Add a `pytest-qt` suite that exercises the **real** Picard adapter
(`contrib/picard/musefs/__init__.py`) against **real Picard 2.13.3 + PyQt5**,
headless, covering the three issue bullets (plugin loads, DB write round-trip,
registration smoke test) plus the OptionsPage and callback glue. Wire it into CI
as a new `picard:` job. The pure-logic `_core` tests are unchanged.

## Context for the implementer

Read these before starting:

- `contrib/picard/musefs/__init__.py` — the adapter under test. Key facts:
  - `_PICARD` is `True` when `import picard` succeeds; the whole adapter lives
    inside `if _PICARD:`.
  - Option keys: `OPT_DB="musefs_db"`, `OPT_BIN="musefs_bin"`,
    `OPT_AUTOSCAN="musefs_autoscan"`, `OPT_FIELDS="musefs_fields"`.
  - `_resolved_files(objs)` → `{realpath_key(f.filename): f}` via `obj.iterfiles()`,
    skipping files with falsy `.filename`.
  - `_do_sync(opts, files)` → raises `MusefsError` if `opts.db` falsy; if
    `opts.autoscan` runs `run_scan` per file, else errors if the DB file is
    missing; then `connect` → `check_schema_version` → loop
    `map_fields`/`front_cover`/`sync_one` → single `conn.commit()` → returns
    `SyncStats`.
  - `MusefsSync.callback(objs)` builds a `settings` dict from `config.setting[...]`,
    calls `resolve_config(settings, os.environ)`, then
    `thread.run_task(partial(_do_sync, opts, files), partial(self._done, len(files)))`.
  - `MusefsSync._done(n_files, result=None, error=None)` logs
    `"musefs: <summary> (files=N)"` via `log.info` on success, `log.error` on error.
  - `MusefsOptionsPage.options` declares the four options via
    `config.TextOption`/`config.BoolOption` in the class body (runs at import);
    `load()` reads `config.setting`, `save()` writes it.
  - Module bottom: `_action = MusefsSync()`, then `register_file_action`,
    `register_track_action`, `register_album_action`, `register_cluster_action`,
    `register_options_page(MusefsOptionsPage)`.
- `contrib/picard/musefs/_core.py` — pure logic. `EXPECTED_USER_VERSION = 1`.
- `contrib/picard/tests/conftest.py` — existing fixtures: `db_path`, `make_track`,
  `insert_track`, `FakeImage`, `FakeMetadata`, `fake_metadata`, `fake_image`.
- `contrib/picard/tests/test_sync.py` — style reference; note
  `JPEG = b"\xff\xd8\xff\xe0" + b"\x00" * 32`.
- `contrib/picard/tests/schema_v1.sql` — applied by `db_path`; sets
  `PRAGMA user_version = 1` (so `check_schema_version` passes) and the
  `tags`/`track_art` content_version triggers.
- `.github/workflows/ci.yml` — actions are SHA-pinned; jobs use
  `persist-credentials: false`. The `beets` job is the structural template.

### Tooling note (Serena)

Serena's symbolic tools are Rust-only in this repo and cannot parse Python.
Per CLAUDE.md, the built-in `Read`/`Edit`/`Write` tools are the correct choice
for these `.py` / `.toml` / `.md` / `.yml` files.

### Environment to run the suite (local and CI, identical)

```bash
sudo apt-get update && sudo apt-get install -y picard      # Picard at /usr/lib/picard + system PyQt5
uv venv --system-site-packages --python "$(which python3)" # bind to the system python apt's Picard C-ext targets
uv pip install -e 'contrib/picard[test]' ruff              # test extra includes pytest-qt
PYTHONPATH=/usr/lib/picard QT_QPA_PLATFORM=offscreen \
  .venv/bin/python -m pytest contrib/picard/tests -v
```

The new real-Picard test modules each begin with `pytest.importorskip("picard")`
so they **skip cleanly** where Picard is unavailable; `_core` tests still run.
They carry **no pytest marker**, so they are not the `musefs_bin` gate and run by
default.

## Tasks

Work top to bottom. Tasks 1–2 are packaging/fixtures (prereqs). Tasks 3–7 are the
test modules — each is independently TDD-shaped: the tests **characterize existing
adapter behavior**, so against the real plugin they pass on first run (a green run
is the proof the characterization is faithful). Tasks 8–9 are docs/CI.

After each test-module task, run the suite under the env recipe and confirm the
new tests pass (or skip, off-Picard).

---

### Task 1 — Packaging: add `pytest-qt`

**Files:** `contrib/picard/pyproject.toml`, `contrib/picard/requirements.txt`

In `pyproject.toml`, change the test extra:

```toml
[project.optional-dependencies]
test = ["pytest>=7", "pytest-qt>=4"]
```

In `requirements.txt`, add `pytest-qt` under the existing `pytest>=7`:

```
pytest>=7
pytest-qt>=4
```

**Verify:** `uv pip install -e 'contrib/picard[test]'` pulls in `pytest-qt` and
`PyQt5` resolves from system site-packages (no build of PyQt5).

---

### Task 2 — conftest: headless-config + file fixtures

**File:** `contrib/picard/tests/conftest.py`

Append the following. `picard_config` initializes Picard's global config against a
temp ini (required: `config.setting` is `None` until `setup_config` runs, and it
needs a `QApplication` — supplied by pytest-qt's `qapp`). `FakeFile` mimics a
Picard `File`: `.filename`, `.metadata`, and `iterfiles()` yielding itself (so it
flows through `_resolved_files`).

```python
@pytest.fixture
def picard_config(qapp, tmp_path):
    """Initialize Picard's global config headless against a temp ini.

    config.setting is None until setup_config runs; it needs a QApplication
    (pytest-qt's qapp). Importing the plugin after this declares its options,
    so config.setting[OPT_*] is then readable/writable.
    """
    from picard import config

    config.setup_config(qapp, str(tmp_path / "picard.ini"))
    return config


class FakeFile:
    """Stand-in for a Picard File: .filename + .metadata, and iterfiles()
    yields itself (matching how _resolved_files walks a selection)."""

    def __init__(self, filename, metadata):
        self.filename = filename
        self.metadata = metadata

    def iterfiles(self):
        return [self]


@pytest.fixture
def fake_file():
    return FakeFile  # the class; call it directly in tests
```

> Note: pytest-qt provides `qapp`/`qtbot` only when PyQt5 is importable. In a
> no-Picard environment the real-Picard modules `importorskip` before any fixture
> using `qapp` is requested, so this fixture is never exercised there.

#### Config-init vs import ordering (read before writing Tasks 4, 6, 7)

`MusefsOptionsPage.options`' `config.TextOption(...)`/`BoolOption(...)` entries
register into Picard's **process-global option registry** (on the `config.Option`
class), which is independent of `config.setup_config`. So *declaring* options is
robust to whether `setup_config` has run yet, and `test_plugin_loads`/
`test_registration` may `import musefs` with config uninitialized without error.

What `load()`/`save()` need is a **live `config.setting`**, which only exists after
`setup_config` — i.e. after the `picard_config` fixture has run. The contract for
the config-touching tests (Tasks 6 and 7) is therefore:

1. request `picard_config` (runs `setup_config`), then
2. `import musefs` **inside the test body** (after the fixture), so the import —
   whether fresh or cached — happens with config live.

`test_registration`'s autouse teardown `sys.modules.pop("musefs", None)` means a
later test's `import musefs` re-runs the class-body declarations; that only logs a
harmless idempotent "Option ... already declared" and re-registers identical
defaults — it does not depend on, or corrupt, `config.setting`. Keep the
import-inside-the-body convention in Tasks 6 and 7 so collection order can't matter.

**Verify:** existing `_core` tests still collect and pass unchanged
(`python -m pytest contrib/picard/tests/test_sync.py`).

---

### Task 3 — `test_plugin_loads.py` (issue bullet 1)

**File:** `contrib/picard/tests/test_plugin_loads.py` (new)

Importing the plugin against real Picard must succeed and define the adapter
symbols.

```python
import pytest

pytest.importorskip("picard")


def test_plugin_imports_with_picard_present():
    import musefs

    assert musefs._PICARD is True


def test_adapter_symbols_defined():
    import musefs

    assert hasattr(musefs, "MusefsSync")
    assert hasattr(musefs, "MusefsOptionsPage")
    assert callable(musefs._do_sync)
```

**Verify:** passes under the env recipe; skips without Picard.

---

### Task 4 — `test_registration.py` (issue bullet 3)

**File:** `contrib/picard/tests/test_registration.py` (new)

Assert the plugin **calls Picard's public registration API** at import. Spy on the
public `register_*` functions (version-stable; the adapter already imports them),
then force a **fresh import** so module-level registration re-runs against the
spies. Do **not** inspect Picard's internal registries.

```python
import importlib
import sys

import pytest

pytest.importorskip("picard")


def test_plugin_registers_actions_and_options_page(monkeypatch):
    import picard.ui.itemviews as itemviews
    import picard.ui.options as options

    calls = {}

    def record(name):
        def _spy(arg):
            calls.setdefault(name, []).append(arg)

        return _spy

    monkeypatch.setattr(itemviews, "register_file_action", record("file"))
    monkeypatch.setattr(itemviews, "register_track_action", record("track"))
    monkeypatch.setattr(itemviews, "register_album_action", record("album"))
    monkeypatch.setattr(itemviews, "register_cluster_action", record("cluster"))
    monkeypatch.setattr(options, "register_options_page", record("options"))

    # Force the module-level registration to re-run against the spies.
    sys.modules.pop("musefs", None)
    musefs = importlib.import_module("musefs")

    # The four item actions register exactly once, all the SAME instance.
    for kind in ("file", "track", "album", "cluster"):
        assert len(calls[kind]) == 1, kind
    action = calls["file"][0]
    assert isinstance(action, musefs.MusefsSync)
    assert all(calls[k][0] is action for k in ("track", "album", "cluster"))

    # The options page registers the class.
    assert calls["options"] == [musefs.MusefsOptionsPage]


@pytest.fixture(autouse=True)
def _reimport_clean():
    """Drop the forced re-import so later tests get a normally-imported module."""
    yield
    sys.modules.pop("musefs", None)
```

> Re-importing re-runs the `config.TextOption(...)` class-body declarations and
> Picard logs a harmless idempotent "Option ... already declared" message — not an
> error. The `autouse` teardown drops the module so a later test's `import musefs`
> is a clean load.

**Verify:** passes under the env recipe; skips without Picard.

---

### Task 5 — `test_sync_roundtrip.py` (issue bullet 2)

**File:** `contrib/picard/tests/test_sync_roundtrip.py` (new)

Drive `_do_sync(opts, files)` with **autoscan off** against a seeded temp DB, a
`FakeMetadata` carrying tags + a `FakeImage` front cover, and assert the tags and
art land in the store with the right `SyncStats`. Autoscan off avoids shelling
out to the Rust binary; `db_path` already seeds `user_version = 1` so
`check_schema_version` passes.

```python
import pytest

from musefs._core import Opts, connect

pytest.importorskip("picard")

JPEG = b"\xff\xd8\xff\xe0" + b"\x00" * 32


def test_do_sync_writes_tags_and_art(db_path, make_track, fake_file, fake_metadata, fake_image):
    import musefs

    path = "/music/a.flac"
    tid = make_track(path)
    meta = fake_metadata(images=[fake_image(JPEG, "image/jpeg")], title="Song", artist="Band")
    f = fake_file(path, meta)
    files = {path: f}  # key is already a realpath for an absolute test path
    opts = Opts(db=db_path, bin="musefs", autoscan=False, fields={})

    stats = musefs._do_sync(opts, files)

    assert stats.synced == 1
    assert stats.art_linked == 1
    conn = connect(db_path)
    try:
        title = conn.execute(
            "SELECT value FROM tags WHERE track_id=? AND key='title'", (tid,)
        ).fetchone()[0]
        assert title == "Song"
        assert (
            conn.execute("SELECT COUNT(*) FROM track_art WHERE track_id=?", (tid,)).fetchone()[0]
            == 1
        )
    finally:
        conn.close()


def test_do_sync_no_db_raises():
    import musefs
    from musefs._core import MusefsError

    opts = Opts(db=None, bin="musefs", autoscan=False, fields={})
    with pytest.raises(MusefsError):
        musefs._do_sync(opts, {})
```

> `files` is keyed directly by the absolute test path. `_do_sync` uses the dict
> keys as realpath keys (it does not re-canonicalize), matching the
> `backing_path` seeded by `make_track`, so the row resolves.

**Verify:** passes under the env recipe; skips without Picard.

---

### Task 6 — `test_options_page.py` (glue)

**File:** `contrib/picard/tests/test_options_page.py` (new)

`qtbot`-driven load/save round-trip on the real `MusefsOptionsPage`. Under
`picard_config` the options are declared (import side effect), so `load()`/`save()`
have live `config.setting` to read/write.

```python
import pytest

pytest.importorskip("picard")


def test_options_page_load_reflects_config(qtbot, picard_config):
    import musefs

    picard_config.setting["musefs_db"] = "/tmp/seed.db"
    picard_config.setting["musefs_bin"] = "musefs"
    picard_config.setting["musefs_autoscan"] = True
    picard_config.setting["musefs_fields"] = ""

    page = musefs.MusefsOptionsPage()
    qtbot.addWidget(page)
    page.load()

    assert page._db.text() == "/tmp/seed.db"
    assert page._autoscan.isChecked() is True


def test_options_page_save_writes_config(qtbot, picard_config):
    import musefs

    page = musefs.MusefsOptionsPage()
    qtbot.addWidget(page)
    page.load()
    page._db.setText("/tmp/edited.db")
    page._autoscan.setChecked(False)
    page.save()

    assert picard_config.setting["musefs_db"] == "/tmp/edited.db"
    assert picard_config.setting["musefs_autoscan"] is False
```

> `picard_config` must come *before* a clean `import musefs` so the option
> declarations register against the initialized config. Importing inside the test
> body (after the fixture has run) guarantees that order regardless of collection
> order.

**Verify:** passes under the env recipe; skips without Picard.

---

### Task 7 — `test_callback_flow.py` (glue)

**File:** `contrib/picard/tests/test_callback_flow.py` (new)

Drive `MusefsSync.callback` end to end with `thread.run_task` replaced by a
synchronous stand-in matching its real signature
(`run_task(func, next_func=None, priority=0, thread_pool=None, traceback=True)`,
calling `next_func(result=...)`). Assert the worker ran (tags landed) and `_done`
logged the success summary.

```python
import pytest

pytest.importorskip("picard")

JPEG = b"\xff\xd8\xff\xe0" + b"\x00" * 32


def test_callback_runs_sync_and_logs_summary(
    monkeypatch, db_path, make_track, fake_file, fake_metadata, picard_config
):
    import musefs

    path = "/music/a.flac"
    make_track(path)

    # callback() runs resolve_config(settings, os.environ), and MUSEFS_DB/
    # MUSEFS_BIN env vars take precedence over the configured values
    # (_core.py resolve_config). Clear them so a stray env var on the test
    # host can't redirect the write away from the seeded DB.
    monkeypatch.delenv("MUSEFS_DB", raising=False)
    monkeypatch.delenv("MUSEFS_BIN", raising=False)

    # Point the plugin at the seeded DB with autoscan off (no Rust binary).
    picard_config.setting["musefs_db"] = db_path
    picard_config.setting["musefs_bin"] = "musefs"
    picard_config.setting["musefs_autoscan"] = False
    picard_config.setting["musefs_fields"] = ""

    # Synchronous run_task: run the worker, hand its result to the callback.
    def fake_run_task(func, next_func=None, priority=0, thread_pool=None, traceback=True):
        result = func()
        if next_func is not None:
            next_func(result=result)

    monkeypatch.setattr(musefs.thread, "run_task", fake_run_task)

    logged = []
    monkeypatch.setattr(musefs.log, "info", lambda fmt, *a: logged.append(fmt % a))

    meta = fake_metadata(title="Song")
    f = fake_file(path, meta)

    musefs._action.callback([f])

    # _do_sync ran against the real DB.
    from musefs._core import connect

    conn = connect(db_path)
    try:
        assert (
            conn.execute("SELECT value FROM tags WHERE key='title'").fetchone()[0] == "Song"
        )
    finally:
        conn.close()

    # _done logged the success summary.
    assert any("synced=1" in line for line in logged)
```

> `realpath_key` is applied to `f.filename` inside `_resolved_files`. The test
> path `/music/a.flac` is absolute and non-existent, so `os.path.realpath`
> returns it unchanged — matching the `backing_path` `make_track` seeded. (If a
> path component existed as a symlink this could diverge; these synthetic paths
> don't, so the key matches.)
>
> `_status` also calls `log.info` with a single positional `message`, so the
> lambda's `fmt % a` handles both the `"%s", message` and
> `"musefs: %s (files=%d)", ...` call shapes.

**Verify:** passes under the env recipe; skips without Picard.

---

### Task 8 — README: real-Picard env recipe

**File:** `contrib/picard/README.md`

Extend the **Tests** section with the real-Picard recipe and note the graceful
skip. Add after the existing `musefs_bin` paragraph (before the "Manual smoke
test" heading):

```markdown
### Real-Picard (pytest-qt) tests

The adapter (`musefs/__init__.py`) is exercised against a real Picard + PyQt5
install, headless. Picard isn't a clean pip wheel, so use the distro package and
bind a uv venv to the system Python it targets:

​```bash
sudo apt-get install -y picard                              # Picard at /usr/lib/picard + system PyQt5
uv venv --system-site-packages --python "$(which python3)"  # match apt Picard's C-ext interpreter
uv pip install -e 'contrib/picard[test]'                    # test extra includes pytest-qt
PYTHONPATH=/usr/lib/picard QT_QPA_PLATFORM=offscreen \
  .venv/bin/python -m pytest contrib/picard/tests -v
​```

These tests `importorskip("picard")`, so on a machine without Picard they skip
cleanly and only the Qt-free `_core` tests run.
```

(The `​` above are zero-width placeholders to show the nested fence — write a real
triple-backtick fence in the README.)

The existing "Manual smoke test (the GUI path is not unit-tested)" heading is now
partly outdated; soften it to "Manual smoke test (full GUI round-trip)" since the
adapter is now covered, but keep the steps.

**Verify:** `ruff` is not applied to markdown; just confirm the fences render.

---

### Task 9 — CI: add the `picard:` job

**File:** `.github/workflows/ci.yml`

Add a `picard:` job mirroring the `beets` job's *structure* but **without**
`actions/setup-python` (the venv must bind to the runner's apt system Python).
Match the repo's SHA-pinning convention for every action.

```yaml
  picard:
    runs-on: ubuntu-24.04
    steps:
      - uses: actions/checkout@de0fac2e4500dabe0009e67214ff5f5447ce83dd
        with:
          persist-credentials: false
      - name: Install Picard (system Python + PyQt5)
        run: sudo apt-get update && sudo apt-get install -y picard
      - uses: astral-sh/setup-uv@08807647e7069bb48b6ef5acd8ec9567f424441b
      - name: Create venv on the system Python
        run: uv venv --system-site-packages --python "$(which python3)"
      - name: Install plugin test deps + ruff
        run: uv pip install -e 'contrib/picard[test]' ruff
      - name: Lint
        run: |
          .venv/bin/ruff check contrib/picard/
          .venv/bin/ruff format --check contrib/picard/
      - name: Test (real Picard, headless)
        env:
          PYTHONPATH: /usr/lib/picard
          QT_QPA_PLATFORM: offscreen
        run: .venv/bin/python -m pytest contrib/picard/tests -v
```

> Pins are verbatim with **no version comment** (the repo convention — see
> `ci.yml`). `actions/checkout@de0fac2e...` matches the SHA already used by every
> other job, so copy it verbatim. `astral-sh/setup-uv@08807647...` has **no
> in-repo precedent**: independently confirm this SHA resolves to a real
> `astral-sh/setup-uv` tag (intended v8.1.0) before committing — don't trust the
> design-time value blindly.

**Verify:** `yamllint`/CI parses the job; locally re-run the full env recipe one
final time to confirm green.

## Self-review checklist

- [ ] `_core` tests untouched and still pass (no Picard needed).
- [ ] Each real-Picard module starts with `pytest.importorskip("picard")` and
      carries no pytest marker.
- [ ] Registration test spies on public functions only; forced re-import is
      cleaned up so later tests import normally.
- [ ] Round-trip and callback tests use autoscan **off** (no Rust binary needed).
- [ ] `test_callback_flow` is the only place `thread.run_task` is replaced; its
      stand-in matches the real signature and calls `next_func(result=...)`.
- [ ] CI job uses `ubuntu-24.04` (pinned), no `setup-python`, SHA-pinned actions,
      `persist-credentials: false`.
- [ ] README recipe matches the CI invocation exactly.
- [ ] Full suite green under
      `PYTHONPATH=/usr/lib/picard QT_QPA_PLATFORM=offscreen .venv/bin/python -m pytest contrib/picard/tests`.

## Out of scope (from the spec)

- Deep adapter error-path/edge-case matrices (covered against `_core`).
- The opt-in `musefs_bin` Rust-binary gate — unchanged.
- Any change to plugin runtime behavior — test/CI only.
