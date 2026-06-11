import sqlite3

import pytest
from conftest import JPEG, insert_track, text_tags

from musefs_common import (
    ArtImage,
    Record,
    SyncStats,
    connect,
    merge_tags,
    replace_tags,
    replace_track_art,
    sync_files,
    sync_one,
    upsert_art,
)


class _FailInsert(sqlite3.Connection):
    """Connection that raises on a chosen INSERT executemany, to enter a
    DELETE-then-INSERT torn window deterministically. ``fail`` may be:
    False (never), True (any INSERT), or "art" (only the track_art INSERT)."""

    fail = False

    def executemany(self, sql, parameters):
        upper = sql.lstrip().upper()
        if upper.startswith("INSERT"):
            if self.fail is True or (self.fail == "art" and "TRACK_ART" in upper):
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


def test_replace_track_art_atomic_on_fk_violation(db_path):
    conn = _autocommit(db_path)  # FK on, autocommit; no failure injection needed
    try:
        tid = insert_track(conn, "/m/a.flac")
        art_id = upsert_art(conn, JPEG, "image/jpeg")
        replace_track_art(conn, tid, [(art_id, 3, "")])
        before_cv = conn.execute(
            "SELECT content_version FROM tracks WHERE id = ?", (tid,)
        ).fetchone()[0]
        # 999999 has no row in `art`: the INSERT (after the DELETE) trips the FK.
        with pytest.raises(sqlite3.IntegrityError):
            replace_track_art(conn, tid, [(999999, 3, "")])
        rows = conn.execute(
            "SELECT art_id, picture_type, ordinal FROM track_art WHERE track_id = ?", (tid,)
        ).fetchall()
        after_cv = conn.execute(
            "SELECT content_version FROM tracks WHERE id = ?", (tid,)
        ).fetchone()[0]
    finally:
        conn.close()
    # The DELETE + the content_version trigger bump both rolled back with the FK failure.
    assert rows == [(art_id, 3, 0)]
    assert after_cv == before_cv


def test_sync_one_whole_record_atomic_on_autocommit(db_path):
    # On an autocommit connection, if art linking fails the tags written earlier
    # in the same record must roll back too (record is all-or-nothing).
    conn = _autocommit(db_path, factory=_FailInsert)
    try:
        tid = insert_track(conn, "/m/a.flac")
        # fail="art" makes _FailInsert raise only on the track_art INSERT, so
        # tags write first and the failure lands on the later art step.
        conn.fail = "art"
        with pytest.raises(RuntimeError):
            sync_one(
                conn,
                Record(key="/m/a.flac", pairs=[("title", "T")], art=[ArtImage(JPEG, "image/jpeg")]),
                SyncStats(),
            )
    finally:
        conn.close()
    check = connect(db_path)
    try:
        assert text_tags(check, tid) == {}
        assert check.execute("SELECT COUNT(*) FROM track_art").fetchone()[0] == 0
    finally:
        check.close()


def test_sync_files_deferred_batch_commits_atomically(db_path):
    # Shipped beets-style path: deferred mode, several records, one final commit.
    conn = connect(db_path)
    try:
        insert_track(conn, "/m/a.flac")
        insert_track(conn, "/m/b.flac")
        conn.commit()
        records = [
            Record(key="/m/a.flac", pairs=[("title", "A")]),
            Record(key="/m/b.flac", pairs=[("title", "B")]),
        ]
        stats = sync_files(conn, records)
        assert stats.synced == 2
        conn.commit()
    finally:
        conn.close()
    check = connect(db_path)
    try:
        got = {
            path: check.execute(
                "SELECT value FROM tags t JOIN tracks tr ON tr.id = t.track_id "
                "WHERE tr.backing_path = ? AND t.key = 'title'",
                (path,),
            ).fetchone()[0]
            for path in ("/m/a.flac", "/m/b.flac")
        }
        assert got == {"/m/a.flac": "A", "/m/b.flac": "B"}
    finally:
        check.close()


def test_sync_files_deferred_batch_rolls_back_as_unit(db_path):
    # Deferred mode: per-record savepoints must NOT self-commit, so a caller
    # rollback abandons the whole batch.
    conn = connect(db_path)
    try:
        tid = insert_track(conn, "/m/a.flac")
        conn.commit()
        sync_one(conn, Record(key="/m/a.flac", pairs=[("title", "A")]), SyncStats())
        conn.rollback()  # caller abandons the batch after the write
    finally:
        conn.close()
    check = connect(db_path)
    try:
        assert text_tags(check, tid) == {}
    finally:
        check.close()
