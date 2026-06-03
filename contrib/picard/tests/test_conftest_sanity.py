from musefs._core import connect, track_id_for_path


def test_db_path_has_schema(db_path):
    conn = connect(db_path)
    try:
        # user_version applied from the fixture SQL.
        assert conn.execute("PRAGMA user_version").fetchone()[0] == 2
    finally:
        conn.close()


def test_make_track_inserts_row(db_path, make_track):
    tid = make_track("/music/a.flac")
    conn = connect(db_path)
    try:
        assert track_id_for_path(conn, "/music/a.flac") == tid
    finally:
        conn.close()
