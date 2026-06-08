from musefs_lidarr.events import EventType, parse_event, split_paths


def test_split_paths_handles_empty_value():
    assert split_paths("") == []


def test_split_paths_splits_pipe_separated_paths():
    assert split_paths("/a.flac|/b.flac") == ["/a.flac", "/b.flac"]


def test_parse_test_event():
    event = parse_event({"Lidarr_EventType": "Test"})

    assert event.event_type == EventType.TEST
    assert event.raw_type == "Test"
    assert event.paths == []
    assert event.previous_paths == []
    assert event.artist_id is None
    assert event.album_id is None


def test_parse_album_download_event():
    event = parse_event(
        {
            "Lidarr_EventType": "AlbumDownload",
            "Lidarr_Artist_Id": "12",
            "Lidarr_Album_Id": "34",
            "Lidarr_AddedTrackPaths": "/music/a.flac|/music/b.flac",
        }
    )

    assert event.event_type == EventType.ALBUM_DOWNLOAD
    assert event.raw_type == "AlbumDownload"
    assert event.artist_id == 12
    assert event.album_id == 34
    assert event.paths == ["/music/a.flac", "/music/b.flac"]
    assert event.previous_paths == []


def test_parse_rename_event():
    event = parse_event(
        {
            "Lidarr_EventType": "Rename",
            "Lidarr_Artist_Id": "12",
            "Lidarr_TrackFile_Paths": "/new/a.flac|/new/b.flac",
            "Lidarr_TrackFile_PreviousPaths": "/old/a.flac|/old/b.flac",
        }
    )

    assert event.event_type == EventType.RENAME
    assert event.raw_type == "Rename"
    assert event.artist_id == 12
    assert event.album_id is None
    assert event.paths == ["/new/a.flac", "/new/b.flac"]
    assert event.previous_paths == ["/old/a.flac", "/old/b.flac"]


def test_parse_track_retag_event():
    event = parse_event({"Lidarr_EventType": "TrackRetag", "Lidarr_Artist_Id": "5"})

    assert event.event_type == EventType.TRACK_RETAG
    assert event.raw_type == "TrackRetag"
    assert event.artist_id == 5
    assert event.paths == []
    assert event.previous_paths == []


def test_parse_album_download_event_with_lowercase_keys():
    # Real Lidarr emits lowercased env var names (StringDictionary).
    event = parse_event(
        {
            "lidarr_eventtype": "AlbumDownload",
            "lidarr_artist_id": "12",
            "lidarr_album_id": "34",
            "lidarr_addedtrackpaths": "/music/a.flac|/music/b.flac",
        }
    )

    assert event.event_type == EventType.ALBUM_DOWNLOAD
    assert event.artist_id == 12
    assert event.album_id == 34
    assert event.paths == ["/music/a.flac", "/music/b.flac"]


def test_parse_rename_event_with_lowercase_keys():
    event = parse_event(
        {
            "lidarr_eventtype": "Rename",
            "lidarr_trackfile_paths": "/new/a.flac",
            "lidarr_trackfile_previouspaths": "/old/a.flac",
        }
    )

    assert event.event_type == EventType.RENAME
    assert event.paths == ["/new/a.flac"]
    assert event.previous_paths == ["/old/a.flac"]


def test_parse_unknown_event_is_unsupported():
    event = parse_event({"Lidarr_EventType": "Grab"})

    assert event.event_type == EventType.UNSUPPORTED
    assert event.raw_type == "Grab"
    assert event.paths == []
    assert event.previous_paths == []


def test_parse_event_coerces_integer_ids():
    event = parse_event(
        {
            "Lidarr_EventType": "AlbumDownload",
            "Lidarr_Artist_Id": "0012",
            "Lidarr_Album_Id": "34",
        }
    )

    assert event.artist_id == 12
    assert event.album_id == 34


def test_parse_event_ignores_invalid_integer_ids():
    event = parse_event(
        {
            "Lidarr_EventType": "Rename",
            "Lidarr_Artist_Id": "not-a-number",
            "Lidarr_Album_Id": "",
        }
    )

    assert event.artist_id is None
    assert event.album_id is None
