import pytest
from musefs_common import ArtImage, realpath_key

from musefs_lidarr.errors import MappingError
from musefs_lidarr.mapping import (
    SkippedPath,
    _album_cover_url,
    build_pairs,
    match_track_file,
    records_for_paths,
)


def test_build_pairs_maps_core_tags(sample_artist, sample_album, sample_track):
    pairs = build_pairs(track=sample_track, album=sample_album, artist=sample_artist)

    assert ("title", "Wildlife Analysis") in pairs
    assert ("artist", "Boards of Canada") in pairs
    assert ("albumartist", "Boards of Canada") in pairs
    assert ("album", "Music Has the Right to Children") in pairs
    assert ("tracknumber", "1") in pairs
    assert ("discnumber", "1") in pairs
    assert ("date", "1998-04-20") in pairs
    assert ("musicbrainz_artistid", "artist-mbid") in pairs
    assert ("musicbrainz_albumid", "release-group-mbid") in pairs
    assert ("musicbrainz_trackid", "track-mbid") in pairs
    assert ("musicbrainz_releasetrackid", "recording-mbid") in pairs
    assert pairs.count(("genre", "Electronic")) == 1
    assert ("genre", "IDM") in pairs


def test_match_track_file_by_realpath(sample_track_file):
    key = realpath_key(sample_track_file["path"])

    assert match_track_file(key, [sample_track_file]) == sample_track_file


def test_match_track_file_zero_match_returns_none(sample_track_file, tmp_path):
    key = realpath_key(tmp_path / "other.flac")

    assert match_track_file(key, [sample_track_file]) is None


def test_match_track_file_multiple_matches_fails(sample_track_file):
    key = realpath_key(sample_track_file["path"])
    duplicate = dict(sample_track_file, id=31)

    with pytest.raises(MappingError, match="multiple Lidarr track files"):
        match_track_file(key, [sample_track_file, duplicate])


def test_records_for_paths_builds_record(
    sample_artist, sample_album, sample_track, sample_track_file
):
    records, skipped = records_for_paths(
        paths=[sample_track_file["path"]],
        track_files=[sample_track_file],
        tracks=[sample_track],
        albums_by_id={20: sample_album},
        artists_by_id={10: sample_artist},
    )

    assert skipped == []
    assert len(records) == 1
    assert records[0].key == realpath_key(sample_track_file["path"])
    assert ("title", "Wildlife Analysis") in records[0].pairs


def test_records_for_paths_uses_each_track_files_own_album(
    sample_artist, sample_album, sample_track_file, tmp_path
):
    second_path = tmp_path / "library" / "02 - Second.flac"
    second_path.write_bytes(b"audio")
    second_album = dict(sample_album, id=21, title="Geogaddi")
    first_track = {
        "id": 40,
        "artistId": 10,
        "albumId": 20,
        "trackFileId": sample_track_file["id"],
        "trackNumber": "1",
        "mediumNumber": 1,
        "title": "Wildlife Analysis",
    }
    second_track_file = dict(sample_track_file, id=31, albumId=21, path=str(second_path))
    second_track = dict(first_track, id=41, albumId=21, trackFileId=31, title="Ready Lets Go")

    records, skipped = records_for_paths(
        paths=[sample_track_file["path"], str(second_path)],
        track_files=[sample_track_file, second_track_file],
        tracks=[first_track, second_track],
        albums_by_id={20: sample_album, 21: second_album},
        artists_by_id={10: sample_artist},
    )

    assert skipped == []
    by_key = {record.key: record for record in records}
    assert ("album", "Music Has the Right to Children") in by_key[
        realpath_key(sample_track_file["path"])
    ].pairs
    assert ("album", "Geogaddi") in by_key[realpath_key(second_path)].pairs


def test_records_for_paths_emits_multitrack_pairs_in_lidarr_order(
    sample_artist, sample_album, sample_track, sample_track_file
):
    second_track = dict(
        sample_track,
        id=41,
        title="Second linked track",
        trackNumber="2",
    )

    records, skipped = records_for_paths(
        paths=[sample_track_file["path"]],
        track_files=[sample_track_file],
        tracks=[sample_track, second_track],
        albums_by_id={20: sample_album},
        artists_by_id={10: sample_artist},
    )

    assert skipped == []
    titles = [value for key, value in records[0].pairs if key == "title"]
    assert titles == ["Wildlife Analysis", "Second linked track"]


