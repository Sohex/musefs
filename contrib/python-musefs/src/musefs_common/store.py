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
