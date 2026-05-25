# beets-musefs Plugin Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A beets plugin that syncs beets' canonical tags and cover art into the musefs SQLite store (keyed by realpath), so a live musefs mount re-synthesizes FLAC/MP3 headers from beets without remounting or rewriting audio.

**Architecture:** Two modules under `contrib/beets/beetsplug/`. `_core.py` holds all pure logic (field mapping, path keying, SQLite reads/writes, the `sync_items` orchestrator) and imports **no beets** — so it is unit-testable in isolation. `musefs.py` is the thin `BeetsPlugin` glue: config, the `beet musefs` subcommand, and event listeners, all delegating to `_core`. The plugin writes directly to the musefs DB; triggers bump `content_version`/`data_version`, and the mount picks changes up on its own.

**Tech Stack:** Python 3.9+, `sqlite3` (stdlib), `hashlib` (stdlib), beets (runtime + glue tests only), pytest (tests). The path-matching gate shells out to the real `musefs` (Rust) binary.

**Spec:** `docs/superpowers/specs/2026-05-25-beets-musefs-plugin-design.md`

**Note on the command name:** The spec writes `beet musefs sync`. beets has no native nested subcommands, so this is realized as a single `beet musefs [QUERY]` subcommand that also accepts an optional leading `sync` verb (`beet musefs sync [QUERY]`), stripped by the handler. Both spellings work.

---

## File Structure

- `contrib/beets/pyproject.toml` — package metadata, deps, pytest config/markers.
- `contrib/beets/beetsplug/__init__.py` — pkgutil namespace-package stub.
- `contrib/beets/beetsplug/_core.py` — pure logic (no beets import).
- `contrib/beets/beetsplug/musefs.py` — `MusefsPlugin` (beets glue).
- `contrib/beets/README.md` — install, config, scan→sync→mount workflow.
- `contrib/beets/tests/conftest.py` — fixtures: temp musefs DB, `FakeItem`/`FakeAlbum`.
- `contrib/beets/tests/schema_v1.sql` — embedded copy of musefs schema V1 DDL.
- `contrib/beets/tests/test_map_fields.py`
- `contrib/beets/tests/test_paths.py`
- `contrib/beets/tests/test_db.py`
- `contrib/beets/tests/test_art.py`
- `contrib/beets/tests/test_sync.py`
- `contrib/beets/tests/test_plugin.py` — imports beets.
- `contrib/beets/tests/test_path_gate.py` — opt-in, marker `musefs_bin`, shells to `musefs`.

All commands below are run from `contrib/beets/` unless stated otherwise.

---

## Task 1: Project scaffold and packaging

**Files:**
- Create: `contrib/beets/pyproject.toml`
- Create: `contrib/beets/beetsplug/__init__.py`
- Create: `contrib/beets/beetsplug/_core.py`
- Create: `contrib/beets/tests/test_smoke.py`

- [ ] **Step 1: Write the failing test**

Create `contrib/beets/tests/test_smoke.py`:

```python
def test_core_imports_without_beets():
    import beetsplug._core as core

    assert hasattr(core, "EXPECTED_USER_VERSION")
    assert core.EXPECTED_USER_VERSION == 1
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd contrib/beets && python -m pytest tests/test_smoke.py -v`
Expected: FAIL — `ModuleNotFoundError: No module named 'beetsplug'`.

- [ ] **Step 3: Create the package files**

Create `contrib/beets/beetsplug/__init__.py`:

```python
# beetsplug is a namespace package shared by all beets plugins.
from pkgutil import extend_path

__path__ = extend_path(__path__, __name__)
```

Create `contrib/beets/beetsplug/_core.py`:

```python
"""Pure logic for the musefs beets plugin: no beets imports live here.

Everything beets-specific (the BeetsPlugin subclass, commands, event
listeners) is in ``musefs.py``; this module is unit-testable on its own.
"""

# Schema version this plugin was written against (musefs schema.rs MIGRATIONS
# length). The plugin refuses to run against any other version.
EXPECTED_USER_VERSION = 1

# Mirror of musefs-core scan.rs MAX_ART_BYTES: 16 MiB minus 64 KiB headroom.
MAX_ART_BYTES = 16 * 1024 * 1024 - 64 * 1024
```

Create `contrib/beets/pyproject.toml`:

```toml
[build-system]
requires = ["setuptools>=61"]
build-backend = "setuptools.build_meta"

[project]
name = "beets-musefs"
version = "0.1.0"
description = "Sync beets metadata into the musefs SQLite store"
requires-python = ">=3.9"
dependencies = ["beets>=1.6"]

[project.optional-dependencies]
test = ["pytest>=7"]

[tool.setuptools]
packages = ["beetsplug"]

[tool.pytest.ini_options]
testpaths = ["tests"]
markers = [
    "musefs_bin: tests that shell out to the real `musefs` Rust binary (opt-in)",
]
# By default, skip the binary gate; run it explicitly with `-m musefs_bin`.
addopts = "-m 'not musefs_bin'"
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd contrib/beets && python -m pytest tests/test_smoke.py -v`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add contrib/beets/pyproject.toml contrib/beets/beetsplug/__init__.py contrib/beets/beetsplug/_core.py contrib/beets/tests/test_smoke.py
git commit -m "feat(beets): scaffold beets-musefs plugin package"
```

---

## Task 2: Field mapping (`map_fields`)

**Files:**
- Modify: `contrib/beets/beetsplug/_core.py`
- Test: `contrib/beets/tests/test_map_fields.py`

- [ ] **Step 1: Write the failing tests**

Create `contrib/beets/tests/test_map_fields.py`:

```python
from types import SimpleNamespace

from beetsplug._core import map_fields


def item(**kw):
    base = dict(
        title="", artist="", albumartist="", album="", genre="", composer="",
        track=0, disc=0, year=0, month=0, day=0,
    )
    base.update(kw)
    return SimpleNamespace(**base)


