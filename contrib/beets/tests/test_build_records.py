from musefs_common import SyncStats

from beetsplug import _core


def test_build_records_maps_fields(fake_item):
    item = fake_item(b"/m/a.flac", title="T", artist="A", genre=["Rock", "Pop"])
    stats = SyncStats()
    records = _core.build_records([item], fields=None, stats=stats)
    assert len(records) == 1
    pairs = records[0].pairs
    assert ("title", "T") in pairs
    assert ("genre", "Rock") in pairs
    assert ("genre", "Pop") in pairs
    assert records[0].art is None


def test_build_records_reads_album_art(fake_item, fake_album, tmp_path):
    cover = tmp_path / "cover.jpg"
    cover.write_bytes(b"\xff\xd8\xff" + b"\x00" * 16)
    album = fake_album(artpath=str(cover).encode())
    item = fake_item(b"/m/a.flac", album=album, title="T")
    stats = SyncStats()
    records = _core.build_records([item], fields=None, stats=stats)
    assert records[0].art is not None
    (img,) = records[0].art
    assert img.mime == "image/jpeg"
    assert img.picture_type == 3
    assert img.description == ""
    assert stats.skipped_art == 0


def test_build_records_counts_unreadable_art_once(fake_item, fake_album):
    album = fake_album(artpath=b"/does/not/exist.jpg")
    items = [fake_item(b"/m/a.flac", album=album), fake_item(b"/m/b.flac", album=album)]
    stats = SyncStats()
    records = _core.build_records(items, fields=None, stats=stats)
    assert all(r.art is None for r in records)
    # Cached per realpath, so a shared missing cover counts once (legacy behavior).
    assert stats.skipped_art == 1


def test_build_records_counts_oversized_art_once(fake_item, fake_album, tmp_path):
    from musefs_common.constants import MAX_ART_BYTES

    cover = tmp_path / "big.jpg"
    cover.write_bytes(b"\xff\xd8\xff" + b"\x00" * (MAX_ART_BYTES + 1))
    album = fake_album(artpath=str(cover).encode())
    items = [fake_item(b"/m/a.flac", album=album), fake_item(b"/m/b.flac", album=album)]
    stats = SyncStats()
    records = _core.build_records(items, fields=None, stats=stats)
    assert all(r.art is None for r in records)
    assert stats.skipped_art == 1
