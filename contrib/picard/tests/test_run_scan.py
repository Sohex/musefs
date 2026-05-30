import subprocess
from types import SimpleNamespace

import pytest

from musefs._core import MusefsError, run_scan


def test_run_scan_invokes_binary(monkeypatch):
    calls = []

    def fake_run(cmd, capture_output):
        calls.append(cmd)
        return SimpleNamespace(returncode=0, stdout=b"", stderr=b"")

    monkeypatch.setattr(subprocess, "run", fake_run)
    run_scan("musefs", "/db.sqlite", "/music/a.flac")
    assert calls == [["musefs", "scan", "/music/a.flac", "--db", "/db.sqlite"]]


def test_run_scan_missing_binary_raises(monkeypatch):
    def fake_run(cmd, capture_output):
        raise FileNotFoundError()

    monkeypatch.setattr(subprocess, "run", fake_run)
    with pytest.raises(MusefsError, match="not found"):
        run_scan("nope", "/db.sqlite", "/music/a.flac")


def test_run_scan_nonzero_exit_raises(monkeypatch):
    def fake_run(cmd, capture_output):
        return SimpleNamespace(returncode=2, stdout=b"", stderr=b"boom")

    monkeypatch.setattr(subprocess, "run", fake_run)
    with pytest.raises(MusefsError, match="boom"):
        run_scan("musefs", "/db.sqlite", "/music/a.flac")
