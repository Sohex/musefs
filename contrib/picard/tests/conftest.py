import sqlite3
import time
from pathlib import Path

import pytest

from musefs._common import connect as musefs_connect

SCHEMA_SQL = (Path(__file__).parent / "schema.sql").read_text()

# Minimal valid JPEG header + padding; used as fake cover-art bytes in tests.
JPEG = b"\xff\xd8\xff\xe0" + b"\x00" * 32


@pytest.fixture
def db_path(tmp_path):
    """A temp musefs DB with the V2 schema applied."""
    path = tmp_path / "musefs.db"
    conn = sqlite3.connect(str(path))
    conn.executescript(SCHEMA_SQL)
    conn.commit()
    conn.close()
    return str(path)


def insert_track(conn, backing_path, fmt="flac"):
    """Insert a minimal track row (as `musefs scan` would) and return its id."""
    now = int(time.time())
    cur = conn.execute(
        "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, "
        "backing_size, backing_mtime, updated_at) VALUES (?, ?, 0, 0, 0, 0, ?)",
        (backing_path, fmt, now),
    )
    return cur.lastrowid


@pytest.fixture
def make_track(db_path):
    """Return a helper that inserts a track row and returns its id."""

    def _make(backing_path, fmt="flac"):
        conn = musefs_connect(db_path)
        try:
            tid = insert_track(conn, backing_path, fmt)
            conn.commit()
            return tid
        finally:
            conn.close()

    return _make


class FakeImage:
    """Stand-in for a Picard CoverArtImage: is_front_image() + data + mimetype."""

    def __init__(self, data, mimetype, front=True):
        self.data = data
        self.mimetype = mimetype
        self._front = front

    def is_front_image(self):
        return self._front


class FakeMetadata:
    """Stand-in for Picard's Metadata: getall() + images."""

    def __init__(self, images=(), **tags):
        self._tags = {k: (v if isinstance(v, list) else [v]) for k, v in tags.items()}
        self.images = list(images)

    def getall(self, key):
        return self._tags.get(key, [])


@pytest.fixture
def fake_metadata():
    return FakeMetadata  # the class; call it directly in tests


@pytest.fixture
def fake_image():
    return FakeImage  # the class; call it directly in tests


@pytest.fixture
def picard_config(qapp, tmp_path):
    """Initialize Picard's global config headless against a temp ini.

    config.setting is None until setup_config runs; it needs a QApplication
    (pytest-qt's qapp). Importing the plugin after this declares its options,
    so config.setting[OPT_*] is then readable/writable.
    """
    from picard import config

    config.setup_config(qapp, str(tmp_path / "picard.ini"))
    return config


class FakeFile:
    """Stand-in for a Picard File: .filename + .metadata, and iterfiles()
    yields itself (matching how _resolved_files walks a selection)."""

    def __init__(self, filename, metadata):
        self.filename = filename
        self.metadata = metadata

    def iterfiles(self):
        return [self]


@pytest.fixture
def fake_file():
    return FakeFile  # the class; call it directly in tests
