import os
import sqlite3

import pytest

pytest.importorskip("beets")

from beets.library import Item  # noqa: E402

from beetsplug._core import map_fields  # noqa: E402
from beetsplug.musefs import MusefsPlugin  # noqa: E402


def test_map_fields_handles_real_beets_multivalue():
    # Regression: beets 2.x stores genre/composer as multi-valued genres/
    # composers (lists), not scalars. FakeItem hid this; a real Item exposes it.
    it = Item()
    it.title = "Song"
    it.genres = ["Rock", "Indie"]
    it.composers = ["J.S. Bach"]
    grouped = {}
    for key, value in map_fields(it):
        grouped.setdefault(key, []).append(value)
    assert grouped["title"] == ["Song"]
    assert grouped["genre"] == ["Rock", "Indie"]  # expanded, not "['Rock', ...]"
    assert grouped["composer"] == ["J.S. Bach"]


class FakeConfigView:
    def __init__(self, data):
        self._data = data

    def __getitem__(self, key):
        return FakeConfigView(self._data.get(key))

    def get(self, template=None):
        return self._data

    def as_filename(self):
        return os.path.expanduser(self._data)


class FakeLib:
    def __init__(self, items, directory=b"/music"):
        self._items = items
        self.directory = directory  # beets stores the music dir as bytes

    def items(self, query):
        return self._items


def _real_track(tmp_path, make_track, fake_item, name="a.flac", **fields):
    """Create a real file + its track row + a matching fake item. A real path
    matters now that sync prunes rows whose backing file is missing."""
    p = tmp_path / name
    p.write_bytes(b"x")
    real = os.path.realpath(str(p))
    tid = make_track(real)
    return real, tid, fake_item(os.fsencode(real), **fields)


def _autoscan_plugin(db_path, monkeypatch):
    """Plugin with autoscan on, config -> db_path, and _run_scan replaced by a
    recorder (so tests don't need the real musefs binary)."""
    plugin = MusefsPlugin()
    monkeypatch.setattr(
        plugin, "config",
        FakeConfigView({"db": db_path, "fields": {}, "autoscan": True}),
        raising=False,
    )
    calls = []
    monkeypatch.setattr(
        plugin, "_run_scan", lambda db, targets: calls.append(list(targets))
    )
    return plugin, calls


def _musefs_cmd(plugin, argv):
    cmd = next(c for c in plugin.commands() if c.name == "musefs")
    opts, args = cmd.parser.parse_args(argv)
    return cmd, opts, args


def test_commands_exposes_musefs_subcommand():
    plugin = MusefsPlugin()
    assert "musefs" in [c.name for c in plugin.commands()]


def test_command_strips_leading_sync_verb():
    plugin = MusefsPlugin()
    assert plugin._query_from_args(["sync", "artist:Band"]) == ["artist:Band"]
    assert plugin._query_from_args(["artist:Band"]) == ["artist:Band"]
    assert plugin._query_from_args([]) == []


def test_command_run_syncs(db_path, make_track, fake_item, tmp_path, monkeypatch):
    real, tid, item = _real_track(tmp_path, make_track, fake_item, title="Song")
    plugin = MusefsPlugin()
    monkeypatch.setattr(
        plugin, "config", FakeConfigView({"db": db_path, "fields": {}}), raising=False
    )
    cmd, opts, args = _musefs_cmd(plugin, [])
    cmd.func(FakeLib([item]), opts, args)

    conn = sqlite3.connect(db_path)
    try:
        assert conn.execute(
            "SELECT value FROM tags WHERE track_id=? AND key='title'", (tid,)
        ).fetchone()[0] == "Song"
    finally:
        conn.close()


def test_command_autoscan_scans_matched_files(db_path, make_track, fake_item, tmp_path, monkeypatch):
    real, tid, item = _real_track(tmp_path, make_track, fake_item, title="Song")
    plugin, calls = _autoscan_plugin(db_path, monkeypatch)
    cmd, opts, args = _musefs_cmd(plugin, ["title:Song"])  # a query -> matched files
    cmd.func(FakeLib([item]), opts, args)

    assert calls == [[real]]  # scanned the matched file, not the directory
    conn = sqlite3.connect(db_path)
    try:
        assert conn.execute(
            "SELECT value FROM tags WHERE key='title'"
        ).fetchone()[0] == "Song"
    finally:
        conn.close()


