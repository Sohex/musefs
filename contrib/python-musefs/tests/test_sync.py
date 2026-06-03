from musefs_common import Record, SyncStats, connect, sync_files, sync_one
from musefs_common.constants import MAX_ART_BYTES

from conftest import JPEG, insert_track


def _seed(db_path, path="/m/a.flac"):
    conn = connect(db_path)
    tid = insert_track(conn, path)
    conn.commit()
    return conn, tid


def test_sync_one_skips_unmatched_path(db_path):
    conn = connect(db_path)
    try:
        stats = SyncStats()
        sync_one(conn, Record(key="/nope.flac", pairs=[("title", "T")], art=None), stats)
        assert stats.skipped == 1
        assert stats.synced == 0
    finally:
        conn.close()


def test_sync_one_writes_tags_and_art(db_path):
    conn, _ = _seed(db_path)
    try:
        stats = SyncStats()
        sync_one(conn, Record(key="/m/a.flac", pairs=[("title", "T")], art=(JPEG, "image/jpeg")), stats)
        conn.commit()
        assert stats.synced == 1
        assert stats.art_linked == 1
        assert conn.execute("SELECT value FROM tags WHERE key='title'").fetchone()[0] == "T"
        assert conn.execute("SELECT COUNT(*) FROM track_art").fetchone()[0] == 1
    finally:
        conn.close()


def test_sync_one_over_cap_art_skipped_not_linked(db_path):
    conn, _ = _seed(db_path)
    try:
        big = b"\xff\xd8\xff" + b"\x00" * (MAX_ART_BYTES + 1)
        stats = SyncStats()
        sync_one(conn, Record(key="/m/a.flac", pairs=[], art=(big, "image/jpeg")), stats)
        conn.commit()
        assert stats.synced == 1
        assert stats.skipped_art == 1
        assert stats.art_linked == 0
        assert conn.execute("SELECT COUNT(*) FROM track_art").fetchone()[0] == 0
    finally:
        conn.close()


def test_sync_one_dry_run_counts_without_writing(db_path):
    conn, _ = _seed(db_path)
    try:
        stats = SyncStats()
        sync_one(
            conn,
            Record(key="/m/a.flac", pairs=[("title", "T")], art=(JPEG, "image/jpeg")),
            stats,
            dry_run=True,
        )
        assert stats.synced == 1
        assert stats.art_linked == 1
        assert conn.execute("SELECT COUNT(*) FROM tags").fetchone()[0] == 0
        assert conn.execute("SELECT COUNT(*) FROM track_art").fetchone()[0] == 0
    finally:
        conn.close()


def test_sync_files_returns_aggregated_stats(db_path):
    conn = connect(db_path)
    try:
        insert_track(conn, "/m/a.flac")
        conn.commit()
        records = [
            Record(key="/m/a.flac", pairs=[("title", "A")], art=None),
            Record(key="/m/missing.flac", pairs=[("title", "B")], art=None),
        ]
        stats = sync_files(conn, records)
        conn.commit()
        assert stats.synced == 1
        assert stats.skipped == 1
    finally:
        conn.close()


def test_sync_files_reuses_caller_seeded_stats(db_path):
    conn = connect(db_path)
    try:
        insert_track(conn, "/m/a.flac")
        conn.commit()
        seeded = SyncStats(skipped_art=2)  # e.g. beets pre-counted unreadable art
        out = sync_files(conn, [Record(key="/m/a.flac", pairs=[], art=None)], stats=seeded)
        assert out is seeded
        assert out.skipped_art == 2
        assert out.synced == 1
    finally:
        conn.close()


def test_tags_fully_replaced(db_path):
    conn, tid = _seed(db_path)
    try:
        sync_one(conn, Record(key="/m/a.flac", pairs=[("title", "Old"), ("genre", "Rock")]), SyncStats())
        sync_one(conn, Record(key="/m/a.flac", pairs=[("title", "New")]), SyncStats())
        conn.commit()
        rows = dict(conn.execute("SELECT key, value FROM tags WHERE track_id=?", (tid,)))
        assert rows == {"title": "New"}  # genre gone after replace
    finally:
        conn.close()


def test_no_art_leaves_existing_track_art_untouched(db_path):
    conn, tid = _seed(db_path)
    try:
        conn.execute(
            "INSERT INTO art (sha256, mime, byte_len, data) VALUES "
            "('deadbeef', 'image/jpeg', 3, X'aabbcc')"
        )
        art_id = conn.execute("SELECT id FROM art WHERE sha256='deadbeef'").fetchone()[0]
        conn.execute("INSERT INTO track_art (track_id, art_id) VALUES (?, ?)", (tid, art_id))
        conn.commit()
        stats = SyncStats()
        sync_one(conn, Record(key="/m/a.flac", pairs=[("title", "T")], art=None), stats)
        conn.commit()
        assert stats.art_linked == 0
        row = conn.execute("SELECT art_id FROM track_art WHERE track_id=?", (tid,)).fetchone()
        assert row == (art_id,)  # scan-seeded art untouched when Record has no art
    finally:
        conn.close()


def test_tags_write_bumps_content_version(db_path):
    conn, tid = _seed(db_path)
    try:
        before = conn.execute("SELECT content_version FROM tracks WHERE id=?", (tid,)).fetchone()[0]
        sync_one(conn, Record(key="/m/a.flac", pairs=[("title", "T")]), SyncStats())
        conn.commit()
        after = conn.execute("SELECT content_version FROM tracks WHERE id=?", (tid,)).fetchone()[0]
        assert after > before
    finally:
        conn.close()


def test_skip_mid_batch_does_not_abort_others(db_path):
    conn = connect(db_path)
    try:
        a = insert_track(conn, "/m/a.flac")
        b = insert_track(conn, "/m/b.flac")
        conn.commit()
        records = [
            Record(key="/m/a.flac", pairs=[("title", "T")]),
            Record(key="/m/missing.flac", pairs=[("title", "T")]),
            Record(key="/m/b.flac", pairs=[("title", "T")]),
        ]
        stats = sync_files(conn, records)
        conn.commit()
        assert stats.synced == 2
        assert stats.skipped == 1
        for tid in (a, b):
            assert (
                conn.execute(
                    "SELECT value FROM tags WHERE track_id=? AND key='title'", (tid,)
                ).fetchone()[0]
                == "T"
            )
    finally:
        conn.close()


def test_art_deduped_across_records(db_path):
    conn = connect(db_path)
    try:
        insert_track(conn, "/m/a.flac")
        insert_track(conn, "/m/b.flac")
        conn.commit()
        records = [
            Record(key="/m/a.flac", pairs=[], art=(JPEG, "image/jpeg")),
            Record(key="/m/b.flac", pairs=[], art=(JPEG, "image/jpeg")),
        ]
        sync_files(conn, records)
        conn.commit()
        assert conn.execute("SELECT COUNT(*) FROM art").fetchone()[0] == 1
        assert conn.execute("SELECT COUNT(*) FROM track_art").fetchone()[0] == 2
    finally:
        conn.close()


def test_summary_format():
    s = SyncStats(synced=3, skipped=1, art_linked=2, skipped_art=1)
    assert s.summary() == "synced=3 skipped=1 art_linked=2 skipped_art=1"
