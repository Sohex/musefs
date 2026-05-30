from musefs._core import map_fields


def test_direct_fields_copied(fake_metadata):
    d = dict(map_fields(fake_metadata(title="Song", artist="Band", album="Disc")))
    assert d["title"] == "Song"
    assert d["artist"] == "Band"
    assert d["album"] == "Disc"


def test_first_value_of_multivalued_field(fake_metadata):
    # Picard multi-valued: getall returns a list; we take the first.
    d = dict(map_fields(fake_metadata(artist=["First", "Second"])))
    assert d["artist"] == "First"


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