def test_direct_fields_copied():
    pairs = map_fields(item(title="Song", artist="Band", album="Disc"))
    d = dict(pairs)
    assert d["title"] == "Song"
    assert d["artist"] == "Band"
    assert d["album"] == "Disc"


def test_track_and_disc_renamed():
    pairs = dict(map_fields(item(track=7, disc=2)))
    assert pairs["tracknumber"] == "7"
    assert pairs["discnumber"] == "2"


def test_year_only_date():
    assert dict(map_fields(item(year=1999)))["date"] == "1999"


def test_full_date_when_month_and_day():
    assert dict(map_fields(item(year=1999, month=3, day=5)))["date"] == "1999-03-05"


def test_partial_date_falls_back_to_year():
    # month without day -> year only (we only emit a full date when both set)
    assert dict(map_fields(item(year=1999, month=3)))["date"] == "1999"


def test_empty_and_zero_omitted():
    pairs = dict(map_fields(item()))
    assert pairs == {}


def test_whitespace_only_omitted():
    assert "title" not in dict(map_fields(item(title="   ")))


def test_extra_field_override():
    it = item(title="Song")
    it.comments = "hi there"
    pairs = dict(map_fields(it, extra_fields={"comments": "comment"}))
    assert pairs["comment"] == "hi there"
    assert pairs["title"] == "Song"
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd contrib/beets && python -m pytest tests/test_map_fields.py -v`
Expected: FAIL — `ImportError: cannot import name 'map_fields'`.

- [ ] **Step 3: Implement `map_fields`**

Append to `contrib/beets/beetsplug/_core.py`:

```python
# beets field name -> musefs (Vorbis-lowercase) tag key, for direct copies.
DIRECT_FIELDS = {
    "title": "title",
    "artist": "artist",
    "albumartist": "albumartist",
    "album": "album",
    "genre": "genre",
    "composer": "composer",
}


def _format_date(item):
    year = int(getattr(item, "year", 0) or 0)
    if not year:
        return None
    month = int(getattr(item, "month", 0) or 0)
    day = int(getattr(item, "day", 0) or 0)
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
        value = getattr(item, beets_field, None)
        if value is None:
            continue
        text = str(value).strip()
        if text:
            pairs.append((key, text))

    track = int(getattr(item, "track", 0) or 0)
    if track:
        pairs.append(("tracknumber", str(track)))
    disc = int(getattr(item, "disc", 0) or 0)
    if disc:
        pairs.append(("discnumber", str(disc)))
    date = _format_date(item)
    if date:
        pairs.append(("date", date))

    return pairs
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd contrib/beets && python -m pytest tests/test_map_fields.py -v`
Expected: PASS (8 passed).

- [ ] **Step 5: Commit**

```bash
git add contrib/beets/beetsplug/_core.py contrib/beets/tests/test_map_fields.py
git commit -m "feat(beets): map beets fields to musefs tag keys"
```

---

## Task 3: Path keying (`realpath_key`)

**Files:**
- Modify: `contrib/beets/beetsplug/_core.py`
- Test: `contrib/beets/tests/test_paths.py`

- [ ] **Step 1: Write the failing tests**

Create `contrib/beets/tests/test_paths.py`:

```python
import os

from beetsplug._core import realpath_key


def test_str_path_absolutised(tmp_path):
    f = tmp_path / "a.flac"
    f.write_bytes(b"x")
    assert realpath_key(str(f)) == os.path.realpath(str(f))


def test_bytes_path_returns_str(tmp_path):
    f = tmp_path / "a.flac"
    f.write_bytes(b"x")
    key = realpath_key(os.fsencode(str(f)))
    assert isinstance(key, str)
    assert key == os.path.realpath(str(f))


def test_relative_and_dotdot_resolved(tmp_path, monkeypatch):
    (tmp_path / "sub").mkdir()
    f = tmp_path / "sub" / "a.flac"
    f.write_bytes(b"x")
    monkeypatch.chdir(tmp_path)
    assert realpath_key("sub/../sub/a.flac") == os.path.realpath(str(f))


def test_symlink_resolved(tmp_path):
    real = tmp_path / "real.flac"
    real.write_bytes(b"x")
    link = tmp_path / "link.flac"
    link.symlink_to(real)
    assert realpath_key(str(link)) == os.path.realpath(str(real))
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd contrib/beets && python -m pytest tests/test_paths.py -v`
Expected: FAIL — `ImportError: cannot import name 'realpath_key'`.

- [ ] **Step 3: Implement `realpath_key`**

Append to `contrib/beets/beetsplug/_core.py`:

```python
import os


def realpath_key(path):
    """Canonical absolute path string matching musefs scan's stored
    ``backing_path`` (``std::fs::canonicalize`` + ``to_string_lossy``).

    Accepts ``str`` or ``bytes`` (beets stores ``item.path`` as bytes) and
    always returns ``str`` via the filesystem encoding.
    """
    real = os.path.realpath(path)
    if isinstance(real, bytes):
        return os.fsdecode(real)
    return real
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd contrib/beets && python -m pytest tests/test_paths.py -v`
Expected: PASS (4 passed).

- [ ] **Step 5: Commit**

```bash
git add contrib/beets/beetsplug/_core.py contrib/beets/tests/test_paths.py
git commit -m "feat(beets): canonical realpath keying for track lookup"
```

---

## Task 4: Test schema fixture, DB connect, and version guard

**Files:**
- Create: `contrib/beets/tests/schema_v1.sql`
- Create: `contrib/beets/tests/conftest.py`
- Modify: `contrib/beets/beetsplug/_core.py`
- Test: `contrib/beets/tests/test_db.py`

- [ ] **Step 1: Create the embedded schema DDL**

Create `contrib/beets/tests/schema_v1.sql` (verbatim copy of musefs `MIGRATION_V1` plus the version stamp `migrate` would set):

```sql
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

- [ ] **Step 2: Create the shared fixtures**

