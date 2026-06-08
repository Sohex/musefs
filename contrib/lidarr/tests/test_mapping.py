import pytest
from musefs_common import realpath_key

from musefs_lidarr.errors import MappingError
from musefs_lidarr.mapping import SkippedPath, build_pairs, match_track_file, records_for_paths


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
