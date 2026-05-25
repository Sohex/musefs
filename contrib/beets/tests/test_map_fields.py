from types import SimpleNamespace

from beetsplug._core import map_fields


def item(**kw):
    base = dict(
        title="", artist="", albumartist="", album="", genre="", composer="",
        track=0, disc=0, year=0, month=0, day=0,
    )
    base.update(kw)
    return SimpleNamespace(**base)


def test_direct_fields_copied():
    pairs = map_fields(item(title="Song", artist="Band", album="Disc"))
    d = dict(pairs)
    assert d["title"] == "Song"
    assert d["artist"] == "Band"
    assert d["album"] == "Disc"


def test_track_and_disc_renamed():
    pairs = dict(map_fields(item(track=7, disc=2)))
    assert pairs["tracknumber"] == "7"
    assert pairs["discnumber"] == "2"


def test_year_only_date():
    assert dict(map_fields(item(year=1999)))["date"] == "1999"


def test_full_date_when_month_and_day():
    assert dict(map_fields(item(year=1999, month=3, day=5)))["date"] == "1999-03-05"


def test_partial_date_falls_back_to_year():
    # month without day -> year only (we only emit a full date when both set)
    assert dict(map_fields(item(year=1999, month=3)))["date"] == "1999"


def test_empty_and_zero_omitted():
    pairs = dict(map_fields(item()))
    assert pairs == {}


def test_whitespace_only_omitted():
    assert "title" not in dict(map_fields(item(title="   ")))


def test_extra_field_override():
    it = item(title="Song")
    it.comments = "hi there"
    pairs = dict(map_fields(it, extra_fields={"comments": "comment"}))
    assert pairs["comment"] == "hi there"
    assert pairs["title"] == "Song"


def test_extra_field_overrides_existing_key():
    # Overriding an existing beets field remaps its destination key.
    pairs = dict(map_fields(item(title="Song"), extra_fields={"title": "subtitle"}))
    assert pairs["subtitle"] == "Song"
    assert "title" not in pairs


def test_non_numeric_track_does_not_crash():
    # A malformed track like "1/12" must not raise; it is dropped, not emitted.
    pairs = dict(map_fields(item(track="1/12")))
    assert "tracknumber" not in pairs
