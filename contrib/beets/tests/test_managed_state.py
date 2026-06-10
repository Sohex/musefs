from conftest import FakeItem
from musefs_common import SyncStats

from beetsplug import _core


def _item(**kw):
    return FakeItem(b"/m/a.flac", **kw)


def test_read_managed_empty_and_parsed():
    it = _item()
    assert _core.read_managed(it) == []
    it["musefs_managed"] = "artist,comment,title"
    assert _core.read_managed(it) == ["artist", "comment", "title"]


def test_format_managed_sorts_and_dedupes():
    assert _core.format_managed(["title", "artist", "artist"]) == "artist,title"


def test_persist_managed_writes_flexattr_via_store():
    it = _item()
    _core.persist_managed([(it, ["artist", "title"])])
    assert it.musefs_managed == "artist,title"
    assert it.stored == 1


def test_build_records_delete_keys_and_union_persist():
    it = _item(title="T", artist="A")
    it["musefs_managed"] = "artist,title,grouping"  # grouping was managed before
    records, writes = _core.build_records(
        [it], fields={}, stats=SyncStats(), write_path=False, restore_backing=False
    )
    rec = records[0]
    assert rec.delete_keys == ["grouping"]  # dropped from M -> delete
    assert ("title", "T") in rec.pairs and ("artist", "A") in rec.pairs
    item, managed = writes[0]
    assert item is it
    # UNION: grouping stays in musefs_managed as a tombstone so the delete sticks
    assert set(managed) == {"title", "artist", "grouping"}


def test_build_records_restore_backing_clears_deletes_and_tombstones():
    it = _item(title="T")
    it["musefs_managed"] = "title,grouping"
    records, writes = _core.build_records(
        [it], fields={}, stats=SyncStats(), write_path=False, restore_backing=True
    )
    assert records[0].delete_keys == []
    assert set(writes[0][1]) == {"title"}


def test_build_records_beets_path_is_managed():
    it = _item(title="T", destination=b"Artist/Album/01 T.flac")
    records, writes = _core.build_records(
        [it], fields={}, stats=SyncStats(), write_path=True, restore_backing=False
    )
    assert "beets_path" in {k for k, _ in records[0].pairs}
    assert "beets_path" in writes[0][1]
