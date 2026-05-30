import sqlite3

import pytest

from musefs._core import (
    SchemaMismatch,
    check_schema_version,
    realpath_key,
    sniff_mime,
)


def test_schema_guard_rejects_wrong_version():
    conn = sqlite3.connect(":memory:")
    with pytest.raises(SchemaMismatch):
        check_schema_version(conn)


def test_schema_guard_accepts_v1():
    conn = sqlite3.connect(":memory:")
    conn.execute("PRAGMA user_version = 1")
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
