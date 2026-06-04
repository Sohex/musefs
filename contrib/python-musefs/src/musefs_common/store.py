import hashlib
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


def replace_track_art(conn, track_id, arts):
    """Replace the track's art rows. ``arts`` is an ordered list of
    ``(art_id, picture_type, description)``; each row's ``ordinal`` is its
    list index."""
    conn.execute("DELETE FROM track_art WHERE track_id = ?", (track_id,))
    conn.executemany(
        "INSERT INTO track_art (track_id, art_id, picture_type, description, "
        "ordinal) VALUES (?, ?, ?, ?, ?)",
        [
            (track_id, art_id, picture_type, description, i)
            for i, (art_id, picture_type, description) in enumerate(arts)
        ],
    )
