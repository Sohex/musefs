import sqlite3

import pytest
from conftest import insert_track, text_tags

from musefs_common import connect, merge_tags, replace_tags


class _FailInsert(sqlite3.Connection):
    """Connection that raises on the next INSERT executemany, to enter the
    DELETE-then-INSERT torn window deterministically."""

    fail = False

    def executemany(self, sql, parameters):
        if self.fail and sql.lstrip().upper().startswith("INSERT"):
            raise RuntimeError("boom")
        return super().executemany(sql, parameters)


def _autocommit(db_path, factory=None):
    """An autocommit connection with foreign keys on (mirrors connect()'s FK
    pragma), used to exercise the undefended-against-autocommit path."""
    conn = sqlite3.connect(db_path, factory=factory) if factory else sqlite3.connect(db_path)
    conn.execute("PRAGMA foreign_keys = ON")
    conn.isolation_level = None  # legacy autocommit
    return conn


def test_replace_tags_atomic_on_autocommit_failure(db_path):
    # Seed a track + one text tag (committed) on an autocommit connection.
    conn = _autocommit(db_path, factory=_FailInsert)
    try:
        tid = insert_track(conn, "/m/a.flac")
        replace_tags(conn, tid, [("title", "ORIG")])
        # Now force the INSERT half of replace_tags to crash after the DELETE.
        conn.fail = True
        with pytest.raises(RuntimeError):
            replace_tags(conn, tid, [("title", "NEW")])
    finally:
        conn.close()
    # The original tag must survive: the savepoint rolled the torn write back.
    check = connect(db_path)
    try:
        assert text_tags(check, tid) == {"title": ["ORIG"]}
    finally:
        check.close()


def test_replace_tags_no_premature_commit_in_deferred_mode(db_path):
    # The legacy-mode trap guard: in default deferred mode the savepoint must
    # nest, so a caller rollback still wins. A bare savepoint would commit here.
    conn = connect(db_path)
    try:
        tid = insert_track(conn, "/m/a.flac")
        conn.commit()
        replace_tags(conn, tid, [("title", "X")])
        conn.rollback()
    finally:
        conn.close()
    check = connect(db_path)
    try:
        assert text_tags(check, tid) == {}
    finally:
        check.close()


def test_merge_tags_atomic_on_autocommit_failure(db_path):
    conn = _autocommit(db_path, factory=_FailInsert)
    try:
        tid = insert_track(conn, "/m/a.flac")
        # Seed both a managed key we'll overwrite and an unmanaged baseline key.
        merge_tags(conn, tid, [("artist", "ORIG")], [])
        conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (?, 'genre', 'Jazz', 0)",
            (tid,),
        )
        conn.fail = True
        with pytest.raises(RuntimeError):
            merge_tags(conn, tid, [("artist", "NEW")], [])
    finally:
        conn.close()
    check = connect(db_path)
    try:
        # The failed merge rolled back: both the managed and baseline rows remain.
        assert text_tags(check, tid) == {"artist": ["ORIG"], "genre": ["Jazz"]}
    finally:
        check.close()
