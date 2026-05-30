# musefs Picard plugin Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship a MusicBrainz Picard plugin (`contrib/picard/`) whose "Sync to musefs" context-menu action pushes Picard's in-memory tags + front cover into the musefs SQLite store, keyed by canonical path, without ever rewriting audio.

**Architecture:** A self-contained folder plugin mirroring the shipped beets plugin. All pure logic (DB-contract SQL, field mapping, art extraction, config resolution, the per-file `sync_one`) lives in `musefs/_core.py` with **no Picard imports**, so it is fully unit/integration tested. The Picard-coupled glue (the `BaseAction`, the Qt options page, registration, file resolution, background-thread orchestration) lives in `musefs/__init__.py` behind a guarded import, so the package imports cleanly in a Picard-free test environment; per the spec that glue is verified by a manual smoke test, not unit tests.

**Tech Stack:** Python 3.8+ (stdlib `sqlite3`, `subprocess`, `hashlib`), pytest, ruff. Picard 2.x plugin API v2 (`BaseAction`, `register_*_action`, `register_options_page`). The `musefs` Rust binary is shelled out to for row creation (autoscan).

**Spec:** `docs/superpowers/specs/2026-05-30-picard-plugin-design.md`

**Reference implementation (read, do not modify):** `contrib/beets/beetsplug/_core.py`, `contrib/beets/beetsplug/musefs.py`, `contrib/beets/tests/`.

---

## File Structure

```
contrib/picard/
  musefs/
    __init__.py          # Picard plugin entry: PLUGIN_* constants + guarded glue (Task 1 shell, Task 10 full)
    _core.py             # ALL pure logic: DB contract, map_fields, front_cover, resolve_config, sync_one, run_scan
  tests/
    schema_v1.sql        # copy of musefs-db MIGRATION_V1 (Task 1)
    conftest.py          # db_path / make_track / FakeMetadata / FakeImage fixtures (Task 3)
    test_core_db.py      # schema guard, realpath_key, sniff_mime (Task 2)
    test_map_fields.py   # Picard metadata -> tag pairs (Task 4)
    test_front_cover.py  # front-cover extraction from images (Task 5)
    test_resolve_config.py # env-over-page precedence + field-map parse (Task 6)
    test_sync.py         # sync_one integration over primitives (Task 7)
    test_run_scan.py     # subprocess autoscan wrapper (Task 8)
    test_path_gate.py    # opt-in gate vs the real musefs binary (Task 9)
  README.md              # install + workflow + manual smoke test (Task 10)
  pyproject.toml         # packaging + pytest config (Task 1)
  requirements.txt       # picard + pytest (Task 1)
  ruff.toml              # lint config (Task 1)
```

**Responsibility split:** `_core.py` is the only file with logic worth testing; it never imports Picard. `__init__.py` is a thin Picard adapter. Tests target `_core` exclusively.

---

## Task 1: Scaffold packaging, schema fixture, and the importable package shell

**Files:**
- Create: `contrib/picard/pyproject.toml`
- Create: `contrib/picard/requirements.txt`
- Create: `contrib/picard/ruff.toml`
- Create: `contrib/picard/tests/schema_v1.sql`
- Create: `contrib/picard/musefs/__init__.py`

- [ ] **Step 1: Create `contrib/picard/pyproject.toml`**

```toml
[build-system]
requires = ["setuptools>=61"]
build-backend = "setuptools.build_meta"

[project]
name = "musefs-picard"
version = "0.1.0"
description = "Sync MusicBrainz Picard metadata into the musefs SQLite store"
requires-python = ">=3.8"

[project.optional-dependencies]
test = ["pytest>=7"]

[tool.setuptools]
packages = ["musefs"]

[tool.pytest.ini_options]
testpaths = ["tests"]
# So `import musefs._core` resolves to contrib/picard/musefs without an install.
pythonpath = ["."]
markers = [
    "musefs_bin: tests that shell out to the real `musefs` Rust binary (opt-in)",
]
# Skip the opt-in binary gate by default; run it explicitly with `-m musefs_bin`.
addopts = "-m 'not musefs_bin'"
```

- [ ] **Step 2: Create `contrib/picard/requirements.txt`**

```text
pytest>=7
```

(Picard itself is not pip-installed for the tests — `_core` has no Picard imports. The plugin runs inside a Picard install at runtime.)

- [ ] **Step 3: Create `contrib/picard/ruff.toml`**

```toml
line-length = 100
target-version = "py38"

[lint]
select = ["E", "F", "I", "N", "W"]

[format]
preview = true
```

- [ ] **Step 4: Create `contrib/picard/tests/schema_v1.sql`** (verbatim copy of the beets fixture; this is the copied-not-shared decision from spec §4)

```sql
-- Copy of MIGRATION_V1 in musefs-db/src/schema.rs (the authoritative schema).
-- If the Rust schema changes: bump EXPECTED_USER_VERSION in musefs/_core.py
-- and update this file. Drift otherwise surfaces as a SchemaMismatch at connect.
CREATE TABLE tracks (
    id              INTEGER PRIMARY KEY,
    backing_path    TEXT NOT NULL UNIQUE,
    format          TEXT NOT NULL,
    audio_offset    INTEGER NOT NULL,
    audio_length    INTEGER NOT NULL,
    backing_size    INTEGER NOT NULL,
    backing_mtime   INTEGER NOT NULL,
    content_version INTEGER NOT NULL DEFAULT 0,
    updated_at      INTEGER NOT NULL
);

CREATE TABLE tags (
    track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    key      TEXT NOT NULL,
    value    TEXT NOT NULL,
    ordinal  INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (track_id, key, ordinal)
);

CREATE TABLE art (
    id       INTEGER PRIMARY KEY,
    sha256   TEXT NOT NULL UNIQUE,
    mime     TEXT NOT NULL,
    width    INTEGER,
    height   INTEGER,
    byte_len INTEGER NOT NULL,
    data     BLOB NOT NULL
);

CREATE TABLE track_art (
    track_id     INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    art_id       INTEGER NOT NULL REFERENCES art(id),
    picture_type INTEGER NOT NULL DEFAULT 3,
    description  TEXT NOT NULL DEFAULT '',
    ordinal      INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (track_id, ordinal)
);

CREATE TRIGGER tags_ai AFTER INSERT ON tags BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = NEW.track_id;
END;
CREATE TRIGGER tags_au AFTER UPDATE ON tags BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = NEW.track_id;
END;
CREATE TRIGGER tags_ad AFTER DELETE ON tags BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = OLD.track_id;
END;

CREATE TRIGGER track_art_ai AFTER INSERT ON track_art BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = NEW.track_id;
END;
CREATE TRIGGER track_art_au AFTER UPDATE ON track_art BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = NEW.track_id;
END;
CREATE TRIGGER track_art_ad AFTER DELETE ON track_art BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = OLD.track_id;
END;

PRAGMA user_version = 1;
```

- [ ] **Step 5: Create `contrib/picard/musefs/__init__.py`** (shell — Task 10 replaces it with full glue). The guarded import is what lets `import musefs._core` work in the test env without Picard installed.

