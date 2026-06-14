# GENERATED from python-musefs/src/musefs_common/store.py — do not edit.
# Run contrib/python-musefs/vendor_to_picard.py after changing the library.
#
import contextlib
import hashlib
import os
import sqlite3

from .constants import EXPECTED_USER_VERSION
from .errors import SchemaMismatch

# sqlite3.LEGACY_TRANSACTION_CONTROL is 3.12+; it is == -1. Use getattr so this
# module still imports on the 3.8 floor (where the constant does not exist).
_LEGACY = getattr(sqlite3, "LEGACY_TRANSACTION_CONTROL", -1)


def _is_autocommit(conn):
    """True if the connection auto-commits each statement (no caller-owned
    transaction will be committed for us)."""
    ac = getattr(conn, "autocommit", _LEGACY)  # 3.12+ attribute; _LEGACY on <3.12
    if ac is True:
        return True
    if ac is False:
        return False
    return conn.isolation_level is None  # legacy transaction control


def _is_legacy(conn):
    """True if the connection uses legacy transaction control (the <3.12 default
    and the 3.12+ LEGACY_TRANSACTION_CONTROL mode)."""
    return getattr(conn, "autocommit", _LEGACY) == _LEGACY


@contextlib.contextmanager
def _savepoint(conn, name):
    """Make a DELETE+INSERT block atomic regardless of the connection's
    transaction mode. On a caller-managed connection it nests via SAVEPOINT and
    never commits the enclosing transaction; on an autocommit connection the
    outermost call owns a transaction for the block (commit on success, rollback
    on failure). Nested calls only nest -- they never BEGIN or commit -- so a
    sync_one savepoint may wrap these per-function savepoints safely.

    ``name`` must be a hardcoded SQL identifier (it is interpolated into the SQL,
    so never pass caller-controlled text)."""
    autocommit = _is_autocommit(conn)
    owns = not conn.in_transaction  # outermost call: it opens & owns the txn
    # Legacy mode never auto-BEGINs before SAVEPOINT, so a savepoint opened as
    # the first statement of a batch would become the outermost transaction and
    # commit durably on RELEASE. Force a nesting BEGIN there. PEP-249 modes
    # auto-begin before any statement, so they need no nudge.
    if owns and _is_legacy(conn):
        conn.execute("BEGIN")
    conn.execute(f"SAVEPOINT {name}")
    try:
        yield
    except BaseException:
        try:
            conn.execute(f"ROLLBACK TO {name}")
            conn.execute(f"RELEASE {name}")
            if owns and autocommit:
                conn.rollback()
        except sqlite3.Error:
            pass  # never mask the original exception with a cleanup failure
        raise
    else:
        conn.execute(f"RELEASE {name}")
        if owns and autocommit:
            conn.commit()


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
    (mirroring musefs scan ingest).

    Atomic via an internal savepoint (see ``_savepoint``), so a crash between the
    DELETE and the INSERT can never leave the track's text tags wiped -- safe
    even when called on an autocommit connection."""
    # Scope to the plugin-owned text rows: scanner-written binary tags
    # (value_blob NOT NULL) must survive a sync (#82).
    with _savepoint(conn, "musefs_replace_tags"):
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


def merge_tags(conn, track_id, managed_pairs, delete_keys):
    """Per-key replace of the plugin-managed text tags, leaving unmanaged text
    rows (the scan-seeded baseline) intact. ``managed_pairs`` is an ordered list
    of (key, value); every key it names is cleared and rewritten with contiguous
    ordinals. ``delete_keys`` names keys to clear without rewriting (tags the
    plugin previously managed and the user has now removed). Both deletes are
    scoped to ``value_blob IS NULL`` so scanner-written binary tags survive.

    Atomic via an internal savepoint (see ``_savepoint``): the per-key deletes
    and the rewrite either all land or none do, even on an autocommit
    connection."""
    with _savepoint(conn, "musefs_merge_tags"):
        by_key = {}
        for key, value in managed_pairs:
            by_key.setdefault(key, []).append(value)

        # Case-fold the key match: a scan seeds an unmapped tag in the file's
        # native case (e.g. Vorbis ``LABEL``) while the plugin canonicalises to
        # lowercase (``label``). Vorbis keys render case-insensitively, so an
        # exact-case delete would leave the scan row and render a duplicate (#407).
        for key in set(by_key) | set(delete_keys or ()):
            conn.execute(
                "DELETE FROM tags WHERE track_id = ? AND lower(key) = lower(?) "
                "AND value_blob IS NULL",
                (track_id, key),
            )

        rows = [
            (track_id, key, value, ordinal)
            for key, values in by_key.items()
            for ordinal, value in enumerate(values)
        ]
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
    list index.

    Atomic via an internal savepoint (see ``_savepoint``): the DELETE and the
    re-insert either both land or neither does, even on an autocommit
    connection."""
    with _savepoint(conn, "musefs_replace_track_art"):
        conn.execute("DELETE FROM track_art WHERE track_id = ?", (track_id,))
        conn.executemany(
            "INSERT INTO track_art (track_id, art_id, picture_type, description, "
            "ordinal) VALUES (?, ?, ?, ?, ?)",
            [
                (track_id, art_id, picture_type, description, i)
                for i, (art_id, picture_type, description) in enumerate(arts)
            ],
        )
