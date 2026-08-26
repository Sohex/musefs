from musefs._common import EXPECTED_USER_VERSION, connect, track_id_for_path


def test_db_path_has_schema(db_path):
    conn = connect(db_path)
    try:
        # user_version applied from the fixture SQL. Compared against the
        # vendored mirror rather than a literal, so a schema migration does not
        # break a test that is really only asserting "the fixture ran".
        assert conn.execute("PRAGMA user_version").fetchone()[0] == EXPECTED_USER_VERSION
    finally:
        conn.close()


def test_make_track_inserts_row(db_path, make_track):
    tid = make_track("/music/a.flac")
    conn = connect(db_path)
    try:
        assert track_id_for_path(conn, "/music/a.flac") == tid
    finally:
        conn.close()