```python
"""musefs Picard plugin: sync Picard metadata into the musefs SQLite store.

The Picard-coupled glue is filled in during implementation; until then this
guarded block is a no-op so the package imports cleanly without Picard (the
test suite only exercises ``musefs._core``).
"""

PLUGIN_NAME = "musefs sync"
PLUGIN_AUTHOR = "musefs contributors"
PLUGIN_DESCRIPTION = (
    "Right-click a file/album → 'Sync to musefs' to push Picard's tags and "
    "front cover into a musefs SQLite store, without rewriting the audio file."
)
PLUGIN_VERSION = "0.1.0"
PLUGIN_API_VERSIONS = ["2.2", "2.6", "2.7", "2.8", "2.9", "2.10", "2.11", "2.12"]
PLUGIN_LICENSE = "MIT"
PLUGIN_LICENSE_URL = "https://opensource.org/licenses/MIT"

try:
    from picard.ui.itemviews import BaseAction  # noqa: F401
except ImportError:
    # Picard not present (e.g. running the unit tests). Glue is registered in
    # the full __init__.py; _core imports remain available regardless.
    pass
```

- [ ] **Step 6: Verify the package imports without Picard**

Run: `cd contrib/picard && python -c "import musefs; print(musefs.PLUGIN_VERSION)"`
Expected: prints `0.1.0` with no ImportError.

- [ ] **Step 7: Commit**

```bash
git add contrib/picard/pyproject.toml contrib/picard/requirements.txt contrib/picard/ruff.toml contrib/picard/tests/schema_v1.sql contrib/picard/musefs/__init__.py
git commit -m "feat(picard): scaffold plugin package, schema fixture, packaging"
```

---

## Task 2: `_core` DB-contract primitives + schema/path/mime tests

**Files:**
- Create: `contrib/picard/musefs/_core.py`
- Test: `contrib/picard/tests/test_core_db.py`

- [ ] **Step 1: Write the failing test** — `contrib/picard/tests/test_core_db.py`

```python
import sqlite3

import pytest

from musefs._core import (
    SchemaMismatch,
    check_schema_version,
    realpath_key,
    sniff_mime,
)


def test_schema_guard_rejects_wrong_version():
    conn = sqlite3.connect(":memory:")
    with pytest.raises(SchemaMismatch):
        check_schema_version(conn)


def test_schema_guard_accepts_v1():
    conn = sqlite3.connect(":memory:")
    conn.execute("PRAGMA user_version = 1")
    check_schema_version(conn)  # must not raise


def test_realpath_key_returns_absolute_str(tmp_path):
    f = tmp_path / "a.flac"
    f.write_bytes(b"x")
    key = realpath_key(str(f))
    assert key == str(f.resolve())
    assert isinstance(key, str)


def test_realpath_key_accepts_bytes(tmp_path):
    import os

    f = tmp_path / "a.flac"
    f.write_bytes(b"x")
    key = realpath_key(os.fsencode(str(f)))
    assert key == str(f.resolve())


def test_sniff_mime_magic_bytes():
    assert sniff_mime(b"\xff\xd8\xff\xe0junk", "x") == "image/jpeg"
    assert sniff_mime(b"\x89PNG\r\n\x1a\n" + b"\x00" * 8, "x") == "image/png"
    assert sniff_mime(b"RIFF\x00\x00\x00\x00WEBPxxxx", "x") == "image/webp"


def test_sniff_mime_extension_fallback():
    assert sniff_mime(b"nope", "cover.PNG") == "image/png"
    assert sniff_mime(b"nope", "cover.bin") == "application/octet-stream"
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd contrib/picard && python -m pytest tests/test_core_db.py -v`
Expected: FAIL — `ModuleNotFoundError: No module named 'musefs._core'`.

- [ ] **Step 3: Create `contrib/picard/musefs/_core.py`** with the full pure-logic module. (This single module also contains the mapping/sync/config functions used by later tasks; later tasks add tests against them but no further edits to this file are required.)

```python
"""Pure logic for the musefs Picard plugin: no Picard imports live here.

Everything Picard-specific (the BaseAction, the options page, registration)
is in ``__init__.py``; this module is unit-testable on its own.
"""

from __future__ import annotations

import hashlib
import os
import sqlite3
import subprocess
from dataclasses import dataclass, field

# Schema version this plugin was written against (musefs schema.rs MIGRATIONS
# length). The plugin refuses to run against any other version.
EXPECTED_USER_VERSION = 1

# Mirror of musefs-core scan.rs MAX_ART_BYTES: 16 MiB minus 64 KiB headroom.
MAX_ART_BYTES = 16 * 1024 * 1024 - 64 * 1024

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


class SchemaMismatch(Exception):  # noqa: N818
    """Raised when the musefs DB schema version differs from what the plugin
    targets (``EXPECTED_USER_VERSION``)."""

    def __init__(self, found):
        self.found = found
        super().__init__(
            f"musefs DB user_version is {found}, plugin targets "
            f"{EXPECTED_USER_VERSION}; the musefs and plugin versions have "
            f"diverged."
        )


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


def connect(db_path):
    """Open the musefs DB with a busy timeout and foreign keys enabled."""
    conn = sqlite3.connect(db_path)
    # 5s busy timeout so a brief write doesn't fail while the FUSE mount reads.
    conn.execute("PRAGMA busy_timeout = 5000")
    conn.execute("PRAGMA foreign_keys = ON")
    return conn


def check_schema_version(conn):
    """Raise ``SchemaMismatch`` unless the DB's ``user_version`` matches the
    version this plugin targets."""
    found = conn.execute("PRAGMA user_version").fetchone()[0]
    if found != EXPECTED_USER_VERSION:
        raise SchemaMismatch(found)


def track_id_for_path(conn, key):
    """Return the track id whose backing_path equals ``key``, or None."""
    row = conn.execute("SELECT id FROM tracks WHERE backing_path = ?", (key,)).fetchone()
    return row[0] if row else None


def replace_tags(conn, track_id, pairs):
    """Replace all tags for a track. Duplicate keys get incrementing ordinals
    (mirroring musefs scan ingest)."""
    conn.execute("DELETE FROM tags WHERE track_id = ?", (track_id,))
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
    if data[:4] == b"RIFF" and data[8:12] == b"WEBP":
        return "image/webp"
    ext = os.path.splitext(path)[1].lower()
    return _EXT_MIME.get(ext, "application/octet-stream")


def upsert_art(conn, data, mime):
    """Content-address ``data`` by sha256 and return its art id, inserting only
    if new (mirrors musefs Db::upsert_art)."""
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


@dataclass
class SyncStats:
    synced: int = 0
    skipped: int = 0  # path had no matching track row
    art_linked: int = 0
    skipped_art: int = 0  # front cover oversized

    def summary(self):
        return (
            f"synced={self.synced} skipped={self.skipped} "
            f"art_linked={self.art_linked} skipped_art={self.skipped_art}"
        )


def sync_one(conn, key, pairs, art, stats, *, dry_run=False):
    """Sync one file's primitives into the DB. ``key`` is the realpath key,
    ``pairs`` the tag list, ``art`` an ``(bytes, mime)`` tuple or ``None``.
    Mutates ``stats``. Caller owns the transaction.

    Tags are always fully replaced. Art is replaced **only** when present and
    within the size cap (conditional replacement, spec §7): no front cover, or
    an oversized one, leaves any scan-seeded ``track_art`` untouched.
    """
    track_id = track_id_for_path(conn, key)
    if track_id is None:
        stats.skipped += 1
        return

    will_link_art = False
    if art is not None:
        data, _mime = art
        if len(data) > MAX_ART_BYTES:
            stats.skipped_art += 1
        else:
            will_link_art = True

    if not dry_run:
        replace_tags(conn, track_id, pairs)
        if will_link_art:
            data, mime = art
            art_id = upsert_art(conn, data, mime)
            replace_track_art(conn, track_id, art_id)

    if will_link_art:
        stats.art_linked += 1
    stats.synced += 1


def run_scan(binary, db_path, target):
    """Run ``<binary> scan <target> --db <db_path>``. Creates the DB if absent
    and fills the structural columns the plugin can't compute. Raises
    ``MusefsError`` on a missing binary or non-zero exit."""
    try:
        result = subprocess.run(
            [binary, "scan", target, "--db", db_path],
            capture_output=True,
        )
    except FileNotFoundError:
        raise MusefsError(
            f"musefs binary '{binary}' not found; set the binary path in the "
            f"musefs options"
        )
    if result.returncode != 0:
        raise MusefsError(
            f"`{binary} scan` failed for {target} (exit {result.returncode}): "
            f"{result.stderr.decode(errors='replace').strip()}"
        )
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd contrib/picard && python -m pytest tests/test_core_db.py -v`
Expected: PASS (6 passed).

