import os

from musefs_common.paths import realpath_key


def test_returns_absolute_canonical_str(tmp_path):
    f = tmp_path / "a.flac"
    f.write_bytes(b"x")
    key = realpath_key(str(f))
    assert key == os.path.realpath(str(f))
    assert isinstance(key, str)


def test_accepts_bytes_path(tmp_path):
    f = tmp_path / "b.flac"
    f.write_bytes(b"x")
    key = realpath_key(os.fsencode(str(f)))
    assert isinstance(key, str)
    assert key.endswith("b.flac")


def test_non_utf8_byte_maps_to_replacement_char(tmp_path):
    raw = os.fsencode(str(tmp_path)) + b"/\xff.flac"
    key = realpath_key(raw)
    assert "\ufffd" in key
