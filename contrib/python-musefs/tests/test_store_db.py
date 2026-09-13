import os
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


def test_prune_missing_counts_a_repeated_track_id_once(db_path, tmp_path):
    """A caller passing the same id twice gets one delete and one count."""
    conn = connect(db_path)
    try:
        tid = insert_track(conn, str(tmp_path / "gone.flac"))
        conn.commit()
        assert prune_missing(conn, track_ids=[tid, tid]) == 1
        conn.commit()
        assert conn.execute("SELECT COUNT(*) FROM tracks").fetchone()[0] == 0
    finally:
        conn.close()


def test_prune_missing_keeps_a_track_it_cannot_stat(db_path, tmp_path, monkeypatch):
    """A stat failure is not a deletion: the row and its cascaded tags survive,
    and the caller can see why nothing was pruned (#692)."""
    from musefs_common.store import replace_tags

    unstattable = tmp_path / "locked" / "a.flac"
    real_stat = os.stat

    def fake_stat(path, *args, **kwargs):
        if str(path) == str(unstattable):
            raise PermissionError(13, "Permission denied")
        return real_stat(path, *args, **kwargs)

    monkeypatch.setattr(os, "stat", fake_stat)

    conn = connect(db_path)
    try:
        tid = insert_track(conn, str(unstattable))
        replace_tags(conn, tid, [("artist", "Alice")])
        conn.commit()
        unreadable = []
        pruned = prune_missing(conn, unreadable=unreadable)
        conn.commit()
        assert pruned == 0
        assert track_id_for_path(conn, str(unstattable)) == tid
        assert conn.execute("SELECT COUNT(*) FROM tags WHERE track_id=?", (tid,)).fetchone()[0] == 1
        assert unreadable == [(tid, str(unstattable), "[Errno 13] Permission denied")]
    finally:
        conn.close()


def test_prune_missing_scoped_keeps_a_track_it_cannot_stat(db_path, tmp_path, monkeypatch):
    """The scoped path shares the confirmed-absent rule with the full sweep."""
    unstattable = tmp_path / "locked" / "a.flac"
    real_stat = os.stat

    def fake_stat(path, *args, **kwargs):
        if str(path) == str(unstattable):
            raise OSError(5, "Input/output error")
        return real_stat(path, *args, **kwargs)

    monkeypatch.setattr(os, "stat", fake_stat)

    conn = connect(db_path)
    try:
        tid = insert_track(conn, str(unstattable))
        conn.commit()
        unreadable = []
        assert prune_missing(conn, track_ids=[tid], unreadable=unreadable) == 0
        conn.commit()
        assert track_id_for_path(conn, str(unstattable)) == tid
        assert [row[0] for row in unreadable] == [tid]
    finally:
        conn.close()


@pytest.mark.skipif(os.geteuid() == 0, reason="root can stat through a 0o000 directory")
def test_prune_missing_keeps_a_track_under_an_unsearchable_directory(db_path, tmp_path):
    """The real-world shape of #692: a permissions change on a parent directory
    makes an existing file unstattable, and the row must survive it."""
    locked = tmp_path / "locked"
    locked.mkdir()
    track = locked / "a.flac"
    track.write_bytes(b"x")
    locked.chmod(0o000)
    conn = connect(db_path)
    try:
        tid = insert_track(conn, str(track))
        conn.commit()
        unreadable = []
        assert prune_missing(conn, unreadable=unreadable) == 0
        conn.commit()
        assert track_id_for_path(conn, str(track)) == tid
        assert [row[:2] for row in unreadable] == [(tid, str(track))]
    finally:
        conn.close()
        locked.chmod(0o700)  # let tmp_path cleanup remove it


def test_prune_missing_reports_nothing_unreadable_for_a_clean_sweep(db_path, tmp_path):
    present = tmp_path / "present.flac"
    present.write_bytes(b"x")
    conn = connect(db_path)
    try:
        insert_track(conn, str(present))
        insert_track(conn, str(tmp_path / "gone.flac"))
        conn.commit()
        unreadable = []
        assert prune_missing(conn, unreadable=unreadable) == 1
        conn.commit()
        assert unreadable == []
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
    from musefs_common import path_param, track_ids_for_paths

    # backing_path is UNIQUE in the real schema, so the {key: id} dict can never
    # collapse against a conformant DB. Guard against a non-conformant one anyway:
    # silently dropping a duplicate row would hide a track from prune (#478).
    conn = sqlite3.connect(":memory:")
    try:
        # Bytes, as the real schema's BLOB column stores them: a TEXT row here
        # would not be found at all, and the guard under test would never see
        # the duplicate it exists to catch.
        conn.execute("CREATE TABLE tracks (id INTEGER PRIMARY KEY, backing_path BLOB)")
        for track_id in (1, 2):
            conn.execute(
                "INSERT INTO tracks (id, backing_path) VALUES (?, ?)",
                (track_id, path_param("/m/a.flac")),
            )
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


def test_delete_tracks_removes_rows_and_cascades(db_path):
    from musefs_common import (
        delete_tracks,
        replace_track_art,
        tags_for_track,
        track_id_for_path,
        upsert_art,
    )
    from musefs_common.store import replace_tags

    conn = connect(db_path)
    try:
        a = insert_track(conn, "/m/a.flac")
        b = insert_track(conn, "/m/b.flac")
        replace_tags(conn, a, [("artist", "Alice"), ("genre", "Rock")])
        # An art row referencing the track, to prove the cascade reaches track_art.
        # Use the public helpers so the inserts match the real art/track_art schema
        # (content-addressed sha256 + byte_len + data; track_art references art_id).
        art_id = upsert_art(conn, b"coverbytes")
        replace_track_art(conn, a, [(art_id, 3, "", "image/png")])
        conn.commit()

        deleted = delete_tracks(conn, [a])
        conn.commit()

        assert deleted == 1
        assert track_id_for_path(conn, "/m/a.flac") is None
        assert tags_for_track(conn, a) == []  # tags cascaded
        assert (
            conn.execute("SELECT COUNT(*) FROM track_art WHERE track_id=?", (a,)).fetchone()[0] == 0
        )
        assert track_id_for_path(conn, "/m/b.flac") == b  # untouched
    finally:
        conn.close()


def test_delete_tracks_counts_only_rows_actually_deleted(db_path):
    from musefs_common import delete_tracks

    conn = connect(db_path)
    try:
        a = insert_track(conn, "/m/a.flac")
        conn.commit()
        # 999 is not a real id, so it contributes 0 to the count.
        assert delete_tracks(conn, [a, 999]) == 1
        conn.commit()
    finally:
        conn.close()


def test_delete_tracks_empty_input(db_path):
    from musefs_common import delete_tracks

    conn = connect(db_path)
    try:
        assert delete_tracks(conn, []) == 0
    finally:
        conn.close()
