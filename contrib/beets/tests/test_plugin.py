import os

import pytest

pytest.importorskip("beets")

from beets.library import Item  # noqa: E402
from conftest import FakeItem, insert_track  # noqa: E402
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
        FakeConfigView({"db": db_path, "fields": {}, "autoscan": True, "write_path": True}),
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
    # A tuple must not leak through the sync branch as a tuple slice.
    result = plugin._query_from_args(("sync", "artist:Band"))
    assert result == ["artist:Band"]
    assert type(result) is list


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


def test_reconcile_does_not_prune_only_syncs(db_path, make_track, fake_item, tmp_path, monkeypatch):
    """Pruning is a deliberate act (#538): the passive cli_exit reconcile syncs
    touched items but never prunes, so a transient backing-storage loss can no
    longer mass-delete plugin metadata. Stale rows are left for the explicit
    ``beet musefs`` command / ``musefs scan``."""
    make_track("/old/moved-away.flac")  # stale: backing file gone
    real, tid, item = _real_track(tmp_path, make_track, fake_item, title="Now")
    plugin, _ = _autoscan_plugin(db_path, monkeypatch)
    plugin._record(item=item)
    plugin._reconcile_pending()

    conn = connect(db_path)
    try:
        paths = [r[0] for r in conn.execute("SELECT backing_path FROM tracks")]
        assert "/old/moved-away.flac" in paths  # NOT pruned — reconcile never prunes
        assert real in paths  # new path kept + synced
        assert (
            conn.execute(
                "SELECT value FROM tags WHERE track_id=? AND key='title'", (tid,)
            ).fetchone()[0]
            == "Now"
        )
    finally:
        conn.close()


def test_prune_missing_refuses_on_schema_mismatch(db_path, make_track, tmp_path):
    """The destructive prune path honours the schema guard (#545): an out-of-date
    plugin must not delete rows from a store schema it cannot understand."""
    from beets import ui

    gone = tmp_path / "gone.flac"  # never created == missing == would be pruned
    make_track(str(gone))
    conn = connect(db_path)
    try:
        conn.execute("PRAGMA user_version = 99999")  # diverged schema
        conn.commit()
    finally:
        conn.close()

    plugin = MusefsPlugin.__new__(MusefsPlugin)
    with pytest.raises(ui.UserError):
        plugin._prune_missing(db_path)

    conn = connect(db_path)
    try:
        assert conn.execute("SELECT COUNT(*) FROM tracks").fetchone()[0] == 1
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


def test_sync_writes_beets_path_when_enabled(db_path, make_track, fake_item, tmp_path, monkeypatch):
    real, tid, item = _real_track(
        tmp_path,
        make_track,
        fake_item,
        title="Song",
        destination=b"Artist/Album/01 Song.flac",
    )
    plugin = MusefsPlugin()
    monkeypatch.setattr(
        plugin,
        "config",
        FakeConfigView({"db": db_path, "fields": {}, "write_path": True}),
        raising=False,
    )
    plugin._sync(db_path, [item])

    conn = connect(db_path)
    try:
        assert (
            conn.execute(
                "SELECT value FROM tags WHERE track_id=? AND key='beets_path'", (tid,)
            ).fetchone()[0]
            == "Artist/Album/01 Song"
        )
    finally:
        conn.close()


def test_sync_omits_beets_path_when_disabled(db_path, make_track, fake_item, tmp_path, monkeypatch):
    real, tid, item = _real_track(
        tmp_path,
        make_track,
        fake_item,
        title="Song",
        destination=b"Artist/Album/01 Song.flac",
    )
    plugin = MusefsPlugin()
    monkeypatch.setattr(
        plugin,
        "config",
        FakeConfigView({"db": db_path, "fields": {}, "write_path": False}),
        raising=False,
    )
    plugin._sync(db_path, [item])

    conn = connect(db_path)
    try:
        assert (
            conn.execute(
                "SELECT value FROM tags WHERE track_id=? AND key='beets_path'", (tid,)
            ).fetchone()
            is None
        )
    finally:
        conn.close()


