from lidarr_smoke_lib import build_album_download_env, build_import_env


def test_build_album_download_env_for_sync():
    env = build_album_download_env(
        album_id=34,
        track_paths=["/music/Artist/Album/01.flac", "/music/Artist/Album/02.flac"],
        db_path="/work/store.db",
        lidarr_url="http://127.0.0.1:9999",
        api_key="KEY",
        musefs_bin="/usr/local/bin/musefs",
    )
    assert env["Lidarr_EventType"] == "AlbumDownload"
    assert env["Lidarr_Album_Id"] == "34"
    assert (
        env["Lidarr_AddedTrackPaths"] == "/music/Artist/Album/01.flac|/music/Artist/Album/02.flac"
    )
    assert env["MUSEFS_DB"] == "/work/store.db"
    assert env["MUSEFS_LIDARR_URL"] == "http://127.0.0.1:9999"
    assert env["MUSEFS_LIDARR_API_KEY"] == "KEY"
    assert env["MUSEFS_BIN"] == "/usr/local/bin/musefs"


def test_build_import_env_for_symlink():
    env = build_import_env(source="/music/Artist/Album/01.flac", destination="/links/01.flac")
    assert env["Lidarr_EventType"] == "Download"
    assert env["Lidarr_SourcePath"] == "/music/Artist/Album/01.flac"
    assert env["Lidarr_DestinationPath"] == "/links/01.flac"
