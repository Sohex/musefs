import sqlite3
import time
from pathlib import Path

import pytest

from beetsplug._core import connect as musefs_connect

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


def insert_track(
    conn, backing_path, fmt="flac", audio_offset=0, audio_length=0, backing_size=0, backing_mtime=0
):
    """Insert a minimal track row (as `musefs scan` would) and return its id."""
    now = int(time.time())
    cur = conn.execute(
        "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, "
        "backing_size, backing_mtime, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
        (backing_path, fmt, audio_offset, audio_length, backing_size, backing_mtime, now),
    )
    return cur.lastrowid


class FakeAlbum:
    def __init__(self, artpath=None, items=()):
        self.artpath = artpath  # bytes path to cover file, or None
        self._items = list(items)

    def items(self):
        return self._items


_fake_item_counter = 0


class FakeItem:
    """Minimal stand-in for a beets Item: attribute reads + get_album()."""

    def __init__(self, path, album=None, **fields):
        global _fake_item_counter
        _fake_item_counter += 1
        self.id = _fake_item_counter
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
    return FakeItem  # returns the class; call it directly in tests


@pytest.fixture
def fake_album():
    return FakeAlbum  # returns the class; call it directly in tests


@pytest.fixture
def make_track(db_path):
    """Return a helper that inserts a track row and returns its id."""

    def _make(backing_path, fmt="flac"):
        # Use the plugin's connect() so foreign_keys=ON matches real writes.
        conn = musefs_connect(db_path)
        try:
            tid = insert_track(conn, backing_path, fmt)
            conn.commit()
            return tid
        finally:
            conn.close()

    return _make