def test_command_full_sync_scans_directory(db_path, fake_item, monkeypatch):
    plugin, calls = _autoscan_plugin(db_path, monkeypatch)
    cmd, opts, args = _musefs_cmd(plugin, [])  # no query == full sync
    lib = FakeLib([fake_item(os.fsencode("/music/a.flac"))], directory=b"/music")
    cmd.func(lib, opts, args)

    assert calls == [["/music"]]  # one scan of the whole music directory


def test_command_dry_run_skips_autoscan(db_path, fake_item, monkeypatch):
    plugin, calls = _autoscan_plugin(db_path, monkeypatch)
    cmd, opts, args = _musefs_cmd(plugin, ["-n"])
    cmd.func(FakeLib([fake_item(os.fsencode("/music/a.flac"))]), opts, args)

    assert calls == []  # dry-run must not mutate the DB via scan


def test_command_prunes_missing_rows(db_path, make_track, fake_item, monkeypatch):
    make_track("/gone/x.flac")  # a stale row: its backing file does not exist
    plugin, _ = _autoscan_plugin(db_path, monkeypatch)
    cmd, opts, args = _musefs_cmd(plugin, ["q"])
    cmd.func(FakeLib([fake_item(os.fsencode("/music/a.flac"))]), opts, args)

    conn = sqlite3.connect(db_path)
    try:
        paths = [r[0] for r in conn.execute("SELECT backing_path FROM tracks")]
        assert "/gone/x.flac" not in paths  # pruned because the file is gone
    finally:
        conn.close()


def test_reconcile_at_cli_exit_syncs_recorded_items(db_path, make_track, fake_item, tmp_path, monkeypatch):
    real, tid, item = _real_track(tmp_path, make_track, fake_item, title="Song")
    plugin, calls = _autoscan_plugin(db_path, monkeypatch)
    plugin._record(item=item)     # an import/write hook fired during the command
    plugin._reconcile_pending()   # cli_exit

    assert calls == [[real]]
    conn = sqlite3.connect(db_path)
    try:
        assert conn.execute(
            "SELECT value FROM tags WHERE track_id=? AND key='title'", (tid,)
        ).fetchone()[0] == "Song"
    finally:
        conn.close()


def test_reconcile_prunes_moved_away_row(db_path, make_track, fake_item, tmp_path, monkeypatch):
    # A previously-scanned file moved away (stale row); the item now lives at a
    # new real path. Reconcile syncs the new path and prunes the stale row.
    make_track("/old/moved-away.flac")  # stale: file gone
    real, tid, item = _real_track(tmp_path, make_track, fake_item, title="Now")
    plugin, _ = _autoscan_plugin(db_path, monkeypatch)
    plugin._record(item=item)
    plugin._reconcile_pending()

    conn = sqlite3.connect(db_path)
    try:
        paths = [r[0] for r in conn.execute("SELECT backing_path FROM tracks")]
        assert "/old/moved-away.flac" not in paths  # stale row pruned
        assert real in paths                         # new path kept + synced
        assert conn.execute(
            "SELECT value FROM tags WHERE track_id=? AND key='title'", (tid,)
        ).fetchone()[0] == "Now"
    finally:
        conn.close()


def test_reconcile_without_db_skips_gracefully(fake_item, fake_album, monkeypatch):
    # Regression: reconcile must not pass a None db path downstream. With no db
    # configured it should warn + skip, never raise (which would abort beets).
    plugin = MusefsPlugin()
    monkeypatch.setattr(
        plugin, "config", FakeConfigView({"db": None, "fields": {}}), raising=False
    )
    plugin._record_album(album=fake_album(items=[fake_item(os.fsencode("/music/a.flac"))]))
    plugin._reconcile_pending()       # must not raise
    plugin._record_album(album=None)  # records nothing
    plugin._reconcile_pending()       # no-op


def test_reconcile_best_effort_on_scan_failure(db_path, fake_item, monkeypatch):
    from beets import ui

    plugin, _ = _autoscan_plugin(db_path, monkeypatch)

    def boom(db, targets):
        raise ui.UserError("scan blew up")

    monkeypatch.setattr(plugin, "_run_scan", boom)
    plugin._record(item=fake_item(os.fsencode("/music/a.flac")))
    # A passive hook must swallow the error (warn), never abort the beets op.
    plugin._reconcile_pending()