Create `contrib/beets/tests/conftest.py`:

```python
import os
import sqlite3
import time
from pathlib import Path

import pytest

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


class FakeAlbum:
    def __init__(self, artpath=None, items=()):
        self.artpath = artpath  # bytes path to cover file, or None
        self._items = list(items)

    def items(self):
        return self._items


class FakeItem:
    """Minimal stand-in for a beets Item: attribute reads + get_album()."""

    def __init__(self, path, album=None, **fields):
        self.path = path  # bytes, like beets
        self._album = album
        for k in ("title", "artist", "albumartist", "album", "genre", "composer"):
            setattr(self, k, fields.pop(k, ""))
        self.track = fields.pop("track", 0)
        self.disc = fields.pop("disc", 0)
        self.year = fields.pop("year", 0)
        self.month = fields.pop("month", 0)
        self.day = fields.pop("day", 0)
        for k, v in fields.items():
            setattr(self, k, v)

    def get_album(self):
        return self._album


@pytest.fixture
def fake_item():
    return FakeItem


@pytest.fixture
def fake_album():
    return FakeAlbum


@pytest.fixture
def make_track(db_path):
    """Return a helper that inserts a track row and returns its id."""
    def _make(backing_path, fmt="flac"):
        conn = sqlite3.connect(db_path)
        try:
            tid = insert_track(conn, backing_path, fmt)
            conn.commit()
            return tid
        finally:
            conn.close()
    return _make
```

- [ ] **Step 3: Write the failing tests**

Create `contrib/beets/tests/test_db.py`:

```python
import sqlite3

import pytest

from beetsplug._core import SchemaMismatch, check_schema_version, connect


def test_connect_and_version_ok(db_path):
    conn = connect(db_path)
    try:
        check_schema_version(conn)  # must not raise
    finally:
        conn.close()


def test_version_mismatch_raises(db_path):
    conn = sqlite3.connect(db_path)
    conn.execute("PRAGMA user_version = 2")
    conn.commit()
    conn.close()

    conn = connect(db_path)
    try:
        with pytest.raises(SchemaMismatch):
            check_schema_version(conn)
    finally:
        conn.close()
```

- [ ] **Step 4: Run tests to verify they fail**

Run: `cd contrib/beets && python -m pytest tests/test_db.py -v`
Expected: FAIL — `ImportError: cannot import name 'connect'`.

- [ ] **Step 5: Implement connect + version guard**

Append to `contrib/beets/beetsplug/_core.py`:

```python
import sqlite3


class SchemaMismatch(Exception):
    """Raised when the musefs DB schema version differs from what the plugin
    targets (``EXPECTED_USER_VERSION``)."""

    def __init__(self, found):
        self.found = found
        super().__init__(
            f"musefs DB user_version is {found}, plugin targets "
            f"{EXPECTED_USER_VERSION}; the musefs and plugin versions have "
            f"diverged."
        )


def connect(db_path):
    """Open the musefs DB with a busy timeout and foreign keys enabled."""
    conn = sqlite3.connect(db_path, timeout=5.0)
    conn.execute("PRAGMA busy_timeout = 5000")
    conn.execute("PRAGMA foreign_keys = ON")
    return conn


def check_schema_version(conn):
    found = conn.execute("PRAGMA user_version").fetchone()[0]
    if found != EXPECTED_USER_VERSION:
        raise SchemaMismatch(found)
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cd contrib/beets && python -m pytest tests/test_db.py -v`
Expected: PASS (2 passed).

- [ ] **Step 7: Commit**

```bash
git add contrib/beets/tests/schema_v1.sql contrib/beets/tests/conftest.py contrib/beets/beetsplug/_core.py contrib/beets/tests/test_db.py
git commit -m "feat(beets): DB connect with schema-version guard + test fixtures"
```

---

## Task 5: Track lookup and tag replacement

**Files:**
- Modify: `contrib/beets/beetsplug/_core.py`
- Test: `contrib/beets/tests/test_db.py`

- [ ] **Step 1: Write the failing tests**

Append to `contrib/beets/tests/test_db.py`:

```python
from beetsplug._core import replace_tags, track_id_for_path


def test_track_id_lookup(db_path, make_track):
    tid = make_track("/music/a.flac")
    conn = connect(db_path)
    try:
        assert track_id_for_path(conn, "/music/a.flac") == tid
        assert track_id_for_path(conn, "/music/missing.flac") is None
    finally:
        conn.close()


def test_replace_tags_writes_rows_and_bumps_version(db_path, make_track):
    tid = make_track("/music/a.flac")
    conn = connect(db_path)
    try:
        before = conn.execute(
            "SELECT content_version FROM tracks WHERE id=?", (tid,)
        ).fetchone()[0]
        replace_tags(conn, tid, [("title", "Song"), ("artist", "Band")])
        conn.commit()
        rows = conn.execute(
            "SELECT key, value, ordinal FROM tags WHERE track_id=? ORDER BY key", (tid,)
        ).fetchall()
        assert rows == [("artist", "Band", 0), ("title", "Song", 0)]
        after = conn.execute(
            "SELECT content_version FROM tracks WHERE id=?", (tid,)
        ).fetchone()[0]
        assert after > before
    finally:
        conn.close()


def test_replace_tags_clears_previous(db_path, make_track):
    tid = make_track("/music/a.flac")
    conn = connect(db_path)
    try:
        replace_tags(conn, tid, [("title", "Old")])
        replace_tags(conn, tid, [("title", "New")])
        conn.commit()
        vals = conn.execute(
            "SELECT value FROM tags WHERE track_id=? AND key='title'", (tid,)
        ).fetchall()
        assert vals == [("New",)]
    finally:
        conn.close()


def test_replace_tags_duplicate_keys_get_distinct_ordinals(db_path, make_track):
    tid = make_track("/music/a.flac")
    conn = connect(db_path)
    try:
        replace_tags(conn, tid, [("genre", "Rock"), ("genre", "Indie")])
        conn.commit()
        rows = conn.execute(
            "SELECT value, ordinal FROM tags WHERE track_id=? AND key='genre' "
            "ORDER BY ordinal", (tid,)
        ).fetchall()
        assert rows == [("Rock", 0), ("Indie", 1)]
    finally:
        conn.close()
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd contrib/beets && python -m pytest tests/test_db.py -v`
Expected: FAIL — `ImportError: cannot import name 'replace_tags'`.

