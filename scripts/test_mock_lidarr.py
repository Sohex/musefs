from mock_lidarr import build_fixture


def test_fixture_has_preflight_safe_settings():
    fx = build_fixture(
        album_id=34,
        artist_id=7,
        artist_name="Alice",
        album_title="Demo",
        tracks=[(100, "/m/01.flac", "One", 1)],
    )
    assert fx["/api/v1/config/mediamanagement"]["fileDate"] == "none"
    assert fx["/api/v1/config/mediamanagement"]["setPermissionsLinux"] is False
    assert fx["/api/v1/config/metadataprovider"]["writeAudioTags"] == "no"


def test_fixture_trackfiles_carry_album_and_artist_ids():
    fx = build_fixture(
        album_id=34,
        artist_id=7,
        artist_name="Alice",
        album_title="Demo",
        tracks=[(100, "/m/01.flac", "One", 1), (101, "/m/02.flac", "Two", 2)],
    )
    tfs = fx["/api/v1/trackfile"]
    assert {tf["path"] for tf in tfs} == {"/m/01.flac", "/m/02.flac"}
    assert all(tf["albumId"] == 34 and tf["artistId"] == 7 for tf in tfs)
    assert {tf["id"] for tf in tfs} == {100, 101}


def test_fixture_tracks_link_to_trackfiles():
    fx = build_fixture(
        album_id=34,
        artist_id=7,
        artist_name="Alice",
        album_title="Demo",
        tracks=[(100, "/m/01.flac", "One", 1)],
    )
    tracks = fx["/api/v1/track"]
    assert tracks[0]["trackFileId"] == 100
    assert tracks[0]["title"] == "One"


def test_fixture_album_and_artist_have_required_fields():
    fx = build_fixture(
        album_id=34,
        artist_id=7,
        artist_name="Alice",
        album_title="Demo",
        tracks=[(100, "/m/01.flac", "One", 1)],
    )
    assert fx["/api/v1/album/34"]["id"] == 34
    assert fx["/api/v1/album/34"]["title"] == "Demo"
    assert fx["/api/v1/artist/7"]["artistName"] == "Alice"
    assert fx["/api/v1/artist/7"]["id"] == 7
