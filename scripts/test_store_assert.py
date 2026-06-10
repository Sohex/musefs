import sqlite3

from store_assert import count_artist_tagged_tracks


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
