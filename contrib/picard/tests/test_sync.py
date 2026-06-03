from conftest import JPEG

from musefs._common import SyncStats, connect
from musefs._core import sync_one


def _sync(conn, key, pairs, art=None, dry_run=False):
    stats = SyncStats()
    sync_one(conn, key, pairs, art, stats, dry_run=dry_run)
    return stats


def test_skip_when_no_row(db_path):
    conn = connect(db_path)
    try:
        stats = _sync(conn, "/music/missing.flac", [("title", "X")])
        conn.commit()
        assert stats.synced == 0
        assert stats.skipped == 1
    finally:
        conn.close()


def test_tags_written_for_existing_row(db_path, make_track):
    tid = make_track("/music/a.flac")
    conn = connect(db_path)
    try:
        stats = _sync(conn, "/music/a.flac", [("title", "Song"), ("artist", "Band")])
        conn.commit()
        assert stats.synced == 1
        title = conn.execute(
            "SELECT value FROM tags WHERE track_id=? AND key='title'", (tid,)
        ).fetchone()[0]
        assert title == "Song"
        # Spec §3.5: the tags trigger bumped the track's content_version, so the
        # mount's HeaderCache rebuilds the layout. Make that observable.
        cv = conn.execute("SELECT content_version FROM tracks WHERE id=?", (tid,)).fetchone()[0]
        assert cv >= 1
    finally:
        conn.close()


def test_skip_mid_batch_does_not_abort_others(db_path, make_track):
    # Spec §9: a per-file "no row" skip must not roll back the run — the real
    # files around it still get their tags, sharing one SyncStats and one txn.
    tid_a = make_track("/music/a.flac")
    tid_b = make_track("/music/b.flac")
    conn = connect(db_path)
    try:
        stats = SyncStats()
        for key in ("/music/a.flac", "/music/missing.flac", "/music/b.flac"):
            sync_one(conn, key, [("title", "T")], None, stats)
        conn.commit()
        assert stats.synced == 2
        assert stats.skipped == 1
        for tid in (tid_a, tid_b):
            assert (
                conn.execute(
                    "SELECT value FROM tags WHERE track_id=? AND key='title'", (tid,)
                ).fetchone()[0]
                == "T"
            )
    finally:
        conn.close()


def test_tags_fully_replaced(db_path, make_track):
    tid = make_track("/music/a.flac")
    conn = connect(db_path)
    try:
        _sync(conn, "/music/a.flac", [("title", "Old"), ("genre", "Rock")])
        conn.commit()
        _sync(conn, "/music/a.flac", [("title", "New")])
        conn.commit()
        rows = dict(conn.execute("SELECT key, value FROM tags WHERE track_id=?", (tid,)))
        assert rows == {"title": "New"}  # genre gone after replace
    finally:
        conn.close()


def test_art_linked_when_front_cover_present(db_path, make_track):
    tid = make_track("/music/a.flac")
    conn = connect(db_path)
    try:
        stats = _sync(conn, "/music/a.flac", [("title", "Song")], art=(JPEG, "image/jpeg"))
        conn.commit()
        assert stats.art_linked == 1
        assert (
            conn.execute("SELECT COUNT(*) FROM track_art WHERE track_id=?", (tid,)).fetchone()[0]
            == 1
        )
    finally:
        conn.close()


def test_embedded_art_preserved_when_no_front_cover(db_path, make_track):
    # Simulate scan-ingested art already linked to the track.
    tid = make_track("/music/a.flac")
    conn = connect(db_path)
    try:
        conn.execute(
            "INSERT INTO art (sha256, mime, byte_len, data) VALUES "
            "('deadbeef', 'image/jpeg', 3, X'aabbcc')"
        )
        art_id = conn.execute("SELECT id FROM art WHERE sha256='deadbeef'").fetchone()[0]
        conn.execute("INSERT INTO track_art (track_id, art_id) VALUES (?, ?)", (tid, art_id))
        conn.commit()
        stats = _sync(conn, "/music/a.flac", [("title", "Song")], art=None)
        conn.commit()
        assert stats.art_linked == 0
        row = conn.execute("SELECT art_id FROM track_art WHERE track_id=?", (tid,)).fetchone()
        assert row == (art_id,)  # untouched
    finally:
        conn.close()


def test_oversized_art_skipped_but_tags_written(db_path, make_track, monkeypatch):
    from musefs._common import sync as _sync_module

    monkeypatch.setattr(_sync_module, "MAX_ART_BYTES", 8)
    tid = make_track("/music/a.flac")
    conn = connect(db_path)
    try:
        stats = _sync(conn, "/music/a.flac", [("title", "Song")], art=(b"X" * 64, "image/jpeg"))
        conn.commit()
        assert stats.skipped_art == 1
        assert stats.art_linked == 0
        # Tags still written despite oversized art.
        assert (
            conn.execute(
                "SELECT value FROM tags WHERE track_id=? AND key='title'", (tid,)
            ).fetchone()[0]
            == "Song"
        )
    finally:
        conn.close()


def test_art_deduped_across_files(db_path, make_track):
    make_track("/music/a.flac")
    make_track("/music/b.flac")
    conn = connect(db_path)
    try:
        _sync(conn, "/music/a.flac", [("title", "A")], art=(JPEG, "image/jpeg"))
        _sync(conn, "/music/b.flac", [("title", "B")], art=(JPEG, "image/jpeg"))
        conn.commit()
        assert conn.execute("SELECT COUNT(*) FROM art").fetchone()[0] == 1
        assert conn.execute("SELECT COUNT(*) FROM track_art").fetchone()[0] == 2
    finally:
        conn.close()


def test_dry_run_writes_nothing(db_path, make_track):
    tid = make_track("/music/a.flac")
    conn = connect(db_path)
    try:
        stats = _sync(
            conn, "/music/a.flac", [("title", "Song")], art=(JPEG, "image/jpeg"), dry_run=True
        )
        # Commit (not rollback): proves dry_run itself suppressed the writes,
        # rather than a rollback merely undoing them.
        conn.commit()
        assert stats.synced == 1
        assert stats.art_linked == 1  # "would link"
        assert conn.execute("SELECT COUNT(*) FROM tags WHERE track_id=?", (tid,)).fetchone()[0] == 0
        assert conn.execute("SELECT COUNT(*) FROM art").fetchone()[0] == 0
        assert conn.execute("SELECT COUNT(*) FROM track_art").fetchone()[0] == 0
    finally:
        conn.close()
