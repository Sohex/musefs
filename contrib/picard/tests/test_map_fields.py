from musefs._core import map_fields


def test_direct_fields_copied(fake_metadata):
    d = dict(map_fields(fake_metadata(title="Song", artist="Band", album="Disc")))
    assert d["title"] == "Song"
    assert d["artist"] == "Band"
    assert d["album"] == "Disc"


def test_multivalued_field_expands(fake_metadata):
    # Picard multi-valued: getall returns a list; multi-value-eligible keys
    # (artist/albumartist/genre/composer) emit one row per value.
    pairs = map_fields(fake_metadata(artist=["First", "Second"]))
    artists = [v for k, v in pairs if k == "artist"]
    assert artists == ["First", "Second"]


def test_genre_multivalue_expands(fake_metadata):
    pairs = map_fields(fake_metadata(genre=["Rock", "Pop"]))
    assert [v for k, v in pairs if k == "genre"] == ["Rock", "Pop"]


def test_date_not_multivalue_expanded(fake_metadata):
    # date is NOT in the multi-value allowlist: stays a single scalar row even
    # if Picard happens to expose multiple values.
    pairs = map_fields(fake_metadata(date=["2020", "2021"]))
    assert [v for k, v in pairs if k == "date"] == ["2020"]


def test_empty_and_whitespace_omitted(fake_metadata):
    d = dict(map_fields(fake_metadata(title="", artist="   ")))
    assert d == {}


def test_tracknumber_and_discnumber_passthrough(fake_metadata):
    d = dict(map_fields(fake_metadata(tracknumber="7", discnumber="2")))
    assert d["tracknumber"] == "7"
    assert d["discnumber"] == "2"


def test_zero_tracknumber_omitted(fake_metadata):
    d = dict(map_fields(fake_metadata(tracknumber="0", discnumber="0")))
    assert "tracknumber" not in d
    assert "discnumber" not in d


def test_date_passthrough(fake_metadata):
    assert dict(map_fields(fake_metadata(date="1999-03-05")))["date"] == "1999-03-05"
    assert dict(map_fields(fake_metadata(date="1999")))["date"] == "1999"


def test_extra_field_override_adds_mapping(fake_metadata):
    md = fake_metadata(title="Song", comment="hi")
    d = dict(map_fields(md, extra_fields={"comment": "comment"}))
    assert d["comment"] == "hi"
    assert d["title"] == "Song"


def test_extra_field_remaps_existing_key(fake_metadata):
    d = dict(map_fields(fake_metadata(title="Song"), extra_fields={"title": "subtitle"}))
    assert d["subtitle"] == "Song"
    assert "title" not in d
