from types import SimpleNamespace

import pytest

pytest.importorskip("picard")

import musefs as plugin_mod


def test_autoscan_batches_into_one_run_scan(monkeypatch, db_path):
    calls = []

    def fake_run_scan(binary, db, targets, *, timeout=None):
        calls.append((targets, timeout))

    monkeypatch.setattr(plugin_mod, "run_scan", fake_run_scan)
    monkeypatch.setattr(plugin_mod, "check_schema_version", lambda conn: None)
    monkeypatch.setattr(plugin_mod, "sync_files", lambda conn, records: SimpleNamespace())
    monkeypatch.setattr(plugin_mod, "map_fields", lambda md, fields: [])
    monkeypatch.setattr(plugin_mod, "images", lambda md: [])

    opts = SimpleNamespace(db=db_path, bin="musefs", autoscan=True, fields={})
    files = {
        "/music/a.flac": SimpleNamespace(filename="/music/a.flac", metadata=object()),
        "/music/b.flac": SimpleNamespace(filename="/music/b.flac", metadata=object()),
    }
    plugin_mod._do_sync(opts, files)

    assert len(calls) == 1
    targets, timeout = calls[0]
    assert sorted(targets) == ["/music/a.flac", "/music/b.flac"]
    assert timeout == plugin_mod.SCAN_TIMEOUT_SECONDS == 120


def test_run_scan_force_appends_force(monkeypatch):
    import subprocess

    import musefs._common.scan as scan

    captured = {}

    class FakeResult:
        returncode = 0
        stderr = b""

    def fake_run(argv, **kw):
        captured["argv"] = argv
        return FakeResult()

    monkeypatch.setattr(subprocess, "run", fake_run)
    scan.run_scan("musefs", "/db.sqlite", "/only.flac", force=True)
    assert captured["argv"] == ["musefs", "scan", "/only.flac", "--db", "/db.sqlite", "--force"]


def test_run_scan_revalidate_uses_subcommand_and_prune(monkeypatch):
    import subprocess

    import musefs._common.scan as scan

    captured = {}

    class FakeResult:
        returncode = 0
        stderr = b""

    def fake_run(argv, **kw):
        captured["argv"] = argv
        return FakeResult()

    monkeypatch.setattr(subprocess, "run", fake_run)
    scan.run_scan("musefs", "/db.sqlite", "/only.flac", revalidate=True, prune=True)
    assert captured["argv"] == [
        "musefs",
        "revalidate",
        "/only.flac",
        "--db",
        "/db.sqlite",
        "--prune",
    ]
