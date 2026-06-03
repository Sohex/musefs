import sqlite3
from types import SimpleNamespace

import pytest

pytest.importorskip("beets")

from beetsplug import musefs as musefs_mod  # noqa: E402
from beetsplug.musefs import MusefsPlugin  # noqa: E402


class FakeLog:
    def __init__(self):
        self.warnings = []

    def warning(self, msg, *args):
        self.warnings.append((msg, args))


def _plugin(monkeypatch, *, sync_raises=None):
    """A MusefsPlugin with __init__ bypassed and its collaborators stubbed."""
    plugin = MusefsPlugin.__new__(MusefsPlugin)
    plugin._log = FakeLog()
    plugin._pending = [SimpleNamespace(path=b"/music/a.flac")]
    plugin._db_path = lambda: "/db.sqlite"
    plugin._autoscan = lambda: False
    plugin._prune_missing = lambda db: None

    def _sync(db, items):
        if sync_raises is not None:
            raise sync_raises

    plugin._sync = _sync
    return plugin


def test_reconcile_swallows_db_error_as_warning(monkeypatch):
    plugin = _plugin(monkeypatch, sync_raises=sqlite3.OperationalError("database is locked"))
    plugin._reconcile_pending()  # must NOT raise
    assert len(plugin._log.warnings) == 1


def test_reconcile_swallows_os_error_as_warning(monkeypatch):
    plugin = _plugin(monkeypatch, sync_raises=OSError("disk gone"))
    plugin._reconcile_pending()
    assert len(plugin._log.warnings) == 1


def test_reconcile_propagates_unexpected_error(monkeypatch):
    plugin = _plugin(monkeypatch, sync_raises=ValueError("a real bug"))
    with pytest.raises(ValueError):
        plugin._reconcile_pending()


def test_run_scan_passes_shared_timeout(monkeypatch):
    captured = {}

    def fake_run_scan(binary, db_path, targets, *, timeout=None):
        captured["targets"] = targets
        captured["timeout"] = timeout

    monkeypatch.setattr(musefs_mod, "run_scan", fake_run_scan)
    plugin = MusefsPlugin.__new__(MusefsPlugin)
    plugin._bin = lambda: "musefs"
    plugin._run_scan("/db.sqlite", ["/a.flac", "/b.flac"])

    assert captured["targets"] == ["/a.flac", "/b.flac"]  # one call, full list
    assert captured["timeout"] == musefs_mod.SCAN_TIMEOUT_SECONDS == 120
