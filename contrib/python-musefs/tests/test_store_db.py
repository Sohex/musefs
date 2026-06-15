import sqlite3

import pytest
from conftest import insert_track

from musefs_common import connect, prune_missing, track_id_for_path
from musefs_common.errors import SchemaMismatch
from musefs_common.store import check_schema_version


def test_connect_sets_pragmas(db_path):
    conn = connect(db_path)
    try:
        assert conn.execute("PRAGMA foreign_keys").fetchone()[0] == 1
        assert conn.execute("PRAGMA busy_timeout").fetchone()[0] == 5000
    finally:
        conn.close()


def test_check_schema_version_passes_on_v3(db_path):
    conn = connect(db_path)
    try:
        check_schema_version(conn)  # SCHEMA_SQL stamps the latest user_version
    finally:
        conn.close()


def test_check_schema_version_raises_on_mismatch(db_path):
    conn = connect(db_path)
    try:
        conn.execute("PRAGMA user_version = 99")
        with pytest.raises(SchemaMismatch) as ei:
            check_schema_version(conn)
        assert ei.value.found == 99
    finally:
        conn.close()


def test_track_id_for_path_found_and_missing(db_path):
    conn = connect(db_path)
    try:
        tid = insert_track(conn, "/music/a.flac")
        conn.commit()
        assert track_id_for_path(conn, "/music/a.flac") == tid
        assert track_id_for_path(conn, "/music/nope.flac") is None
    finally:
        conn.close()


def test_prune_missing_removes_absent_backing_files(db_path, tmp_path):
    present = tmp_path / "present.flac"
    present.write_bytes(b"x")
    conn = connect(db_path)
    try:
        keep = insert_track(conn, str(present))
        gone = insert_track(conn, str(tmp_path / "gone.flac"))
        conn.commit()
        pruned = prune_missing(conn)
        conn.commit()
        assert pruned == 1
        assert track_id_for_path(conn, str(present)) == keep
        assert conn.execute("SELECT COUNT(*) FROM tracks WHERE id=?", (gone,)).fetchone()[0] == 0
    finally:
        conn.close()


def test_prune_missing_scoped_to_track_ids(db_path, tmp_path):
    conn = connect(db_path)
    try:
        a = insert_track(conn, str(tmp_path / "a.flac"))  # absent
        b = insert_track(conn, str(tmp_path / "b.flac"))  # absent, but not in scope
        conn.commit()
        pruned = prune_missing(conn, track_ids=[a])
        conn.commit()
        assert pruned == 1
        assert track_id_for_path(conn, str(tmp_path / "b.flac")) == b
    finally:
        conn.close()


def test_tags_for_track_returns_text_and_binary_rows(db_path):
    from musefs_common import TagRow, tags_for_track
    from musefs_common.store import replace_tags

    conn = connect(db_path)
    try:
        tid = insert_track(conn, "/m/a.flac")
        replace_tags(conn, tid, [("artist", "Alice"), ("genre", "Rock"), ("genre", "Pop")])
        conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal, value_blob) "
            "VALUES (?, 'cover', '', 0, ?)",
            (tid, b"\x00\x01"),
        )
        conn.commit()
        assert tags_for_track(conn, tid) == [
            TagRow("artist", "Alice", None),
            TagRow("cover", "", b"\x00\x01"),
            TagRow("genre", "Rock", None),
            TagRow("genre", "Pop", None),
        ]
    finally:
        conn.close()


def test_tags_for_track_empty_when_no_tags(db_path):
    from musefs_common import tags_for_track

    conn = connect(db_path)
    try:
        tid = insert_track(conn, "/m/a.flac")
        conn.commit()
        assert tags_for_track(conn, tid) == []
    finally:
        conn.close()


def test_track_ids_for_paths_resolves_present_only(db_path):
    from musefs_common import track_ids_for_paths

    conn = connect(db_path)
    try:
        a = insert_track(conn, "/m/a.flac")
        b = insert_track(conn, "/m/b.flac")
        conn.commit()
        assert track_ids_for_paths(conn, ["/m/a.flac", "/m/x.flac", "/m/b.flac"]) == {
            "/m/a.flac": a,
            "/m/b.flac": b,
        }
    finally:
        conn.close()


def test_track_ids_for_paths_empty_input(db_path):
    from musefs_common import track_ids_for_paths

    conn = connect(db_path)
    try:
        assert track_ids_for_paths(conn, []) == {}
    finally:
        conn.close()


def test_track_ids_for_paths_chunks_past_variable_limit(db_path):
    from musefs_common import track_ids_for_paths

    conn = connect(db_path)
    try:
        paths = [f"/m/{i:05d}.flac" for i in range(1500)]
        expected = {path: insert_track(conn, path) for path in paths}
        conn.commit()
        assert track_ids_for_paths(conn, paths) == expected
    finally:
        conn.close()


def test_track_ids_for_paths_raises_on_duplicate_backing_path():
    from musefs_common import track_ids_for_paths

    # backing_path is UNIQUE in the real schema, so the {key: id} dict can never
    # collapse against a conformant DB. Guard against a non-conformant one anyway:
    # silently dropping a duplicate row would hide a track from prune (#478).
    conn = sqlite3.connect(":memory:")
    try:
        conn.execute("CREATE TABLE tracks (id INTEGER PRIMARY KEY, backing_path TEXT)")
        conn.execute("INSERT INTO tracks (id, backing_path) VALUES (1, '/m/a.flac')")
        conn.execute("INSERT INTO tracks (id, backing_path) VALUES (2, '/m/a.flac')")
        with pytest.raises(ValueError):
            track_ids_for_paths(conn, ["/m/a.flac"])
    finally:
        conn.close()


def test_track_ids_by_tag_matches_text_rows(db_path):
    from musefs_common import track_ids_by_tag
    from musefs_common.store import replace_tags

    conn = connect(db_path)
    try:
        a = insert_track(conn, "/m/a.flac")
        b = insert_track(conn, "/m/b.flac")
        c = insert_track(conn, "/m/c.flac")
        replace_tags(conn, a, [("musicbrainz_albumid", "rg-1")])
        replace_tags(conn, b, [("musicbrainz_albumid", "rg-1")])
        replace_tags(conn, c, [("musicbrainz_albumid", "rg-2")])
        conn.commit()
        assert set(track_ids_by_tag(conn, "musicbrainz_albumid", "rg-1")) == {a, b}
        assert track_ids_by_tag(conn, "musicbrainz_albumid", "rg-2") == [c]
        assert track_ids_by_tag(conn, "musicbrainz_albumid", "nope") == []
    finally:
        conn.close()


def test_track_ids_by_tag_ignores_binary_tags(db_path):
    from musefs_common import track_ids_by_tag

    conn = connect(db_path)
    try:
        tid = insert_track(conn, "/m/a.flac")
        # A scanner-written binary tag (value_blob NOT NULL) must never match.
        conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal, value_blob) "
            "VALUES (?, 'cover', '', 0, ?)",
            (tid, b"\x00\x01"),
        )
        conn.commit()
        assert track_ids_by_tag(conn, "cover", "") == []
    finally:
        conn.close()
