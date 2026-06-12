import sqlite3
import time

import pytest

from musefs_common import connect as musefs_connect
from musefs_common.schema import SCHEMA_SQL

# Minimal valid JPEG/PNG headers + padding; used as fake cover-art bytes in tests.
JPEG = b"\xff\xd8\xff\xe0" + b"\x00" * 32
PNG = b"\x89PNG\r\n\x1a\n" + b"\x00" * 16


@pytest.fixture
def db_path(tmp_path):
    """A temp musefs DB with the full schema applied."""
    path = tmp_path / "musefs.db"
    conn = sqlite3.connect(str(path))
    conn.executescript(SCHEMA_SQL)
    conn.commit()
    conn.close()
    return str(path)


def text_tags(conn, track_id):
    """Return {key: [values in ordinal order]} for a track's text rows only
    (binary tags excluded)."""
    rows = conn.execute(
        "SELECT key, value FROM tags WHERE track_id=? AND value_blob IS NULL ORDER BY key, ordinal",
        (track_id,),
    ).fetchall()
    out = {}
    for key, value in rows:
        out.setdefault(key, []).append(value)
    return out


def insert_track(conn, backing_path, fmt="flac"):
    """Insert a minimal track row (as `musefs scan` would) and return its id."""
    now = int(time.time())
    cur = conn.execute(
        "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, "
        "backing_size, backing_mtime_ns, updated_at) VALUES (?, ?, 0, 0, 0, 0, ?)",
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
