import os
import sqlite3

import pytest

pytest.importorskip("beets")

from beetsplug.musefs import MusefsPlugin  # noqa: E402


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
    def __init__(self, items):
        self._items = items

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
