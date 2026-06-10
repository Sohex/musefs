import pytest
from lidarr_smoke_lib import (
    assert_bytes_unchanged,
    build_album_download_env,
    build_import_env,
    parse_ffprobe_tags,
    sha256_file,
)


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


def test_parse_ffprobe_tags_lowercases_keys():
    payload = '{"format": {"tags": {"ARTIST": "Alice", "album": "Demo"}}}'
    assert parse_ffprobe_tags(payload) == {"artist": "Alice", "album": "Demo"}


def test_parse_ffprobe_tags_empty_when_no_tags():
    assert parse_ffprobe_tags('{"format": {}}') == {}


def test_sha256_file_roundtrip(tmp_path):
    p = tmp_path / "a.bin"
    p.write_bytes(b"hello")
    assert sha256_file(str(p)) == (
        "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
    )


def test_assert_bytes_unchanged_passes_when_equal():
    assert_bytes_unchanged({"a": "x"}, {"a": "x"})


def test_assert_bytes_unchanged_raises_on_change():
    with pytest.raises(AssertionError, match="a.flac"):
        assert_bytes_unchanged({"a.flac": "x"}, {"a.flac": "y"})
