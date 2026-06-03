# python-musefs Shared Library Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extract the duplicated musefs SQLite-store contract, `musefs scan` shell-out, and per-file sync write-loop out of the beets and Picard plugins into one publishable Python library, `python-musefs` (import package `musefs_common`), consumed by beets as a pip dependency and by Picard via committed vendoring.

**Architecture:** A new `contrib/python-musefs/` package holds the store contract (schema check, `tags`/`art`/`track_art` writes, art content-addressing, `realpath_key`), `run_scan` (raising a structured `ScanError`), and a `Record`/`sync_files` write-loop. The two plugins keep only host-shaped logic (field mapping, art acquisition, options/event wiring) and translate `ScanError`/`SchemaMismatch` to their existing host-native error types with **verbatim** message text. Picard gets the library vendored into `musefs/_common/`, guarded by a byte-identical drift test.

**Tech Stack:** Python 3.8+ (Picard's declared floor), `sqlite3`, `pytest`, `ruff`, setuptools. No Rust/schema changes.

**Spec:** `docs/superpowers/specs/2026-06-03-python-musefs-design.md`

**A note on `skipped_art` counting (refines the spec):** beets dedups one album cover across its tracks via a per-run art cache and counts an unreadable *or* over-cap cover **once**. To preserve that exact count, the **beets adapter** reads art, size-gates it, and counts *both* unreadable and over-cap into `skipped_art` (once per unique cover) — passing only within-cap bytes as `Record.art`. Picard passes one raw front-cover per file and relies on `sync_one`'s size-gate (counted once per file). Both match legacy behavior; `sync_files` accepts a caller-seeded `stats` so beets' pre-counted stats flow through.

---

## File Structure

**New — `contrib/python-musefs/`:**
- `pyproject.toml` — dist `python-musefs`, package discovery under `src/`.
- `ruff.toml` — lint/format config (py38 target).
- `src/musefs_common/__init__.py` — public API re-exports + `__version__`.
- `src/musefs_common/constants.py` — `EXPECTED_USER_VERSION`, `MAX_ART_BYTES`.
- `src/musefs_common/errors.py` — `SchemaMismatch`, `ScanError`.
- `src/musefs_common/paths.py` — `realpath_key`.
- `src/musefs_common/store.py` — DB connect/schema/tags/art writes.
- `src/musefs_common/scan.py` — `run_scan`.
- `src/musefs_common/sync.py` — `Record`, `SyncStats`, `sync_one`, `sync_files`.
- `tests/conftest.py`, `tests/schema.sql`, `tests/test_*.py` — canonical suite.
- `vendor_to_picard.py` — copies the package into Picard's `_common/`.

**Modified — beets (`contrib/beets/`):**
- `beetsplug/_core.py` — slimmed to beets mapping + `build_records`.
- `beetsplug/musefs.py` — imports library; `_sync`/`_run_scan` rewritten.
- `tests/conftest.py` — import `connect` from `musefs_common`.
- `pyproject.toml` — add `python-musefs` dependency.
- Delete duplicated store/scan tests (`tests/test_db.py`, and the store-level cases in `tests/test_sync.py`/`tests/test_art.py` per Task 11).

**Modified — Picard (`contrib/picard/`):**
- `musefs/_common/` — vendored copy (generated; committed).
- `musefs/_core.py` — slimmed to Picard mapping/options + `MusefsError`.
- `musefs/__init__.py` — imports library from `musefs._common`; `_do_sync` rewritten.
- `tests/conftest.py` — import `connect` from `musefs._common`.
- `tests/test_vendor_sync.py` — new drift guard.
- `ruff.toml` — exclude `musefs/_common/`.
- Delete duplicated store tests (`tests/test_core_db.py`, store-level cases in `tests/test_sync.py`/`tests/test_run_scan.py` per Task 14).

**Modified — repo root:**
- `.github/workflows/ci.yml` — new `python-musefs` job; add to `ci-ok` needs; beets install ordering.
- `contrib/beets/README.md`, `contrib/picard/README.md`, new `contrib/python-musefs/README.md`, `docs/ROADMAP.md`.

---

## Task 0: Spike the Picard vendored-subpackage import (GATING)

This validates the load-bearing assumption that Picard can import a committed subpackage `musefs._common` while `musefs/__init__.py` (the plugin) is mid-import. A negative result reshapes the layout (flat `musefs/_store.py` etc.), so resolve it before any code moves.

**Files:**
- Create (throwaway): `/tmp/musefs_spike/musefs/__init__.py`, `/tmp/musefs_spike/musefs/_common/__init__.py`, `/tmp/musefs_spike/musefs/_common/store.py`

- [ ] **Step 1: Build a minimal folder-plugin mimic**

```bash
mkdir -p /tmp/musefs_spike/musefs/_common
cat > /tmp/musefs_spike/musefs/_common/store.py <<'EOF'
def connect():
    return "ok"
EOF
cat > /tmp/musefs_spike/musefs/_common/__init__.py <<'EOF'
from .store import connect
EOF
cat > /tmp/musefs_spike/musefs/__init__.py <<'EOF'
from musefs._common import connect
PLUGIN_NAME = "spike"
RESULT = connect()
EOF
```

- [ ] **Step 2: Import it the way Picard does (plugins dir on sys.path, import top-level package)**

Run:
```bash
cd /tmp/musefs_spike && python -c "import sys; sys.path.insert(0, '.'); import musefs; print(musefs.RESULT)"
```
Expected: prints `ok`. This confirms `from musefs._common import connect` resolves during the package's own `__init__` execution.

- [ ] **Step 3: Confirm against the real Picard plugin loader (if a Picard install is available)**

If the machine has Picard (`test -d /usr/lib/picard/picard`), additionally confirm Picard's loader handles a subpackage by checking its plugin loader does a normal `importlib` import of the folder (it does for "folder plugins"). If no Picard is available, the Step 2 result plus the existing `musefs.__init__` already using `from musefs._core import …` (a sibling-module import that works today) is sufficient evidence the subpackage form will load.

- [ ] **Step 4: Decision gate**

If Step 2 printed `ok`: proceed with the `musefs/_common/` subpackage layout as written in this plan.
If it failed: STOP and switch the vendored layout to flat relatively-imported modules (`musefs/_store.py`, `musefs/_scan.py`, `musefs/_sync.py`, etc.) and adjust Tasks 12–14 accordingly before continuing.

- [ ] **Step 5: Clean up**

```bash
rm -rf /tmp/musefs_spike
```

(No commit — throwaway spike.)

---

## Task 1: Scaffold the `python-musefs` package

**Files:**
- Create: `contrib/python-musefs/pyproject.toml`
- Create: `contrib/python-musefs/ruff.toml`
- Create: `contrib/python-musefs/src/musefs_common/__init__.py` (placeholder)
- Create: `contrib/python-musefs/tests/schema.sql`
- Create: `contrib/python-musefs/tests/conftest.py`

- [ ] **Step 1: Write `pyproject.toml`**

```toml
[build-system]
requires = ["setuptools>=61"]
build-backend = "setuptools.build_meta"

[project]
name = "python-musefs"
version = "0.1.0"
description = "Shared musefs SQLite-store contract for the beets and Picard plugins"
requires-python = ">=3.8"

[project.optional-dependencies]
test = ["pytest>=7"]

[tool.setuptools.packages.find]
where = ["src"]

[tool.pytest.ini_options]
testpaths = ["tests"]
pythonpath = ["src"]
markers = [
    "musefs_bin: tests that shell out to the real `musefs` Rust binary (opt-in)",
]
addopts = "-m 'not musefs_bin'"
```

- [ ] **Step 2: Write `ruff.toml`** (matches the Picard target so vendored code lints clean there)

```toml
line-length = 100
target-version = "py38"

[lint]
select = ["E", "F", "I", "N", "W"]

[format]
preview = true
```

- [ ] **Step 3: Placeholder package init**

`src/musefs_common/__init__.py`:
```python
"""python-musefs: the shared musefs SQLite-store contract."""
```

- [ ] **Step 4: Copy the canonical schema fixture**

```bash
cp contrib/beets/tests/schema.sql contrib/python-musefs/tests/schema.sql
```

- [ ] **Step 5: Write `tests/conftest.py`**

```python
import sqlite3
import time
from pathlib import Path

import pytest

from musefs_common import connect as musefs_connect

SCHEMA_SQL = (Path(__file__).parent / "schema.sql").read_text()

# Minimal valid JPEG header + padding; used as fake cover-art bytes in tests.
JPEG = b"\xff\xd8\xff\xe0" + b"\x00" * 32


@pytest.fixture
def db_path(tmp_path):
    """A temp musefs DB with the V2 schema applied."""
    path = tmp_path / "musefs.db"
    conn = sqlite3.connect(str(path))
    conn.executescript(SCHEMA_SQL)
    conn.commit()
    conn.close()
    return str(path)


def insert_track(conn, backing_path, fmt="flac"):
    """Insert a minimal track row (as `musefs scan` would) and return its id."""
    now = int(time.time())
    cur = conn.execute(
        "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, "
        "backing_size, backing_mtime, updated_at) VALUES (?, ?, 0, 0, 0, 0, ?)",
        (backing_path, fmt, now),
    )
    return cur.lastrowid


@pytest.fixture
def make_track(db_path):
    """Return a helper that inserts a track row and returns its id."""

    def _make(backing_path, fmt="flac"):
        conn = musefs_connect(db_path)
        try:
            tid = insert_track(conn, backing_path, fmt)
            conn.commit()
            return tid
        finally:
            conn.close()

    return _make
```

- [ ] **Step 6: Verify the package imports (will fail until `connect` exists — expected)**

Run: `cd contrib/python-musefs && python -m pytest --collect-only 2>&1 | tail -5`
Expected: collection errors referencing `cannot import name 'connect' from 'musefs_common'` (no tests yet; `connect` lands in Task 5). This confirms `pythonpath`/discovery is wired.

- [ ] **Step 7: Commit**

```bash
git add contrib/python-musefs/pyproject.toml contrib/python-musefs/ruff.toml \
  contrib/python-musefs/src/musefs_common/__init__.py \
  contrib/python-musefs/tests/schema.sql contrib/python-musefs/tests/conftest.py
git commit -m "feat(python-musefs): scaffold shared store-contract package"
```

---

## Task 2: `constants.py`

**Files:**
- Create: `contrib/python-musefs/src/musefs_common/constants.py`
- Test: `contrib/python-musefs/tests/test_constants.py`

- [ ] **Step 1: Write the failing test**

`tests/test_constants.py`:
```python
from musefs_common import constants


def test_expected_user_version_matches_rust_migrations():
    # Mirrors musefs-db/src/schema.rs MIGRATIONS length (V2).
    assert constants.EXPECTED_USER_VERSION == 2


def test_max_art_bytes_is_16mib_minus_64kib():
    assert constants.MAX_ART_BYTES == 16 * 1024 * 1024 - 64 * 1024
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd contrib/python-musefs && python -m pytest tests/test_constants.py -v`
Expected: FAIL — `ModuleNotFoundError: No module named 'musefs_common.constants'`.

- [ ] **Step 3: Write `constants.py`**

```python
# Schema version this library was written against (musefs schema.rs MIGRATIONS
# length). Consumers refuse to run against any other version.
EXPECTED_USER_VERSION = 2

# Mirror of musefs-core scan.rs MAX_ART_BYTES: 16 MiB minus 64 KiB headroom.
MAX_ART_BYTES = 16 * 1024 * 1024 - 64 * 1024
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd contrib/python-musefs && python -m pytest tests/test_constants.py -v`
Expected: PASS (2 passed).

- [ ] **Step 5: Commit**

```bash
git add contrib/python-musefs/src/musefs_common/constants.py contrib/python-musefs/tests/test_constants.py
git commit -m "feat(python-musefs): add schema/art constants"
```

---

## Task 3: `errors.py` — `SchemaMismatch` and structured `ScanError`

`ScanError` is structured (`kind` + context attributes) so each host can rebuild its exact legacy error string.

**Files:**
- Create: `contrib/python-musefs/src/musefs_common/errors.py`
- Test: `contrib/python-musefs/tests/test_errors.py`

- [ ] **Step 1: Write the failing test**

`tests/test_errors.py`:
```python
import pytest

from musefs_common.errors import SchemaMismatch, ScanError


def test_schema_mismatch_message_and_found():
    exc = SchemaMismatch(5)
    assert exc.found == 5
    assert "user_version is 5" in str(exc)
    assert "diverged" in str(exc)


def test_scan_error_not_found():
    exc = ScanError("not_found", binary="musefs", target="/x.flac")
    assert exc.kind == "not_found"
    assert exc.binary == "musefs"
    assert "not found" in str(exc)


def test_scan_error_timeout_carries_timeout():
    exc = ScanError("timeout", binary="musefs", target="/x.flac", timeout=120)
    assert exc.kind == "timeout"
    assert exc.timeout == 120
    assert "timed out" in str(exc)


def test_scan_error_failed_carries_returncode_and_stderr():
    exc = ScanError("failed", binary="musefs", target="/x.flac", returncode=2, stderr="boom")
    assert exc.kind == "failed"
    assert exc.returncode == 2
    assert exc.stderr == "boom"
    assert "exit 2" in str(exc)


def test_scan_error_is_an_exception():
    with pytest.raises(ScanError):
        raise ScanError("not_found", binary="m", target="/x")
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd contrib/python-musefs && python -m pytest tests/test_errors.py -v`
Expected: FAIL — `ModuleNotFoundError: No module named 'musefs_common.errors'`.

- [ ] **Step 3: Write `errors.py`**

```python
from .constants import EXPECTED_USER_VERSION


class SchemaMismatch(Exception):  # noqa: N818
    """Raised when the musefs DB schema version differs from what this library
    targets (``EXPECTED_USER_VERSION``)."""

    def __init__(self, found):
        self.found = found
        super().__init__(
            f"musefs DB user_version is {found}, plugin targets "
            f"{EXPECTED_USER_VERSION}; the musefs and plugin versions have "
            f"diverged."
        )


class ScanError(Exception):  # noqa: N818
    """A `musefs scan` invocation failed. ``kind`` is one of ``"not_found"``,
    ``"timeout"``, ``"failed"``; the remaining attributes carry enough context
    for a host adapter to format its own user-facing message."""

    def __init__(self, kind, *, binary, target, timeout=None, returncode=None, stderr=""):
        self.kind = kind
        self.binary = binary
        self.target = target
        self.timeout = timeout
        self.returncode = returncode
        self.stderr = stderr
        super().__init__(self._default_message())

    def _default_message(self):
        if self.kind == "not_found":
            return f"musefs binary '{self.binary}' not found"
        if self.kind == "timeout":
            return f"`{self.binary} scan` for {self.target} timed out after {self.timeout}s"
        return (
            f"`{self.binary} scan` failed for {self.target} "
            f"(exit {self.returncode}): {self.stderr}"
        )
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd contrib/python-musefs && python -m pytest tests/test_errors.py -v`
Expected: PASS (5 passed).

- [ ] **Step 5: Commit**

```bash
git add contrib/python-musefs/src/musefs_common/errors.py contrib/python-musefs/tests/test_errors.py
git commit -m "feat(python-musefs): add SchemaMismatch and structured ScanError"
```

---

## Task 4: `paths.py` — `realpath_key`

> **Intentional deviation from the spec's file layout:** the spec's package-layout
> sketch lists `_to_int` alongside `realpath_key` in `paths.py`. We deliberately
> keep `_to_int` per-host (in each plugin's slimmed `_core.py`) instead, because
> the library itself never calls it and the two hosts' `_to_int` are mapping
> helpers, not store contract. `paths.py` holds only `realpath_key`.

**Files:**
- Create: `contrib/python-musefs/src/musefs_common/paths.py`
- Test: `contrib/python-musefs/tests/test_paths.py`

- [ ] **Step 1: Write the failing test**

`tests/test_paths.py`:
```python
import os

from musefs_common.paths import realpath_key


def test_returns_absolute_canonical_str(tmp_path):
    f = tmp_path / "a.flac"
    f.write_bytes(b"x")
    key = realpath_key(str(f))
    assert key == os.path.realpath(str(f))
    assert isinstance(key, str)


def test_accepts_bytes_path(tmp_path):
    f = tmp_path / "b.flac"
    f.write_bytes(b"x")
    key = realpath_key(os.fsencode(str(f)))
    assert isinstance(key, str)
    assert key.endswith("b.flac")


def test_non_utf8_byte_maps_to_replacement_char(tmp_path):
    # A path byte that isn't valid UTF-8 must normalize to U+FFFD, matching
    # Rust's to_string_lossy (not surrogateescape), so both sides key alike.
    raw = os.fsencode(str(tmp_path)) + b"/\xff.flac"
    key = realpath_key(raw)
    assert "�" in key
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd contrib/python-musefs && python -m pytest tests/test_paths.py -v`
Expected: FAIL — `ModuleNotFoundError: No module named 'musefs_common.paths'`.

- [ ] **Step 3: Write `paths.py`**

```python
import os


def realpath_key(path):
    """Canonical absolute path string matching musefs scan's stored
    ``backing_path`` (``std::fs::canonicalize`` + ``to_string_lossy``).

    Accepts ``str`` or ``bytes`` and always returns ``str``.
    """
    real = os.path.realpath(path)
    if isinstance(real, bytes):
        real = os.fsdecode(real)
    # os.fsdecode uses surrogateescape; Rust's to_string_lossy uses U+FFFD for
    # undecodable bytes. Normalize so a non-UTF-8 path component produces the
    # same key string on both sides instead of silently mismatching.
    return real.encode("utf-8", "surrogateescape").decode("utf-8", "replace")
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd contrib/python-musefs && python -m pytest tests/test_paths.py -v`
Expected: PASS (3 passed).

- [ ] **Step 5: Commit**

```bash
git add contrib/python-musefs/src/musefs_common/paths.py contrib/python-musefs/tests/test_paths.py
git commit -m "feat(python-musefs): add realpath_key path normalization"
```

---

## Task 5: `store.py` — connect, schema check, track lookup, prune

**Files:**
- Create: `contrib/python-musefs/src/musefs_common/store.py`
- Test: `contrib/python-musefs/tests/test_store_db.py`

- [ ] **Step 1: Write the failing test**

`tests/test_store_db.py`:
```python
import os

import pytest

from musefs_common import connect, prune_missing, track_id_for_path
from musefs_common.errors import SchemaMismatch
from musefs_common.store import check_schema_version

from conftest import insert_track


def test_connect_sets_pragmas(db_path):
    conn = connect(db_path)
    try:
        assert conn.execute("PRAGMA foreign_keys").fetchone()[0] == 1
        assert conn.execute("PRAGMA busy_timeout").fetchone()[0] == 5000
    finally:
        conn.close()


def test_check_schema_version_passes_on_v2(db_path):
    conn = connect(db_path)
    try:
        check_schema_version(conn)  # schema.sql applies user_version=2
    finally:
        conn.close()


def test_check_schema_version_raises_on_mismatch(db_path):
    conn = connect(db_path)
    try:
        conn.execute("PRAGMA user_version = 99")
        with pytest.raises(SchemaMismatch) as ei:
            check_schema_version(conn)
        assert ei.value.found == 99
    finally:
        conn.close()


def test_track_id_for_path_found_and_missing(db_path):
    conn = connect(db_path)
    try:
        tid = insert_track(conn, "/music/a.flac")
        conn.commit()
        assert track_id_for_path(conn, "/music/a.flac") == tid
        assert track_id_for_path(conn, "/music/nope.flac") is None
    finally:
        conn.close()


def test_prune_missing_removes_absent_backing_files(db_path, tmp_path):
    present = tmp_path / "present.flac"
    present.write_bytes(b"x")
    conn = connect(db_path)
    try:
        keep = insert_track(conn, str(present))
        gone = insert_track(conn, str(tmp_path / "gone.flac"))
        conn.commit()
        pruned = prune_missing(conn)
        conn.commit()
        assert pruned == 1
        assert track_id_for_path(conn, str(present)) == keep
        assert conn.execute("SELECT COUNT(*) FROM tracks WHERE id=?", (gone,)).fetchone()[0] == 0
    finally:
        conn.close()


def test_prune_missing_scoped_to_track_ids(db_path, tmp_path):
    conn = connect(db_path)
    try:
        a = insert_track(conn, str(tmp_path / "a.flac"))  # absent
        b = insert_track(conn, str(tmp_path / "b.flac"))  # absent, but not in scope
        conn.commit()
        pruned = prune_missing(conn, track_ids=[a])
        conn.commit()
        assert pruned == 1
        assert track_id_for_path(conn, str(tmp_path / "b.flac")) == b
    finally:
        conn.close()
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd contrib/python-musefs && python -m pytest tests/test_store_db.py -v`
Expected: FAIL — `ImportError: cannot import name 'connect' from 'musefs_common'`.

- [ ] **Step 3: Write `store.py` (this task's portion)**

```python
import os
import sqlite3

from .constants import EXPECTED_USER_VERSION
from .errors import SchemaMismatch


def connect(db_path):
    """Open the musefs DB with a busy timeout and foreign keys enabled."""
    conn = sqlite3.connect(db_path)
    # 5s busy timeout so a brief write doesn't fail while the FUSE mount reads.
    conn.execute("PRAGMA busy_timeout = 5000")
    conn.execute("PRAGMA foreign_keys = ON")
    return conn


def check_schema_version(conn):
    """Raise ``SchemaMismatch`` unless the DB's ``user_version`` matches the
    version this library targets. Call on an open connection from ``connect``."""
    found = conn.execute("PRAGMA user_version").fetchone()[0]
    if found != EXPECTED_USER_VERSION:
        raise SchemaMismatch(found)


def track_id_for_path(conn, key):
    """Return the track id whose backing_path equals ``key``, or None."""
    row = conn.execute("SELECT id FROM tracks WHERE backing_path = ?", (key,)).fetchone()
    return row[0] if row else None


def prune_missing(conn, track_ids=None):
    """Delete track rows whose backing file no longer exists on disk.

    When ``track_ids`` is provided, only those tracks are checked and
    potentially pruned. Otherwise, every track in the database is checked.
    Returns the number pruned.
    """
    if track_ids is not None:
        gone = []
        for tid in track_ids:
            row = conn.execute("SELECT backing_path FROM tracks WHERE id=?", (tid,)).fetchone()
            if row is not None and not os.path.exists(row[0]):
                gone.append((tid,))
    else:
        gone = [
            (tid,)
            for tid, path in conn.execute("SELECT id, backing_path FROM tracks")
            if not os.path.exists(path)
        ]
    conn.executemany("DELETE FROM tracks WHERE id = ?", gone)
    return len(gone)
```

- [ ] **Step 4: Add the temporary re-export so `connect` is importable from the package root**

Append to `src/musefs_common/__init__.py`:
```python
from .store import check_schema_version, connect, prune_missing, track_id_for_path
```
(The full `__init__` is finalized in Task 9; this interim line keeps the conftest and tests importing now.)

- [ ] **Step 5: Run test to verify it passes**

Run: `cd contrib/python-musefs && python -m pytest tests/test_store_db.py -v`
Expected: PASS (6 passed).

- [ ] **Step 6: Commit**

```bash
git add contrib/python-musefs/src/musefs_common/store.py \
  contrib/python-musefs/src/musefs_common/__init__.py \
  contrib/python-musefs/tests/test_store_db.py
git commit -m "feat(python-musefs): add connect/schema-check/track-lookup/prune"
```

---

## Task 6: `store.py` — tags and art writes

**Files:**
- Modify: `contrib/python-musefs/src/musefs_common/store.py`
- Modify: `contrib/python-musefs/src/musefs_common/__init__.py`
- Test: `contrib/python-musefs/tests/test_store_art.py`

- [ ] **Step 1: Write the failing test**

`tests/test_store_art.py`:
```python
from musefs_common import (
    connect,
    replace_tags,
    replace_track_art,
    sniff_mime,
    upsert_art,
)

from conftest import JPEG, insert_track

PNG = b"\x89PNG\r\n\x1a\n" + b"\x00" * 16
WEBP = b"RIFF" + b"\x00\x00\x00\x00" + b"WEBP" + b"\x00" * 8


def test_sniff_mime_magic_bytes():
    assert sniff_mime(JPEG, "/x") == "image/jpeg"
    assert sniff_mime(PNG, "/x") == "image/png"
    assert sniff_mime(WEBP, "/x") == "image/webp"


def test_sniff_mime_extension_fallback():
    assert sniff_mime(b"nope", "/x.png") == "image/png"
    assert sniff_mime(b"nope", "/x.bin") == "application/octet-stream"


def test_replace_tags_assigns_incrementing_ordinals(db_path):
    conn = connect(db_path)
    try:
        tid = insert_track(conn, "/m/a.flac")
        replace_tags(conn, tid, [("genre", "Rock"), ("genre", "Pop"), ("title", "T")])
        conn.commit()
        rows = conn.execute(
            "SELECT key, value, ordinal FROM tags WHERE track_id=? ORDER BY key, ordinal", (tid,)
        ).fetchall()
        assert ("genre", "Rock", 0) in rows
        assert ("genre", "Pop", 1) in rows
        assert ("title", "T", 0) in rows
    finally:
        conn.close()


def test_replace_tags_preserves_binary_tags(db_path):
    # Scanner-written binary tags (value_blob NOT NULL) must survive a sync (#82).
    conn = connect(db_path)
    try:
        tid = insert_track(conn, "/m/a.flac")
        conn.execute(
            "INSERT INTO tags (track_id, key, value, value_blob, ordinal) "
            "VALUES (?, 'priv', '', ?, 0)",
            (tid, b"\x01\x02"),
        )
        conn.commit()
        replace_tags(conn, tid, [("title", "T")])
        conn.commit()
        blobs = conn.execute(
            "SELECT COUNT(*) FROM tags WHERE track_id=? AND value_blob IS NOT NULL", (tid,)
        ).fetchone()[0]
        assert blobs == 1
    finally:
        conn.close()


def test_upsert_art_is_content_addressed(db_path):
    conn = connect(db_path)
    try:
        first = upsert_art(conn, JPEG, "image/jpeg")
        again = upsert_art(conn, JPEG, "image/png")  # same bytes -> same id, mime ignored
        conn.commit()
        assert first == again
        mime = conn.execute("SELECT mime FROM art WHERE id=?", (first,)).fetchone()[0]
        assert mime == "image/jpeg"
    finally:
        conn.close()


def test_replace_track_art_sets_and_replaces_front_cover(db_path):
    conn = connect(db_path)
    try:
        tid = insert_track(conn, "/m/a.flac")
        first = upsert_art(conn, JPEG, "image/jpeg")
        before = conn.execute("SELECT content_version FROM tracks WHERE id=?", (tid,)).fetchone()[0]
        replace_track_art(conn, tid, first)
        conn.commit()
        row = conn.execute(
            "SELECT art_id, picture_type, ordinal FROM track_art WHERE track_id=?", (tid,)
        ).fetchone()
        assert row == (first, 3, 0)
        # The track_art trigger bumps content_version (mount cache invalidation).
        after = conn.execute("SELECT content_version FROM tracks WHERE id=?", (tid,)).fetchone()[0]
        assert after > before
        # Replacing leaves exactly one row, now pointing at the new art.
        second = upsert_art(conn, PNG, "image/png")
        replace_track_art(conn, tid, second)
        conn.commit()
        rows = conn.execute("SELECT art_id FROM track_art WHERE track_id=?", (tid,)).fetchall()
        assert rows == [(second,)]
    finally:
        conn.close()
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd contrib/python-musefs && python -m pytest tests/test_store_art.py -v`
Expected: FAIL — `ImportError: cannot import name 'replace_tags' from 'musefs_common'`.

- [ ] **Step 3: Append to `store.py`**

```python
import hashlib  # add to the imports at the top of store.py


def replace_tags(conn, track_id, pairs):
    """Replace all tags for a track. Duplicate keys get incrementing ordinals
    (mirroring musefs scan ingest)."""
    # Scope to the plugin-owned text rows: scanner-written binary tags
    # (value_blob NOT NULL) must survive a sync (#82).
    conn.execute("DELETE FROM tags WHERE track_id = ? AND value_blob IS NULL", (track_id,))
    ordinals = {}
    rows = []
    for key, value in pairs:
        ordinal = ordinals.get(key, 0)
        ordinals[key] = ordinal + 1
        rows.append((track_id, key, value, ordinal))
    conn.executemany(
        "INSERT INTO tags (track_id, key, value, ordinal) VALUES (?, ?, ?, ?)",
        rows,
    )


_EXT_MIME = {
    ".jpg": "image/jpeg",
    ".jpeg": "image/jpeg",
    ".png": "image/png",
    ".webp": "image/webp",
}


def sniff_mime(data, path):
    """Detect image mime from magic bytes, falling back to file extension."""
    if data[:3] == b"\xff\xd8\xff":
        return "image/jpeg"
    if data[:8] == b"\x89PNG\r\n\x1a\n":
        return "image/png"
    # WebP: 'RIFF' <4-byte size> 'WEBP'.
    if data[:4] == b"RIFF" and data[8:12] == b"WEBP":
        return "image/webp"
    ext = os.path.splitext(path)[1].lower()
    return _EXT_MIME.get(ext, "application/octet-stream")


def upsert_art(conn, data, mime):
    """Content-address ``data`` by sha256 and return its art id, inserting only
    if new (mirrors musefs Db::upsert_art). If the sha256 already exists, the
    stored row (and its mime) is kept and the ``mime`` argument is ignored."""
    sha = hashlib.sha256(data).hexdigest()
    conn.execute(
        "INSERT INTO art (sha256, mime, width, height, byte_len, data) "
        "VALUES (?, ?, NULL, NULL, ?, ?) ON CONFLICT(sha256) DO NOTHING",
        (sha, mime, len(data), data),
    )
    return conn.execute("SELECT id FROM art WHERE sha256 = ?", (sha,)).fetchone()[0]


def replace_track_art(conn, track_id, art_id):
    """Set the track's single front-cover art (picture_type 3, ordinal 0)."""
    conn.execute("DELETE FROM track_art WHERE track_id = ?", (track_id,))
    conn.execute(
        "INSERT INTO track_art (track_id, art_id, picture_type, description, "
        "ordinal) VALUES (?, ?, 3, '', 0)",
        (track_id, art_id),
    )
```

- [ ] **Step 4: Extend the interim re-export in `__init__.py`**

Replace the interim store import line with:
```python
from .store import (
    check_schema_version,
    connect,
    prune_missing,
    replace_tags,
    replace_track_art,
    sniff_mime,
    track_id_for_path,
    upsert_art,
)
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cd contrib/python-musefs && python -m pytest tests/test_store_art.py -v`
Expected: PASS (6 passed).

- [ ] **Step 6: Commit**

```bash
git add contrib/python-musefs/src/musefs_common/store.py \
  contrib/python-musefs/src/musefs_common/__init__.py \
  contrib/python-musefs/tests/test_store_art.py
git commit -m "feat(python-musefs): add tag + art writes (binary-tag-safe)"
```

---

## Task 7: `scan.py` — `run_scan`

**Files:**
- Create: `contrib/python-musefs/src/musefs_common/scan.py`
- Modify: `contrib/python-musefs/src/musefs_common/__init__.py`
- Test: `contrib/python-musefs/tests/test_scan.py`

- [ ] **Step 1: Write the failing test**

`tests/test_scan.py`:
```python
import os
import stat

import pytest

from musefs_common import run_scan
from musefs_common.errors import ScanError


def _fake_binary(tmp_path, body):
    p = tmp_path / "fakemusefs"
    p.write_text("#!/bin/sh\n" + body + "\n")
    p.chmod(p.stat().st_mode | stat.S_IEXEC)
    return str(p)


def test_run_scan_success(tmp_path):
    binary = _fake_binary(tmp_path, "exit 0")
    run_scan(binary, str(tmp_path / "m.db"), str(tmp_path / "a.flac"))  # no raise


def test_run_scan_binary_not_found(tmp_path):
    with pytest.raises(ScanError) as ei:
        run_scan(str(tmp_path / "does-not-exist"), str(tmp_path / "m.db"), "/a.flac")
    assert ei.value.kind == "not_found"
    assert ei.value.binary.endswith("does-not-exist")


def test_run_scan_nonzero_exit_carries_stderr(tmp_path):
    binary = _fake_binary(tmp_path, "echo 'bad file' >&2; exit 3")
    with pytest.raises(ScanError) as ei:
        run_scan(binary, str(tmp_path / "m.db"), "/a.flac")
    assert ei.value.kind == "failed"
    assert ei.value.returncode == 3
    assert ei.value.stderr == "bad file"


def test_run_scan_timeout(tmp_path):
    binary = _fake_binary(tmp_path, "sleep 5")
    with pytest.raises(ScanError) as ei:
        run_scan(binary, str(tmp_path / "m.db"), "/a.flac", timeout=1)
    assert ei.value.kind == "timeout"
    assert ei.value.timeout == 1
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd contrib/python-musefs && python -m pytest tests/test_scan.py -v`
Expected: FAIL — `ImportError: cannot import name 'run_scan' from 'musefs_common'`.

- [ ] **Step 3: Write `scan.py`**

```python
import subprocess

from .errors import ScanError


def run_scan(binary, db_path, target, *, timeout=None):
    """Run ``<binary> scan <target> --db <db_path>``. Creates the DB if absent
    and fills the structural columns a plugin can't compute. Raises ``ScanError``
    (with ``kind`` in ``"not_found" | "timeout" | "failed"``) on failure; the
    caller formats its own user-facing message from the exception attributes."""
    try:
        result = subprocess.run(
            [binary, "scan", target, "--db", db_path],
            capture_output=True,
            timeout=timeout,
        )
    except FileNotFoundError:
        raise ScanError("not_found", binary=binary, target=target)
    except subprocess.TimeoutExpired:
        raise ScanError("timeout", binary=binary, target=target, timeout=timeout)
    if result.returncode != 0:
        raise ScanError(
            "failed",
            binary=binary,
            target=target,
            returncode=result.returncode,
            stderr=result.stderr.decode(errors="replace").strip(),
        )
```

- [ ] **Step 4: Add the interim re-export**

Append to `__init__.py`:
```python
from .errors import SchemaMismatch, ScanError
from .scan import run_scan
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cd contrib/python-musefs && python -m pytest tests/test_scan.py -v`
Expected: PASS (4 passed).

- [ ] **Step 6: Commit**

```bash
git add contrib/python-musefs/src/musefs_common/scan.py \
  contrib/python-musefs/src/musefs_common/__init__.py \
  contrib/python-musefs/tests/test_scan.py
git commit -m "feat(python-musefs): add run_scan shell-out raising ScanError"
```

---

## Task 8: `sync.py` — `Record`, `SyncStats`, `sync_one`, `sync_files`

**Files:**
- Create: `contrib/python-musefs/src/musefs_common/sync.py`
- Modify: `contrib/python-musefs/src/musefs_common/__init__.py`
- Test: `contrib/python-musefs/tests/test_sync.py`

- [ ] **Step 1: Write the failing test**

`tests/test_sync.py`:
```python
from musefs_common import Record, SyncStats, connect, sync_files, sync_one
from musefs_common.constants import MAX_ART_BYTES

from conftest import JPEG, insert_track


def _seed(db_path, path="/m/a.flac"):
    conn = connect(db_path)
    tid = insert_track(conn, path)
    conn.commit()
    return conn, tid


def test_sync_one_skips_unmatched_path(db_path):
    conn = connect(db_path)
    try:
        stats = SyncStats()
        sync_one(conn, Record(key="/nope.flac", pairs=[("title", "T")], art=None), stats)
        assert stats.skipped == 1
        assert stats.synced == 0
    finally:
        conn.close()


def test_sync_one_writes_tags_and_art(db_path):
    conn, _ = _seed(db_path)
    try:
        stats = SyncStats()
        sync_one(conn, Record(key="/m/a.flac", pairs=[("title", "T")], art=(JPEG, "image/jpeg")), stats)
        conn.commit()
        assert stats.synced == 1
        assert stats.art_linked == 1
        assert conn.execute("SELECT value FROM tags WHERE key='title'").fetchone()[0] == "T"
        assert conn.execute("SELECT COUNT(*) FROM track_art").fetchone()[0] == 1
    finally:
        conn.close()


def test_sync_one_over_cap_art_skipped_not_linked(db_path):
    conn, _ = _seed(db_path)
    try:
        big = b"\xff\xd8\xff" + b"\x00" * (MAX_ART_BYTES + 1)
        stats = SyncStats()
        sync_one(conn, Record(key="/m/a.flac", pairs=[], art=(big, "image/jpeg")), stats)
        conn.commit()
        assert stats.synced == 1
        assert stats.skipped_art == 1
        assert stats.art_linked == 0
        assert conn.execute("SELECT COUNT(*) FROM track_art").fetchone()[0] == 0
    finally:
        conn.close()


def test_sync_one_dry_run_counts_without_writing(db_path):
    conn, _ = _seed(db_path)
    try:
        stats = SyncStats()
        sync_one(
            conn,
            Record(key="/m/a.flac", pairs=[("title", "T")], art=(JPEG, "image/jpeg")),
            stats,
            dry_run=True,
        )
        assert stats.synced == 1
        assert stats.art_linked == 1
        assert conn.execute("SELECT COUNT(*) FROM tags").fetchone()[0] == 0
        assert conn.execute("SELECT COUNT(*) FROM track_art").fetchone()[0] == 0
    finally:
        conn.close()


def test_sync_files_returns_aggregated_stats(db_path):
    conn = connect(db_path)
    try:
        insert_track(conn, "/m/a.flac")
        conn.commit()
        records = [
            Record(key="/m/a.flac", pairs=[("title", "A")], art=None),
            Record(key="/m/missing.flac", pairs=[("title", "B")], art=None),
        ]
        stats = sync_files(conn, records)
        conn.commit()
        assert stats.synced == 1
        assert stats.skipped == 1
    finally:
        conn.close()


def test_sync_files_reuses_caller_seeded_stats(db_path):
    conn = connect(db_path)
    try:
        insert_track(conn, "/m/a.flac")
        conn.commit()
        seeded = SyncStats(skipped_art=2)  # e.g. beets pre-counted unreadable art
        out = sync_files(conn, [Record(key="/m/a.flac", pairs=[], art=None)], stats=seeded)
        assert out is seeded
        assert out.skipped_art == 2
        assert out.synced == 1
    finally:
        conn.close()


def test_tags_fully_replaced(db_path):
    conn, tid = _seed(db_path)
    try:
        sync_one(conn, Record(key="/m/a.flac", pairs=[("title", "Old"), ("genre", "Rock")]), SyncStats())
        sync_one(conn, Record(key="/m/a.flac", pairs=[("title", "New")]), SyncStats())
        conn.commit()
        rows = dict(conn.execute("SELECT key, value FROM tags WHERE track_id=?", (tid,)))
        assert rows == {"title": "New"}  # genre gone after replace
    finally:
        conn.close()


def test_no_art_leaves_existing_track_art_untouched(db_path):
    conn, tid = _seed(db_path)
    try:
        conn.execute(
            "INSERT INTO art (sha256, mime, byte_len, data) VALUES "
            "('deadbeef', 'image/jpeg', 3, X'aabbcc')"
        )
        art_id = conn.execute("SELECT id FROM art WHERE sha256='deadbeef'").fetchone()[0]
        conn.execute("INSERT INTO track_art (track_id, art_id) VALUES (?, ?)", (tid, art_id))
        conn.commit()
        stats = SyncStats()
        sync_one(conn, Record(key="/m/a.flac", pairs=[("title", "T")], art=None), stats)
        conn.commit()
        assert stats.art_linked == 0
        row = conn.execute("SELECT art_id FROM track_art WHERE track_id=?", (tid,)).fetchone()
        assert row == (art_id,)  # scan-seeded art untouched when Record has no art
    finally:
        conn.close()


def test_tags_write_bumps_content_version(db_path):
    conn, tid = _seed(db_path)
    try:
        before = conn.execute("SELECT content_version FROM tracks WHERE id=?", (tid,)).fetchone()[0]
        sync_one(conn, Record(key="/m/a.flac", pairs=[("title", "T")]), SyncStats())
        conn.commit()
        after = conn.execute("SELECT content_version FROM tracks WHERE id=?", (tid,)).fetchone()[0]
        assert after > before
    finally:
        conn.close()


def test_skip_mid_batch_does_not_abort_others(db_path):
    conn = connect(db_path)
    try:
        a = insert_track(conn, "/m/a.flac")
        b = insert_track(conn, "/m/b.flac")
        conn.commit()
        records = [
            Record(key="/m/a.flac", pairs=[("title", "T")]),
            Record(key="/m/missing.flac", pairs=[("title", "T")]),
            Record(key="/m/b.flac", pairs=[("title", "T")]),
        ]
        stats = sync_files(conn, records)
        conn.commit()
        assert stats.synced == 2
        assert stats.skipped == 1
        for tid in (a, b):
            assert (
                conn.execute(
                    "SELECT value FROM tags WHERE track_id=? AND key='title'", (tid,)
                ).fetchone()[0]
                == "T"
            )
    finally:
        conn.close()


def test_art_deduped_across_records(db_path):
    conn = connect(db_path)
    try:
        insert_track(conn, "/m/a.flac")
        insert_track(conn, "/m/b.flac")
        conn.commit()
        records = [
            Record(key="/m/a.flac", pairs=[], art=(JPEG, "image/jpeg")),
            Record(key="/m/b.flac", pairs=[], art=(JPEG, "image/jpeg")),
        ]
        sync_files(conn, records)
        conn.commit()
        assert conn.execute("SELECT COUNT(*) FROM art").fetchone()[0] == 1
        assert conn.execute("SELECT COUNT(*) FROM track_art").fetchone()[0] == 2
    finally:
        conn.close()


def test_summary_format():
    s = SyncStats(synced=3, skipped=1, art_linked=2, skipped_art=1)
    assert s.summary() == "synced=3 skipped=1 art_linked=2 skipped_art=1"
```

These five cases migrate the sync-loop coverage that the host suites' `test_sync.py` files held (full tag replacement, scan-seeded art preserved when a record has no art, the content_version trigger, mid-batch skip-continues, and cross-record art dedup), so deleting those host tests in Tasks 11/14 loses nothing.

- [ ] **Step 2: Run test to verify it fails**

Run: `cd contrib/python-musefs && python -m pytest tests/test_sync.py -v`
Expected: FAIL — `ImportError: cannot import name 'Record' from 'musefs_common'`.

- [ ] **Step 3: Write `sync.py`**

```python
from __future__ import annotations

from dataclasses import dataclass, field

from .constants import MAX_ART_BYTES
from .store import replace_tags, replace_track_art, track_id_for_path, upsert_art


@dataclass
class Record:
    """One file's sync inputs: the realpath key, the (key, value) tag pairs, and
    pre-resolved cover art as an ``(bytes, mime)`` tuple or ``None``."""

    key: str
    pairs: list = field(default_factory=list)
    art: object = None  # tuple[bytes, str] | None


@dataclass
class SyncStats:
    synced: int = 0
    skipped: int = 0  # path had no matching track row
    art_linked: int = 0
    skipped_art: int = 0  # art over the size cap (or, in the beets adapter, unreadable)

    def summary(self):
        return (
            f"synced={self.synced} skipped={self.skipped} "
            f"art_linked={self.art_linked} skipped_art={self.skipped_art}"
        )


def sync_one(conn, record, stats, *, dry_run=False):
    """Sync one ``Record`` into the DB, mutating ``stats``. Caller owns the
    transaction. Tags are always fully replaced (scanner-written binary tags
    survive — see ``replace_tags``). Art is replaced only when present and within
    ``MAX_ART_BYTES``; an over-cap image bumps ``skipped_art`` and leaves any
    scan-seeded ``track_art`` untouched."""
    track_id = track_id_for_path(conn, record.key)
    if track_id is None:
        stats.skipped += 1
        return

    will_link_art = False
    if record.art is not None:
        data, _mime = record.art
        if len(data) > MAX_ART_BYTES:
            stats.skipped_art += 1
        else:
            will_link_art = True

    if not dry_run:
        replace_tags(conn, track_id, record.pairs)
        if will_link_art:
            data, mime = record.art
            art_id = upsert_art(conn, data, mime)
            replace_track_art(conn, track_id, art_id)

    if will_link_art:
        stats.art_linked += 1
    stats.synced += 1


def sync_files(conn, records, *, dry_run=False, stats=None):
    """Sync an iterable of ``Record``s, returning the ``SyncStats``. Pass
    ``stats`` to accumulate into a caller-seeded instance (e.g. beets pre-counts
    unreadable art); otherwise a fresh one is created. Caller owns the
    transaction (commit on success, rollback for dry runs)."""
    if stats is None:
        stats = SyncStats()
    for record in records:
        sync_one(conn, record, stats, dry_run=dry_run)
    return stats
```

- [ ] **Step 4: Add the interim re-export**

Append to `__init__.py`:
```python
from .sync import Record, SyncStats, sync_files, sync_one
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cd contrib/python-musefs && python -m pytest tests/test_sync.py -v`
Expected: PASS (12 passed).

- [ ] **Step 6: Commit**

```bash
git add contrib/python-musefs/src/musefs_common/sync.py \
  contrib/python-musefs/src/musefs_common/__init__.py \
  contrib/python-musefs/tests/test_sync.py
git commit -m "feat(python-musefs): add Record/SyncStats/sync_one/sync_files write-loop"
```

---

## Task 9: Finalize the public API (`__init__.py`) + `__version__`

**Files:**
- Modify: `contrib/python-musefs/src/musefs_common/__init__.py`
- Test: `contrib/python-musefs/tests/test_public_api.py`

- [ ] **Step 1: Write the failing test**

`tests/test_public_api.py`:
```python
import musefs_common


def test_version_is_package_semver_not_schema_version():
    # __version__ is the library's own SemVer, independent of EXPECTED_USER_VERSION.
    assert musefs_common.__version__ == "0.1.0"
    assert musefs_common.__version__ != str(musefs_common.EXPECTED_USER_VERSION)


def test_public_api_surface():
    expected = {
        "EXPECTED_USER_VERSION",
        "MAX_ART_BYTES",
        "SchemaMismatch",
        "ScanError",
        "realpath_key",
        "run_scan",
        "connect",
        "check_schema_version",
        "track_id_for_path",
        "prune_missing",
        "replace_tags",
        "upsert_art",
        "replace_track_art",
        "sniff_mime",
        "Record",
        "SyncStats",
        "sync_one",
        "sync_files",
    }
    assert expected <= set(musefs_common.__all__)
    for name in expected:
        assert hasattr(musefs_common, name), name
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd contrib/python-musefs && python -m pytest tests/test_public_api.py -v`
Expected: FAIL — `AttributeError: module 'musefs_common' has no attribute '__version__'`.

- [ ] **Step 3: Replace `__init__.py` with the finalized version**

```python
"""python-musefs: the shared musefs SQLite-store contract.

Single source of truth for the schema-version check, the tags/art/track_art
writes, art content-addressing, path-key normalization, the `musefs scan`
shell-out, and the per-file sync write-loop. Consumed by the beets plugin (as a
pip dependency) and by the Picard plugin (vendored into ``musefs/_common``).
"""

from .constants import EXPECTED_USER_VERSION, MAX_ART_BYTES
from .errors import SchemaMismatch, ScanError
from .paths import realpath_key
from .scan import run_scan
from .store import (
    check_schema_version,
    connect,
    prune_missing,
    replace_tags,
    replace_track_art,
    sniff_mime,
    track_id_for_path,
    upsert_art,
)
from .sync import Record, SyncStats, sync_files, sync_one

__version__ = "0.1.0"

__all__ = [
    "EXPECTED_USER_VERSION",
    "MAX_ART_BYTES",
    "SchemaMismatch",
    "ScanError",
    "realpath_key",
    "run_scan",
    "connect",
    "check_schema_version",
    "track_id_for_path",
    "prune_missing",
    "replace_tags",
    "upsert_art",
    "replace_track_art",
    "sniff_mime",
    "Record",
    "SyncStats",
    "sync_one",
    "sync_files",
    "__version__",
]
```

- [ ] **Step 4: Run the whole library suite + lint**

Run:
```bash
cd contrib/python-musefs && python -m pytest -v && ruff check . && ruff format --check .
```
Expected: all tests PASS; ruff reports no issues. (If `ruff format --check` flags files, run `ruff format .`, re-run the check, and include the formatting in this commit.)

- [ ] **Step 5: Commit**

```bash
git add contrib/python-musefs/src/musefs_common/__init__.py contrib/python-musefs/tests/test_public_api.py
git commit -m "feat(python-musefs): finalize public API and __version__"
```

---

## Task 10: Slim the beets `_core.py` to mapping + `build_records`

**Files:**
- Modify: `contrib/beets/beetsplug/_core.py` (replace entire file)
- Test: `contrib/beets/tests/test_build_records.py` (new)

- [ ] **Step 1: Install the shared library (prerequisite)**

The beets package now depends on `python-musefs`; install it from the working tree so imports resolve and the next step's failure is the intended `AttributeError`, not `ModuleNotFoundError`.

Run: `pip install -e contrib/python-musefs`
Expected: `Successfully installed python-musefs-0.1.0`.

- [ ] **Step 2: Write the failing test**

`contrib/beets/tests/test_build_records.py`:
```python
from beetsplug import _core
from musefs_common import SyncStats


def test_build_records_maps_fields(fake_item):
    item = fake_item(b"/m/a.flac", title="T", artist="A", genre=["Rock", "Pop"])
    stats = SyncStats()
    records = _core.build_records([item], fields=None, stats=stats)
    assert len(records) == 1
    pairs = records[0].pairs
    assert ("title", "T") in pairs
    assert ("genre", "Rock") in pairs
    assert ("genre", "Pop") in pairs
    assert records[0].art is None


def test_build_records_reads_album_art(fake_item, fake_album, tmp_path):
    cover = tmp_path / "cover.jpg"
    cover.write_bytes(b"\xff\xd8\xff" + b"\x00" * 16)
    album = fake_album(artpath=str(cover).encode())
    item = fake_item(b"/m/a.flac", album=album, title="T")
    stats = SyncStats()
    records = _core.build_records([item], fields=None, stats=stats)
    assert records[0].art is not None
    data, mime = records[0].art
    assert mime == "image/jpeg"
    assert stats.skipped_art == 0


def test_build_records_counts_unreadable_art_once(fake_item, fake_album):
    album = fake_album(artpath=b"/does/not/exist.jpg")
    items = [fake_item(b"/m/a.flac", album=album), fake_item(b"/m/b.flac", album=album)]
    stats = SyncStats()
    records = _core.build_records(items, fields=None, stats=stats)
    assert all(r.art is None for r in records)
    # Cached per realpath, so a shared missing cover counts once (legacy behavior).
    assert stats.skipped_art == 1


def test_build_records_counts_oversized_art_once(fake_item, fake_album, tmp_path):
    from musefs_common.constants import MAX_ART_BYTES

    cover = tmp_path / "big.jpg"
    cover.write_bytes(b"\xff\xd8\xff" + b"\x00" * (MAX_ART_BYTES + 1))
    album = fake_album(artpath=str(cover).encode())
    items = [fake_item(b"/m/a.flac", album=album), fake_item(b"/m/b.flac", album=album)]
    stats = SyncStats()
    records = _core.build_records(items, fields=None, stats=stats)
    assert all(r.art is None for r in records)
    assert stats.skipped_art == 1
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cd contrib/beets && python -m pytest tests/test_build_records.py -v`
Expected: FAIL — `AttributeError: module 'beetsplug._core' has no attribute 'build_records'` (the slim `_core` from the next step doesn't exist yet; `musefs_common` is already installed by Step 1).

- [ ] **Step 4: Replace `contrib/beets/beetsplug/_core.py` entirely**

```python
"""beets-specific mapping for the musefs sync plugin: no beets imports here.

The shared store/scan/sync contract lives in the ``musefs_common`` package
(python-musefs); this module only maps beets items to musefs tag pairs and reads
album cover art into ``Record``s. ``musefs.py`` holds the BeetsPlugin adapter.
"""

import os

from musefs_common import MAX_ART_BYTES, Record, realpath_key, sniff_mime

# beets field name -> musefs (Vorbis-lowercase) tag key, for direct copies.
# beets 2.x exposes genre/composer as the multi-valued `genres`/`composers`
# (lists); the singular keys are kept for simpler/older items. List values are
# expanded into one tag per element by _values().
DIRECT_FIELDS = {
    "title": "title",
    "artist": "artist",
    "albumartist": "albumartist",
    "album": "album",
    "genre": "genre",
    "genres": "genre",
    "composer": "composer",
    "composers": "composer",
}


def _values(value):
    """Normalize a beets field value to a list of non-empty string values.
    Multi-valued beets fields (genres, composers) arrive as lists; scalars
    become a single-element list. Avoids stringifying a list as ``['Rock']``."""
    if value is None:
        return []
    items = value if isinstance(value, (list, tuple)) else [value]
    return [text for v in items if (text := str(v).strip())]


def _to_int(value):
    """Coerce a beets field to int, tolerating None and non-numeric strings
    (e.g. a malformed ``"1/12"`` track-of-total) so a bad tag can't abort sync."""
    try:
        return int(value or 0)
    except (ValueError, TypeError):
        return 0


def _format_date(item):
    year = _to_int(getattr(item, "year", 0))
    if not year:
        return None
    month = _to_int(getattr(item, "month", 0))
    day = _to_int(getattr(item, "day", 0))
    if month and day:
        return f"{year:04d}-{month:02d}-{day:02d}"
    return f"{year:04d}"


def map_fields(item, extra_fields=None):
    """Map a beets item to a list of (musefs_key, value) pairs.

    Empty strings and zero numerics are omitted. ``extra_fields`` merges into
    (and can override) the direct-copy table.
    """
    fields = dict(DIRECT_FIELDS)
    if extra_fields:
        fields.update(extra_fields)

    pairs = []
    for beets_field, key in fields.items():
        for text in _values(getattr(item, beets_field, None)):
            pairs.append((key, text))

    track = _to_int(getattr(item, "track", 0))
    if track:
        pairs.append(("tracknumber", str(track)))
    disc = _to_int(getattr(item, "disc", 0))
    if disc:
        pairs.append(("discnumber", str(disc)))
    date = _format_date(item)
    if date:
        pairs.append(("date", date))

    return pairs


def _album_art_path(item):
    """Return the album cover path (bytes/str) for an item, or None."""
    get_album = getattr(item, "get_album", None)
    album = get_album() if get_album else None
    if album is None:
        return None
    artpath = getattr(album, "artpath", None)
    return artpath or None


def _read_album_art(item, cache, stats):
    """Return ``(data, mime)`` for the item's album cover, or None. Reads each
    distinct cover once (cached by realpath). An unreadable or over-cap cover is
    counted into ``stats.skipped_art`` once and cached as None (matches the
    legacy ``_prepare_art`` counting before the python-musefs split)."""
    artpath = _album_art_path(item)
    if not artpath:
        return None
    key = realpath_key(artpath)
    if key in cache:
        return cache[key]
    try:
        # Open the raw realpath, not realpath_key's lossy U+FFFD form: the file
        # is only opened, not matched against the DB.
        with open(os.path.realpath(artpath), "rb") as fh:
            data = fh.read()
    except OSError:
        stats.skipped_art += 1
        cache[key] = None
        return None
    if len(data) > MAX_ART_BYTES:
        stats.skipped_art += 1
        cache[key] = None
        return None
    art = (data, sniff_mime(data, key))
    cache[key] = art
    return art


def build_records(items, *, fields=None, stats):
    """Build ``Record``s for beets items: map tags and resolve album art (with a
    per-run cache; unreadable/over-cap covers counted into ``stats.skipped_art``).
    ``stats`` is mutated and must be the same instance passed to ``sync_files``."""
    records = []
    art_cache = {}
    for item in items:
        records.append(
            Record(
                key=realpath_key(item.path),
                pairs=map_fields(item, fields),
                art=_read_album_art(item, art_cache, stats),
            )
        )
    return records
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cd contrib/beets && python -m pytest tests/test_build_records.py -v`
Expected: PASS (4 passed).

- [ ] **Step 6: Commit**

```bash
git add contrib/beets/beetsplug/_core.py contrib/beets/tests/test_build_records.py
git commit -m "refactor(beets): slim _core to mapping + build_records over python-musefs"
```

---

## Task 11: Repoint the beets adapter + conftest + packaging; prune duplicated tests

**Files:**
- Modify: `contrib/beets/beetsplug/musefs.py`
- Modify: `contrib/beets/tests/conftest.py`
- Modify: `contrib/beets/pyproject.toml`
- Delete: `contrib/beets/tests/test_db.py`, `contrib/beets/tests/test_art.py`, `contrib/beets/tests/test_sync.py`
- Rewrite: `contrib/beets/tests/test_smoke.py`
- Repoint imports: `contrib/beets/tests/test_path_gate.py`, `contrib/beets/tests/test_paths.py`, `contrib/beets/tests/test_plugin.py`
- Keep unchanged: `contrib/beets/tests/test_map_fields.py`, `contrib/beets/tests/test_build_records.py`

**Why these dispositions** — after Task 10 the beets `_core` no longer defines `connect`, `track_id_for_path`, `sync_items`, `replace_tags`, `upsert_art`, `replace_track_art`, `EXPECTED_USER_VERSION`, or `SchemaMismatch` (it only re-exports `realpath_key`/`sniff_mime`/`MAX_ART_BYTES`/`Record` via its `musefs_common` import). Any test importing a removed name fails the **whole** `pytest tests` run at collection, so every importer must be deleted or repointed. The deleted files' behavior now lives in the library suite (`test_store_db.py`, `test_store_art.py`, `test_sync.py`) and `test_build_records.py`.

- [ ] **Step 1: Update `tests/conftest.py` import line**

Change line 7 from:
```python
from beetsplug._core import connect as musefs_connect
```
to:
```python
from musefs_common import connect as musefs_connect
```

- [ ] **Step 2: Add the dependency in `pyproject.toml`**

Change the `dependencies` line:
```toml
dependencies = ["python-musefs>=0.1.0", "beets>=1.6"]
```

- [ ] **Step 3: Rewrite `contrib/beets/beetsplug/musefs.py`**

```python
"""beets plugin: sync canonical beets metadata into the musefs SQLite store."""

import os

from beets import ui
from beets.plugins import BeetsPlugin
from musefs_common import (
    ScanError,
    SchemaMismatch,
    SyncStats,
    check_schema_version,
    connect,
    prune_missing,
    realpath_key,
    run_scan,
    sync_files,
    track_id_for_path,
)

from beetsplug import _core


class MusefsPlugin(BeetsPlugin):
    def __init__(self):
        super().__init__()
        self.config.add({
            "db": None,
            "fields": {},
            "bin": "musefs",  # musefs executable (PATH name or full path)
            "autoscan": True,  # run `musefs scan` automatically before syncing
        })
        # beets has no file-move event, and `after_write` fires *before* a move
        # (at the old path). So imports/writes are recorded and reconciled once
        # at cli_exit, when each item's path is final, where we also prune rows
        # whose backing file has moved away.
        self._pending = []
        self.register_listener("after_write", self._record)
        self.register_listener("item_imported", self._record)
        self.register_listener("album_imported", self._record_album)
        self.register_listener("cli_exit", self._reconcile_pending)

    # --- command ---------------------------------------------------------

    def commands(self):
        cmd = ui.Subcommand("musefs", help="sync beets metadata into the musefs DB")
        cmd.parser.add_option(
            "--db",
            dest="db",
            default=None,
            help="path to the musefs SQLite store (overrides config)",
        )
        cmd.parser.add_option(
            "-n",
            "--dry-run",
            dest="dry_run",
            action="store_true",
            default=False,
            help="report what would change without writing",
        )
        cmd.func = self._command
        return [cmd]

    @staticmethod
    def _query_from_args(args):
        """Drop an optional leading `sync` verb so `beet musefs sync QUERY`
        and `beet musefs QUERY` both work."""
        if args and args[0] == "sync":
            return args[1:]
        return list(args)

    def _command(self, lib, opts, args):
        db_path = opts.db or self._db_path()
        if not db_path:
            raise ui.UserError("musefs: set `musefs.db` in config or pass --db")

        query = self._query_from_args(args)
        items = list(lib.items(query))
        if self._autoscan() and not opts.dry_run:
            # Full sync: one scan of the music dir. Query: scan only the matched
            # files, so non-matched rows aren't re-seeded from their files.
            targets = (
                [os.fsdecode(i.path) for i in items] if query else [os.fsdecode(lib.directory)]
            )
            self._run_scan(db_path, targets)
        stats = self._sync(db_path, items, dry_run=opts.dry_run)
        if opts.dry_run:
            pruned = 0
        else:
            prune_items = items if query else None
            pruned = self._prune_missing(db_path, items=prune_items)
        # ui.print_ (not self._log) so the summary always shows, not only at -v.
        ui.print_(f"musefs: {stats.summary()} pruned={pruned}")

    # --- event listeners -------------------------------------------------

    def _record(self, item=None, **kwargs):
        if item is not None:
            self._pending.append(item)

    def _record_album(self, album=None, **kwargs):
        if album is not None:
            self._pending.extend(album.items())

    def _reconcile_pending(self, lib=None, **kwargs):
        """End-of-command reconcile: sync every touched item at its final path,
        then prune rows whose backing file moved away. Best-effort — a passive
        hook must never abort the beets operation, so errors become warnings."""
        pending, self._pending = self._pending, []
        # Dedup by final on-disk path (an item may fire several events).
        items = list({os.fsdecode(i.path): i for i in pending if i is not None}.values())
        if not items:
            return
        db_path = self._db_path()
        if not db_path:
            self._log.warning("musefs: no `musefs.db` configured; skipping sync")
            return
        try:
            if self._autoscan():
                self._run_scan(db_path, [os.fsdecode(i.path) for i in items])
            self._sync(db_path, items)
            self._prune_missing(db_path)
        except ui.UserError as exc:
            self._log.warning("musefs: {}", exc)

    # --- helpers ---------------------------------------------------------

    def _db_path(self):
        # `.get()` returns the raw config value (None if unset); only call
        # as_filename() when set, so a genuine bad-type value still raises.
        if self.config["db"].get() is None:
            return None
        return self.config["db"].as_filename()

    def _fields(self):
        return self.config["fields"].get(dict) or {}

    def _autoscan(self):
        return bool(self.config["autoscan"].get(bool))

    def _bin(self):
        return self.config["bin"].get(str) or "musefs"

    def _run_scan(self, db_path, targets):
        """Run `musefs scan <target> --db <db>` for each target (file or dir).
        Creates the DB if missing and fills the structural columns the plugin
        can't compute itself. Raises ui.UserError on failure."""
        binary = self._bin()
        for target in targets:
            try:
                run_scan(binary, db_path, target, timeout=None)
            except ScanError as exc:
                raise self._scan_user_error(exc)

    @staticmethod
    def _scan_user_error(exc):
        """Translate a python-musefs ScanError to beets' ui.UserError, preserving
        the plugin's historical message text."""
        if exc.kind == "not_found":
            return ui.UserError(
                f"musefs: binary '{exc.binary}' not found; set `musefs.bin` to "
                f"the musefs executable path"
            )
        return ui.UserError(
            f"musefs: `{exc.binary} scan` failed for {exc.target} "
            f"(exit {exc.returncode}):\n{exc.stderr}"
        )

    @staticmethod
    def _track_ids_for_items(conn, items):
        ids = []
        for item in items:
            key = realpath_key(item.path)
            track_id = track_id_for_path(conn, key)
            if track_id is not None:
                ids.append(track_id)
        return ids

    def _prune_missing(self, db_path, items=None):
        """Drop rows whose backing file no longer exists (moved/deleted).
        When ``items`` is provided, only their musefs track rows are checked.
        Returns the number pruned."""
        if not os.path.exists(db_path):
            return 0
        conn = connect(db_path)
        try:
            track_ids = None if items is None else self._track_ids_for_items(conn, items)
            pruned = prune_missing(conn, track_ids)
            conn.commit()
            return pruned
        finally:
            conn.close()

    def _sync(self, db_path, items, dry_run=False):
        if not os.path.exists(db_path):
            raise ui.UserError(
                f"musefs: DB not found at {db_path}; enable `musefs.autoscan` "
                f"or run `musefs scan` first"
            )
        conn = connect(db_path)
        try:
            check_schema_version(conn)
            stats = SyncStats()
            records = _core.build_records(items, fields=self._fields(), stats=stats)
            sync_files(conn, records, dry_run=dry_run, stats=stats)
            if dry_run:
                conn.rollback()
            else:
                conn.commit()
            return stats
        except SchemaMismatch as exc:
            conn.rollback()
            raise ui.UserError(f"musefs: {exc}")
        finally:
            conn.close()
```

- [ ] **Step 4: Delete the three library-internal test files**

```bash
git rm contrib/beets/tests/test_db.py contrib/beets/tests/test_art.py contrib/beets/tests/test_sync.py
```
- `test_db.py` — tested `connect`/`replace_tags`/`prune_missing`/schema → library `test_store_db.py`.
- `test_art.py` — tested `sniff_mime`/`upsert_art`/`replace_track_art` (incl. replacement + content_version bump) → library `test_store_art.py` (Task 6, now includes those assertions).
- `test_sync.py` — tested the removed `_core.sync_items` (skip, tags, art link, embedded-preserved, oversized, dedup, dry-run) → library `test_sync.py` (Task 8, now includes the migrated cases) + `test_build_records.py` (art-file read/cache/oversized/unreadable).

- [ ] **Step 5: Rewrite `contrib/beets/tests/test_smoke.py`**

The old smoke test asserted `_core.EXPECTED_USER_VERSION`, which the slim `_core` no longer defines. Replace the file with a check that the slim `_core` still imports without beets and exposes its mapping API:
```python
def test_core_imports_without_beets():
    # The slimmed _core depends only on musefs_common, never on beets itself.
    import beetsplug._core as core

    assert hasattr(core, "DIRECT_FIELDS")
    assert hasattr(core, "map_fields")
    assert hasattr(core, "build_records")
```

- [ ] **Step 6: Repoint imports in the three kept test files**

- `contrib/beets/tests/test_path_gate.py` line 12 — change
  `from beetsplug._core import connect, realpath_key, track_id_for_path`
  to
  `from musefs_common import connect, realpath_key, track_id_for_path`
- `contrib/beets/tests/test_paths.py` line 3 — change
  `from beetsplug._core import realpath_key`
  to
  `from musefs_common import realpath_key`
- `contrib/beets/tests/test_plugin.py` line 9 — change the combined line
  `from beetsplug._core import connect, map_fields  # noqa: E402`
  to two lines (map_fields stays in `_core`, connect moves to the library):
  ```python
  from beetsplug._core import map_fields  # noqa: E402
  from musefs_common import connect  # noqa: E402
  ```

- [ ] **Step 7: Run the full beets suite**

Run:
```bash
cd contrib/beets && pip install -e ../python-musefs && pip install -e ".[test]" \
  && python -m pytest tests -v && ruff check . && ruff format --check .
```
Expected: all tests PASS (no collection errors — every importer of a removed `_core` symbol is now deleted or repointed); the `musefs_bin`-marked `test_path_gate.py` is deselected by default; ruff clean. (Run `ruff format .` if the check flags files.)

- [ ] **Step 8: Commit** (stage by name; the `git rm` deletions from Step 4 are already staged)

```bash
git add contrib/beets/beetsplug/musefs.py contrib/beets/tests/conftest.py \
  contrib/beets/pyproject.toml contrib/beets/tests/test_smoke.py \
  contrib/beets/tests/test_path_gate.py contrib/beets/tests/test_paths.py \
  contrib/beets/tests/test_plugin.py
git commit -m "refactor(beets): consume python-musefs for store/scan/sync"
```

---

## Task 12: Vendor the library into Picard + ruff exclude

**Files:**
- Create: `contrib/python-musefs/vendor_to_picard.py`
- Create (generated): `contrib/picard/musefs/_common/*.py`
- Modify: `contrib/picard/ruff.toml`

- [ ] **Step 1: Write `vendor_to_picard.py`**

```python
#!/usr/bin/env python3
"""Vendor python-musefs into the Picard folder plugin's ``_common`` subpackage.

Picard does not pip-install plugin dependencies, so the shared library is copied
(verbatim, with a generated header) into ``contrib/picard/musefs/_common``. Run
this after any change to ``src/musefs_common``. The Picard test suite's
``test_vendor_sync.py`` fails if the committed copy drifts from canonical.
"""

from pathlib import Path

SRC = Path(__file__).parent / "src" / "musefs_common"
DST = Path(__file__).parent.parent / "picard" / "musefs" / "_common"

HEADER = (
    "# GENERATED from python-musefs/src/musefs_common/{name} — do not edit.\n"
    "# Run contrib/python-musefs/vendor_to_picard.py after changing the library.\n"
    "#\n"
)


def main():
    DST.mkdir(parents=True, exist_ok=True)
    src_names = {p.name for p in SRC.glob("*.py")}
    # Drop vendored files no longer present in the source package.
    for old in DST.glob("*.py"):
        if old.name not in src_names:
            old.unlink()
    for src in sorted(SRC.glob("*.py")):
        header = HEADER.format(name=src.name).encode("utf-8")
        (DST / src.name).write_bytes(header + src.read_bytes())


if __name__ == "__main__":
    main()
```

- [ ] **Step 2: Run the vendor script**

Run: `python contrib/python-musefs/vendor_to_picard.py && ls contrib/picard/musefs/_common`
Expected: lists `__init__.py constants.py errors.py paths.py scan.py store.py sync.py`.

- [ ] **Step 3: Confirm the vendored package imports as a subpackage**

Run:
```bash
cd contrib/picard && python -c "import sys; sys.path.insert(0,'.'); from musefs._common import connect, run_scan, sync_files, Record, SyncStats; print('ok')"
```
Expected: prints `ok`. (This is the Task 0 assumption, now against the real vendored files.)

- [ ] **Step 4: Exclude the vendored dir from Picard's ruff**

Edit `contrib/picard/ruff.toml` to add the exclude (the vendored code is formatted only at its canonical source):
```toml
line-length = 100
target-version = "py38"
extend-exclude = ["musefs/_common"]

[lint]
select = ["E", "F", "I", "N", "W"]

[format]
preview = true
```

- [ ] **Step 5: Commit**

```bash
git add contrib/python-musefs/vendor_to_picard.py contrib/picard/musefs/_common contrib/picard/ruff.toml
git commit -m "feat(picard): vendor python-musefs into musefs/_common"
```

---

## Task 13: Slim Picard `_core.py` + repoint the adapter

**Files:**
- Modify: `contrib/picard/musefs/_core.py` (replace entire file)
- Modify: `contrib/picard/musefs/__init__.py`
- Modify: `contrib/picard/tests/conftest.py`

- [ ] **Step 1: Update `tests/conftest.py` import line**

Change line 7 from:
```python
from musefs._core import connect as musefs_connect
```
to:
```python
from musefs._common import connect as musefs_connect
```

- [ ] **Step 2: Replace `contrib/picard/musefs/_core.py` entirely**

```python
"""Picard-specific logic for the musefs sync plugin: no Picard imports here.

The shared store/scan/sync contract lives in the vendored ``musefs._common``
package (python-musefs); this module only maps Picard metadata to musefs tag
pairs, extracts the front cover, and resolves plugin options. ``__init__.py``
holds the Picard adapter (actions, options page, registration).
"""

from __future__ import annotations

from dataclasses import dataclass, field

# Upper bound on a single-file `musefs scan` autoscan. A scan probes one file,
# so this only fires on a genuine hang (e.g. a wedged binary or stuck DB lock);
# without it a hung scan would block the Picard worker thread forever.
SCAN_TIMEOUT_SECONDS = 120

# Picard internal tag name -> musefs (Vorbis-lowercase) key. Picard's internal
# names already match musefs keys, so this is mostly identity.
DIRECT_FIELDS = {
    "title": "title",
    "artist": "artist",
    "albumartist": "albumartist",
    "album": "album",
    "genre": "genre",
    "composer": "composer",
    "tracknumber": "tracknumber",
    "discnumber": "discnumber",
    "date": "date",
}

# Keys whose value is dropped when it normalizes to zero (a 0 track/disc is noise).
_NUMERIC_KEYS = {"tracknumber", "discnumber"}


class MusefsError(Exception):  # noqa: N818
    """A user-facing failure (binary missing, scan failed, DB absent)."""


def _to_int(value):
    """Coerce to int, tolerating None and non-numeric strings so a bad tag
    can't abort sync."""
    try:
        return int(value or 0)
    except (ValueError, TypeError):
        return 0


def _first_value(metadata, field_name):
    """First non-empty, stripped string value of a Picard metadata field.
    Reads ``metadata.getall(field)`` when available (Picard's multi-valued
    accessor), else falls back to a plain ``.get``."""
    getall = getattr(metadata, "getall", None)
    if getall is not None:
        values = getall(field_name)
    else:
        v = metadata.get(field_name) if hasattr(metadata, "get") else None
        values = v if isinstance(v, (list, tuple)) else ([] if v is None else [v])
    for v in values:
        text = str(v).strip()
        if text:
            return text
    return ""


def map_fields(metadata, extra_fields=None):
    """Map a Picard Metadata (dict-like) to a list of (musefs_key, value) pairs.

    One value per key (the first non-empty), empty strings omitted, and a zero
    tracknumber/discnumber omitted. ``extra_fields`` merges into (and can
    override) the direct-copy table.
    """
    fields = dict(DIRECT_FIELDS)
    if extra_fields:
        fields.update(extra_fields)

    pairs = []
    for pic_field, key in fields.items():
        text = _first_value(metadata, pic_field)
        if not text:
            continue
        if key in _NUMERIC_KEYS and _to_int(text) == 0:
            continue
        pairs.append((key, text))
    return pairs


def front_cover(metadata):
    """Return ``(data, mime)`` for the first front-cover image in a Picard
    Metadata, or ``None``. Duck-typed: images expose ``is_front_image()``,
    ``data``, and ``mimetype``."""
    images = getattr(metadata, "images", None) or []
    for img in images:
        is_front = getattr(img, "is_front_image", None)
        if is_front is not None and is_front():
            return (img.data, img.mimetype)
    return None


@dataclass
class Opts:
    db: "str | None"
    bin: str
    autoscan: bool
    fields: dict = field(default_factory=dict)


def parse_field_map(text):
    """Parse a ``key=value`` field map (from the options page) into a dict.
    Entries are separated by commas or newlines; blank/invalid entries ignored."""
    result = {}
    if not text:
        return result
    for entry in str(text).replace("\n", ",").split(","):
        entry = entry.strip()
        if not entry or "=" not in entry:
            continue
        k, v = entry.split("=", 1)
        k, v = k.strip(), v.strip()
        if k and v:
            result[k] = v
    return result


def resolve_config(settings, environ):
    """Resolve plugin options from Picard settings (a dict-like) with env
    overrides. ``MUSEFS_DB``/``MUSEFS_BIN`` take precedence over the page;
    autoscan and the field map are page-only."""
    db = environ.get("MUSEFS_DB") or (settings.get("musefs_db") or None)
    binary = environ.get("MUSEFS_BIN") or (settings.get("musefs_bin") or "musefs")
    autoscan = bool(settings.get("musefs_autoscan", True))
    fields = settings.get("musefs_fields") or {}
    if isinstance(fields, str):
        fields = parse_field_map(fields)
    return Opts(db=db, bin=binary, autoscan=autoscan, fields=fields)
```

- [ ] **Step 3: Update the imports and `_do_sync` in `contrib/picard/musefs/__init__.py`**

Replace the import block (lines 17–28, the `from musefs._core import (...)`) with these two blocks:
```python
from musefs._common import (
    Record,
    ScanError,
    check_schema_version,
    connect,
    realpath_key,
    run_scan,
    sync_files,
)
from musefs._core import (
    SCAN_TIMEOUT_SECONDS,
    MusefsError,
    front_cover,
    map_fields,
    resolve_config,
)
```

Then replace the `_do_sync` function body (the block beginning `def _do_sync(opts, files):`) with:
```python
    def _scan_error(exc):
        """Translate a python-musefs ScanError to MusefsError, preserving the
        plugin's historical message text."""
        if exc.kind == "not_found":
            return MusefsError(
                f"musefs binary '{exc.binary}' not found; set the binary path "
                f"in the musefs options"
            )
        if exc.kind == "timeout":
            return MusefsError(
                f"`{exc.binary} scan` for {exc.target} timed out after "
                f"{SCAN_TIMEOUT_SECONDS}s; the scan may be stuck — check the "
                f"binary and DB."
            )
        return MusefsError(
            f"`{exc.binary} scan` failed for {exc.target} "
            f"(exit {exc.returncode}): {exc.stderr}"
        )

    def _do_sync(opts, files):
        """Background-thread worker: autoscan each file, then write tags/art.
        Returns SyncStats. Raises MusefsError / SchemaMismatch on hard failure."""
        if not opts.db:
            raise MusefsError("no musefs DB configured; set the DB path in Options → musefs sync")
        if opts.autoscan:
            for f in files.values():
                try:
                    run_scan(opts.bin, opts.db, f.filename, timeout=SCAN_TIMEOUT_SECONDS)
                except ScanError as exc:
                    raise _scan_error(exc)
        elif not os.path.exists(opts.db):
            raise MusefsError(
                f"musefs DB not found at {opts.db}; enable autoscan or run `musefs scan` first"
            )

        conn = connect(opts.db)
        try:
            check_schema_version(conn)
            records = []
            for key, f in files.items():
                pairs = map_fields(f.metadata, opts.fields)
                art = front_cover(f.metadata)
                records.append(Record(key=key, pairs=pairs, art=art))
            stats = sync_files(conn, records)
            # Single commit: a mid-loop raise rolls back all tag/art writes for
            # this batch. Autoscan's structural rows are already committed by
            # run_scan (one txn per file), so a retry only re-syncs, not re-scans.
            conn.commit()
            return stats
        finally:
            conn.close()
```

Note: `_scan_error` and `_do_sync` live inside the `if _PICARD:` block (same indentation as the existing `_do_sync`). `realpath_key` stays used by `_resolved_files`. `SyncStats` is **not** imported anymore — `sync_files` constructs it internally and `_done` only calls `stats.summary()`; importing it unused would trip ruff F401. If the real-Picard callback-flow tests reference `SyncStats` by patching it on this module, import it and add it back, but the default is to omit it.

- [ ] **Step 4: Run the Picard `_core` (Qt-free) tests**

Run:
```bash
cd contrib/picard && python -m pytest tests -v -p no:cacheprovider 2>&1 | tail -30
```
Expected: the Qt-free `_core` tests (map_fields, front_cover, resolve_config, parse_field_map) PASS. Tests that `importorskip("picard")` skip if Picard isn't installed; that's fine here. If running with real Picard (CI), the callback-flow/options tests also pass.

- [ ] **Step 5: Commit**

```bash
git add contrib/picard/musefs/_core.py contrib/picard/musefs/__init__.py contrib/picard/tests/conftest.py
git commit -m "refactor(picard): consume vendored python-musefs for store/scan/sync"
```

---

## Task 14: Picard drift-guard test + prune duplicated store tests

**Files:**
- Create: `contrib/picard/tests/test_vendor_sync.py`
- Delete: `contrib/picard/tests/test_core_db.py`, `contrib/picard/tests/test_sync.py`, `contrib/picard/tests/test_run_scan.py`
- Repoint imports: `contrib/picard/tests/test_path_gate.py`, `contrib/picard/tests/test_callback_flow.py`, `contrib/picard/tests/test_conftest_sanity.py`, `contrib/picard/tests/test_sync_roundtrip.py`
- Keep unchanged: `contrib/picard/tests/test_front_cover.py`, `contrib/picard/tests/test_map_fields.py`, `contrib/picard/tests/test_resolve_config.py`, `contrib/picard/tests/test_options_page.py`, `contrib/picard/tests/test_plugin_loads.py`

**Why these dispositions** — after Task 13 the Picard `_core` keeps only mapping/options (`DIRECT_FIELDS`, `_to_int`, `_first_value`, `map_fields`, `front_cover`, `Opts`, `parse_field_map`, `resolve_config`, `MusefsError`, `SCAN_TIMEOUT_SECONDS`) and imports nothing from `musefs._common`. So `connect`, `track_id_for_path`, `realpath_key`, `run_scan`, `sync_one`, and `SyncStats` are no longer importable from `musefs._core`; every test importing them must be deleted or repointed, or the whole `pytest tests` run fails at collection.

- [ ] **Step 1: Write the drift-guard test**

`contrib/picard/tests/test_vendor_sync.py`:
```python
"""Guard: the vendored musefs/_common must match python-musefs byte-for-byte
(after the 3-line generated header). Run vendor_to_picard.py to refresh it."""

from pathlib import Path

CANON = Path(__file__).parents[2] / "python-musefs" / "src" / "musefs_common"
VENDORED = Path(__file__).parents[1] / "musefs" / "_common"


def test_vendored_file_set_matches_canonical():
    canon = {p.name for p in CANON.glob("*.py")}
    vend = {p.name for p in VENDORED.glob("*.py")}
    assert vend == canon, f"vendored set {vend} != canonical {canon}; re-run vendor_to_picard.py"


def test_vendored_bodies_are_byte_identical():
    for src in sorted(CANON.glob("*.py")):
        vend = VENDORED / src.name
        # Drop exactly the 3 generated header lines, compare the rest verbatim.
        body = vend.read_bytes().split(b"\n", 3)[3]
        assert body == src.read_bytes(), f"{src.name} drifted; re-run vendor_to_picard.py"
```

- [ ] **Step 2: Run the drift guard**

Run: `cd contrib/picard && python -m pytest tests/test_vendor_sync.py -v`
Expected: PASS (2 passed). (If it fails, run `python contrib/python-musefs/vendor_to_picard.py` and re-stage.)

- [ ] **Step 3: Delete the three library-internal test files**

```bash
git rm contrib/picard/tests/test_core_db.py contrib/picard/tests/test_sync.py contrib/picard/tests/test_run_scan.py
```
- `test_core_db.py` — tested `connect`/store writes → library `test_store_db.py` / `test_store_art.py`.
- `test_sync.py` — tested the removed `_core.sync_one`/`SyncStats` (skip, tags-replaced, mid-batch, art link, embedded-preserved, oversized, dedup, dry-run, content_version) → library `test_sync.py` (Task 8, now includes those migrated cases).
- `test_run_scan.py` — tested `_core.run_scan` raising `MusefsError` → `run_scan` now lives in the library and raises `ScanError` (library `test_scan.py`); the `ScanError → MusefsError` translation is exercised by the real-Picard `test_callback_flow.py` / `test_sync_roundtrip.py` `_do_sync` path.

- [ ] **Step 4: Repoint imports in the four kept test files**

- `contrib/picard/tests/test_path_gate.py` line 11 — change
  `from musefs._core import connect, realpath_key, track_id_for_path`
  to
  `from musefs._common import connect, realpath_key, track_id_for_path`
- `contrib/picard/tests/test_callback_flow.py` line 44 — change
  `from musefs._core import connect`
  to
  `from musefs._common import connect`
- `contrib/picard/tests/test_conftest_sanity.py` line 1 — change
  `from musefs._core import connect, track_id_for_path`
  to
  `from musefs._common import connect, track_id_for_path`
- `contrib/picard/tests/test_sync_roundtrip.py` line 4 — change
  `from musefs._core import Opts, connect`
  to two lines (`Opts` stays in `_core`, `connect` moves to the vendored library):
  ```python
  from musefs._core import Opts
  from musefs._common import connect
  ```
  (Leave line 39's `from musefs._core import MusefsError` as-is — `MusefsError` stays in `_core`.)

- [ ] **Step 5: Run the full Picard suite (Qt-free locally; real-Picard in CI)**

Run:
```bash
cd contrib/picard && python -m pytest tests -v 2>&1 | tail -30 && ruff check . && ruff format --check .
```
Expected: drift guard + `_core` tests PASS; ruff clean (vendored dir excluded). real-Picard tests skip locally without Picard.

- [ ] **Step 6: Commit** (stage by name; the `git rm` deletions from Step 3 are already staged)

```bash
git add contrib/picard/tests/test_vendor_sync.py \
  contrib/picard/tests/test_path_gate.py contrib/picard/tests/test_callback_flow.py \
  contrib/picard/tests/test_conftest_sanity.py contrib/picard/tests/test_sync_roundtrip.py
git commit -m "test(picard): add vendor drift guard; drop store tests owned by python-musefs"
```

---

## Task 15: CI — new job, `ci-ok` wiring, beets install ordering

**Files:**
- Modify: `.github/workflows/ci.yml`

- [ ] **Step 1: Add the `python-musefs` job after the `interop` job (before `beets`)**

Insert this block in `.github/workflows/ci.yml` between the `interop:` job (ends at line 109) and `beets:` (line 111):
```yaml
  python-musefs:
    needs: changes
    if: needs.changes.outputs.src == 'true'
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@de0fac2e4500dabe0009e67214ff5f5447ce83dd
        with:
          persist-credentials: false
      - uses: actions/setup-python@a309ff8b426b58ec0e2a45f0f869d46889d02405
        with:
          python-version: '3.x'
      - name: Install Ruff
        run: pip install ruff
      - name: Lint
        run: |
          ruff check contrib/python-musefs/
          ruff format --check contrib/python-musefs/
      - name: Install library
        run: pip install -e "contrib/python-musefs[test]"
      - name: Test
        run: python -m pytest contrib/python-musefs/tests -v
```

- [ ] **Step 2: Fix the beets job's install ordering**

In the `beets:` job, replace the `Install beets` step:
```yaml
      - name: Install beets
        run: pip install -e "contrib/beets[test]"
```
with a local-first pair (the unpublished `python-musefs` dependency must resolve from the working tree, not PyPI):
```yaml
      - name: Install python-musefs (local, unpublished dependency)
        run: pip install -e contrib/python-musefs
      - name: Install beets
        run: pip install -e "contrib/beets[test]"
```

- [ ] **Step 3: Add the job to the `ci-ok` aggregator `needs:` list**

Change line 181 from:
```yaml
    needs: [changes, check, interop, beets, picard, e2e]
```
to:
```yaml
    needs: [changes, check, interop, python-musefs, beets, picard, e2e]
```

- [ ] **Step 4: Validate the workflow YAML parses**

Run:
```bash
python -c "import yaml,sys; yaml.safe_load(open('.github/workflows/ci.yml')); print('yaml ok')"
```
Expected: prints `yaml ok`.

- [ ] **Step 5: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: add python-musefs job, gate it in ci-ok, fix beets install ordering"
```

---

## Task 16: Documentation

**Files:**
- Create: `contrib/python-musefs/README.md`
- Modify: `contrib/beets/README.md`
- Modify: `contrib/picard/README.md`
- Modify: `docs/ROADMAP.md`

- [ ] **Step 1: Write `contrib/python-musefs/README.md`**

```markdown
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
```

- [ ] **Step 2: Update `contrib/beets/README.md` install section**

In the "Install (local / development)" section, after the `pluginpath` paragraph, add a note that the plugin now depends on the local `python-musefs` package:
```markdown
The plugin depends on the shared `python-musefs` library, which is unpublished
and lives in this repo. Install it from the working tree **before** the plugin:

```bash
pip install -e contrib/python-musefs
pip install -e "contrib/beets[test]"
```
```
Also update the "Tests" section's `uv pip install -r requirements.txt` flow to install `python-musefs` first (add `uv pip install -e ../python-musefs` before the requirements install).

- [ ] **Step 3: Update `contrib/picard/README.md`**

In the "Install (local / development)" section, after the `cp -r contrib/picard/musefs …` line, add:
```markdown
The `musefs/_common/` subfolder is the vendored `python-musefs` library, copied
in so the plugin folder is self-contained (Picard does not install plugin
dependencies). It is committed; you don't need to do anything to use it. If you
change the shared library, re-run `python contrib/python-musefs/vendor_to_picard.py`
and commit the refreshed copy — CI's drift guard enforces it.
```

- [ ] **Step 4: Update `docs/ROADMAP.md`**

Add a short note (under the contrib/plugins section, or wherever the beets/Picard plugins are tracked) recording that the plugins share the `python-musefs` library for the store contract, beets via pip and Picard via vendoring. Match the surrounding format; one or two sentences.

- [ ] **Step 5: Commit**

```bash
git add contrib/python-musefs/README.md contrib/beets/README.md contrib/picard/README.md docs/ROADMAP.md
git commit -m "docs: describe python-musefs library, vendoring, and install ordering"
```

---

## Final verification

- [ ] **Step 1: Library suite + lint**

Run:
```bash
cd contrib/python-musefs && pip install -e ".[test]" && python -m pytest -v && ruff check . && ruff format --check .
```
Expected: all PASS, ruff clean.

- [ ] **Step 2: beets suite + lint**

Run:
```bash
cd contrib/beets && pip install -e ../python-musefs && pip install -e ".[test]" && python -m pytest tests -v && ruff check . && ruff format --check .
```
Expected: all PASS, ruff clean.

- [ ] **Step 3: Picard Qt-free suite + drift guard + lint**

Run:
```bash
cd contrib/picard && python -m pytest tests -v 2>&1 | tail -20 && ruff check . && ruff format --check .
```
Expected: `_core` + drift-guard tests PASS; real-Picard tests skip without Picard; ruff clean.

- [ ] **Step 4: Drift guard is honest (negative check)**

Run:
```bash
printf '\n# drift\n' >> contrib/python-musefs/src/musefs_common/constants.py
cd contrib/picard && python -m pytest tests/test_vendor_sync.py -q; echo "exit=$?"
cd - >/dev/null && git checkout contrib/python-musefs/src/musefs_common/constants.py
```
Expected: the drift test FAILS (exit non-zero) before the `git checkout` restores the file — proving the guard catches un-vendored edits.

- [ ] **Step 5: Confirm no lingering references to removed `_core` symbols**

Run:
```bash
grep -rn "_core\.\(sync_items\|sync_one\|connect\|upsert_art\|replace_tags\|replace_track_art\|run_scan\|prune_missing\|track_id_for_path\|check_schema_version\)" contrib/ || echo "clean"
```
Expected: `clean` (the adapters and tests call the library for store/scan/sync, not the old `_core` functions). Also confirm no test still imports a removed name:
```bash
grep -rn "from beetsplug._core import.*\(connect\|sync_items\|track_id_for_path\|replace_tags\|upsert_art\)" contrib/beets/tests/ ; \
grep -rn "from musefs._core import.*\(connect\|sync_one\|run_scan\|track_id_for_path\|SyncStats\)" contrib/picard/tests/ ; \
echo "checked"
```
Expected: no matches printed before `checked`.

- [ ] **Step 6: Confirm the mirrored constants still match the Rust source**

The Python `EXPECTED_USER_VERSION` and `MAX_ART_BYTES` mirror Rust; the plan's tests only assert the Python value against itself, so cross-check the source once:
```bash
grep -n "MAX_ART_BYTES" musefs-core/src/scan.rs
python -c "import sys; sys.path.insert(0,'musefs-db/src'); print('check user_version in schema.rs MIGRATIONS == 2')"
grep -c "MIGRATION_V" musefs-db/src/schema.rs
```
Expected: `scan.rs` defines `MAX_ART_BYTES` as `16 * 1024 * 1024 - 64 * 1024` (16 MiB − 64 KiB); `schema.rs` has 2 `MIGRATION_V*` constants (V1, V2), matching `EXPECTED_USER_VERSION = 2`. If either drifted, update `constants.py` (and re-vendor) before merging.
```
