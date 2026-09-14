import re

import pytest

pytest.importorskip("picard")
pytest.importorskip("pytestqt")  # qapp fixture, via picard_config


def test_callback_runs_sync_and_logs_summary(
    monkeypatch, db_path, make_track, fake_file, fake_metadata, picard_config
):
    import musefs

    path = "/music/a.flac"
    make_track(path)

    # callback() runs resolve_config(settings, os.environ), and MUSEFS_DB/
    # MUSEFS_BIN env vars take precedence over the configured values
    # (_core.py resolve_config). Clear them so a stray env var on the test
    # host can't redirect the write away from the seeded DB.
    monkeypatch.delenv("MUSEFS_DB", raising=False)
    monkeypatch.delenv("MUSEFS_BIN", raising=False)

    # Point the plugin at the seeded DB with autoscan off (no Rust binary).
    picard_config.setting["musefs_db"] = db_path
    picard_config.setting["musefs_bin"] = "musefs"
    picard_config.setting["musefs_autoscan"] = False
    picard_config.setting["musefs_fields"] = ""

    # Synchronous run_task: run the worker, hand its result to the callback.
    def fake_run_task(func, next_func=None, priority=0, thread_pool=None, traceback=True):
        result = func()
        if next_func is not None:
            next_func(result=result)

    monkeypatch.setattr(musefs.thread, "run_task", fake_run_task)

    logged = []
    monkeypatch.setattr(musefs.log, "info", lambda fmt, *a: logged.append(fmt % a))

    meta = fake_metadata(title="Song")
    f = fake_file(path, meta)

    musefs._action.callback([f])

    # _do_sync ran against the real DB.
    from musefs._common import connect

    conn = connect(db_path)
    try:
        assert conn.execute("SELECT value FROM tags WHERE key='title'").fetchone()[0] == "Song"
    finally:
        conn.close()

    # _done logged the success summary once, with exact counts: a substring test
    # would also accept "synced=10". The same summary is echoed to the status
    # line (logged without the file count), so match the summary's own shape.
    summaries = [
        (int(m[1]), int(m[2]))
        for line in logged
        if (m := re.fullmatch(r"musefs: synced=(\d+) .*\(files=(\d+)\)", line))
    ]
    assert summaries == [(1, 1)], logged