- [ ] **Step 3: Implement lookup + tag replacement**

Append to `contrib/beets/beetsplug/_core.py`:

```python
def track_id_for_path(conn, key):
    """Return the track id whose backing_path equals ``key``, or None."""
    row = conn.execute(
        "SELECT id FROM tracks WHERE backing_path = ?", (key,)
    ).fetchone()
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
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd contrib/beets && python -m pytest tests/test_db.py -v`
Expected: PASS (6 passed).

- [ ] **Step 5: Commit**

```bash
git add contrib/beets/beetsplug/_core.py contrib/beets/tests/test_db.py
git commit -m "feat(beets): track lookup and tag replacement"
```

---

## Task 6: Art helpers (mime sniff, dedup upsert, link)

**Files:**
- Modify: `contrib/beets/beetsplug/_core.py`
- Test: `contrib/beets/tests/test_art.py`

- [ ] **Step 1: Write the failing tests**

Create `contrib/beets/tests/test_art.py`:

```python
import hashlib

from beetsplug._core import (
    connect,
    replace_track_art,
    sniff_mime,
    upsert_art,
)

JPEG = b"\xff\xd8\xff\xe0" + b"\x00" * 16
PNG = b"\x89PNG\r\n\x1a\n" + b"\x00" * 16


def test_sniff_mime_magic_bytes():
    assert sniff_mime(JPEG, "/x/cover.bin") == "image/jpeg"
    assert sniff_mime(PNG, "/x/cover.bin") == "image/png"


def test_sniff_mime_extension_fallback():
    assert sniff_mime(b"garbage", "/x/cover.jpg") == "image/jpeg"
    assert sniff_mime(b"garbage", "/x/cover.png") == "image/png"
    assert sniff_mime(b"garbage", "/x/cover.bin") == "application/octet-stream"


def test_upsert_art_dedup(db_path):
    conn = connect(db_path)
    try:
        a = upsert_art(conn, JPEG, "image/jpeg")
        b = upsert_art(conn, JPEG, "image/jpeg")
        conn.commit()
        assert a == b
        count = conn.execute("SELECT COUNT(*) FROM art").fetchone()[0]
        assert count == 1
        sha = conn.execute("SELECT sha256 FROM art WHERE id=?", (a,)).fetchone()[0]
        assert sha == hashlib.sha256(JPEG).hexdigest()
    finally:
        conn.close()


def test_replace_track_art_links_front_cover(db_path, make_track):
    tid = make_track("/music/a.flac")
    conn = connect(db_path)
    try:
        art_id = upsert_art(conn, JPEG, "image/jpeg")
        before = conn.execute(
            "SELECT content_version FROM tracks WHERE id=?", (tid,)
        ).fetchone()[0]
        replace_track_art(conn, tid, art_id)
        conn.commit()
        row = conn.execute(
            "SELECT art_id, picture_type, description, ordinal FROM track_art "
            "WHERE track_id=?", (tid,)
        ).fetchone()
        assert row == (art_id, 3, "", 0)
        after = conn.execute(
            "SELECT content_version FROM tracks WHERE id=?", (tid,)
        ).fetchone()[0]
        assert after > before
    finally:
        conn.close()
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd contrib/beets && python -m pytest tests/test_art.py -v`
Expected: FAIL — `ImportError: cannot import name 'sniff_mime'`.

- [ ] **Step 3: Implement the art helpers**

Append to `contrib/beets/beetsplug/_core.py`:

```python
import hashlib

_EXT_MIME = {
    ".jpg": "image/jpeg",
    ".jpeg": "image/jpeg",
    ".png": "image/png",
}


def sniff_mime(data, path):
    """Detect image mime from magic bytes, falling back to file extension."""
    if data[:3] == b"\xff\xd8\xff":
        return "image/jpeg"
    if data[:8] == b"\x89PNG\r\n\x1a\n":
        return "image/png"
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
    return conn.execute(
        "SELECT id FROM art WHERE sha256 = ?", (sha,)
    ).fetchone()[0]


def replace_track_art(conn, track_id, art_id):
    """Set the track's single front-cover art (picture_type 3, ordinal 0)."""
    conn.execute("DELETE FROM track_art WHERE track_id = ?", (track_id,))
    conn.execute(
        "INSERT INTO track_art (track_id, art_id, picture_type, description, "
        "ordinal) VALUES (?, ?, 3, '', 0)",
        (track_id, art_id),
    )
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd contrib/beets && python -m pytest tests/test_art.py -v`
Expected: PASS (4 passed).

- [ ] **Step 5: Commit**

```bash
git add contrib/beets/beetsplug/_core.py contrib/beets/tests/test_art.py
git commit -m "feat(beets): content-addressed art upsert and front-cover link"
```

---

## Task 7: Sync orchestrator (`sync_items` + `SyncStats`)

**Files:**
- Modify: `contrib/beets/beetsplug/_core.py`
- Test: `contrib/beets/tests/test_sync.py`

- [ ] **Step 1: Write the failing tests**

Create `contrib/beets/tests/test_sync.py`:

