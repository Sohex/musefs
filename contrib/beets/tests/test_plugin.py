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


def test_commands_exposes_musefs_subcommand():
    plugin = MusefsPlugin()
    names = [c.name for c in plugin.commands()]
    assert "musefs" in names


def test_command_run_syncs(db_path, make_track, fake_item, monkeypatch):
    tid = make_track("/music/a.flac")
    plugin = MusefsPlugin()

    # Point config at our temp DB and an empty fields override.
    monkeypatch.setattr(
        plugin, "config",
        FakeConfigView({"db": db_path, "fields": {}}),
        raising=False,
    )

    cmd = next(c for c in plugin.commands() if c.name == "musefs")
    opts, _ = cmd.parser.parse_args([])
    lib = FakeLib([fake_item(os.fsencode("/music/a.flac"), title="Song")])

    cmd.func(lib, opts, [])

    conn = sqlite3.connect(db_path)
    try:
        title = conn.execute(
            "SELECT value FROM tags WHERE track_id=? AND key='title'", (tid,)
        ).fetchone()[0]
        assert title == "Song"
    finally:
        conn.close()


def test_command_strips_leading_sync_verb():
    plugin = MusefsPlugin()
    assert plugin._query_from_args(["sync", "artist:Band"]) == ["artist:Band"]
    assert plugin._query_from_args(["artist:Band"]) == ["artist:Band"]
    assert plugin._query_from_args([]) == []


def test_album_imported_without_db_skips_gracefully(fake_item, fake_album, monkeypatch):
    # Regression: _on_album_imported must not pass a None db path into _sync
    # (which would TypeError in os.path.exists). With no db it should warn+skip.
    plugin = MusefsPlugin()
    monkeypatch.setattr(
        plugin, "config", FakeConfigView({"db": None, "fields": {}}), raising=False
    )
    album = fake_album(items=[fake_item(os.fsencode("/music/a.flac"), title="X")])
    plugin._on_album_imported(album=album)  # must not raise
    plugin._on_album_imported(album=None)   # must not raise


def _autoscan_plugin(db_path, monkeypatch):
    """A plugin with autoscan on, config pointed at db_path, and _run_scan
    replaced by a recorder (so tests don't need the real musefs binary)."""
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


def test_command_autoscan_scans_matched_files(db_path, make_track, fake_item, monkeypatch):
    make_track("/music/a.flac")
    plugin, calls = _autoscan_plugin(db_path, monkeypatch)
    cmd, opts, args = _musefs_cmd(plugin, ["title:Song"])  # a query -> matched files
    lib = FakeLib([fake_item(os.fsencode("/music/a.flac"), title="Song")])
    cmd.func(lib, opts, args)

    assert calls == [["/music/a.flac"]]  # scanned the matched file, not the dir
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
    lib = FakeLib([fake_item(os.fsencode("/music/a.flac"))])
    cmd.func(lib, opts, args)

    assert calls == []  # dry-run must not mutate the DB via scan


def test_hook_autoscans_then_syncs(db_path, make_track, fake_item, monkeypatch):
    make_track("/music/a.flac")
    plugin, calls = _autoscan_plugin(db_path, monkeypatch)
    plugin._on_item_imported(item=fake_item(os.fsencode("/music/a.flac"), title="Song"))

    assert calls == [["/music/a.flac"]]
    conn = sqlite3.connect(db_path)
    try:
        assert conn.execute(
            "SELECT value FROM tags WHERE key='title'"
        ).fetchone()[0] == "Song"
    finally:
        conn.close()


def test_hook_best_effort_on_scan_failure(db_path, fake_item, monkeypatch):
    from beets import ui

    plugin, _ = _autoscan_plugin(db_path, monkeypatch)

    def boom(db, targets):
        raise ui.UserError("scan blew up")

    monkeypatch.setattr(plugin, "_run_scan", boom)
    # A passive hook must swallow the error (warn), never abort the beets op.
    plugin._on_after_write(item=fake_item(os.fsencode("/music/a.flac"), title="X"))