- [ ] **Step 5: Lint**

Run: `cd contrib/picard && ruff check . && ruff format --check .`
Expected: `All checks passed!` (run `ruff format .` first if the formatter rewrites anything, then re-run `--check`).

- [ ] **Step 6: Commit**

```bash
git add contrib/picard/musefs/_core.py contrib/picard/tests/test_core_db.py
git commit -m "feat(picard): add _core DB-contract logic with schema/path/mime tests"
```

---

## Task 3: Test fixtures (conftest)

**Files:**
- Create: `contrib/picard/tests/conftest.py`
- Test: `contrib/picard/tests/test_conftest_sanity.py`

- [ ] **Step 1: Write the failing test** — `contrib/picard/tests/test_conftest_sanity.py`

```python
from musefs._core import connect, track_id_for_path


def test_db_path_has_schema(db_path):
    conn = connect(db_path)
    try:
        # user_version applied from the fixture SQL.
        assert conn.execute("PRAGMA user_version").fetchone()[0] == 1
    finally:
        conn.close()


def test_make_track_inserts_row(db_path, make_track):
    tid = make_track("/music/a.flac")
    conn = connect(db_path)
    try:
        assert track_id_for_path(conn, "/music/a.flac") == tid
    finally:
        conn.close()
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd contrib/picard && python -m pytest tests/test_conftest_sanity.py -v`
Expected: FAIL — fixtures `db_path` / `make_track` not found.

- [ ] **Step 3: Create `contrib/picard/tests/conftest.py`**

```python
import sqlite3
import time
from pathlib import Path

import pytest

from musefs._core import connect as musefs_connect

SCHEMA_SQL = (Path(__file__).parent / "schema_v1.sql").read_text()


@pytest.fixture
def db_path(tmp_path):
    """A temp musefs DB with the V1 schema applied."""
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


class FakeImage:
    """Stand-in for a Picard CoverArtImage: is_front_image() + data + mimetype."""

    def __init__(self, data, mimetype, front=True):
        self.data = data
        self.mimetype = mimetype
        self._front = front

    def is_front_image(self):
        return self._front


class FakeMetadata:
    """Stand-in for Picard's Metadata: getall() + images."""

    def __init__(self, images=(), **tags):
        self._tags = {k: (v if isinstance(v, list) else [v]) for k, v in tags.items()}
        self.images = list(images)

    def getall(self, key):
        return self._tags.get(key, [])


@pytest.fixture
def fake_metadata():
    return FakeMetadata  # the class; call it directly in tests


@pytest.fixture
def fake_image():
    return FakeImage  # the class; call it directly in tests
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd contrib/picard && python -m pytest tests/test_conftest_sanity.py -v`
Expected: PASS (2 passed).

- [ ] **Step 5: Commit**

```bash
git add contrib/picard/tests/conftest.py contrib/picard/tests/test_conftest_sanity.py
git commit -m "test(picard): add db + Picard-fake fixtures (conftest)"
```

---

## Task 4: `map_fields` tests

**Files:**
- Test: `contrib/picard/tests/test_map_fields.py`
- (Implementation already in `_core.py` from Task 2.)

- [ ] **Step 1: Write the characterization test** (passes against the `_core` written in Task 2) — `contrib/picard/tests/test_map_fields.py`

```python
from musefs._core import map_fields


def test_direct_fields_copied(fake_metadata):
    d = dict(map_fields(fake_metadata(title="Song", artist="Band", album="Disc")))
    assert d["title"] == "Song"
    assert d["artist"] == "Band"
    assert d["album"] == "Disc"


def test_first_value_of_multivalued_field(fake_metadata):
    # Picard multi-valued: getall returns a list; we take the first.
    d = dict(map_fields(fake_metadata(artist=["First", "Second"])))
    assert d["artist"] == "First"


def test_empty_and_whitespace_omitted(fake_metadata):
    d = dict(map_fields(fake_metadata(title="", artist="   ")))
    assert d == {}


def test_tracknumber_and_discnumber_passthrough(fake_metadata):
    d = dict(map_fields(fake_metadata(tracknumber="7", discnumber="2")))
    assert d["tracknumber"] == "7"
    assert d["discnumber"] == "2"


def test_zero_tracknumber_omitted(fake_metadata):
    d = dict(map_fields(fake_metadata(tracknumber="0", discnumber="0")))
    assert "tracknumber" not in d
    assert "discnumber" not in d


def test_date_passthrough(fake_metadata):
    assert dict(map_fields(fake_metadata(date="1999-03-05")))["date"] == "1999-03-05"
    assert dict(map_fields(fake_metadata(date="1999")))["date"] == "1999"


def test_extra_field_override_adds_mapping(fake_metadata):
    md = fake_metadata(title="Song", comment="hi")
    d = dict(map_fields(md, extra_fields={"comment": "comment"}))
    assert d["comment"] == "hi"
    assert d["title"] == "Song"


def test_extra_field_remaps_existing_key(fake_metadata):
    d = dict(map_fields(fake_metadata(title="Song"), extra_fields={"title": "subtitle"}))
    assert d["subtitle"] == "Song"
    assert "title" not in d
```

- [ ] **Step 2: Run test to verify it passes** (implementation already exists)

Run: `cd contrib/picard && python -m pytest tests/test_map_fields.py -v`
Expected: PASS (8 passed). If any fail, the bug is in `_core.map_fields` / `_first_value` — fix there, not in the test.

- [ ] **Step 3: Commit**