```python
import os

from beetsplug._core import connect, sync_items

JPEG = b"\xff\xd8\xff\xe0" + b"\x00" * 32


def write_cover(tmp_path, name, data=JPEG):
    p = tmp_path / name
    p.write_bytes(data)
    return os.fsencode(str(p))


def test_skip_when_no_row(db_path, fake_item):
    conn = connect(db_path)
    try:
        item = fake_item(os.fsencode("/music/missing.flac"), title="X")
        stats = sync_items(conn, [item])
        conn.commit()
        assert stats.synced == 0
        assert stats.skipped == 1
    finally:
        conn.close()


def test_tags_written_for_existing_row(db_path, make_track, fake_item):
    tid = make_track("/music/a.flac")
    conn = connect(db_path)
    try:
        item = fake_item(os.fsencode("/music/a.flac"), title="Song", artist="Band")
        stats = sync_items(conn, [item])
        conn.commit()
        assert stats.synced == 1
        title = conn.execute(
            "SELECT value FROM tags WHERE track_id=? AND key='title'", (tid,)
        ).fetchone()[0]
        assert title == "Song"
    finally:
        conn.close()


def test_art_linked_when_album_has_cover(tmp_path, db_path, make_track, fake_item, fake_album):
    tid = make_track("/music/a.flac")
    cover = write_cover(tmp_path, "cover.jpg")
    conn = connect(db_path)
    try:
        album = fake_album(artpath=cover)
        item = fake_item(os.fsencode("/music/a.flac"), album=album, title="Song")
        stats = sync_items(conn, [item])
        conn.commit()
        assert stats.art_linked == 1
        assert conn.execute(
            "SELECT COUNT(*) FROM track_art WHERE track_id=?", (tid,)
        ).fetchone()[0] == 1
    finally:
        conn.close()


def test_existing_embedded_art_preserved_when_no_beets_art(db_path, make_track, fake_item):
    # Simulate scan-ingested art already linked to the track.
    tid = make_track("/music/a.flac")
    conn = connect(db_path)
    try:
        conn.execute(
            "INSERT INTO art (sha256, mime, byte_len, data) VALUES "
            "('deadbeef', 'image/jpeg', 3, X'aabbcc')"
        )
        art_id = conn.execute("SELECT id FROM art WHERE sha256='deadbeef'").fetchone()[0]
        conn.execute(
            "INSERT INTO track_art (track_id, art_id) VALUES (?, ?)", (tid, art_id)
        )
        conn.commit()
        # beets item with no album art:
        item = fake_item(os.fsencode("/music/a.flac"), album=None, title="Song")
        stats = sync_items(conn, [item])
        conn.commit()
        assert stats.art_linked == 0
        # The pre-existing track_art row is untouched.
        row = conn.execute(
            "SELECT art_id FROM track_art WHERE track_id=?", (tid,)
        ).fetchone()
        assert row == (art_id,)
    finally:
        conn.close()


def test_oversized_art_skipped(tmp_path, db_path, make_track, fake_item, fake_album, monkeypatch):
    import beetsplug._core as core

    monkeypatch.setattr(core, "MAX_ART_BYTES", 8)
    tid = make_track("/music/a.flac")
    cover = write_cover(tmp_path, "big.jpg", data=b"X" * 64)
    conn = connect(db_path)
    try:
        album = fake_album(artpath=cover)
        item = fake_item(os.fsencode("/music/a.flac"), album=album, title="Song")
        stats = sync_items(conn, [item])
        conn.commit()
        assert stats.skipped_art == 1
        assert stats.art_linked == 0
    finally:
        conn.close()


def test_art_deduped_across_items(tmp_path, db_path, make_track, fake_item, fake_album):
    t1 = make_track("/music/a.flac")
    t2 = make_track("/music/b.flac")
    cover = write_cover(tmp_path, "cover.jpg")
    album = fake_album(artpath=cover)
    conn = connect(db_path)
    try:
        items = [
            fake_item(os.fsencode("/music/a.flac"), album=album, title="A"),
            fake_item(os.fsencode("/music/b.flac"), album=album, title="B"),
        ]
        sync_items(conn, items)
        conn.commit()
        assert conn.execute("SELECT COUNT(*) FROM art").fetchone()[0] == 1
        assert conn.execute("SELECT COUNT(*) FROM track_art").fetchone()[0] == 2
    finally:
        conn.close()


def test_dry_run_writes_nothing(db_path, make_track, fake_item):
    tid = make_track("/music/a.flac")
    conn = connect(db_path)
    try:
        item = fake_item(os.fsencode("/music/a.flac"), title="Song")
        stats = sync_items(conn, [item], dry_run=True)
        conn.rollback()
        assert stats.synced == 1
        assert conn.execute(
            "SELECT COUNT(*) FROM tags WHERE track_id=?", (tid,)
        ).fetchone()[0] == 0
    finally:
        conn.close()
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd contrib/beets && python -m pytest tests/test_sync.py -v`
Expected: FAIL — `ImportError: cannot import name 'sync_items'`.

- [ ] **Step 3: Implement `SyncStats` and `sync_items`**

Append to `contrib/beets/beetsplug/_core.py`:

