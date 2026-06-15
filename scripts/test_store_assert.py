import sqlite3

from store_assert import count_arted_tracks, count_artist_tagged_tracks


def _store(tmp_path, rows):
    db = tmp_path / "store.db"
    con = sqlite3.connect(db)
    con.execute(
        "CREATE TABLE tags (track_id INTEGER, key TEXT, value TEXT, ordinal INTEGER DEFAULT 0)"
    )
    con.executemany("INSERT INTO tags (track_id, key, value) VALUES (?, ?, ?)", rows)
    con.commit()
    con.close()
    return str(db)


def test_counts_distinct_artist_tagged_tracks(tmp_path):
    db = _store(tmp_path, [(1, "artist", "Alice"), (1, "album", "Demo"), (2, "artist", "Alice")])
    assert count_artist_tagged_tracks(db) == 2


def test_ignores_empty_artist_values(tmp_path):
    db = _store(tmp_path, [(1, "artist", ""), (2, "artist", "Alice")])
    assert count_artist_tagged_tracks(db) == 1


def test_zero_when_no_artist_tags(tmp_path):
    db = _store(tmp_path, [(1, "title", "One")])
    assert count_artist_tagged_tracks(db) == 0


def _store_with_art(tmp_path, track_ids):
    db = tmp_path / "art.db"
    con = sqlite3.connect(db)
    con.executescript(
        "CREATE TABLE track_art (track_id INTEGER, art_id INTEGER, ordinal INTEGER DEFAULT 0);"
    )
    con.executemany(
        "INSERT INTO track_art (track_id, art_id) VALUES (?, 1)", [(t,) for t in track_ids]
    )
    con.commit()
    con.close()
    return str(db)


def test_counts_distinct_arted_tracks(tmp_path):
    db = _store_with_art(tmp_path, [1, 1, 2])
    assert count_arted_tracks(db) == 2


def test_zero_when_no_art(tmp_path):
    db = _store_with_art(tmp_path, [])
    assert count_arted_tracks(db) == 0
