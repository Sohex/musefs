from types import SimpleNamespace

from beetsplug._core import map_fields

# Fields a real beets Item exposes via Item._media_tag_fields. Tests attach this
# so map_fields iterates the same boundary it will in production.
_TAG_FIELDS = (
    "title",
    "artist",
    "artists",
    "albumartist",
    "albumartists",
    "album",
    "genre",
    "genres",
    "composer",
    "composers",
    "comments",
    "grouping",
    "isrc",
    "lyrics",
    "bpm",
    "comp",
    "track",
    "tracktotal",
    "disc",
    "disctotal",
    "year",
    "month",
    "day",
    "rg_track_gain",
    "rg_album_gain",
    "rg_track_peak",
    "rg_album_peak",
    "mb_albumid",
    "mb_artistid",
    "mb_trackid",
    "artist_sort",
    "artists_sort",
    "albumartist_sort",
    "albumartists_sort",
    "bitrate",
    "length",
    "format",  # file facts: present on item, NOT tag fields
)
_FILE_FACTS = {"bitrate", "length", "format"}
_TAG_ONLY = tuple(f for f in _TAG_FIELDS if f not in _FILE_FACTS)


def item(**kw):
    base = {f: "" for f in _TAG_FIELDS}
    base.update({
        f: 0 for f in ("track", "tracktotal", "disc", "disctotal", "year", "month", "day", "bpm")
    })
    base.update({"comp": False, "bitrate": 320000, "length": 210.0, "format": "FLAC"})
    base.update(kw)
    ns = SimpleNamespace(**base)
    ns._media_tag_fields = _TAG_ONLY  # boundary excludes the file facts
    return ns


def test_core_fields_copied():
    d = dict(map_fields(item(title="Song", artist="Band", album="Disc")))
    assert d["title"] == "Song" and d["artist"] == "Band" and d["album"] == "Disc"


def test_track_disc_renamed_and_zero_dropped():
    d = dict(map_fields(item(track=7, disc=2)))
    assert d["tracknumber"] == "7" and d["discnumber"] == "2"
    assert "tracknumber" not in dict(map_fields(item(track=0)))


def test_replaygain_renamed_and_formatted():
    pairs = dict(map_fields(item(rg_track_gain=-7.5, rg_track_peak=0.987654321)))
    assert pairs["replaygain_track_gain"] == "-7.50 dB"
    assert pairs["replaygain_track_peak"].startswith("0.98")
    assert "rg_track_gain" not in pairs


def test_replaygain_zero_gain_survives():
    # 0 dB is a real measured value and must NOT be dropped.
    assert dict(map_fields(item(rg_track_gain=0.0)))["replaygain_track_gain"] == "0.00 dB"


def test_musicbrainz_renamed():
    d = dict(map_fields(item(mb_albumid="abc", mb_artistid="def", mb_trackid="ghi")))
    assert d["musicbrainz_albumid"] == "abc"
    assert d["musicbrainz_artistid"] == "def"
    assert d["musicbrainz_trackid"] == "ghi"


def test_comments_renamed_to_comment():
    assert dict(map_fields(item(comments="hi")))["comment"] == "hi"


def test_plural_artist_wins_and_expands():
    pairs = map_fields(item(artist="Joined", artists=["A", "B"]))
    artists = [v for k, v in pairs if k == "artist"]
    assert artists == ["A", "B"]  # plural list wins, one row each


def test_singular_artist_used_when_plural_empty():
    pairs = map_fields(item(artist="Solo", artists=[]))
    assert [v for k, v in pairs if k == "artist"] == ["Solo"]


def test_genre_plural_collapses_to_genre_key():
    pairs = map_fields(item(genres=["Rock", "Pop"]))
    assert [v for k, v in pairs if k == "genre"] == ["Rock", "Pop"]


def test_comp_renamed_to_compilation_and_zero_dropped():
    # beets `comp` is a 0/1 int; it maps to the on-disk `compilation` key.
    # 1 -> kept "1", 0/False -> dropped.
    assert dict(map_fields(item(comp=True)))["compilation"] == "1"
    assert dict(map_fields(item(comp=1)))["compilation"] == "1"
    assert "comp" not in dict(map_fields(item(comp=True)))
    assert "compilation" not in dict(map_fields(item(comp=False)))
    assert "compilation" not in dict(map_fields(item(comp=0)))


def test_sort_fields_renamed_to_on_disk_keys():
    # artist_sort/albumartist_sort are beets' internal attribute names; the
    # on-disk standard (matching Picard) is artistsort/albumartistsort.
    d = dict(map_fields(item(artist_sort="Beatles, The", albumartist_sort="V, The")))
    assert d["artistsort"] == "Beatles, The"
    assert d["albumartistsort"] == "V, The"
    assert "artist_sort" not in d and "albumartist_sort" not in d
    # plural twin collapses to the singular attr, then renames to on-disk key
    pairs = map_fields(item(artists_sort=["A", "B"]))
    assert [v for k, v in pairs if k == "artistsort"] == ["A", "B"]
    pairs = map_fields(item(albumartists_sort=["A", "B"]))
    assert [v for k, v in pairs if k == "albumartistsort"] == ["A", "B"]


def test_file_facts_excluded():
    d = dict(map_fields(item()))
    assert "bitrate" not in d and "length" not in d and "format" not in d


def test_date_assembled_and_parts_not_emitted():
    d = dict(map_fields(item(year=1999, month=3, day=5)))
    assert d["date"] == "1999-03-05"
    assert "year" not in d and "month" not in d and "day" not in d


def test_arbitrary_passthrough_lowercased():
    d = dict(map_fields(item(grouping="Set", isrc="US-X", lyrics="la")))
    assert d["grouping"] == "Set" and d["isrc"] == "US-X" and d["lyrics"] == "la"


def test_extra_fields_override_wins():
    # `fields:` maps a beets field onto a store key, last-wins.
    d = dict(map_fields(item(comments="orig", bpm=120), extra_fields={"bpm": "comment"}))
    assert d["comment"] == "120"  # override beat the comments->comment rename


def test_bpm_int_no_trailing_dot_zero():
    assert dict(map_fields(item(bpm=120)))["bpm"] == "120"


def test_real_item_bare_emits_no_zero_sentinel_fields():
    """Against a REAL beets Item (not a stub), the integer fields beets defaults to
    0 as an 'unset' sentinel must not leak as "0" tags. This exercises the real
    `_media_tag_fields` boundary that the SimpleNamespace tests above do not."""
    import pytest

    beets_library = pytest.importorskip("beets.library")
    it = beets_library.Item()
    it.title = "Bare"
    it.artist = "X"
    d = dict(map_fields(it))
    for noise in ("bpm", "original_year", "original_month", "original_day"):
        assert noise not in d, f"{noise} leaked as {d.get(noise)!r}"
    # A real value still comes through (we drop the zero sentinel, not the field).
    it.original_year = 1999
    it.bpm = 128
    d = dict(map_fields(it))
    assert d["original_year"] == "1999"
    assert d["bpm"] == "128"
