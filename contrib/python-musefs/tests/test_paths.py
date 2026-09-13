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


def test_a_non_utf8_byte_survives_the_key_instead_of_being_replaced(tmp_path):
    """The key must re-encode to the bytes on disk, which is what the store
    holds from schema v4 (#680).

    It used to normalize the byte to ``U+FFFD``, matching the scanner's own
    ``to_string_lossy()``. Both sides agreed and both were wrong: that mapping
    is not injective, so two distinct files collapsed onto one key.
    """
    raw = os.fsencode(str(tmp_path)) + b"/\xff.flac"
    key = realpath_key(raw)
    assert "\ufffd" not in key, "the replacement character is a lost byte"
    assert os.fsencode(key) == os.path.realpath(raw)


def test_paths_differing_only_in_undecodable_bytes_give_different_keys(tmp_path):
    """The property the old form broke. Two files, one key, one row — and the
    scan reported success."""
    base = os.fsencode(str(tmp_path))
    a = realpath_key(base + b"/bad\x80name.flac")
    b = realpath_key(base + b"/bad\x81name.flac")
    assert a != b
    assert os.fsencode(a) != os.fsencode(b)
