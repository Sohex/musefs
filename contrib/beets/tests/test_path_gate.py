"""§9.1 path-matching gate: assert the plugin's realpath key is byte-identical
to what the real `musefs scan` binary stores in `tracks.backing_path`."""

import os
import sqlite3
import subprocess
from pathlib import Path

import pytest

from beetsplug._core import realpath_key, track_id_for_path, connect

pytestmark = pytest.mark.musefs_bin

REPO_ROOT = Path(__file__).resolve().parents[3]
# Prefer debug build (fastest iteration); fall back to release.
_debug = REPO_ROOT / "target" / "debug" / "musefs"
_release = REPO_ROOT / "target" / "release" / "musefs"
MUSEFS_BIN = _debug if _debug.exists() else _release

# A minimal valid FLAC: 'fLaC' + a STREAMINFO metadata block (last-block flag set,
# type 0, length 34) of 34 zero bytes. Enough for `musefs scan` to probe.
MINIMAL_FLAC = b"fLaC" + b"\x80\x00\x00\x22" + b"\x00" * 34


def _scan(tmp_path, tree):
    db = tmp_path / "musefs.db"
    subprocess.run(
        [str(MUSEFS_BIN), "scan", str(tree), "--db", str(db)],
        check=True,
        capture_output=True,
    )
    return str(db)


def _stored_paths(db):
    conn = sqlite3.connect(db)
    try:
        return [r[0] for r in conn.execute("SELECT backing_path FROM tracks")]
    finally:
        conn.close()


@pytest.fixture(autouse=True)
def require_binary():
    if not MUSEFS_BIN.exists():
        pytest.skip(f"musefs binary not built at {MUSEFS_BIN}; run `cargo build -p musefs-cli`")


def _write_flac(path):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(MINIMAL_FLAC)


@pytest.mark.parametrize("rel", [
    "Artist/Album/01 Track.flac",
    "Accénted/テスト/01.flac",   # accented + CJK
    "Spaced Out/cover %20 thing/02 song.flac",     # spaces and percent
])
def test_plain_paths_match(tmp_path, rel):
    tree = tmp_path / "music"
    _write_flac(tree / rel)
    db = _scan(tmp_path, tree)
    stored = _stored_paths(db)
    assert len(stored) == 1
    # The path beets would hand us is the on-disk file path:
    item_path = os.fsencode(str(tree / rel))
    key = realpath_key(item_path)
    assert key == stored[0]
    conn = connect(db)
    try:
        assert track_id_for_path(conn, key) is not None
    finally:
        conn.close()


def test_symlinked_directory_component(tmp_path):
    real_tree = tmp_path / "real_music"
    _write_flac(real_tree / "Artist/Album/01.flac")
    link_tree = tmp_path / "linked_music"
    link_tree.symlink_to(real_tree)
    db = _scan(tmp_path, link_tree)
    stored = _stored_paths(db)
    assert len(stored) == 1
    # beets stores the path as accessed through the symlink; realpath resolves it.
    key = realpath_key(os.fsencode(str(link_tree / "Artist/Album/01.flac")))
    assert key == stored[0]


def test_symlink_to_file(tmp_path):
    tree = tmp_path / "music"
    real = tree / "real.flac"
    _write_flac(real)
    link = tree / "link.flac"
    link.symlink_to(real)
    db = _scan(tmp_path, tree)
    stored = set(_stored_paths(db))
    # Both names resolve to the same real file; realpath of either is in the set.
    assert realpath_key(os.fsencode(str(link))) in stored


def test_relative_and_dotdot_input(tmp_path, monkeypatch):
    tree = tmp_path / "music"
    _write_flac(tree / "Artist/01.flac")
    db = _scan(tmp_path, tree)
    stored = _stored_paths(db)
    monkeypatch.chdir(tree)
    key = realpath_key(os.fsencode("Artist/../Artist/01.flac"))
    assert key == stored[0]


def test_trailing_slash_and_nonnormalised_input(tmp_path):
    tree = tmp_path / "music"
    _write_flac(tree / "Artist/01.flac")
    db = _scan(tmp_path, tree)
    stored = _stored_paths(db)
    key = realpath_key(os.fsencode(str(tree) + "/Artist/./01.flac"))
    assert key == stored[0]


def test_path_under_different_tree_is_skipped_not_mismatched(tmp_path):
    tree_a = tmp_path / "a"
    _write_flac(tree_a / "01.flac")
    db = _scan(tmp_path, tree_a)
    # A file beets knows under a different tree that was never scanned:
    tree_b = tmp_path / "b"
    _write_flac(tree_b / "01.flac")
    key = realpath_key(os.fsencode(str(tree_b / "01.flac")))
    conn = connect(db)
    try:
        assert track_id_for_path(conn, key) is None  # skipped, never a wrong hit
    finally:
        conn.close()
