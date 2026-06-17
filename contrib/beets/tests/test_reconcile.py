import sqlite3
from types import SimpleNamespace

import pytest

pytest.importorskip("beets")

from conftest import insert_track  # noqa: E402,F401
from musefs_common import connect as musefs_connect  # noqa: E402

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
    plugin._restore_backing = lambda: False

    def _sync(db, items, **kwargs):
        if sync_raises is not None:
            raise sync_raises

    plugin._sync = _sync
    return plugin


def _capture_prints(monkeypatch):
    prints = []
    monkeypatch.setattr(musefs_mod.ui, "print_", lambda *a: prints.append(a))
    return prints


def test_reconcile_swallows_db_error_as_warning(monkeypatch):
    plugin = _plugin(monkeypatch, sync_raises=sqlite3.OperationalError("database is locked"))
    prints = _capture_prints(monkeypatch)
    plugin._reconcile_pending()  # must NOT raise
    assert len(plugin._log.warnings) == 1
    assert prints == []  # transient failures stay quiet


def test_reconcile_swallows_os_error_as_warning(monkeypatch):
    plugin = _plugin(monkeypatch, sync_raises=OSError("disk gone"))
    plugin._reconcile_pending()
    assert len(plugin._log.warnings) == 1


def test_reconcile_propagates_unexpected_error(monkeypatch):
    plugin = _plugin(monkeypatch, sync_raises=ValueError("a real bug"))
    with pytest.raises(ValueError):
        plugin._reconcile_pending()


def test_reconcile_surfaces_readonly_db_loudly(monkeypatch):
    plugin = _plugin(
        monkeypatch,
        sync_raises=sqlite3.OperationalError("attempt to write a readonly database"),
    )
    prints = _capture_prints(monkeypatch)
    plugin._reconcile_pending()  # still must NOT raise
    assert plugin._log.warnings == []  # not buried in a default-hidden warning
    assert len(prints) == 1
    msg = prints[0][0]
    assert "/db.sqlite" in msg and "not synced" in msg


def test_reconcile_surfaces_unwritable_dir_loudly(monkeypatch):
    # A non-writable DB directory surfaces as SQLite "unable to open database
    # file" when it tries to create the -wal/-shm files — also persistent.
    plugin = _plugin(
        monkeypatch,
        sync_raises=sqlite3.OperationalError("unable to open database file"),
    )
    prints = _capture_prints(monkeypatch)
    plugin._reconcile_pending()
    assert plugin._log.warnings == []
    assert len(prints) == 1


def test_reconcile_surfaces_permission_error_loudly(monkeypatch):
    plugin = _plugin(monkeypatch, sync_raises=PermissionError(13, "Permission denied"))
    prints = _capture_prints(monkeypatch)
    plugin._reconcile_pending()
    assert plugin._log.warnings == []
    assert len(prints) == 1
    assert "/db.sqlite" in prints[0][0]


def test_run_scan_passes_shared_timeout(monkeypatch):
    captured = {}

    def fake_run_scan(binary, db_path, targets, *, revalidate=False, timeout=None):
        captured["targets"] = targets
        captured["revalidate"] = revalidate
        captured["timeout"] = timeout

    monkeypatch.setattr(musefs_mod, "run_scan", fake_run_scan)
    plugin = MusefsPlugin.__new__(MusefsPlugin)
    plugin._bin = lambda: "musefs"
    plugin._run_scan("/db.sqlite", ["/a.flac", "/b.flac"])

    assert captured["targets"] == ["/a.flac", "/b.flac"]  # one call, full list
    assert captured["revalidate"] is False  # reconcile/plain scan never revalidates
    assert captured["timeout"] == musefs_mod.SCAN_TIMEOUT_SECONDS == 120


def test_removal_only_command_does_not_prune(db_path, tmp_path):
    # Pruning is a deliberate act (#538): the plugin no longer reacts to
    # item/album removals, and a passive cli_exit with no writes pending is a
    # no-op. A removed-and-deleted backing file's row survives until an explicit
    # `beet musefs --revalidate` / `musefs scan --revalidate` — a transient mount
    # blip can't mass-delete.
    gone = tmp_path / "gone.flac"  # never created on disk == already deleted
    conn = musefs_connect(db_path)
    try:
        tid = insert_track(conn, str(gone))
        conn.commit()
    finally:
        conn.close()

    plugin = MusefsPlugin.__new__(MusefsPlugin)
    plugin._log = FakeLog()
    plugin._pending = []
    plugin._db_path = lambda: db_path
    plugin._reconcile_pending()  # no writes pending -> no-op (no prune)

    conn = musefs_connect(db_path)
    try:
        assert conn.execute("SELECT COUNT(*) FROM tracks WHERE id=?", (tid,)).fetchone()[0] == 1
    finally:
        conn.close()
