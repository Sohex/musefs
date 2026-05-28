"""Test the external-writer contract for the musefs SQLite schema."""

import sqlite3


def test_external_writer_cannot_mutate_scanner_owned_field_gracefully():
    """Attempting to mutate a scanner-owned field should not crash the
    application -- it should handle the inconsistent row gracefully."""
    conn = sqlite3.connect(":memory:")
    conn.executescript("""
        CREATE TABLE tracks (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            backing_path TEXT NOT NULL,
            format TEXT NOT NULL,
            audio_offset INTEGER NOT NULL,
            audio_length INTEGER NOT NULL,
            backing_size INTEGER NOT NULL,
            backing_mtime INTEGER NOT NULL,
            content_version INTEGER NOT NULL DEFAULT 1,
            updated_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE TABLE tags (
            track_id INTEGER NOT NULL,
            key TEXT NOT NULL,
            value TEXT NOT NULL,
            ordinal INTEGER NOT NULL,
            PRIMARY KEY (track_id, key, ordinal),
            FOREIGN KEY (track_id) REFERENCES tracks(id) ON DELETE CASCADE
        );
    """)
    # External writer inserts a track with plausible structural fields.
    conn.execute("""
        INSERT INTO tracks (backing_path, format, audio_offset, audio_length,
                            backing_size, backing_mtime)
        VALUES ('/music/a.flac', 'Flac', 0, 100, 2000, 1700000000)
    """)
    conn.commit()
    # The external writer can write to scanner-owned fields -- SQLite has no
    # constraints preventing this. The test documents that the application
    # must not rely on these fields being stable when written externally.
    cur = conn.execute("SELECT audio_offset FROM tracks WHERE id = 1")
    assert cur.fetchone()[0] == 0

    # External writer should be able to write tags without issues.
    conn.execute(
        "INSERT INTO tags (track_id, key, value, ordinal) VALUES (?, ?, ?, ?)",
        (1, "title", "External Tag", 0),
    )
    conn.commit()
    cur = conn.execute("SELECT value FROM tags WHERE track_id = 1 AND key = 'title'")
    assert cur.fetchone()[0] == "External Tag"