```bash
git add contrib/picard/tests/test_map_fields.py
git commit -m "test(picard): cover map_fields (identity, first-value, omissions, overrides)"
```

---

## Task 5: `front_cover` tests

**Files:**
- Test: `contrib/picard/tests/test_front_cover.py`
- (Implementation already in `_core.py`.)

- [ ] **Step 1: Write the characterization test** (passes against the `_core` written in Task 2) — `contrib/picard/tests/test_front_cover.py`

```python
from musefs._core import front_cover


def test_no_images_returns_none(fake_metadata):
    assert front_cover(fake_metadata()) is None


def test_returns_first_front_image_data_and_mime(fake_metadata, fake_image):
    img = fake_image(b"JPEGBYTES", "image/jpeg", front=True)
    data, mime = front_cover(fake_metadata(images=[img]))
    assert data == b"JPEGBYTES"
    assert mime == "image/jpeg"


def test_skips_non_front_images(fake_metadata, fake_image):
    back = fake_image(b"BACK", "image/png", front=False)
    front = fake_image(b"FRONT", "image/jpeg", front=True)
    data, mime = front_cover(fake_metadata(images=[back, front]))
    assert data == b"FRONT"


def test_all_non_front_returns_none(fake_metadata, fake_image):
    back = fake_image(b"BACK", "image/png", front=False)
    assert front_cover(fake_metadata(images=[back])) is None
```

- [ ] **Step 2: Run test to verify it passes**

Run: `cd contrib/picard && python -m pytest tests/test_front_cover.py -v`
Expected: PASS (4 passed).

- [ ] **Step 3: Commit**

```bash
git add contrib/picard/tests/test_front_cover.py
git commit -m "test(picard): cover front_cover image extraction"
```

---

## Task 6: `resolve_config` + `parse_field_map` tests

**Files:**
- Test: `contrib/picard/tests/test_resolve_config.py`
- (Implementation already in `_core.py`.)

- [ ] **Step 1: Write the characterization test** (passes against the `_core` written in Task 2) — `contrib/picard/tests/test_resolve_config.py`

```python
from musefs._core import parse_field_map, resolve_config


def test_page_values_used_when_no_env():
    settings = {"musefs_db": "/page.db", "musefs_bin": "/page/musefs", "musefs_autoscan": False}
    opts = resolve_config(settings, environ={})
    assert opts.db == "/page.db"
    assert opts.bin == "/page/musefs"
    assert opts.autoscan is False


def test_env_overrides_page():
    settings = {"musefs_db": "/page.db", "musefs_bin": "/page/musefs"}
    environ = {"MUSEFS_DB": "/env.db", "MUSEFS_BIN": "/env/musefs"}
    opts = resolve_config(settings, environ)
    assert opts.db == "/env.db"
    assert opts.bin == "/env/musefs"


def test_defaults_when_unset():
    opts = resolve_config(settings={}, environ={})
    assert opts.db is None
    assert opts.bin == "musefs"
    assert opts.autoscan is True
    assert opts.fields == {}


def test_autoscan_and_fields_have_no_env_form():
    # Only DB/BIN read env; autoscan/fields come from the page regardless of env.
    settings = {"musefs_autoscan": False, "musefs_fields": "comment=comment"}
    environ = {"MUSEFS_AUTOSCAN": "1", "MUSEFS_FIELDS": "x=y"}
    opts = resolve_config(settings, environ)
    assert opts.autoscan is False
    assert opts.fields == {"comment": "comment"}


def test_fields_accepts_dict_directly():
    opts = resolve_config({"musefs_fields": {"comment": "comment"}}, environ={})
    assert opts.fields == {"comment": "comment"}


def test_parse_field_map_variants():
    assert parse_field_map("") == {}
    assert parse_field_map("comment=comment") == {"comment": "comment"}
    assert parse_field_map("a=b, c=d") == {"a": "b", "c": "d"}
    assert parse_field_map("a=b\n c=d ") == {"a": "b", "c": "d"}
    # Invalid entries are dropped, not raised.
    assert parse_field_map("noequals, =novalue, key=") == {}
```

- [ ] **Step 2: Run test to verify it passes**

Run: `cd contrib/picard && python -m pytest tests/test_resolve_config.py -v`
Expected: PASS (6 passed).

- [ ] **Step 3: Commit**

```bash
git add contrib/picard/tests/test_resolve_config.py
git commit -m "test(picard): cover resolve_config env precedence + field-map parsing"
```

---

## Task 7: `sync_one` integration tests

**Files:**
- Test: `contrib/picard/tests/test_sync.py`
- (Implementation already in `_core.py`.)

- [ ] **Step 1: Write the characterization test** (integration; passes against the `_core` written in Task 2) — `contrib/picard/tests/test_sync.py`

