import pytest
from conftest import JPEG

from musefs._common import connect
from musefs._core import Opts

pytest.importorskip("picard")


def test_do_sync_writes_tags_and_art(db_path, make_track, fake_file, fake_metadata, fake_image):
    import musefs

    path = "/music/a.flac"
    tid = make_track(path)
    png = b"\x89PNG\r\n\x1a\n" + b"\x00" * 16
    meta = fake_metadata(
        images=[
            fake_image(JPEG, "image/jpeg"),
            fake_image(png, "image/png", front=False, maintype="back"),
        ],
        title="Song",
        artist="Band",
    )
    f = fake_file(path, meta)
    files = {path: f}  # key is already a realpath for an absolute test path
    opts = Opts(db=db_path, bin="musefs", autoscan=False, fields={})

    stats = musefs._do_sync(opts, files)

    assert stats.synced == 1
    assert stats.art_linked == 1
    conn = connect(db_path)
    try:
        title = conn.execute(
            "SELECT value FROM tags WHERE track_id=? AND key='title'", (tid,)
        ).fetchone()[0]
        assert title == "Song"
        rows = conn.execute(
            "SELECT picture_type, ordinal FROM track_art WHERE track_id=? ORDER BY ordinal",
            (tid,),
        ).fetchall()
        assert rows == [(3, 0), (4, 1)]
    finally:
        conn.close()


def test_do_sync_no_db_raises():
    import musefs
    from musefs._core import MusefsError

    opts = Opts(db=None, bin="musefs", autoscan=False, fields={})
    with pytest.raises(MusefsError):
        musefs._do_sync(opts, {})


def test_do_sync_schema_mismatch_raises_musefs_error(db_path):
    import musefs
    from musefs._core import MusefsError

    conn = connect(db_path)
    try:
        conn.execute("PRAGMA user_version = 99")
        conn.commit()
    finally:
        conn.close()
    opts = Opts(db=db_path, bin="musefs", autoscan=False, fields={})
    # A SchemaMismatch from the library must surface as the host-native
    # MusefsError, like ScanError — not leak the library exception type.
    with pytest.raises(MusefsError):
        musefs._do_sync(opts, {})
