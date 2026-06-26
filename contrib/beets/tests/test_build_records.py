from musefs_common import SyncStats

from beetsplug import _core


def test_build_records_maps_fields(fake_item):
    item = fake_item(b"/m/a.flac", title="T", artist="A", genre=["Rock", "Pop"])
    stats = SyncStats()
    records, _ = _core.build_records([item], fields=None, stats=stats)
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
    records, _ = _core.build_records([item], fields=None, stats=stats)
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
    records, _ = _core.build_records(items, fields=None, stats=stats)
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
    records, _ = _core.build_records(items, fields=None, stats=stats)
    assert all(r.art is None for r in records)
    assert stats.skipped_art == 1


class _RecordingLog:
    """Duck-typed stand-in for the plugin's logger."""

    def __init__(self):
        self.warnings = []

    def warning(self, *args):
        self.warnings.append(args)


def test_build_records_writes_beets_path_stripping_extension(fake_item):
    item = fake_item(b"/m/a.flac", title="T", destination=b"Artist/Album/01 Song.flac")
    stats = SyncStats()
    records, _ = _core.build_records([item], fields=None, stats=stats)
    assert ("beets_path", "Artist/Album/01 Song") in records[0].pairs


def test_build_records_omits_beets_path_when_write_path_false(fake_item):
    item = fake_item(b"/m/a.flac", title="T", destination=b"Artist/Album/01 Song.flac")
    stats = SyncStats()
    records, _ = _core.build_records([item], fields=None, stats=stats, write_path=False)
    assert all(k != "beets_path" for k, _ in records[0].pairs)


def test_build_records_skips_beets_path_on_error_and_warns(fake_item):
    item = fake_item(b"/m/a.flac", title="T", destination_raises=True)
    log = _RecordingLog()
    stats = SyncStats()
    records, _ = _core.build_records([item], fields=None, stats=stats, log=log)
    assert all(k != "beets_path" for k, _ in records[0].pairs)
    assert ("title", "T") in records[0].pairs  # other tags still sync
    assert log.warnings  # a warning was emitted


def test_build_records_beets_path_is_utf8_safe_for_non_unicode_paths(fake_item):
    # A non-UTF-8 byte must normalize to valid UTF-8 (U+FFFD), never a lone
    # surrogate that SQLite's TEXT encoder would reject.
    item = fake_item(b"/m/a.flac", destination=b"Art\xffist/Album/01 Song.flac")
    stats = SyncStats()
    records, _ = _core.build_records([item], fields=None, stats=stats)
    value = dict(records[0].pairs)["beets_path"]
    value.encode("utf-8")  # must not raise
    assert value.startswith("Art")
    assert value.endswith("/Album/01 Song")


def test_build_records_uses_real_beets_destination(tmp_path):
    # Exercises the REAL beets API (not FakeItem), so the default test tier
    # covers item.destination(relative_to_libdir=True), not just our decode.
    from beets.library import Item, Library

    lib = Library(":memory:", directory=str(tmp_path))
    # beets 2.12 dropped the path_formats constructor arg (it now derives from
    # config); assigning the attribute works across versions.
    lib.path_formats = [("default", "$artist/$album/$track $title")]
    item = Item(artist="AC/DC", album="Back in Black", title="Hells Bells", track=1)
    item.path = b"/music/x.flac"  # supplies the .flac extension
    lib.add(item)  # assigns an id and binds the library, required by destination()
    stats = SyncStats()
    records, _ = _core.build_records([item], fields=None, stats=stats)
    # beets sanitizes "AC/DC" -> "AC_DC" and zero-pads $track; we strip ".flac".
    assert ("beets_path", "AC_DC/Back in Black/01 Hells Bells") in records[0].pairs
