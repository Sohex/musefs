"""§9.1 path-matching gate: assert the plugin's realpath key is byte-identical
to what the real `musefs scan` binary stores in `tracks.backing_path`."""

import os
import sqlite3
import subprocess
import warnings
from pathlib import Path

import pytest
from musefs_common import connect, path_value, realpath_key, track_id_for_path

pytestmark = pytest.mark.musefs_bin

REPO_ROOT = Path(__file__).resolve().parents[3]


def _resolve_musefs_bin():
    env = os.environ.get("MUSEFS_BIN")
    if env:
        return Path(env)
    debug = REPO_ROOT / "target" / "debug" / "musefs"
    release = REPO_ROOT / "target" / "release" / "musefs"
    return debug if debug.exists() else release


MUSEFS_BIN = _resolve_musefs_bin()

# A minimal valid FLAC: 'fLaC' + a STREAMINFO metadata block (last-block flag set,
# type 0, length 34) of 34 zero bytes. Enough for `musefs scan` to probe. If a
# future scan prober rejects it (zero rows stored), replace with a real fixture
# under tests/fixtures/ — _scan() below surfaces scan output to diagnose that.
MINIMAL_FLAC = b"fLaC" + b"\x80\x00\x00\x22" + b"\x00" * 34


def _newest_rs_mtime():
    newest = 0.0
    for crate in ("musefs-db", "musefs-format", "musefs-core", "musefs-fuse", "musefs-cli"):
        src = REPO_ROOT / crate / "src"
        if src.exists():
            for rs in src.rglob("*.rs"):
                newest = max(newest, rs.stat().st_mtime)
    return newest


def _scan(tmp_path, tree):
    db = tmp_path / "musefs.db"
    result = subprocess.run(
        [str(MUSEFS_BIN), "scan", str(tree), "--db", str(db)],
        capture_output=True,
    )
    if result.returncode != 0:
        pytest.fail(
            f"musefs scan exited {result.returncode}\n"
            f"stdout: {result.stdout.decode(errors='replace')}\n"
            f"stderr: {result.stderr.decode(errors='replace')}"
        )
    return str(db)


def _stored_paths(db):
    conn = sqlite3.connect(db)
    try:
        return [path_value(r[0]) for r in conn.execute("SELECT backing_path FROM tracks")]
    finally:
        conn.close()


def _stored_rows(db):
    """Every ``(id, backing_path)`` row, the path as the raw value SQLite holds.

    Raw on purpose: ``path_value`` also accepts a ``str``, so comparing decoded
    values would pass a scanner that stored ``TEXT`` — which no ``BLOB`` key
    ever matches (see ``path_param``).
    """
    conn = sqlite3.connect(db)
    try:
        return conn.execute("SELECT id, backing_path FROM tracks ORDER BY id").fetchall()
    finally:
        conn.close()


def _assert_gate(db, path):
    """The gate, both directions, for a scan that stored exactly one row.

    Store side: the raw value is the realpath's bytes, verbatim. Plugin side:
    the key decodes from those bytes and looks up that row's id, through
    ``path_param`` — the direction a plugin actually uses.
    """
    rows = _stored_rows(db)
    assert len(rows) == 1, f"one file, one row: {rows!r}"
    ((track_id, raw),) = rows
    assert isinstance(raw, bytes), f"backing_path stored as {type(raw).__name__}"
    assert raw == os.path.realpath(os.fsencode(path))
    key = realpath_key(path)
    assert key == path_value(raw)
    conn = connect(db)
    try:
        assert track_id_for_path(conn, key) == track_id
    finally:
        conn.close()


@pytest.fixture(autouse=True)
def require_binary():
    if not MUSEFS_BIN.exists():
        msg = f"musefs binary not built at {MUSEFS_BIN}; run `cargo build` or set MUSEFS_BIN"
        # In CI's contract tier a missing binary is a hard failure, not a skip.
        if os.environ.get("MUSEFS_REQUIRE_BIN"):
            pytest.fail(msg)
        pytest.skip(msg)
    if MUSEFS_BIN.stat().st_mtime < _newest_rs_mtime():
        warnings.warn(
            f"{MUSEFS_BIN} is older than the musefs Rust sources; rebuild with "
            f"`cargo build` before trusting a pass.",
            stacklevel=2,
        )


def _write_flac(path):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(MINIMAL_FLAC)