```python
from dataclasses import dataclass

# Sentinel returned by _prepare_art under dry_run: "would link, but not written".
_WOULD_LINK = object()


@dataclass
class SyncStats:
    synced: int = 0
    skipped: int = 0       # item path had no matching track row
    art_linked: int = 0
    skipped_art: int = 0   # art file oversized / unreadable

    def summary(self):
        return (
            f"synced={self.synced} skipped={self.skipped} "
            f"art_linked={self.art_linked} skipped_art={self.skipped_art}"
        )


def _album_art_path(item):
    """Return the album cover path (bytes/str) for an item, or None."""
    get_album = getattr(item, "get_album", None)
    album = get_album() if get_album else None
    if album is None:
        return None
    artpath = getattr(album, "artpath", None)
    return artpath or None


def _prepare_art(conn, artpath, cache, stats, dry_run):
    """Upsert the cover at ``artpath`` and return its art id (cached per run).
    Returns None if unreadable, or under dry_run a non-None sentinel when the
    art would be linked."""
    real = realpath_key(artpath)
    if real in cache:
        return cache[real]

    try:
        with open(os.fsencode(real), "rb") as fh:
            data = fh.read()
    except OSError:
        cache[real] = None
        return None

    if len(data) > MAX_ART_BYTES:
        stats.skipped_art += 1
        cache[real] = None
        return None

    if dry_run:
        cache[real] = _WOULD_LINK
        return _WOULD_LINK

    art_id = upsert_art(conn, data, sniff_mime(data, real))
    cache[real] = art_id
    return art_id


def sync_items(conn, items, *, fields=None, dry_run=False):
    """Sync beets items into the musefs DB. Caller controls the transaction
    (commit on success, rollback for dry runs)."""
    stats = SyncStats()
    art_cache = {}
    for item in items:
        key = realpath_key(item.path)
        track_id = track_id_for_path(conn, key)
        if track_id is None:
            stats.skipped += 1
            continue

        pairs = map_fields(item, fields)
        artpath = _album_art_path(item)
        art_id = _prepare_art(conn, artpath, art_cache, stats, dry_run) if artpath else None

        if not dry_run:
            replace_tags(conn, track_id, pairs)
            if art_id is not None and art_id is not _WOULD_LINK:
                replace_track_art(conn, track_id, art_id)

        if art_id is not None:
            stats.art_linked += 1
        stats.synced += 1

    return stats
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd contrib/beets && python -m pytest tests/test_sync.py -v`
Expected: PASS (7 passed).

- [ ] **Step 5: Commit**

```bash
git add contrib/beets/beetsplug/_core.py contrib/beets/tests/test_sync.py
git commit -m "feat(beets): sync_items orchestrator with art dedup and dry-run"
```

---

## Task 8: beets plugin glue (command + event listeners)

**Files:**
- Create: `contrib/beets/beetsplug/musefs.py`
- Test: `contrib/beets/tests/test_plugin.py`

This task imports beets; its tests are skipped automatically if beets is not installed.

- [ ] **Step 1: Write the failing tests**

Create `contrib/beets/tests/test_plugin.py`:

```python
import os
import sqlite3

import pytest

pytest.importorskip("beets")

from beetsplug.musefs import MusefsPlugin  # noqa: E402


class FakeConfigView:
    def __init__(self, data):
        self._data = data

    def __getitem__(self, key):
        return FakeConfigView(self._data.get(key))

    def get(self, template=None):
        return self._data

    def as_filename(self):
        return os.path.expanduser(self._data)


class FakeLib:
    def __init__(self, items):
        self._items = items

    def items(self, query):
        return self._items


def test_commands_exposes_musefs_subcommand():
    plugin = MusefsPlugin()
    names = [c.name for c in plugin.commands()]
    assert "musefs" in names


def test_command_run_syncs(db_path, make_track, fake_item, monkeypatch):
    from beetsplug import musefs as glue

    tid = make_track("/music/a.flac")
    plugin = MusefsPlugin()

    # Point config at our temp DB and an empty fields override.
    monkeypatch.setattr(
        plugin, "config",
        FakeConfigView({"db": db_path, "fields": {}}),
        raising=False,
    )

    cmd = next(c for c in plugin.commands() if c.name == "musefs")
    opts, _ = cmd.parser.parse_args([])
    lib = FakeLib([fake_item(os.fsencode("/music/a.flac"), title="Song")])

    cmd.func(lib, opts, [])

    conn = sqlite3.connect(db_path)
    try:
        title = conn.execute(
            "SELECT value FROM tags WHERE track_id=? AND key='title'", (tid,)
        ).fetchone()[0]
        assert title == "Song"
    finally:
        conn.close()


def test_command_strips_leading_sync_verb():
    plugin = MusefsPlugin()
    assert plugin._query_from_args(["sync", "artist:Band"]) == ["artist:Band"]
    assert plugin._query_from_args(["artist:Band"]) == ["artist:Band"]
    assert plugin._query_from_args([]) == []
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd contrib/beets && python -m pytest tests/test_plugin.py -v`
Expected: FAIL — `ModuleNotFoundError: No module named 'beetsplug.musefs'` (or skipped if beets absent; install with `pip install beets` to run).

- [ ] **Step 3: Implement the plugin glue**

Create `contrib/beets/beetsplug/musefs.py`:

```python
"""beets plugin: sync canonical beets metadata into the musefs SQLite store."""

from beets import ui
from beets.plugins import BeetsPlugin

from beetsplug import _core


class MusefsPlugin(BeetsPlugin):
    def __init__(self):
        super().__init__()
        self.config.add({"db": None, "fields": {}})
        self.register_listener("after_write", self._on_after_write)
        self.register_listener("item_imported", self._on_item_imported)
        self.register_listener("album_imported", self._on_album_imported)

    # --- command ---------------------------------------------------------

    def commands(self):
        cmd = ui.Subcommand("musefs", help="sync beets metadata into the musefs DB")
        cmd.parser.add_option(
            "--db", dest="db", default=None,
            help="path to the musefs SQLite store (overrides config)",
        )
        cmd.parser.add_option(
            "-n", "--dry-run", dest="dry_run", action="store_true", default=False,
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
        db_path = opts.db or self.config["db"].as_filename()
        if not db_path:
            raise ui.UserError("musefs: set `musefs.db` in config or pass --db")

        query = self._query_from_args(args)
        items = list(lib.items(query))
        stats = self._sync(db_path, items, dry_run=opts.dry_run)
        self._log.info("musefs: {}", stats.summary())

    # --- event listeners -------------------------------------------------

    def _on_after_write(self, item=None, path=None, **kwargs):
        self._sync_one(item)

    def _on_item_imported(self, lib=None, item=None, **kwargs):
        self._sync_one(item)

    def _on_album_imported(self, lib=None, album=None, **kwargs):
        if album is None:
            return
        self._sync(self._db_path(), list(album.items()))

    # --- helpers ---------------------------------------------------------

    def _db_path(self):
        return self.config["db"].as_filename()

    def _fields(self):
        return self.config["fields"].get(dict) or {}

    def _sync_one(self, item):
        if item is None:
            return
        db_path = self._db_path()
        if not db_path:
            self._log.warning("musefs: no `musefs.db` configured; skipping sync")
            return
        self._sync(db_path, [item])

    def _sync(self, db_path, items, dry_run=False):
        import os

        if not os.path.exists(db_path):
            raise ui.UserError(
                f"musefs: DB not found at {db_path}; run `musefs scan` first"
            )
        conn = _core.connect(db_path)
        try:
            _core.check_schema_version(conn)
            stats = _core.sync_items(
                conn, items, fields=self._fields(), dry_run=dry_run
            )
            if dry_run:
                conn.rollback()
            else:
                conn.commit()
            return stats
        except _core.SchemaMismatch as exc:
            conn.rollback()
            raise ui.UserError(f"musefs: {exc}")
        finally:
            conn.close()
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd contrib/beets && pip install beets && python -m pytest tests/test_plugin.py -v`
Expected: PASS (3 passed).