def test_records_for_paths_emits_album_artist_pairs_once_per_file(
    sample_artist, sample_album, sample_track, sample_track_file
):
    # A single-file release linking N tracks (cue-style) must not duplicate the
    # album/artist-level tags N times (#539); only track-level fields repeat.
    second_track = dict(sample_track, id=41, title="Second linked track", trackNumber="2")

    records, skipped = records_for_paths(
        paths=[sample_track_file["path"]],
        track_files=[sample_track_file],
        tracks=[sample_track, second_track],
        albums_by_id={20: sample_album},
        artists_by_id={10: sample_artist},
    )

    assert skipped == []
    pairs = records[0].pairs
    keys = [key for key, _ in pairs]
    for once_key in (
        "artist",
        "albumartist",
        "album",
        "date",
        "musicbrainz_artistid",
        "musicbrainz_albumid",
    ):
        assert keys.count(once_key) == 1, f"{once_key} duplicated: {pairs!r}"
    # Genres (album + artist, deduped) also appear once apiece, not per track.
    assert keys.count("genre") == 2
    # Track-level fields still repeat per linked track.
    assert keys.count("title") == 2
    assert [v for k, v in pairs if k == "tracknumber"] == ["1", "2"]


def test_records_for_paths_marks_records_as_lidarr_managed(
    sample_artist, sample_album, sample_track, sample_track_file
):
    # Lidarr stamps an ownership marker so prune_deleted never touches rows it
    # did not write (scanner-seeded MBIDs look identical otherwise) (#546).
    from musefs_lidarr.mapping import MANAGED_KEY, MANAGED_VALUE

    records, _ = records_for_paths(
        paths=[sample_track_file["path"]],
        track_files=[sample_track_file],
        tracks=[sample_track],
        albums_by_id={20: sample_album},
        artists_by_id={10: sample_artist},
    )

    pairs = records[0].pairs
    assert pairs.count((MANAGED_KEY, MANAGED_VALUE)) == 1


def test_records_for_paths_returns_reason_for_missing_linked_tracks(
    sample_artist, sample_album, sample_track_file
):
    records, skipped = records_for_paths(
        paths=[sample_track_file["path"]],
        track_files=[sample_track_file],
        tracks=[],
        albums_by_id={20: sample_album},
        artists_by_id={10: sample_artist},
    )

    assert records == []
    assert skipped == [
        SkippedPath(
            path=sample_track_file["path"],
            reason="multi-track metadata unavailable",
        )
    ]


def test_album_cover_url_prefers_cover_type():
    album = {
        "images": [
            {"coverType": "fanart", "url": "/MediaCover/Albums/20/fanart.jpg"},
            {"coverType": "cover", "url": "/MediaCover/Albums/20/cover.jpg"},
        ]
    }

    assert _album_cover_url(album) == "/MediaCover/Albums/20/cover.jpg"


def test_album_cover_url_falls_back_to_first_image():
    album = {"images": [{"coverType": "poster", "remoteUrl": "http://img/poster.jpg"}]}

    assert _album_cover_url(album) == "http://img/poster.jpg"


def test_album_cover_url_none_when_no_images():
    assert _album_cover_url({"images": []}) is None
    assert _album_cover_url({}) is None


def test_records_for_paths_attaches_album_art(
    sample_artist, sample_album, sample_track, sample_track_file
):
    art = ArtImage(data=b"\xff\xd8\xffjpeg", mime="image/jpeg")

    records, skipped = records_for_paths(
        paths=[sample_track_file["path"]],
        track_files=[sample_track_file],
        tracks=[sample_track],
        albums_by_id={20: sample_album},
        artists_by_id={10: sample_artist},
        art_by_album_id={20: art},
    )

    assert records[0].art == [art]


def test_records_for_paths_no_art_when_album_missing_from_map(
    sample_artist, sample_album, sample_track, sample_track_file
):
    records, _ = records_for_paths(
        paths=[sample_track_file["path"]],
        track_files=[sample_track_file],
        tracks=[sample_track],
        albums_by_id={20: sample_album},
        artists_by_id={10: sample_artist},
        art_by_album_id={},
    )

    assert records[0].art is None
