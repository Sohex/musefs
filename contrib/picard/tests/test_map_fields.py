from musefs._core import map_fields


def test_direct_fields_copied(fake_metadata):
    d = dict(map_fields(fake_metadata(title="Song", artist="Band", album="Disc")))
    assert d["title"] == "Song"
    assert d["artist"] == "Band"
    assert d["album"] == "Disc"


def test_all_fields_multi_value_expand(fake_metadata):
    # The old _MULTI_VALUE_KEYS allowlist is gone: every field emits one row per
    # value, order preserved.
    pairs = map_fields(fake_metadata(artist=["First", "Second"]))
    assert [v for k, v in pairs if k == "artist"] == ["First", "Second"]
    pairs = map_fields(fake_metadata(genre=["Rock", "Pop"]))
    assert [v for k, v in pairs if k == "genre"] == ["Rock", "Pop"]
    pairs = map_fields(fake_metadata(mood=["Happy", "Sad"]))
    assert [v for k, v in pairs if k == "mood"] == ["Happy", "Sad"]


def test_empty_and_whitespace_omitted(fake_metadata):
    assert dict(map_fields(fake_metadata(title="", artist="   "))) == {}


def test_tracknumber_discnumber_passthrough_and_zero_dropped(fake_metadata):
    d = dict(map_fields(fake_metadata(tracknumber="7", discnumber="2")))
    assert d["tracknumber"] == "7" and d["discnumber"] == "2"
    z = dict(map_fields(fake_metadata(tracknumber="0", discnumber="0")))
    assert "tracknumber" not in z and "discnumber" not in z


def test_date_passthrough(fake_metadata):
    assert dict(map_fields(fake_metadata(date="1999-03-05")))["date"] == "1999-03-05"


def test_replaygain_and_misc_passthrough(fake_metadata):
    d = dict(
        map_fields(
            fake_metadata(
                replaygain_track_gain="-7.50 dB",
                musicbrainz_albumid="abc",
                grouping="set",
                isrc="US-X",
                label="Label",
            )
        )
    )
    assert d["replaygain_track_gain"] == "-7.50 dB"
    assert d["musicbrainz_albumid"] == "abc"
    assert d["grouping"] == "set" and d["isrc"] == "US-X" and d["label"] == "Label"


def test_musicbrainz_id_swap(fake_metadata):
    # Picard's recording id is the on-disk musicbrainz_trackid; Picard's track id
    # is the on-disk musicbrainz_releasetrackid. Both source vars present together.
    d = dict(map_fields(fake_metadata(musicbrainz_recordingid="rec", musicbrainz_trackid="trk")))
    assert d["musicbrainz_trackid"] == "rec"
    assert d["musicbrainz_releasetrackid"] == "trk"
    assert "musicbrainz_recordingid" not in d


def test_musicbrainz_ids_passthrough(fake_metadata):
    d = dict(
        map_fields(
            fake_metadata(
                musicbrainz_albumid="al",
                musicbrainz_artistid="ar",
                musicbrainz_albumartistid="aa",
                musicbrainz_releasegroupid="rg",
                musicbrainz_workid="wk",
            )
        )
    )
    assert d["musicbrainz_albumid"] == "al"
    assert d["musicbrainz_artistid"] == "ar"
    assert d["musicbrainz_albumartistid"] == "aa"
    assert d["musicbrainz_releasegroupid"] == "rg"
    assert d["musicbrainz_workid"] == "wk"


def test_sort_fields_passthrough(fake_metadata):
    d = dict(map_fields(fake_metadata(artistsort="B, The", albumartistsort="A, An")))
    assert d["artistsort"] == "B, The"
    assert d["albumartistsort"] == "A, An"


def test_artist_and_artists_both_emitted(fake_metadata):
    # Picard exposes the credited join string (artist) and the individual list
    # (artists) as distinct on-disk tags; emit both, no twin-collapse.
    pairs = map_fields(
        fake_metadata(
            artist="Alice & Bob",
            artists=["Alice", "Bob"],
            albumartist="Alice & Bob",
            albumartists=["Alice", "Bob"],
        )
    )
    assert [v for k, v in pairs if k == "artist"] == ["Alice & Bob"]
    assert [v for k, v in pairs if k == "artists"] == ["Alice", "Bob"]
    assert [v for k, v in pairs if k == "albumartist"] == ["Alice & Bob"]
    assert [v for k, v in pairs if k == "albumartists"] == ["Alice", "Bob"]


