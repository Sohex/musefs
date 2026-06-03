import os

import pytest

pytest.importorskip("beets")

from beets.library import Item  # noqa: E402
from musefs_common import connect  # noqa: E402

from beetsplug._core import map_fields  # noqa: E402
from beetsplug.musefs import MusefsPlugin  # noqa: E402


def test_map_fields_handles_real_beets_multivalue():
    """Verify map_fields expands multi-valued beets fields."""
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
        """Store fake config data."""
        self._data = data

    def __getitem__(self, key):
        """Return a nested FakeConfigView."""
        return FakeConfigView(self._data.get(key))

    def get(self, template=None):
        """Return the raw data dict."""
        return self._data

    def as_filename(self):
        """Expand home directory in the data string."""
        return os.path.expanduser(self._data)


class FakeLib:
    def __init__(self, items, directory=b"/music"):
        """Store fake items and directory."""
        self._items = items
        self.directory = directory  # beets stores the music dir as bytes

    def items(self, query):
        """Return the stored items regardless of query."""
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
        plugin,
        "config",
        FakeConfigView({"db": db_path, "fields": {}, "autoscan": True}),
        raising=False,
    )
    calls = []
    monkeypatch.setattr(plugin, "_run_scan", lambda db, targets: calls.append(list(targets)))
    return plugin, calls


def _musefs_cmd(plugin, argv):
    """Parse args for the musefs subcommand and return (cmd, opts, args)."""
    cmd = next(c for c in plugin.commands() if c.name == "musefs")
    opts, args = cmd.parser.parse_args(argv)
    return cmd, opts, args


def test_commands_exposes_musefs_subcommand():
    """Verify the plugin registers a musefs command."""
    plugin = MusefsPlugin()
    assert "musefs" in [c.name for c in plugin.commands()]


def test_command_strips_leading_sync_verb():
    """Verify leading 'sync' verb is stripped from query."""
    plugin = MusefsPlugin()
    assert plugin._query_from_args(["sync", "artist:Band"]) == ["artist:Band"]
    assert plugin._query_from_args(["artist:Band"]) == ["artist:Band"]
    assert plugin._query_from_args([]) == []


def test_command_run_syncs(db_path, make_track, fake_item, tmp_path, monkeypatch):
    """Verify the musefs command syncs tags to the DB."""
    real, tid, item = _real_track(tmp_path, make_track, fake_item, title="Song")
    plugin = MusefsPlugin()
    monkeypatch.setattr(
        plugin, "config", FakeConfigView({"db": db_path, "fields": {}}), raising=False
    )
    cmd, opts, args = _musefs_cmd(plugin, [])
    cmd.func(FakeLib([item]), opts, args)

    conn = connect(db_path)
    try:
        assert (
            conn.execute(
                "SELECT value FROM tags WHERE track_id=? AND key='title'", (tid,)
            ).fetchone()[0]
            == "Song"
        )
    finally:
        conn.close()


def test_command_autoscan_scans_matched_files(
    db_path,
    make_track,
    fake_item,
    tmp_path,
    monkeypatch,
):
    """Verify autoscan scans matched files not the dir."""
    real, tid, item = _real_track(tmp_path, make_track, fake_item, title="Song")
    plugin, calls = _autoscan_plugin(db_path, monkeypatch)
    cmd, opts, args = _musefs_cmd(plugin, ["title:Song"])  # a query -> matched files
    cmd.func(FakeLib([item]), opts, args)

    assert calls == [[real]]  # scanned the matched file, not the directory
    conn = connect(db_path)
    try:
        assert conn.execute("SELECT value FROM tags WHERE key='title'").fetchone()[0] == "Song"
    finally:
        conn.close()


def test_command_full_sync_scans_directory(db_path, fake_item, monkeypatch):
    """Verify full sync scans the music directory."""
    plugin, calls = _autoscan_plugin(db_path, monkeypatch)
    cmd, opts, args = _musefs_cmd(plugin, [])  # no query == full sync
    lib = FakeLib([fake_item(os.fsencode("/music/a.flac"))], directory=b"/music")
    cmd.func(lib, opts, args)

    assert calls == [["/music"]]  # one scan of the whole music directory


def test_command_dry_run_skips_autoscan(db_path, fake_item, monkeypatch):
    """Verify dry-run does not trigger autoscan."""
    plugin, calls = _autoscan_plugin(db_path, monkeypatch)
    cmd, opts, args = _musefs_cmd(plugin, ["-n"])
    cmd.func(FakeLib([fake_item(os.fsencode("/music/a.flac"))]), opts, args)

    assert calls == []  # dry-run must not mutate the DB via scan


