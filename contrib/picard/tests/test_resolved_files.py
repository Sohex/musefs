import pytest

pytest.importorskip("picard")


def _capture_debug(monkeypatch, musefs):
    logged = []
    monkeypatch.setattr(musefs.log, "debug", lambda fmt, *a: logged.append(fmt % a))
    return logged


def test_duplicate_realpath_is_dropped_and_logged(monkeypatch, fake_file):
    """Two distinct Files resolving to one realpath key: first wins, and the
    drop is visible at debug level instead of silent."""
    import musefs

    first = fake_file("/music/a.flac", None)
    second = fake_file("/music/a.flac", None)
    logged = _capture_debug(monkeypatch, musefs)

    resolved = musefs._resolved_files([first, second])

    assert list(resolved.values()) == [first]
    assert len(logged) == 1
    assert "duplicate" in logged[0]


def test_same_file_yielded_twice_is_silent(monkeypatch, fake_file):
    """The same File object re-yielded by overlapping selections is expected;
    it is deduplicated without a log line."""
    import musefs

    f = fake_file("/music/a.flac", None)
    logged = _capture_debug(monkeypatch, musefs)

    resolved = musefs._resolved_files([f, f])

    assert list(resolved.values()) == [f]
    assert logged == []