@pytest.mark.parametrize(
    "rel",
    [
        "Artist/Album/01 Track.flac",
        "Accénted/テスト/01.flac",  # accented + CJK
        "Spaced Out/cover %20 thing/02 song.flac",  # spaces and percent
    ],
)
def test_plain_paths_match(tmp_path, rel):
    tree = tmp_path / "music"
    _write_flac(tree / rel)
    db = _scan(tmp_path, tree)
    # The path beets would hand us is the on-disk file path:
    _assert_gate(db, os.fsencode(str(tree / rel)))


def test_symlinked_directory_component(tmp_path):
    real_tree = tmp_path / "real_music"
    _write_flac(real_tree / "Artist/Album/01.flac")
    link_tree = tmp_path / "linked_music"
    link_tree.symlink_to(real_tree)
    db = _scan(tmp_path, link_tree)
    # beets stores the path as accessed through the symlink; realpath resolves it.
    _assert_gate(db, os.fsencode(str(link_tree / "Artist/Album/01.flac")))


def test_symlink_to_file(tmp_path):
    tree = tmp_path / "music"
    real = tree / "real.flac"
    _write_flac(real)
    link = tree / "link.flac"
    link.symlink_to(real)
    db = _scan(tmp_path, tree)
    # Both names resolve to the same real file and dedup to one canonical row,
    # and either name finds it.
    _assert_gate(db, os.fsencode(str(link)))
    _assert_gate(db, os.fsencode(str(real)))


def test_relative_and_dotdot_input(tmp_path, monkeypatch):
    tree = tmp_path / "music"
    _write_flac(tree / "Artist/01.flac")
    db = _scan(tmp_path, tree)
    monkeypatch.chdir(tree)
    _assert_gate(db, os.fsencode("Artist/../Artist/01.flac"))


def test_trailing_slash_and_nonnormalised_input(tmp_path):
    tree = tmp_path / "music"
    _write_flac(tree / "Artist/01.flac")
    db = _scan(tmp_path, tree)
    _assert_gate(db, os.fsencode(str(tree) + "/Artist/./01.flac"))


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


def test_non_utf8_paths_match_and_stay_distinct(tmp_path):
    """The gate this file exists for, at the case that used to break it (#680).

    Two filenames differing only in a byte that is not valid UTF-8. The scanner
    stores those bytes verbatim from schema v4 on, so the plugin's key has to
    encode back to them — and the two must stay two.

    Before #680's plugin half, `realpath_key` normalized both to the same
    `U+FFFD` form. That matched the scanner while *it* was lossy too, and the
    pair collapsed onto one row; once the scanner stored real bytes it matched
    nothing at all, and the plugin skipped such files without saying so.
    """
    tree = tmp_path / "music"
    tree.mkdir(parents=True, exist_ok=True)
    a = os.fsencode(str(tree)) + b"/bad\x80name.flac"
    b = os.fsencode(str(tree)) + b"/bad\x81name.flac"
    try:
        for raw in (a, b):
            with open(raw, "wb") as fh:
                fh.write(MINIMAL_FLAC)
    except OSError as e:
        # APFS and HFS+ enforce valid UTF-8 and refuse the create with EILSEQ.
        # A filesystem that will not host the name cannot reach the defect
        # either, so there is nothing to assert here.
        pytest.skip(f"filesystem will not accept a non-UTF-8 filename: {e}")

    # The precondition the whole test rests on: these two collide under the old
    # lossy rendering, so a key that still used it would confuse them.
    assert os.fsdecode(a) != os.fsdecode(b)
    assert a.decode("utf-8", "replace") == b.decode("utf-8", "replace")

    db = _scan(tmp_path, tree)
    stored = _stored_paths(db)
    assert len(stored) == 2, f"two files, two rows: {stored!r}"

    # Each file's own bytes, verbatim, keyed by the row that holds them.
    by_raw = {raw: track_id for track_id, raw in _stored_rows(db)}
    assert set(by_raw) == {os.path.realpath(a), os.path.realpath(b)}

    conn = connect(db)
    try:
        ids = set()
        for raw in (a, b):
            key = realpath_key(raw)
            assert key in stored, f"{key!r} not in {stored!r}"
            assert os.fsencode(key) == os.path.realpath(raw)
            track_id = track_id_for_path(conn, key)
            # Not just some row: the one holding this file's bytes.
            assert track_id == by_raw[os.path.realpath(raw)], f"wrong row for {key!r}"
            ids.add(track_id)
        assert len(ids) == 2, "the two files must not resolve to one row"
    finally:
        conn.close()
