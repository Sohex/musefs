import sqlite3
import time

import pytest
from musefs_common import connect as musefs_connect
from musefs_common.schema import SCHEMA_SQL


@pytest.fixture
def sample_artist():
    return {
        "id": 10,
        "artistName": "Boards of Canada",
        "foreignArtistId": "artist-mbid",
        "genres": ["Electronic", "IDM"],
    }


@pytest.fixture
def sample_album(sample_artist):
    return {
        "id": 20,
        "title": "Music Has the Right to Children",
        "foreignAlbumId": "release-group-mbid",
        "releaseDate": "1998-04-20T00:00:00Z",
        "genres": ["Electronic"],
        "artist": sample_artist,
    }


@pytest.fixture
def sample_track_file(tmp_path):
    path = tmp_path / "library" / "01 - Wildlife Analysis.flac"
    path.parent.mkdir()
    path.write_bytes(b"audio")
    return {
        "id": 30,
        "artistId": 10,
        "albumId": 20,
        "path": str(path),
        "releaseGroup": "Skam",
    }


@pytest.fixture
def sample_track(sample_track_file):
    return {
        "id": 40,
        "artistId": 10,
        "albumId": 20,
        "trackFileId": sample_track_file["id"],
        "foreignTrackId": "track-mbid",
        "foreignRecordingId": "recording-mbid",
        "trackNumber": "1",
        "mediumNumber": 1,
        "title": "Wildlife Analysis",
    }


@pytest.fixture
def db_path(tmp_path):
    path = tmp_path / "musefs.db"
    conn = sqlite3.connect(str(path))
    conn.executescript(SCHEMA_SQL)
    conn.commit()
    conn.close()
    return str(path)


def insert_track(conn, backing_path, fmt="flac"):
    now = int(time.time())
    cur = conn.execute(
        "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, "
        "backing_size, backing_mtime, updated_at) VALUES (?, ?, 0, 0, 0, 0, ?)",
        (backing_path, fmt, now),
    )
    return cur.lastrowid


@pytest.fixture
def make_track(db_path):
    def _make(backing_path, fmt="flac"):
        conn = musefs_connect(db_path)
        try:
            tid = insert_track(conn, backing_path, fmt)
            conn.commit()
            return tid
        finally:
            conn.close()

    return _make
