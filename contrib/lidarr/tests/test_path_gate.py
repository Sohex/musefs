from __future__ import annotations

import os
import shutil
import subprocess
from pathlib import Path

import pytest
from musefs_common import connect, realpath_key

pytestmark = pytest.mark.musefs_bin


def test_symlink_scan_matches_real_backing_path(tmp_path):
    repo_root = Path(__file__).resolve().parents[3]
    musefs_bin = Path(os.environ.get("MUSEFS_BIN") or repo_root / "target" / "debug" / "musefs")
    if not musefs_bin.exists():
        msg = "musefs binary not found; run `cargo build` or set MUSEFS_BIN"
        if os.environ.get("MUSEFS_REQUIRE_BIN"):
            pytest.fail(msg)
        pytest.skip(msg)
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
    destination.symlink_to(source)

    subprocess.run([str(musefs_bin), "scan", str(destination), "--db", str(db_path)], check=True)

    conn = connect(str(db_path))
    try:
        rows = conn.execute("SELECT backing_path FROM tracks").fetchall()
    finally:
        conn.close()

    assert rows == [(realpath_key(source),)]
