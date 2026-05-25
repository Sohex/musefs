import os

from beetsplug._core import connect, sync_items

JPEG = b"\xff\xd8\xff\xe0" + b"\x00" * 32


def write_cover(tmp_path, name, data=JPEG):
    p = tmp_path / name
    p.write_bytes(data)
    return os.fsencode(str(p))


def test_skip_when_no_row(db_path, fake_item):
    conn = connect(db_path)
    try:
        item = fake_item(os.fsencode("/music/missing.flac"), title="X")
        stats = sync_items(conn, [item])
        conn.commit()
        assert stats.synced == 0
        assert stats.skipped == 1
    finally:
        conn.close()


def test_tags_written_for_existing_row(db_path, make_track, fake_item):
    tid = make_track("/music/a.flac")
    conn = connect(db_path)
    try:
        item = fake_item(os.fsencode("/music/a.flac"), title="Song", artist="Band")
        stats = sync_items(conn, [item])
        conn.commit()
        assert stats.synced == 1
        title = conn.execute(
            "SELECT value FROM tags WHERE track_id=? AND key='title'", (tid,)
        ).fetchone()[0]
        assert title == "Song"
    finally:
        conn.close()


def test_art_linked_when_album_has_cover(tmp_path, db_path, make_track, fake_item, fake_album):
    tid = make_track("/music/a.flac")
    cover = write_cover(tmp_path, "cover.jpg")
    conn = connect(db_path)
    try:
        album = fake_album(artpath=cover)
        item = fake_item(os.fsencode("/music/a.flac"), album=album, title="Song")
        stats = sync_items(conn, [item])
        conn.commit()
        assert stats.art_linked == 1
        assert conn.execute(
            "SELECT COUNT(*) FROM track_art WHERE track_id=?", (tid,)
        ).fetchone()[0] == 1
    finally:
        conn.close()


def test_existing_embedded_art_preserved_when_no_beets_art(db_path, make_track, fake_item):
    # Simulate scan-ingested art already linked to the track.
    tid = make_track("/music/a.flac")
    conn = connect(db_path)
    try:
        conn.execute(
            "INSERT INTO art (sha256, mime, byte_len, data) VALUES "
            "('deadbeef', 'image/jpeg', 3, X'aabbcc')"
        )
        art_id = conn.execute("SELECT id FROM art WHERE sha256='deadbeef'").fetchone()[0]
        conn.execute(
            "INSERT INTO track_art (track_id, art_id) VALUES (?, ?)", (tid, art_id)
        )
        conn.commit()
        # beets item with no album art:
        item = fake_item(os.fsencode("/music/a.flac"), album=None, title="Song")
        stats = sync_items(conn, [item])
        conn.commit()
        assert stats.art_linked == 0
        # The pre-existing track_art row is untouched.
        row = conn.execute(
            "SELECT art_id FROM track_art WHERE track_id=?", (tid,)
        ).fetchone()
        assert row == (art_id,)
    finally:
        conn.close()


def test_oversized_art_skipped(tmp_path, db_path, make_track, fake_item, fake_album, monkeypatch):
    import beetsplug._core as core

    monkeypatch.setattr(core, "MAX_ART_BYTES", 8)
    tid = make_track("/music/a.flac")
    cover = write_cover(tmp_path, "big.jpg", data=b"X" * 64)
    conn = connect(db_path)
    try:
        album = fake_album(artpath=cover)
        item = fake_item(os.fsencode("/music/a.flac"), album=album, title="Song")
        stats = sync_items(conn, [item])
        conn.commit()
        assert stats.skipped_art == 1
        assert stats.art_linked == 0
    finally:
        conn.close()


def test_art_deduped_across_items(tmp_path, db_path, make_track, fake_item, fake_album):
    t1 = make_track("/music/a.flac")
    t2 = make_track("/music/b.flac")
    cover = write_cover(tmp_path, "cover.jpg")
    album = fake_album(artpath=cover)
    conn = connect(db_path)
    try:
        items = [
            fake_item(os.fsencode("/music/a.flac"), album=album, title="A"),
            fake_item(os.fsencode("/music/b.flac"), album=album, title="B"),
        ]
        sync_items(conn, items)
        conn.commit()
        assert conn.execute("SELECT COUNT(*) FROM art").fetchone()[0] == 1
        assert conn.execute("SELECT COUNT(*) FROM track_art").fetchone()[0] == 2
    finally:
        conn.close()


def test_dry_run_writes_nothing(db_path, make_track, fake_item):
    tid = make_track("/music/a.flac")
    conn = connect(db_path)
    try:
        item = fake_item(os.fsencode("/music/a.flac"), title="Song")
        stats = sync_items(conn, [item], dry_run=True)
        conn.rollback()
        assert stats.synced == 1
        assert conn.execute(
            "SELECT COUNT(*) FROM tags WHERE track_id=?", (tid,)
        ).fetchone()[0] == 0
    finally:
        conn.close()