- [ ] **Step 5: Commit**

```bash
git add contrib/beets/beetsplug/musefs.py contrib/beets/tests/test_plugin.py
git commit -m "feat(beets): plugin glue — musefs subcommand and import/write hooks"
```

---

## Task 9: Path-matching gate against the real `musefs` binary

**Files:**
- Create: `contrib/beets/tests/test_path_gate.py`

These tests are marked `musefs_bin` and **deselected by default** (see `addopts` in `pyproject.toml`). Run them explicitly. They require a built `musefs` binary.

- [ ] **Step 1: Build the musefs binary (prerequisite)**

Run (from repo root): `cargo build -p musefs-cli`
Expected: produces `target/debug/musefs`.

- [ ] **Step 2: Write the gate tests**

Create `contrib/beets/tests/test_path_gate.py`:

```python
"""§9.1 path-matching gate: assert the plugin's realpath key is byte-identical
to what the real `musefs scan` binary stores in `tracks.backing_path`."""

import os
import sqlite3
import subprocess
from pathlib import Path

import pytest

from beetsplug._core import realpath_key, track_id_for_path, connect

pytestmark = pytest.mark.musefs_bin

REPO_ROOT = Path(__file__).resolve().parents[3]
MUSEFS_BIN = REPO_ROOT / "target" / "debug" / "musefs"

# A minimal valid FLAC: 'fLaC' + a STREAMINFO metadata block (last-block flag set,
# type 0, length 34) of 34 zero bytes. Enough for `musefs scan` to probe.
MINIMAL_FLAC = b"fLaC" + b"\x80\x00\x00\x22" + b"\x00" * 34


def _scan(tmp_path, tree):
    db = tmp_path / "musefs.db"
    subprocess.run(
        [str(MUSEFS_BIN), "scan", str(tree), "--db", str(db)],
        check=True,
        capture_output=True,
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
        pytest.skip(f"musefs binary not built at {MUSEFS_BIN}; run `cargo build -p musefs-cli`")


def _write_flac(path):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(MINIMAL_FLAC)


@pytest.mark.parametrize("rel", [
    "Artist/Album/01 Track.flac",
    "Accénted/テスト/01.flac",   # accented + CJK
    "Spaced Out/cover %20 thing/02 song.flac",     # spaces and percent
])
def test_plain_paths_match(tmp_path, rel):
    tree = tmp_path / "music"
    _write_flac(tree / rel)
    db = _scan(tmp_path, tree)
    stored = _stored_paths(db)
    assert len(stored) == 1
    # The path beets would hand us is the on-disk file path:
    item_path = os.fsencode(str(tree / rel))
    key = realpath_key(item_path)
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
    # beets stores the path as accessed through the symlink; realpath resolves it.
    key = realpath_key(os.fsencode(str(link_tree / "Artist/Album/01.flac")))
    assert key == stored[0]


def test_symlink_to_file(tmp_path):
    tree = tmp_path / "music"
    real = tree / "real.flac"
    _write_flac(real)
    link = tree / "link.flac"
    link.symlink_to(real)
    db = _scan(tmp_path, tree)
    stored = set(_stored_paths(db))
    # Both names resolve to the same real file; realpath of either is in the set.
    assert realpath_key(os.fsencode(str(link))) in stored


def test_relative_and_dotdot_input(tmp_path, monkeypatch):
    tree = tmp_path / "music"
    _write_flac(tree / "Artist/01.flac")
    db = _scan(tmp_path, tree)
    stored = _stored_paths(db)
    monkeypatch.chdir(tree)
    key = realpath_key(os.fsencode("Artist/../Artist/01.flac"))
    assert key == stored[0]


def test_trailing_slash_and_nonnormalised_input(tmp_path):
    tree = tmp_path / "music"
    _write_flac(tree / "Artist/01.flac")
    db = _scan(tmp_path, tree)
    stored = _stored_paths(db)
    key = realpath_key(os.fsencode(str(tree) + "/Artist/./01.flac"))
    assert key == stored[0]


def test_path_under_different_tree_is_skipped_not_mismatched(tmp_path):
    tree_a = tmp_path / "a"
    _write_flac(tree_a / "01.flac")
    db = _scan(tmp_path, tree_a)
    # A file beets knows under a different tree that was never scanned:
    tree_b = tmp_path / "b"
    _write_flac(tree_b / "01.flac")
    key = realpath_key(os.fsencode(str(tree_b / "01.flac")))
    conn = connect(db)
    try:
        assert track_id_for_path(conn, key) is None  # skipped, never a wrong hit
    finally:
        conn.close()
```

- [ ] **Step 3: Run the gate to verify it passes**

Run: `cd contrib/beets && python -m pytest tests/test_path_gate.py -m musefs_bin -v`
Expected: PASS for all cases. **If any case mismatches, stop:** the keying strategy is wrong and must be fixed (normalise both sides identically) before the plugin is usable — this is a hard gate, not a warning.

