import hashlib
import sqlite3

import pytest
from conftest import JPEG, PNG, insert_track

from musefs_common import (
    ArtDigestMismatch,
    connect,
    replace_tags,
    replace_track_art,
    sniff_mime,
    upsert_art,
)

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
        assert rows == [("genre", "Rock", 0), ("genre", "Pop", 1), ("title", "T", 0)]
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
            "SELECT key, value, value_blob, ordinal FROM tags "
            "WHERE track_id=? AND value_blob IS NOT NULL",
            (tid,),
        ).fetchall()
        assert blobs == [("priv", "", b"\x01\x02", 0)]
    finally:
        conn.close()


def test_upsert_art_is_content_addressed(db_path):
    conn = connect(db_path)
    try:
        first = upsert_art(conn, JPEG)
        again = upsert_art(conn, JPEG)  # same bytes -> same id
        conn.commit()
        assert first == again
        # The digest must be the one `musefs scan` computes (lowercase hex SHA-256
        # of the bytes), or a plugin's row never dedups against a scanner's.
        row = conn.execute("SELECT sha256, byte_len, data FROM art WHERE id=?", (first,)).fetchone()
        assert row == (hashlib.sha256(JPEG).hexdigest(), len(JPEG), JPEG)
        # And the row is the bytes and nothing else: everything that describes
        # one file's embedding of them lives on `track_art` (#716).
        cols = {r[1] for r in conn.execute("PRAGMA table_info(art)")}
        assert cols == {"id", "sha256", "byte_len", "data"}
    finally:
        conn.close()


def test_replace_track_art_sets_and_replaces_front_cover(db_path):
    conn = connect(db_path)
    try:
        tid = insert_track(conn, "/m/a.flac")
        first = upsert_art(conn, JPEG)
        before = conn.execute("SELECT content_version FROM tracks WHERE id=?", (tid,)).fetchone()[0]
        replace_track_art(conn, tid, [(first, 3, "", "image/jpeg")])
        conn.commit()
        row = conn.execute(
            "SELECT art_id, picture_type, mime, ordinal FROM track_art WHERE track_id=?", (tid,)
        ).fetchone()
        assert row == (first, 3, "image/jpeg", 0)
        after = conn.execute("SELECT content_version FROM tracks WHERE id=?", (tid,)).fetchone()[0]
        assert after > before
        second = upsert_art(conn, PNG)
        replace_track_art(conn, tid, [(second, 3, "", "image/png")])
        conn.commit()
        rows = conn.execute(
            "SELECT art_id, mime FROM track_art WHERE track_id=?", (tid,)
        ).fetchall()
        assert rows == [(second, "image/png")]
    finally:
        conn.close()


def test_replace_track_art_stores_the_dimensions_a_row_states(db_path):
    """A six-field row carries the link's width and height (musefs #737); a
    four-field row, beside it, still states none."""
    conn = connect(db_path)
    try:
        tid = insert_track(conn, "/m/a.flac")
        a = upsert_art(conn, JPEG)
        b = upsert_art(conn, PNG)
        replace_track_art(conn, tid, [(a, 3, "", "image/jpeg", 1200, 800), (b, 4, "", "image/png")])
        conn.commit()
        rows = conn.execute(
            "SELECT art_id, width, height, depth, colors FROM track_art "
            "WHERE track_id=? ORDER BY ordinal",
            (tid,),
        ).fetchall()
        assert rows == [(a, 1200, 800, 0, 0), (b, None, None, 0, 0)]
    finally:
        conn.close()


def test_replace_track_art_multiple_rows_ordered(db_path):
    conn = connect(db_path)
    try:
        tid = insert_track(conn, "/m/a.flac")
        a = upsert_art(conn, JPEG)
        b = upsert_art(conn, PNG)
        replace_track_art(conn, tid, [(a, 3, "", "image/jpeg"), (b, 4, "back", "image/png")])
        conn.commit()
        rows = conn.execute(
            "SELECT art_id, picture_type, description, mime, ordinal, "
            "width, height, depth, colors FROM track_art "
            "WHERE track_id=? ORDER BY ordinal",
            (tid,),
        ).fetchall()
        # Each link carries the mime of the picture it links, which is the whole
        # point of the column living here rather than on the shared `art` row.
        # The geometry is left unset ("not stated"), not guessed.
        assert rows == [
            (a, 3, "", "image/jpeg", 0, None, None, 0, 0),
            (b, 4, "back", "image/png", 1, None, None, 0, 0),
        ]
        replace_track_art(conn, tid, [(b, 3, "", "image/png")])
        conn.commit()
        rows = conn.execute("SELECT art_id FROM track_art WHERE track_id=?", (tid,)).fetchall()
        assert rows == [(b,)]
    finally:
        conn.close()


def test_upsert_art_refuses_a_row_whose_digest_names_other_bytes(db_path):
    """musefs #724: a row filed under the digest of bytes it does not hold is
    refused rather than returned, while an honest duplicate still dedups."""
    conn = connect(db_path)
    try:
        honest = upsert_art(conn, JPEG)
        real = b"REAL-IMAGE-X"
        conn.execute(
            "INSERT INTO art (sha256, byte_len, data) VALUES (?, 3, ?)",
            (hashlib.sha256(real).hexdigest(), b"YYY"),
        )
        planted = conn.execute("SELECT last_insert_rowid()").fetchone()[0]

        with pytest.raises(ArtDigestMismatch) as refused:
            upsert_art(conn, real)
        assert refused.value.art_id == planted
        assert refused.value.sha256 == hashlib.sha256(real).hexdigest()
        assert isinstance(refused.value, sqlite3.IntegrityError)

        assert upsert_art(conn, JPEG) == honest
    finally:
        conn.close()