```python
from musefs._core import SyncStats, connect, sync_one

JPEG = b"\xff\xd8\xff\xe0" + b"\x00" * 32


def _sync(conn, key, pairs, art=None, dry_run=False):
    stats = SyncStats()
    sync_one(conn, key, pairs, art, stats, dry_run=dry_run)
    return stats


def test_skip_when_no_row(db_path):
    conn = connect(db_path)
    try:
        stats = _sync(conn, "/music/missing.flac", [("title", "X")])
        conn.commit()
        assert stats.synced == 0
        assert stats.skipped == 1
    finally:
        conn.close()


def test_tags_written_for_existing_row(db_path, make_track):
    tid = make_track("/music/a.flac")
    conn = connect(db_path)
    try:
        stats = _sync(conn, "/music/a.flac", [("title", "Song"), ("artist", "Band")])
        conn.commit()
        assert stats.synced == 1
        title = conn.execute(
            "SELECT value FROM tags WHERE track_id=? AND key='title'", (tid,)
        ).fetchone()[0]
        assert title == "Song"
        # Spec §3.5: the tags trigger bumped the track's content_version, so the
        # mount's HeaderCache rebuilds the layout. Make that observable.
        cv = conn.execute("SELECT content_version FROM tracks WHERE id=?", (tid,)).fetchone()[0]
        assert cv >= 1
    finally:
        conn.close()


def test_skip_mid_batch_does_not_abort_others(db_path, make_track):
    # Spec §9: a per-file "no row" skip must not roll back the run — the real
    # files around it still get their tags, sharing one SyncStats and one txn.
    tid_a = make_track("/music/a.flac")
    tid_b = make_track("/music/b.flac")
    conn = connect(db_path)
    try:
        stats = SyncStats()
        for key in ("/music/a.flac", "/music/missing.flac", "/music/b.flac"):
            sync_one(conn, key, [("title", "T")], None, stats)
        conn.commit()
        assert stats.synced == 2
        assert stats.skipped == 1
        for tid in (tid_a, tid_b):
            assert conn.execute(
                "SELECT value FROM tags WHERE track_id=? AND key='title'", (tid,)
            ).fetchone()[0] == "T"
    finally:
        conn.close()


def test_tags_fully_replaced(db_path, make_track):
    tid = make_track("/music/a.flac")
    conn = connect(db_path)
    try:
        _sync(conn, "/music/a.flac", [("title", "Old"), ("genre", "Rock")])
        conn.commit()
        _sync(conn, "/music/a.flac", [("title", "New")])
        conn.commit()
        rows = dict(conn.execute("SELECT key, value FROM tags WHERE track_id=?", (tid,)))
        assert rows == {"title": "New"}  # genre gone after replace
    finally:
        conn.close()


def test_art_linked_when_front_cover_present(db_path, make_track):
    tid = make_track("/music/a.flac")
    conn = connect(db_path)
    try:
        stats = _sync(conn, "/music/a.flac", [("title", "Song")], art=(JPEG, "image/jpeg"))
        conn.commit()
        assert stats.art_linked == 1
        assert (
            conn.execute("SELECT COUNT(*) FROM track_art WHERE track_id=?", (tid,)).fetchone()[0]
            == 1
        )
    finally:
        conn.close()


def test_embedded_art_preserved_when_no_front_cover(db_path, make_track):
    # Simulate scan-ingested art already linked to the track.
    tid = make_track("/music/a.flac")
    conn = connect(db_path)
    try:
        conn.execute(
            "INSERT INTO art (sha256, mime, byte_len, data) VALUES "
            "('deadbeef', 'image/jpeg', 3, X'aabbcc')"
        )
        art_id = conn.execute("SELECT id FROM art WHERE sha256='deadbeef'").fetchone()[0]
        conn.execute("INSERT INTO track_art (track_id, art_id) VALUES (?, ?)", (tid, art_id))
        conn.commit()
        stats = _sync(conn, "/music/a.flac", [("title", "Song")], art=None)
        conn.commit()
        assert stats.art_linked == 0
        row = conn.execute("SELECT art_id FROM track_art WHERE track_id=?", (tid,)).fetchone()
        assert row == (art_id,)  # untouched
    finally:
        conn.close()


def test_oversized_art_skipped_but_tags_written(db_path, make_track, monkeypatch):
    import musefs._core as core

    monkeypatch.setattr(core, "MAX_ART_BYTES", 8)
    tid = make_track("/music/a.flac")
    conn = connect(db_path)
    try:
        stats = _sync(conn, "/music/a.flac", [("title", "Song")], art=(b"X" * 64, "image/jpeg"))
        conn.commit()
        assert stats.skipped_art == 1
        assert stats.art_linked == 0
        # Tags still written despite oversized art.
        assert conn.execute(
            "SELECT value FROM tags WHERE track_id=? AND key='title'", (tid,)
        ).fetchone()[0] == "Song"
    finally:
        conn.close()


def test_art_deduped_across_files(db_path, make_track):
    make_track("/music/a.flac")
    make_track("/music/b.flac")
    conn = connect(db_path)
    try:
        _sync(conn, "/music/a.flac", [("title", "A")], art=(JPEG, "image/jpeg"))
        _sync(conn, "/music/b.flac", [("title", "B")], art=(JPEG, "image/jpeg"))
        conn.commit()
        assert conn.execute("SELECT COUNT(*) FROM art").fetchone()[0] == 1
        assert conn.execute("SELECT COUNT(*) FROM track_art").fetchone()[0] == 2
    finally:
        conn.close()


def test_dry_run_writes_nothing(db_path, make_track):
    tid = make_track("/music/a.flac")
    conn = connect(db_path)
    try:
        stats = _sync(conn, "/music/a.flac", [("title", "Song")], art=(JPEG, "image/jpeg"), dry_run=True)
        conn.rollback()
        assert stats.synced == 1
        assert stats.art_linked == 1  # "would link"
        assert conn.execute("SELECT COUNT(*) FROM tags WHERE track_id=?", (tid,)).fetchone()[0] == 0
        assert conn.execute("SELECT COUNT(*) FROM art").fetchone()[0] == 0
        assert conn.execute("SELECT COUNT(*) FROM track_art").fetchone()[0] == 0
    finally:
        conn.close()
```

- [ ] **Step 2: Run test to verify it passes**

Run: `cd contrib/picard && python -m pytest tests/test_sync.py -v`
Expected: PASS (9 passed).

- [ ] **Step 3: Commit**

```bash
git add contrib/picard/tests/test_sync.py
git commit -m "test(picard): integration cover sync_one (tags, art, dedup, dry-run)"
```

---

## Task 8: `run_scan` autoscan wrapper tests

**Files:**
- Test: `contrib/picard/tests/test_run_scan.py`
- (Implementation already in `_core.py`.)

- [ ] **Step 1: Write the characterization test** (passes against the `_core` written in Task 2) — `contrib/picard/tests/test_run_scan.py`

```python
import subprocess
from types import SimpleNamespace

import pytest

from musefs._core import MusefsError, run_scan


def test_run_scan_invokes_binary(monkeypatch):
    calls = []

    def fake_run(cmd, capture_output):
        calls.append(cmd)
        return SimpleNamespace(returncode=0, stdout=b"", stderr=b"")

    monkeypatch.setattr(subprocess, "run", fake_run)
    run_scan("musefs", "/db.sqlite", "/music/a.flac")
    assert calls == [["musefs", "scan", "/music/a.flac", "--db", "/db.sqlite"]]


def test_run_scan_missing_binary_raises(monkeypatch):
    def fake_run(cmd, capture_output):
        raise FileNotFoundError()

    monkeypatch.setattr(subprocess, "run", fake_run)
    with pytest.raises(MusefsError, match="not found"):
        run_scan("nope", "/db.sqlite", "/music/a.flac")


def test_run_scan_nonzero_exit_raises(monkeypatch):
    def fake_run(cmd, capture_output):
        return SimpleNamespace(returncode=2, stdout=b"", stderr=b"boom")

    monkeypatch.setattr(subprocess, "run", fake_run)
    with pytest.raises(MusefsError, match="boom"):
        run_scan("musefs", "/db.sqlite", "/music/a.flac")
```

- [ ] **Step 2: Run test to verify it passes**

Run: `cd contrib/picard && python -m pytest tests/test_run_scan.py -v`
Expected: PASS (3 passed).

- [ ] **Step 3: Run the whole default suite + lint**

Run: `cd contrib/picard && python -m pytest && ruff check . && ruff format --check .`
Expected: all tests pass (binary gate deselected), lint clean.

- [ ] **Step 4: Commit**

```bash
git add contrib/picard/tests/test_run_scan.py
git commit -m "test(picard): cover run_scan subprocess wrapper"
```

---

## Task 9: Path-matching gate against the real `musefs` binary (opt-in)

**Files:**
- Test: `contrib/picard/tests/test_path_gate.py`