If `musefs scan` rejects `MINIMAL_FLAC` (skips it as unparseable, leaving zero rows), replace the fixture with a real small `.flac`/`.mp3` committed under `contrib/beets/tests/fixtures/` and copy it into the tree instead; the assertions are unchanged.

- [ ] **Step 4: Commit**

```bash
git add contrib/beets/tests/test_path_gate.py
git commit -m "test(beets): path-matching gate against real musefs scan binary"
```

---

## Task 10: README, config docs, and manual e2e

**Files:**
- Create: `contrib/beets/README.md`

- [ ] **Step 1: Write the README**

Create `contrib/beets/README.md`:

````markdown
# beets-musefs

A [beets](https://beets.io) plugin that syncs your beets metadata (tags + cover
art) into a [musefs](../../README.md) SQLite store, so a live musefs mount shows
a re-tagged view of your library without rewriting any audio.

## How it fits together

- `musefs scan` owns track rows and the structural columns (audio offsets, size,
  mtime). Run it first; it also seeds tags/art from the files' embedded metadata.
- This plugin overwrites the **tags** (and **cover art**, when beets has it) of
  rows that scan already created, keyed by each file's canonical real path.
- musefs's auto-refresh picks the changes up live — no remount.

The plugin **never** creates rows or touches structural columns. A beets item
whose path wasn't scanned is reported as skipped.

## Install (local / development)

No install needed — point beets at this directory. In your beets `config.yaml`:

```yaml
pluginpath: /path/to/musefs/contrib/beets
plugins: musefs
musefs:
  db: ~/musefs.db          # path to the musefs SQLite store (required)
  # fields:                # optional: map extra beets fields to musefs keys
  #   comments: comment
```

## Workflow (test drive)

```bash
# 1. Probe the library; create rows + structural columns + seed metadata.
musefs scan ~/music --db ~/musefs.db

# 2. Overwrite tags/art from beets (whole library, or a query).
beet musefs                      # everything
beet musefs albumartist:"Boards of Canada"   # a subset
beet musefs -n                   # dry run: report counts, write nothing

# 3. Mount the re-tagged view.
musefs mount ~/mnt --db ~/musefs.db \
    --template '$albumartist/$album/$tracknumber - $title'
```

After this, `beet modify -w …` and imports auto-sync via event hooks, and the
mount refreshes on its own. A metadata-only `beet modify` (no `-w`) is picked up
the next time you run `beet musefs`.

## Notes

- **Cover art:** taken from the album's `artpath` (beets' external cover file).
  beets art wins when present; otherwise any art `musefs scan` ingested from
  embedded pictures is preserved.
- **Orphaned art:** replacing art can orphan old blobs; `musefs scan --revalidate`
  garbage-collects them.
- **Schema version:** the plugin refuses to run if the DB's `user_version` differs
  from the version it targets — rebuild after upgrading musefs.

## Tests

```bash
cd contrib/beets
pip install -e '.[test]'
python -m pytest                      # unit + integration (no Rust binary)
cargo build -p musefs-cli             # from repo root, for the gate below
python -m pytest -m musefs_bin        # path-matching gate vs the real binary
```
````

- [ ] **Step 2: Verify the workflow doc against the CLI**

Run (from repo root): `cargo run -p musefs-cli -- scan --help` and confirm the
`scan <dir> --db` and `mount <mnt> --db --template` forms in the README match the
actual CLI. Fix the README if the CLI differs.
Expected: README commands match `musefs --help` output.

- [ ] **Step 3: Commit**

```bash
git add contrib/beets/README.md
git commit -m "docs(beets): plugin README with config and scan/sync/mount workflow"
```

---

## Task 11: Full verification pass

**Files:** none (verification + final commit if anything changed).

- [ ] **Step 1: Run the full default test suite**

Run: `cd contrib/beets && pip install -e '.[test]' && python -m pytest -v`
Expected: all unit + integration + plugin tests PASS; the `musefs_bin` gate is
deselected (shows as deselected, not failed).

- [ ] **Step 2: Run the path-matching gate**

Run (from repo root): `cargo build -p musefs-cli`
Then: `cd contrib/beets && python -m pytest -m musefs_bin -v`
Expected: all gate cases PASS. (If the binary build is unavailable in this
environment, record that the gate must be run where Rust + `/dev/fuse`-independent
build is available; the gate does not need a mount, only `musefs scan`.)

- [ ] **Step 3: Sanity-check the spec is fully covered**

Re-read `docs/superpowers/specs/2026-05-25-beets-musefs-plugin-design.md` §§3–9
and confirm each requirement maps to a test or implementation above:
field mapping (§5, Task 2), realpath keying (§3.1, Tasks 3 & 9), schema guard
(§3.3, Task 4), tags (§3.2, Task 5), art dedup/link/conditional (§3.4/§6, Tasks 6
& 7), triggers/refresh (§3.5 — verified indirectly via `content_version` bumps in
Tasks 5 & 6), error handling (§8, Task 8), tests (§9/§9.1, Tasks 2–9).

- [ ] **Step 4: Final commit (if any fixes were needed)**

```bash
git add -A contrib/beets
git commit -m "chore(beets): verification pass fixes"
```

(Skip if the working tree is clean.)

---

## Self-Review notes (for the implementer)

- The `_core` module is intentionally beets-free so Tasks 2–7 and 9 run without
  beets installed. Only Task 8's tests import beets (and skip if it is absent).
- `content_version` is asserted to increase after tag/art writes (Tasks 5, 6),
  which is the observable proof the triggers — and therefore musefs's live
  refresh — will fire. Full cross-language mount verification is the documented
  manual step (README), not automated here.
- The single highest-risk item (realpath vs canonicalize) is covered by the
  Task 9 gate against the real binary; treat any mismatch there as blocking.