def test_movement_swap(fake_metadata):
    d = dict(map_fields(fake_metadata(movement="Allegro", movementnumber="1")))
    assert d["movementname"] == "Allegro"
    assert d["movement"] == "1"


def test_totals_renamed_and_zero_dropped(fake_metadata):
    d = dict(map_fields(fake_metadata(totaltracks="12", totaldiscs="2")))
    assert d["tracktotal"] == "12" and d["disctotal"] == "2"
    z = dict(map_fields(fake_metadata(totaltracks="0", totaldiscs="0")))
    assert "tracktotal" not in z and "disctotal" not in z


def test_compilation_and_bpm_zero_dropped(fake_metadata):
    assert dict(map_fields(fake_metadata(compilation="1")))["compilation"] == "1"
    assert "compilation" not in dict(map_fields(fake_metadata(compilation="0")))
    assert dict(map_fields(fake_metadata(bpm="120")))["bpm"] == "120"
    assert "bpm" not in dict(map_fields(fake_metadata(bpm="0")))
    assert "bpm" not in dict(map_fields(fake_metadata(bpm="0.0")))
    # a fractional, non-zero value must survive (float-aware zero check)
    assert dict(map_fields(fake_metadata(bpm="128.5")))["bpm"] == "128.5"


def test_performer_role_folded_into_value(fake_metadata):
    pairs = map_fields(fake_metadata(**{"performer:Piano": "Joe Barr"}))
    assert pairs == [("performer", "Joe Barr (Piano)")]


def test_performer_bare_value_only(fake_metadata):
    pairs = map_fields(fake_metadata(performer="Joe Barr"))
    assert pairs == [("performer", "Joe Barr")]


def test_performer_multiple_roles_accumulate(fake_metadata):
    pairs = map_fields(
        fake_metadata(**{
            "performer:Piano": ["Joe Barr"],
            "performer:Guitar": ["Ann Lee", "Max Roe"],
        })
    )
    performers = sorted(v for k, v in pairs if k == "performer")
    assert performers == ["Ann Lee (Guitar)", "Joe Barr (Piano)", "Max Roe (Guitar)"]


def test_comment_and_lyrics_collapse(fake_metadata):
    # Bare and described forms collapse to the base key; description dropped.
    d = dict(map_fields(fake_metadata(**{"comment:eng": "hello", "lyrics": "la"})))
    assert d["comment"] == "hello"
    assert d["lyrics"] == "la"


def test_comment_descriptions_accumulate(fake_metadata):
    pairs = map_fields(fake_metadata(**{"comment": "main", "comment:eng": "english"}))
    assert sorted(v for k, v in pairs if k == "comment") == ["english", "main"]


def test_hidden_vars_skipped(fake_metadata):
    pairs = map_fields(fake_metadata(**{"~length": "210000", "~rating": "5", "title": "Keep"}))
    keys = {k for k, _ in pairs}
    assert keys == {"title"}


def test_extra_fields_override_verbatim(fake_metadata):
    # The options-page map adds/overrides a store key, last-wins, value verbatim
    # (no role-fold, no zero-drop). Other fields' natural mapping is unaffected.
    md = fake_metadata(title="Song", **{"performer:Piano": "Joe Barr"})
    d = dict(map_fields(md, extra_fields={"performer:Piano": "soloist"}))
    assert d["soloist"] == "Joe Barr"  # verbatim: NOT "Joe Barr (Piano)"
    assert d["title"] == "Song"


def test_extra_fields_override_replaces_target_key(fake_metadata):
    md = fake_metadata(title="Song", subtitle="Sub")
    d = dict(map_fields(md, extra_fields={"title": "subtitle"}))
    assert d["subtitle"] == "Song"  # override wins over the natural subtitle row
