import pytest
from conftest import JPEG

from musefs._common import connect
from musefs._core import Opts

pytest.importorskip("picard")


def test_do_sync_writes_tags_and_art(db_path, make_track, fake_file, fake_metadata, fake_image):
    import musefs

    path = "/music/a.flac"
    tid = make_track(path)
    meta = fake_metadata(images=[fake_image(JPEG, "image/jpeg")], title="Song", artist="Band")
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
        assert (
            conn.execute("SELECT COUNT(*) FROM track_art WHERE track_id=?", (tid,)).fetchone()[0]
            == 1
        )
    finally:
        conn.close()


def test_do_sync_no_db_raises():
    import musefs
    from musefs._core import MusefsError

    opts = Opts(db=None, bin="musefs", autoscan=False, fields={})
    with pytest.raises(MusefsError):
        musefs._do_sync(opts, {})
