import sqlite3

import pytest

from beetsplug._core import SchemaMismatch, check_schema_version, connect, replace_tags, track_id_for_path


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
