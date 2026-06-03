from conftest import JPEG, insert_track

from musefs_common import (
    connect,
    replace_tags,
    replace_track_art,
    sniff_mime,
    upsert_art,
)

PNG = b"\x89PNG\r\n\x1a\n" + b"\x00" * 16
WEBP = b"RIFF" + b"\x00\x00\x00\x00" + b"WEBP" + b"\x00" * 8


def test_sniff_mime_magic_bytes():
    assert sniff_mime(JPEG, "/x") == "image/jpeg"
    assert sniff_mime(PNG, "/x") == "image/png"
    assert sniff_mime(WEBP, "/x") == "image/webp"


def test_sniff_mime_extension_fallback():
    assert sniff_mime(b"nope", "/x.png") == "image/png"
    assert sniff_mime(b"nope", "/x.bin") == "application/octet-stream"


def test_replace_tags_assigns_incrementing_ordinals(db_path):
    conn = connect(db_path)
    try:
        tid = insert_track(conn, "/m/a.flac")
        replace_tags(conn, tid, [("genre", "Rock"), ("genre", "Pop"), ("title", "T")])
        conn.commit()
        rows = conn.execute(
            "SELECT key, value, ordinal FROM tags WHERE track_id=? ORDER BY key, ordinal", (tid,)
        ).fetchall()
        assert ("genre", "Rock", 0) in rows
        assert ("genre", "Pop", 1) in rows
        assert ("title", "T", 0) in rows
    finally:
        conn.close()


def test_replace_tags_preserves_binary_tags(db_path):
    conn = connect(db_path)
    try:
        tid = insert_track(conn, "/m/a.flac")
        conn.execute(
            "INSERT INTO tags (track_id, key, value, value_blob, ordinal) "
            "VALUES (?, 'priv', '', ?, 0)",
            (tid, b"\x01\x02"),
        )
        conn.commit()
        replace_tags(conn, tid, [("title", "T")])
        conn.commit()
        blobs = conn.execute(
            "SELECT COUNT(*) FROM tags WHERE track_id=? AND value_blob IS NOT NULL", (tid,)
        ).fetchone()[0]
        assert blobs == 1
    finally:
        conn.close()


def test_upsert_art_is_content_addressed(db_path):
    conn = connect(db_path)
    try:
        first = upsert_art(conn, JPEG, "image/jpeg")
        again = upsert_art(conn, JPEG, "image/png")  # same bytes -> same id, mime ignored
        conn.commit()
        assert first == again
        mime = conn.execute("SELECT mime FROM art WHERE id=?", (first,)).fetchone()[0]
        assert mime == "image/jpeg"
    finally:
        conn.close()


def test_replace_track_art_sets_and_replaces_front_cover(db_path):
    conn = connect(db_path)
    try:
        tid = insert_track(conn, "/m/a.flac")
        first = upsert_art(conn, JPEG, "image/jpeg")
        before = conn.execute("SELECT content_version FROM tracks WHERE id=?", (tid,)).fetchone()[0]
        replace_track_art(conn, tid, first)
        conn.commit()
        row = conn.execute(
            "SELECT art_id, picture_type, ordinal FROM track_art WHERE track_id=?", (tid,)
        ).fetchone()
        assert row == (first, 3, 0)
        after = conn.execute("SELECT content_version FROM tracks WHERE id=?", (tid,)).fetchone()[0]
        assert after > before
        second = upsert_art(conn, PNG, "image/png")
        replace_track_art(conn, tid, second)
        conn.commit()
        rows = conn.execute("SELECT art_id FROM track_art WHERE track_id=?", (tid,)).fetchall()
        assert rows == [(second,)]
    finally:
        conn.close()