# --- restore_backing tests ------------------------------------------------


def _seed_track_with_tag(db_path, real_path, key, value):
    conn = connect(db_path)
    tid = insert_track(conn, real_path)
    conn.execute(
        "INSERT INTO tags (track_id, key, value, ordinal) VALUES (?,?,?,0)", (tid, key, value)
    )
    conn.commit()
    conn.close()
    return tid


def _text_tags(db_path, tid):
    conn = connect(db_path)
    rows = dict(
        conn.execute("SELECT key, value FROM tags WHERE track_id=? AND value_blob IS NULL", (tid,))
    )
    conn.close()
    return rows


def test_sync_merges_keeps_unmanaged_and_persists(db_path, tmp_path, monkeypatch):
    """Command path: B persists, M wins, managed flexattr written via store()."""
    p = tmp_path / "a.flac"
    p.write_bytes(b"")
    real = os.path.realpath(str(p))
    tid = _seed_track_with_tag(db_path, real, "comment", "keep")

    item = FakeItem(str(p).encode(), artist="New")
    plugin = MusefsPlugin()
    monkeypatch.setattr(
        plugin,
        "config",
        FakeConfigView({
            "db": db_path,
            "fields": {},
            "write_path": False,
            "restore_backing": False,
        }),
        raising=False,
    )
    plugin._sync(db_path, [item], dry_run=False, restore_backing=False)

    tags = _text_tags(db_path, tid)
    assert tags["artist"] == "New"  # M wins
    assert tags["comment"] == "keep"  # unmanaged B persists (merge, not replace)
    assert "artist" in item.musefs_managed  # managed set persisted via store()


def test_reconcile_path_merges_and_sticky_deletes(db_path, tmp_path, monkeypatch):
    """Passive cli_exit path runs the same merge + managed-state cycle, and a key
    dropped from a prior managed set is deleted (tombstone)."""
    p = tmp_path / "b.flac"
    p.write_bytes(b"")
    real = os.path.realpath(str(p))
    tid = _seed_track_with_tag(db_path, real, "grouping", "old")

    item = FakeItem(str(p).encode(), title="T")
    item["musefs_managed"] = "grouping,title"  # grouping managed before; now dropped
    plugin = MusefsPlugin()
    monkeypatch.setattr(
        plugin,
        "config",
        FakeConfigView({
            "db": db_path,
            "fields": {},
            "write_path": False,
            "autoscan": False,
            "restore_backing": False,
        }),
        raising=False,
    )
    monkeypatch.setattr(plugin, "_run_scan", lambda db, targets: None)
    plugin._pending = [item]
    plugin._reconcile_pending(lib=None)

    tags = _text_tags(db_path, tid)
    assert tags.get("title") == "T"  # merged on the reconcile path
    assert "grouping" not in tags  # tombstoned delete applied on the reconcile path


def test_restore_backing_skips_deletes(db_path, tmp_path, monkeypatch):
    """With restore_backing, a previously-managed-now-dropped key is NOT deleted."""
    p = tmp_path / "c.flac"
    p.write_bytes(b"")
    real = os.path.realpath(str(p))
    tid = _seed_track_with_tag(db_path, real, "grouping", "frombacking")

    item = FakeItem(str(p).encode(), title="T")
    item["musefs_managed"] = "grouping,title"
    plugin = MusefsPlugin()
    monkeypatch.setattr(
        plugin,
        "config",
        FakeConfigView({"db": db_path, "fields": {}, "write_path": False, "restore_backing": True}),
        raising=False,
    )
    plugin._sync(db_path, [item], dry_run=False, restore_backing=True)

    tags = _text_tags(db_path, tid)
    assert tags["grouping"] == "frombacking"  # backing value left in place
    assert set(item.musefs_managed.split(",")) == {"title"}  # tombstones cleared
