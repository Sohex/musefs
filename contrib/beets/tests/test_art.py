import hashlib

from beetsplug._core import (
    connect,
    replace_track_art,
    sniff_mime,
    upsert_art,
)

JPEG = b"\xff\xd8\xff\xe0" + b"\x00" * 16
PNG = b"\x89PNG\r\n\x1a\n" + b"\x00" * 16


def test_sniff_mime_magic_bytes():
    assert sniff_mime(JPEG, "/x/cover.bin") == "image/jpeg"
    assert sniff_mime(PNG, "/x/cover.bin") == "image/png"


def test_sniff_mime_extension_fallback():
    assert sniff_mime(b"garbage", "/x/cover.jpg") == "image/jpeg"
    assert sniff_mime(b"garbage", "/x/cover.png") == "image/png"
    assert sniff_mime(b"garbage", "/x/cover.bin") == "application/octet-stream"


def test_upsert_art_dedup(db_path):
    conn = connect(db_path)
    try:
        a = upsert_art(conn, JPEG, "image/jpeg")
        b = upsert_art(conn, JPEG, "image/jpeg")
        conn.commit()
        assert a == b
        count = conn.execute("SELECT COUNT(*) FROM art").fetchone()[0]
        assert count == 1
        sha = conn.execute("SELECT sha256 FROM art WHERE id=?", (a,)).fetchone()[0]
        assert sha == hashlib.sha256(JPEG).hexdigest()
    finally:
        conn.close()


def test_replace_track_art_links_front_cover(db_path, make_track):
    tid = make_track("/music/a.flac")
    conn = connect(db_path)
    try:
        art_id = upsert_art(conn, JPEG, "image/jpeg")
        before = conn.execute(
            "SELECT content_version FROM tracks WHERE id=?", (tid,)
        ).fetchone()[0]
        replace_track_art(conn, tid, art_id)
        conn.commit()
        row = conn.execute(
            "SELECT art_id, picture_type, description, ordinal FROM track_art "
            "WHERE track_id=?", (tid,)
        ).fetchone()
        assert row == (art_id, 3, "", 0)
        after = conn.execute(
            "SELECT content_version FROM tracks WHERE id=?", (tid,)
        ).fetchone()[0]
        assert after > before
    finally:
        conn.close()