This mirrors the beets §10.1 gate, adapted for Picard (paths are `str`, since Picard's `File.filename` is a `str`). It is deselected by default (`musefs_bin` marker) and shells out to a built `musefs` binary.

- [ ] **Step 1: Write the gate test** — `contrib/picard/tests/test_path_gate.py`

```python
"""§10.1 path-matching gate: assert the plugin's realpath key is byte-identical
to what the real `musefs scan` binary stores in `tracks.backing_path`."""

import sqlite3
import subprocess
import warnings
from pathlib import Path

import pytest

from musefs._core import connect, realpath_key, track_id_for_path

pytestmark = pytest.mark.musefs_bin

# tests/ -> picard/ -> contrib/ -> repo root
REPO_ROOT = Path(__file__).resolve().parents[3]
_debug = REPO_ROOT / "target" / "debug" / "musefs"
_release = REPO_ROOT / "target" / "release" / "musefs"
MUSEFS_BIN = _debug if _debug.exists() else _release

# A minimal valid FLAC: 'fLaC' + a STREAMINFO block (last-block flag, type 0,
# length 34) of 34 zero bytes. Enough for `musefs scan` to probe.
MINIMAL_FLAC = b"fLaC" + b"\x80\x00\x00\x22" + b"\x00" * 34


def _newest_rs_mtime():
    newest = 0.0
    for crate in ("musefs-db", "musefs-format", "musefs-core", "musefs-fuse", "musefs-cli"):
        src = REPO_ROOT / crate / "src"
        if src.exists():
            for rs in src.rglob("*.rs"):
                newest = max(newest, rs.stat().st_mtime)
    return newest


def _scan(tmp_path, tree):
    db = tmp_path / "musefs.db"
    result = subprocess.run(
        [str(MUSEFS_BIN), "scan", str(tree), "--db", str(db)],
        capture_output=True,
    )
    if result.returncode != 0:
        pytest.fail(
            f"musefs scan exited {result.returncode}\n"
            f"stdout: {result.stdout.decode(errors='replace')}\n"
            f"stderr: {result.stderr.decode(errors='replace')}"
        )
    return str(db)


def _stored_paths(db):
    conn = sqlite3.connect(db)
    try:
        return [r[0] for r in conn.execute("SELECT backing_path FROM tracks")]
    finally:
        conn.close()


@pytest.fixture(autouse=True)
def require_binary():
    if not MUSEFS_BIN.exists():
        pytest.skip(f"musefs binary not built at {MUSEFS_BIN}; run `cargo build`")
    if MUSEFS_BIN.stat().st_mtime < _newest_rs_mtime():
        warnings.warn(
            f"{MUSEFS_BIN} is older than the musefs Rust sources; rebuild with "
            f"`cargo build` before trusting a pass.",
            stacklevel=2,
        )


def _write_flac(path):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(MINIMAL_FLAC)


@pytest.mark.parametrize(
    "rel",
    [
        "Artist/Album/01 Track.flac",
        "Accénted/テスト/01.flac",  # accented + CJK
        "Spaced Out/cover %20 thing/02 song.flac",  # spaces and percent
    ],
)
def test_plain_paths_match(tmp_path, rel):
    tree = tmp_path / "music"
    _write_flac(tree / rel)
    db = _scan(tmp_path, tree)
    stored = _stored_paths(db)
    assert len(stored) == 1
    # Picard hands us file.filename as a str:
    key = realpath_key(str(tree / rel))
    assert key == stored[0]
    conn = connect(db)
    try:
        assert track_id_for_path(conn, key) is not None
    finally:
        conn.close()


def test_symlinked_directory_component(tmp_path):
    real_tree = tmp_path / "real_music"
    _write_flac(real_tree / "Artist/Album/01.flac")
    link_tree = tmp_path / "linked_music"
    link_tree.symlink_to(real_tree)
    db = _scan(tmp_path, link_tree)
    stored = _stored_paths(db)
    assert len(stored) == 1
    key = realpath_key(str(link_tree / "Artist/Album/01.flac"))
    assert key == stored[0]


def test_symlink_to_file(tmp_path):
    tree = tmp_path / "music"
    real = tree / "real.flac"
    _write_flac(real)
    link = tree / "link.flac"
    link.symlink_to(real)
    db = _scan(tmp_path, tree)
    stored = set(_stored_paths(db))
    assert len(stored) == 1
    assert realpath_key(str(link)) in stored


def test_relative_and_dotdot_input(tmp_path, monkeypatch):
    tree = tmp_path / "music"
    _write_flac(tree / "Artist/01.flac")
    db = _scan(tmp_path, tree)
    stored = _stored_paths(db)
    monkeypatch.chdir(tree)
    key = realpath_key("Artist/../Artist/01.flac")
    assert key == stored[0]


def test_trailing_slash_and_nonnormalised_input(tmp_path):
    tree = tmp_path / "music"
    _write_flac(tree / "Artist/01.flac")
    db = _scan(tmp_path, tree)
    stored = _stored_paths(db)
    key = realpath_key(str(tree) + "/Artist/./01.flac")
    assert key == stored[0]


def test_path_under_different_tree_is_skipped_not_mismatched(tmp_path):
    tree_a = tmp_path / "a"
    _write_flac(tree_a / "01.flac")
    db = _scan(tmp_path, tree_a)
    tree_b = tmp_path / "b"
    _write_flac(tree_b / "01.flac")
    key = realpath_key(str(tree_b / "01.flac"))
    conn = connect(db)
    try:
        assert track_id_for_path(conn, key) is None  # skipped, never a wrong hit
    finally:
        conn.close()
```

- [ ] **Step 2: Build the binary and run the gate**

Run: `cargo build` (from repo root), then `cd contrib/picard && python -m pytest -m musefs_bin -v`
Expected: PASS (8 passed). A mismatch here is a hard stop — the realpath/canonicalize keying is wrong and must be fixed in `_core.realpath_key`, not worked around.

- [ ] **Step 3: Confirm the gate is deselected by default**

Run: `cd contrib/picard && python -m pytest -v`
Expected: the `test_path_gate.py` tests are deselected (only the non-gate suite runs).

- [ ] **Step 4: Commit**

```bash
git add contrib/picard/tests/test_path_gate.py
git commit -m "test(picard): add opt-in path-matching gate vs the real musefs binary"
```

---

## Task 10: Picard glue (`__init__.py`) + README

**Files:**
- Modify (replace): `contrib/picard/musefs/__init__.py`
- Create: `contrib/picard/README.md`

This task wires Picard to the tested `_core`. Per spec §10.2 the glue is **not** unit-tested (GUI automation is out of scope); correctness is verified by the README's manual smoke test. Keep this file a thin adapter — all logic stays in `_core`.

> **Verification note for the implementer:** the exact import paths and the background-thread helper differ slightly across Picard 2.x point releases. Before finalizing, confirm against the installed Picard each of the following, and adjust if it differs — the delegation to `_core` does not change:
> - `BaseAction` and `register_*_action` live in `picard.ui.itemviews`; `OptionsPage`/`register_options_page` in `picard.ui.options`.
> - **`picard.util.thread.run_task`'s completion-callback contract** — the `_done` wiring below assumes the callback is invoked with `result=`/`error=` (the partial binds `n_files`, then expects those two). This is the single most fragile assumption; if the installed Picard calls it positionally as `next(result)` or `next(result, error)`, adjust `_done`'s signature accordingly.
> - **`from PyQt5 import QtWidgets`** — some newer Picard 2.x builds ship on PyQt6. Match the installed Qt binding.
> - **Pin `PLUGIN_API_VERSIONS`** to a real floor: identify the minimum Picard 2.x version that provides `file.metadata.images` + `image.is_front_image()` (used in §7) and set the list's lowest entry to it. The `"2.2"` below is a placeholder floor — replace it; do not ship it unverified.

- [ ] **Step 1: Replace `contrib/picard/musefs/__init__.py`** with the full plugin

```python
"""musefs Picard plugin: sync Picard metadata into the musefs SQLite store.

Right-click selected files/albums/clusters → "Sync to musefs". The plugin
runs `musefs scan` on each file (autoscan) to create/refresh its track row,
then writes Picard's tags + front cover into the store keyed by realpath. The
audio file is never saved by Picard, preserving musefs's no-rewrite invariant.

All logic lives in musefs._core (unit-tested); this module is a thin Picard
adapter, verified by the README's manual smoke test (spec §10.2).
"""

from __future__ import annotations

import os
from functools import partial

from musefs._core import (
    MusefsError,
    SyncStats,
    check_schema_version,
    connect,
    front_cover,
    map_fields,
    realpath_key,
    resolve_config,
    run_scan,
    sync_one,
)

PLUGIN_NAME = "musefs sync"
PLUGIN_AUTHOR = "musefs contributors"
PLUGIN_DESCRIPTION = (
    "Right-click a file/album → 'Sync to musefs' to push Picard's tags and "
    "front cover into a musefs SQLite store, without rewriting the audio file."
)
PLUGIN_VERSION = "0.1.0"
PLUGIN_API_VERSIONS = ["2.2", "2.6", "2.7", "2.8", "2.9", "2.10", "2.11", "2.12"]
PLUGIN_LICENSE = "MIT"
PLUGIN_LICENSE_URL = "https://opensource.org/licenses/MIT"

try:
    from picard import config, log
    from picard.ui.itemviews import (
        BaseAction,
        register_album_action,
        register_cluster_action,
        register_file_action,
        register_track_action,
    )
    from picard.ui.options import OptionsPage, register_options_page
    from picard.util import thread

    _PICARD = True
except ImportError:  # Running the unit tests without Picard installed.
    _PICARD = False


if _PICARD:

    # Option keys (also the names registered on the options page).
    OPT_DB = "musefs_db"
    OPT_BIN = "musefs_bin"
    OPT_AUTOSCAN = "musefs_autoscan"
    OPT_FIELDS = "musefs_fields"

    def _resolved_files(objs):
        """Resolve a selection (File/Track/Album/Cluster) to a dict of
        realpath-key -> File, de-duplicated. Picard items all implement
        iterfiles(); a File yields itself; a matched Track with no on-disk
        file yields nothing."""
        seen = {}
        for obj in objs:
            for f in obj.iterfiles():
                seen.setdefault(realpath_key(f.filename), f)
        return seen

    def _do_sync(opts, files):
        """Background-thread worker: autoscan each file, then write tags/art.
        Returns SyncStats. Raises MusefsError / SchemaMismatch on hard failure."""
        if not opts.db:
            raise MusefsError(
                "no musefs DB configured; set the DB path in Options → musefs sync"
            )
        if opts.autoscan:
            for f in files.values():
                run_scan(opts.bin, opts.db, f.filename)
        elif not os.path.exists(opts.db):
            raise MusefsError(
                f"musefs DB not found at {opts.db}; enable autoscan or run "
                f"`musefs scan` first"
            )

        conn = connect(opts.db)
        try:
            check_schema_version(conn)
            stats = SyncStats()
            for key, f in files.items():
                pairs = map_fields(f.metadata, opts.fields)
                art = front_cover(f.metadata)
                sync_one(conn, key, pairs, art, stats)
            conn.commit()
            return stats
        finally:
            conn.close()

    class MusefsSync(BaseAction):
        NAME = "Sync to musefs"

        def callback(self, objs):
            files = _resolved_files(objs)
            if not files:
                self._status("musefs: nothing to sync (no on-disk files selected)")
                return
            # Build a plain dict from Picard's config (subscriptable per
            # registered option) so resolve_config keeps its tested dict
            # contract rather than depending on config.setting's API.
            settings = {
                OPT_DB: config.setting[OPT_DB],
                OPT_BIN: config.setting[OPT_BIN],
                OPT_AUTOSCAN: config.setting[OPT_AUTOSCAN],
                OPT_FIELDS: config.setting[OPT_FIELDS],
            }
            opts = resolve_config(settings, os.environ)
            thread.run_task(
                partial(_do_sync, opts, files),
                partial(self._done, len(files)),
            )

        def _done(self, n_files, result=None, error=None):
            if error is not None:
                log.error("musefs: sync failed: %s", error)
                self._status(f"musefs: sync failed: {error}")
                return
            stats = result
            log.info("musefs: %s (files=%d)", stats.summary(), n_files)
            self._status(f"musefs: {stats.summary()}")

        @staticmethod
        def _status(message):
            # Logging is the reliable cross-version channel; see the status-bar
            # note below to additionally surface this on-screen.
            log.info("%s", message)

    class MusefsOptionsPage(OptionsPage):
        NAME = "musefs_sync"
        TITLE = "musefs sync"
        PARENT = "plugins"

        options = [
            config.TextOption("setting", OPT_DB, ""),
            config.TextOption("setting", OPT_BIN, "musefs"),
            config.BoolOption("setting", OPT_AUTOSCAN, True),
            config.TextOption("setting", OPT_FIELDS, ""),
        ]

        def __init__(self, parent=None):
            super().__init__(parent)
            from PyQt5 import QtWidgets

            layout = QtWidgets.QFormLayout(self)
            self._db = QtWidgets.QLineEdit(self)
            self._bin = QtWidgets.QLineEdit(self)
            self._autoscan = QtWidgets.QCheckBox("Run `musefs scan` before syncing", self)
            self._fields = QtWidgets.QLineEdit(self)
            self._fields.setPlaceholderText("extra map, e.g. comment=comment, mood=mood")
            layout.addRow("musefs DB path", self._db)
            layout.addRow("musefs binary", self._bin)
            layout.addRow("", self._autoscan)
            layout.addRow("Extra field map", self._fields)

        def load(self):
            self._db.setText(config.setting[OPT_DB])
            self._bin.setText(config.setting[OPT_BIN])
            self._autoscan.setChecked(config.setting[OPT_AUTOSCAN])
            self._fields.setText(config.setting[OPT_FIELDS])

        def save(self):
            config.setting[OPT_DB] = self._db.text().strip()
            config.setting[OPT_BIN] = self._bin.text().strip() or "musefs"
            config.setting[OPT_AUTOSCAN] = self._autoscan.isChecked()
            config.setting[OPT_FIELDS] = self._fields.text().strip()

    _action = MusefsSync()
    register_file_action(_action)
    register_track_action(_action)
    register_album_action(_action)
    register_cluster_action(_action)
    register_options_page(MusefsOptionsPage)
```

> **Note on `_status`:** reporting to Picard's status bar varies by version (often `self.tagger.window.set_statusbar_message(...)`). The version above logs via `picard.log` only; during the manual smoke test, optionally extend `_status` to call the installed Picard's status-bar API for on-screen feedback. Logging is the reliable cross-version channel.

- [ ] **Step 2: Re-verify the package still imports without Picard** (the guard must keep the test env clean)

Run: `cd contrib/picard && python -c "import musefs; print(musefs.PLUGIN_NAME)" && python -m pytest`
Expected: prints `musefs sync`; the full default test suite still passes (the `_PICARD` guard skips all Qt/Picard code).

- [ ] **Step 3: Create `contrib/picard/README.md`**

````markdown
# musefs-picard

A [MusicBrainz Picard](https://picard.musicbrainz.org/) plugin that syncs your
Picard metadata (tags + front cover) into a [musefs](../../README.md) SQLite
store, so a live musefs mount shows a re-tagged view of your library **without
rewriting any audio**.

## How it fits together

Picard has no way to redirect its Save to a database, so this plugin adds a
**context-menu action** instead: match/edit as usual, then right-click your
selection → **"Sync to musefs"** *instead of* pressing Save. The plugin:

1. runs `musefs scan` on each selected file to create/refresh its track row and
   structural columns (the offsets only musefs can compute), then
2. writes Picard's tags and front cover into the store, keyed by the file's
   canonical real path.

musefs's auto-refresh surfaces the change at the mount with no remount. The
audio file is never saved by Picard.

## Install (local / development)

Picard loads "folder plugins" from its plugins directory. Copy (or symlink) the
`musefs/` folder there:

- Linux: `~/.config/MusicBrainz/Picard/plugins/`
- macOS: `~/Library/Preferences/MusicBrainz/Picard/plugins/`
- Windows: `%APPDATA%\MusicBrainz\Picard\plugins\`

```bash
cp -r contrib/picard/musefs ~/.config/MusicBrainz/Picard/plugins/
```

Then enable **musefs sync** in Options → Plugins, and configure it in
Options → musefs sync:

- **musefs DB path** — path to the musefs SQLite store (required).
- **musefs binary** — the `musefs` executable (PATH name or full path), used to
  auto-create rows. Default `musefs`.
- **Run `musefs scan` before syncing** — autoscan toggle (default on). With it
  off, run `musefs scan` yourself first or the sync errors on a missing DB.
- **Extra field map** — optional `key=value` list mapping extra Picard tag names
  to musefs keys, e.g. `comment=comment`.

`MUSEFS_DB` and `MUSEFS_BIN` environment variables override the DB/binary
settings (handy for testing).

## Workflow

1. `musefs mount ~/mnt --db ~/musefs.db --template '$albumartist/$album/$tracknumber - $title'`
2. In Picard, match/cluster an album as usual.
3. Right-click the album/files → **Sync to musefs**.
4. Browse `~/mnt` — the files show Picard's tags and cover, audio byte-identical.

## Notes

- **Front cover only:** the first front-cover image Picard holds is synced.
  Picard art wins when present; otherwise any art `musefs scan` ingested from
  the file's embedded picture is preserved. Re-syncing a file with no Picard
  art lets the embedded picture re-seed (musefs scan re-reads the file).
- **Tags are fully replaced** with Picard's view on every sync.
- **Orphaned art:** replacing art can orphan old blobs; `musefs scan --revalidate`
  garbage-collects them.
- **Schema version:** the plugin refuses to run if the DB's `user_version`
  differs from the version it targets — rebuild the store after upgrading musefs.

## Tests

```bash
cd contrib/picard
python -m venv .venv && source .venv/bin/activate
pip install -r requirements.txt

python -m pytest                 # unit + integration (no Picard, no Rust binary)
python -m pytest -m musefs_bin   # path-matching gate vs the real `musefs` binary
```

The `musefs_bin` gate shells out to the real `musefs` binary, so build it first
from the repo root (`cargo build`). It is deselected from the default run and
skips cleanly if the binary is absent.

### Manual smoke test (the GUI path is not unit-tested)

1. `cargo build` and create a store: `musefs scan /path/to/album --db /tmp/m.db`.
2. Copy the plugin into Picard's plugins dir; enable it; set DB path `/tmp/m.db`.
3. Load the album in Picard, change a tag (e.g. title), add a front cover.
4. Right-click → **Sync to musefs**; confirm the status bar / log reports
   `synced=N`.
5. `musefs mount /tmp/mnt --db /tmp/m.db` and verify the mounted file carries the
   new tag and cover, with byte-identical audio.
````

- [ ] **Step 4: Lint**

Run: `cd contrib/picard && ruff check . && ruff format --check .`
Expected: clean. (The `__init__.py` Picard-only block is import-guarded; ruff parses it fine.)

- [ ] **Step 5: Commit**

```bash
git add contrib/picard/musefs/__init__.py contrib/picard/README.md
git commit -m "feat(picard): add Picard action + options page glue and README"
```

---

## Task 11: Update the roadmap

**Files:**
- Modify: `docs/ROADMAP.md`

- [ ] **Step 1: Move the picard plugin from "deferred" to "delivered."** In `docs/ROADMAP.md`, find the deferred bullet under **Distribution / integration**:

```markdown
- **picard plugin as a shipped artifact** — the SQLite *contract* is a target
  for picard too, but only the **beets** plugin ships today (see "Delivered
  since v0.1.0"). A picard plugin is not yet built.
```

Replace it with:

```markdown
- **picard plugin** — delivered (see "Delivered since v0.1.0"). Both the beets
  and picard plugins now write the SQLite contract.
```

- [ ] **Step 2: Add a delivered entry.** In the **Delivered since v0.1.0** section, immediately after the `beets plugin` bullet, add:

```markdown
- **picard plugin** (`contrib/picard/`) — a MusicBrainz Picard plugin whose
  "Sync to musefs" context-menu action pushes Picard's in-memory tags and front
  cover into the SQLite store, keyed by each file's canonical path, without ever
  saving (rewriting) the audio file. Picard has no pre-save hook, so the action
  is the no-rewrite analog of `beet musefs`: it autoscans each selected file via
  the `musefs` binary to create its row, then writes tags/art the live mount
  surfaces with no remount. Shares the DB-contract logic and the realpath
  path-matching gate with the beets plugin; the sync core is unit/integration
  tested without a Picard install, with the GUI path covered by a documented
  manual smoke test.
```

- [ ] **Step 3: Verify the workspace still builds/tests (docs-only change, but confirm nothing references the old wording).**

Run: `cd contrib/picard && python -m pytest`
Expected: full default suite green.

- [ ] **Step 4: Commit**

```bash
git add docs/ROADMAP.md
git commit -m "docs: mark the picard plugin delivered in the roadmap"
```

---

## Final verification

- [ ] **Run the full default test suite + lint**

Run: `cd contrib/picard && python -m pytest && ruff check . && ruff format --check .`
Expected: all non-gate tests pass; lint clean.

- [ ] **Run the opt-in path gate against a fresh binary**

Run: `cargo build && cd contrib/picard && python -m pytest -m musefs_bin -v`
Expected: 8 passed (the realpath key is byte-identical to what `musefs scan` stores).

- [ ] **Confirm the package imports cleanly without Picard** (the test-env invariant)

Run: `cd contrib/picard && python -c "import musefs; print(musefs.PLUGIN_VERSION)"`
Expected: `0.1.0`, no ImportError.
