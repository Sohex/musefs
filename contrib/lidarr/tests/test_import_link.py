import os

import pytest

from musefs_lidarr.errors import ConfigError, ImportLinkError
from musefs_lidarr.import_link import LinkMode, ensure_link, parse_import_env, parse_link_mode


def test_parse_link_mode_defaults_to_symlink():
    assert parse_link_mode({}) == LinkMode.SYMLINK


def test_parse_link_mode_accepts_hardlink():
    assert parse_link_mode({"MUSEFS_LIDARR_LINK_MODE": "hardlink"}) == LinkMode.HARDLINK


def test_parse_link_mode_rejects_unknown_value():
    with pytest.raises(ConfigError, match="MUSEFS_LIDARR_LINK_MODE"):
        parse_link_mode({"MUSEFS_LIDARR_LINK_MODE": "copy"})


def test_parse_import_env_reads_lidarr_paths(tmp_path):
    src = tmp_path / "source.flac"
    dst = tmp_path / "artist" / "album" / "dest.flac"
    src.write_bytes(b"audio")

    env = {
        "Lidarr_SourcePath": os.fsdecode(src),
        "Lidarr_DestinationPath": os.fsdecode(dst),
    }

    parsed = parse_import_env(env)

    assert parsed.source == src
    assert parsed.destination == dst
    assert parsed.mode == LinkMode.SYMLINK


def test_parse_import_env_missing_paths_fails():
    with pytest.raises(ConfigError, match="Lidarr_SourcePath"):
        parse_import_env({})


def test_ensure_link_creates_symlink(tmp_path):
    src = tmp_path / "downloads" / "song.flac"
    dst = tmp_path / "library" / "Artist" / "song.flac"
    src.parent.mkdir()
    src.write_bytes(b"audio")

    ensure_link(src, dst, LinkMode.SYMLINK)

    assert dst.is_symlink()
    assert dst.readlink() == src
    assert dst.resolve() == src


def test_ensure_link_symlink_is_idempotent(tmp_path):
    src = tmp_path / "song.flac"
    dst = tmp_path / "library" / "song.flac"
    src.write_bytes(b"audio")
    dst.parent.mkdir()
    dst.symlink_to(src)

    ensure_link(src, dst, LinkMode.SYMLINK)

    assert dst.is_symlink()
    assert dst.readlink() == src


def test_ensure_link_refuses_conflicting_destination(tmp_path):
    src = tmp_path / "song.flac"
    dst = tmp_path / "library" / "song.flac"
    src.write_bytes(b"audio")
    dst.parent.mkdir()
    dst.write_bytes(b"other")

    with pytest.raises(ImportLinkError, match="destination already exists"):
        ensure_link(src, dst, LinkMode.SYMLINK)


def test_ensure_link_missing_source_fails(tmp_path):
    with pytest.raises(ImportLinkError, match="source does not exist"):
        ensure_link(
            tmp_path / "missing.flac", tmp_path / "library" / "song.flac", LinkMode.SYMLINK
        )


def test_ensure_link_creates_hardlink(tmp_path):
    src = tmp_path / "song.flac"
    dst = tmp_path / "library" / "song.flac"
    src.write_bytes(b"audio")

    ensure_link(src, dst, LinkMode.HARDLINK)

    assert not dst.is_symlink()
    assert os.stat(src).st_ino == os.stat(dst).st_ino
    assert os.stat(src).st_dev == os.stat(dst).st_dev


def test_ensure_link_hardlink_is_idempotent(tmp_path):
    src = tmp_path / "song.flac"
    dst = tmp_path / "library" / "song.flac"
    src.write_bytes(b"audio")
    dst.parent.mkdir()
    os.link(src, dst)

    ensure_link(src, dst, LinkMode.HARDLINK)

    assert os.stat(src).st_ino == os.stat(dst).st_ino
