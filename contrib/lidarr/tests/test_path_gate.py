from __future__ import annotations

import os
import shutil
import subprocess
import warnings
from pathlib import Path

import pytest
from musefs_common import connect, realpath_key, track_id_for_path

pytestmark = pytest.mark.musefs_bin


def _newest_rs_mtime(repo_root):
    newest = 0.0
    for crate in ("musefs-db", "musefs-format", "musefs-core", "musefs-fuse", "musefs-cli"):
        src = repo_root / crate / "src"
        if src.exists():
            for rs in src.rglob("*.rs"):
                newest = max(newest, rs.stat().st_mtime)
    return newest


@pytest.mark.parametrize("link", ["symlink", "hardlink"])
def test_linked_scan_matches_real_backing_path(tmp_path, link):
    """What the scanner stores for each `MUSEFS_LIDARR_LINK_MODE` is the key the
    plugin looks the track up by.

    A symlink resolves to its target, so the key is the download's path. A
    hardlink is a second name for the same inode, so it resolves to itself —
    and the inode, which is what makes the two names one file, is stored too.
    """
    repo_root = Path(__file__).resolve().parents[3]
    env_bin = os.environ.get("MUSEFS_BIN")
    if env_bin:
        musefs_bin = Path(env_bin)
    else:
        debug = repo_root / "target" / "debug" / "musefs"
        release = repo_root / "target" / "release" / "musefs"
        musefs_bin = debug if debug.exists() else release
    if not musefs_bin.exists():
        msg = "musefs binary not found; run `cargo build` or set MUSEFS_BIN"
        if os.environ.get("MUSEFS_REQUIRE_BIN"):
            pytest.fail(msg)
        pytest.skip(msg)
    if musefs_bin.stat().st_mtime < _newest_rs_mtime(repo_root):
        # This tier runs by default, so a stale binary is the common hazard
        # rather than a rare one: it is the only tier that sees the real schema,
        # and an old binary would pass against the shape it was built for.
        warnings.warn(
            f"{musefs_bin} is older than the musefs Rust sources; rebuild with "
            f"`cargo build` before trusting a pass.",
            stacklevel=2,
        )
    if shutil.which("ffmpeg") is None:
        pytest.skip("ffmpeg not installed")

    source = tmp_path / "download.flac"
    destination = tmp_path / "library" / "Artist" / "download.flac"
    db_path = tmp_path / "musefs.db"
    destination.parent.mkdir(parents=True)

    subprocess.run(
        [
            "ffmpeg",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=0.2",
            "-y",
            str(source),
        ],
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    if link == "symlink":
        destination.symlink_to(source)
        expected = realpath_key(source)
    else:
        os.link(source, destination)
        expected = realpath_key(destination)

    subprocess.run([str(musefs_bin), "scan", str(destination), "--db", str(db_path)], check=True)

    conn = connect(str(db_path))
    try:
        rows = conn.execute("SELECT id, backing_path, backing_ino FROM tracks").fetchall()
        assert len(rows) == 1
        track_id, raw, ino = rows[0]
        # The raw column, not a decoded rendering of it: `backing_path` is a BLOB
        # holding the filesystem's bytes from schema v4 on (#680).
        assert isinstance(raw, bytes)
        assert raw == os.fsencode(expected)
        # And the plugin's own lookup direction finds that row.
        assert track_id_for_path(conn, expected) == track_id
    finally:
        conn.close()
    # Stored as the two's-complement bit pattern of `st_ino` (#674).
    assert ino % 2**64 == source.stat().st_ino
