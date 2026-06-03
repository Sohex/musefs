import sqlite3

import pytest

from musefs._core import (
    DIRECT_FIELDS,
    SchemaMismatch,
    check_schema_version,
    realpath_key,
    replace_tags,
    sniff_mime,
)


def test_schema_guard_rejects_wrong_version():
    conn = sqlite3.connect(":memory:")
    with pytest.raises(SchemaMismatch):
        check_schema_version(conn)


def test_schema_guard_accepts_v2():
    conn = sqlite3.connect(":memory:")
    conn.execute("PRAGMA user_version = 2")
    check_schema_version(conn)  # must not raise


def test_realpath_key_returns_absolute_str(tmp_path):
    f = tmp_path / "a.flac"
    f.write_bytes(b"x")
    key = realpath_key(str(f))
    assert key == str(f.resolve())
    assert isinstance(key, str)


def test_realpath_key_accepts_bytes(tmp_path):
    import os

    f = tmp_path / "a.flac"
    f.write_bytes(b"x")
    key = realpath_key(os.fsencode(str(f)))
    assert key == str(f.resolve())


def test_sniff_mime_magic_bytes():
    assert sniff_mime(b"\xff\xd8\xff\xe0junk", "x") == "image/jpeg"
    assert sniff_mime(b"\x89PNG\r\n\x1a\n" + b"\x00" * 8, "x") == "image/png"
    assert sniff_mime(b"RIFF\x00\x00\x00\x00WEBPxxxx", "x") == "image/webp"


def test_sniff_mime_extension_fallback():
    assert sniff_mime(b"nope", "cover.PNG") == "image/png"
    assert sniff_mime(b"nope", "cover.bin") == "application/octet-stream"


def test_replace_tags_preserves_binary_rows(db_path, make_track):
    # Regression test for #82: a plugin sync must not delete scanner-written
    # binary tags (value_blob NOT NULL).
    tid = make_track("/music/a.flac")
    conn = sqlite3.connect(db_path)
    try:
        conn.execute(
            "INSERT INTO tags (track_id, key, value, value_blob, ordinal) "
            "VALUES (?, 'APPLICATION', '', ?, 0)",
            (tid, b"\x00\x01\x02binary"),
        )
        conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (?, 'title', 'Old', 0)",
            (tid,),
        )
        conn.commit()

        replace_tags(conn, tid, [("title", "New")])
        conn.commit()

        binary = conn.execute(
            "SELECT value, value_blob FROM tags WHERE track_id=? AND key='APPLICATION'",
            (tid,),
        ).fetchall()
        assert binary == [("", b"\x00\x01\x02binary")]

        titles = conn.execute(
            "SELECT value FROM tags WHERE track_id=? AND key='title'", (tid,)
        ).fetchall()
        assert titles == [("New",)]
    finally:
        conn.close()


def test_default_vocabulary_disjoint_from_binary_keys():
    binary_keys = {"APPLICATION", "CUESHEET", "PRIV", "GEOB", "APIC"}
    text_keys = set(DIRECT_FIELDS.values())
    assert text_keys.isdisjoint(binary_keys)
    assert not any(k.startswith("----") for k in text_keys)
