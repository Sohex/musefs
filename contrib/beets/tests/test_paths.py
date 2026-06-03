import os

from musefs_common import realpath_key


def test_str_path_absolutised(tmp_path):
    f = tmp_path / "a.flac"
    f.write_bytes(b"x")
    assert realpath_key(str(f)) == os.path.realpath(str(f))


def test_bytes_path_returns_str(tmp_path):
    f = tmp_path / "a.flac"
    f.write_bytes(b"x")
    key = realpath_key(os.fsencode(str(f)))
    assert isinstance(key, str)
    assert key == os.path.realpath(str(f))


def test_relative_and_dotdot_resolved(tmp_path, monkeypatch):
    (tmp_path / "sub").mkdir()
    f = tmp_path / "sub" / "a.flac"
    f.write_bytes(b"x")
    monkeypatch.chdir(tmp_path)
    assert realpath_key("sub/../sub/a.flac") == os.path.realpath(str(f))


def test_symlink_resolved(tmp_path):
    real = tmp_path / "real.flac"
    real.write_bytes(b"x")
    link = tmp_path / "link.flac"
    link.symlink_to(real)
    assert realpath_key(str(link)) == os.path.realpath(str(real))


def test_non_utf8_bytes_replaced_like_rust(tmp_path):
    # A non-UTF-8 filename: Rust's to_string_lossy yields U+FFFD. We must match
    # that, not surrogate-escape it (\udcff), or the key would silently mismatch.
    raw = os.fsencode(str(tmp_path)) + b"/\xff.flac"
    with open(raw, "wb") as fh:
        fh.write(b"x")
    key = realpath_key(raw)
    assert "�" in key
    assert "\udcff" not in key
