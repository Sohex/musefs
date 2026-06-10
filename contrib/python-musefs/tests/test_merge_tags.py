from conftest import insert_track, text_tags

from musefs_common import connect
from musefs_common.store import merge_tags, replace_tags


def test_merge_overwrites_managed_keeps_unmanaged(db_path):
    conn = connect(db_path)
    try:
        tid = insert_track(conn, "/m/a.flac")
        # Baseline B (as a scan would seed it).
        replace_tags(
            conn,
            tid,
            [("artist", "Old"), ("comment", "keep me"), ("replaygain_track_gain", "-3.00 dB")],
        )
        # M overrides artist + replaygain, does not mention comment.
        merge_tags(
            conn, tid, [("artist", "New"), ("replaygain_track_gain", "-7.50 dB")], delete_keys=[]
        )
        conn.commit()
        tags = text_tags(conn, tid)
        assert tags["artist"] == ["New"]  # M wins
        assert tags["comment"] == ["keep me"]  # unmanaged B persists
        assert tags["replaygain_track_gain"] == ["-7.50 dB"]
    finally:
        conn.close()


def test_merge_delete_keys_suppresses_backing(db_path):
    conn = connect(db_path)
    try:
        tid = insert_track(conn, "/m/b.flac")
        replace_tags(conn, tid, [("artist", "Band"), ("comment", "drop me")])
        # M keeps artist; comment was managed before and is now dropped.
        merge_tags(conn, tid, [("artist", "Band")], delete_keys=["comment"])
        conn.commit()
        tags = text_tags(conn, tid)
        assert tags["artist"] == ["Band"]
        assert "comment" not in tags  # suppressed
    finally:
        conn.close()


def test_merge_multivalue_ordinals_contiguous(db_path):
    conn = connect(db_path)
    try:
        tid = insert_track(conn, "/m/c.flac")
        merge_tags(conn, tid, [("artist", "A"), ("artist", "B"), ("genre", "Rock")], delete_keys=[])
        conn.commit()
        ords = conn.execute(
            "SELECT ordinal FROM tags WHERE track_id=? AND key='artist' ORDER BY ordinal", (tid,)
        ).fetchall()
        assert [o[0] for o in ords] == [0, 1]  # 0..n per key
        assert text_tags(conn, tid)["artist"] == ["A", "B"]
    finally:
        conn.close()


def test_merge_preserves_binary_tags(db_path):
    conn = connect(db_path)
    try:
        tid = insert_track(conn, "/m/d.flac")
        # A scanner-written binary tag sharing key 'comment' (value_blob NOT NULL).
        conn.execute(
            "INSERT INTO tags (track_id, key, value, value_blob, ordinal) VALUES (?, ?, '', ?, 1)",
            (tid, "comment", b"\x00\x01"),
        )
        merge_tags(conn, tid, [("comment", "text")], delete_keys=[])
        conn.commit()
        # Binary row survives; text row added.
        bin_rows = conn.execute(
            "SELECT COUNT(*) FROM tags WHERE track_id=? AND value_blob IS NOT NULL", (tid,)
        ).fetchone()[0]
        assert bin_rows == 1
        assert text_tags(conn, tid)["comment"] == ["text"]
    finally:
        conn.close()