def test_command_query_preserves_unrelated_missing_rows(
    db_path,
    make_track,
    fake_item,
    tmp_path,
    monkeypatch,
):
    """Verify query-scoped prune keeps unrelated rows."""
    make_track("/gone/x.flac")  # a stale row: its backing file does not exist
    real, _tid, item = _real_track(tmp_path, make_track, fake_item, title="Song")
    plugin, _ = _autoscan_plugin(db_path, monkeypatch)
    cmd, opts, args = _musefs_cmd(plugin, ["title:Song"])
    cmd.func(FakeLib([item]), opts, args)

    conn = connect(db_path)
    try:
        paths = [r[0] for r in conn.execute("SELECT backing_path FROM tracks")]
        assert "/gone/x.flac" in paths
        assert real in paths
    finally:
        conn.close()


def test_command_full_sync_prunes_missing_rows(db_path, make_track, fake_item, monkeypatch):
    """Verify full sync prunes stale rows."""
    make_track("/gone/x.flac")  # a stale row: its backing file does not exist
    plugin, _ = _autoscan_plugin(db_path, monkeypatch)
    cmd, opts, args = _musefs_cmd(plugin, [])
    cmd.func(FakeLib([fake_item(os.fsencode("/music/a.flac"))]), opts, args)

    conn = connect(db_path)
    try:
        paths = [r[0] for r in conn.execute("SELECT backing_path FROM tracks")]
        assert "/gone/x.flac" not in paths
    finally:
        conn.close()


def test_reconcile_at_cli_exit_syncs_recorded_items(
    db_path,
    make_track,
    fake_item,
    tmp_path,
    monkeypatch,
):
    """Verify cli_exit reconcile syncs recorded items."""
    real, tid, item = _real_track(tmp_path, make_track, fake_item, title="Song")
    plugin, calls = _autoscan_plugin(db_path, monkeypatch)
    plugin._record(item=item)  # an import/write hook fired during the command
    plugin._reconcile_pending()  # cli_exit

    assert calls == [[real]]
    conn = connect(db_path)
    try:
        assert (
            conn.execute(
                "SELECT value FROM tags WHERE track_id=? AND key='title'", (tid,)
            ).fetchone()[0]
            == "Song"
        )
    finally:
        conn.close()


def test_reconcile_prunes_moved_away_row(db_path, make_track, fake_item, tmp_path, monkeypatch):
    """Verify reconcile prunes rows whose backing file moved."""
    # Reconcile uses a full-table prune so stale rows from renames/moves
    # (whose backing file no longer exists) are cleaned up even though the
    # pending items only carry the new path.
    make_track("/old/moved-away.flac")  # stale: file gone
    real, tid, item = _real_track(tmp_path, make_track, fake_item, title="Now")
    plugin, _ = _autoscan_plugin(db_path, monkeypatch)
    plugin._record(item=item)
    plugin._reconcile_pending()

    conn = connect(db_path)
    try:
        paths = [r[0] for r in conn.execute("SELECT backing_path FROM tracks")]
        assert "/old/moved-away.flac" not in paths  # stale row pruned
        assert real in paths  # new path kept + synced
        assert (
            conn.execute(
                "SELECT value FROM tags WHERE track_id=? AND key='title'", (tid,)
            ).fetchone()[0]
            == "Now"
        )
    finally:
        conn.close()


def test_reconcile_without_db_skips_gracefully(fake_item, fake_album, monkeypatch):
    """Verify reconcile skips gracefully with no DB."""
    # Regression: reconcile must not pass a None db path downstream. With no db
    # configured it should warn + skip, never raise (which would abort beets).
    plugin = MusefsPlugin()
    monkeypatch.setattr(plugin, "config", FakeConfigView({"db": None, "fields": {}}), raising=False)
    plugin._record_album(album=fake_album(items=[fake_item(os.fsencode("/music/a.flac"))]))
    plugin._reconcile_pending()  # must not raise
    plugin._record_album(album=None)  # records nothing
    plugin._reconcile_pending()  # no-op


def test_reconcile_best_effort_on_scan_failure(db_path, fake_item, monkeypatch):
    """Verify reconcile swallows scan errors."""
    from beets import ui

    plugin, _ = _autoscan_plugin(db_path, monkeypatch)

    def boom(db, targets):
        """Raise a UserError to simulate scan failure."""
        raise ui.UserError("scan blew up")

    monkeypatch.setattr(plugin, "_run_scan", boom)
    plugin._record(item=fake_item(os.fsencode("/music/a.flac")))
    # A passive hook must swallow the error (warn), never abort the beets op.
    plugin._reconcile_pending()
