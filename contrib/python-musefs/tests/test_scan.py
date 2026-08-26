import stat

import pytest

from musefs_common import run_scan
from musefs_common.errors import ScanError


def _fake_binary(tmp_path, body):
    p = tmp_path / "fakemusefs"
    p.write_text("#!/bin/sh\n" + body + "\n")
    p.chmod(p.stat().st_mode | stat.S_IEXEC)
    return str(p)


def test_run_scan_success(tmp_path):
    binary = _fake_binary(tmp_path, "exit 0")
    run_scan(binary, str(tmp_path / "m.db"), str(tmp_path / "a.flac"))  # no raise


def test_run_scan_binary_not_found(tmp_path):
    with pytest.raises(ScanError) as ei:
        run_scan(str(tmp_path / "does-not-exist"), str(tmp_path / "m.db"), "/a.flac")
    assert ei.value.kind == "not_found"
    assert ei.value.binary.endswith("does-not-exist")


def test_run_scan_nonzero_exit_carries_stderr(tmp_path):
    binary = _fake_binary(tmp_path, "echo 'bad file' >&2; exit 3")
    with pytest.raises(ScanError) as ei:
        run_scan(binary, str(tmp_path / "m.db"), "/a.flac")
    assert ei.value.kind == "failed"
    assert ei.value.returncode == 3
    assert ei.value.stderr == "bad file"


def test_run_scan_timeout(tmp_path):
    binary = _fake_binary(tmp_path, "sleep 5")
    with pytest.raises(ScanError) as ei:
        run_scan(binary, str(tmp_path / "m.db"), "/a.flac", timeout=1)
    assert ei.value.kind == "timeout"
    assert ei.value.timeout == 1


def test_run_scan_multiple_targets_one_invocation(monkeypatch):
    import subprocess

    import musefs_common.scan as scan

    calls = []

    class FakeResult:
        returncode = 0
        stderr = b""

    def fake_run(argv, **kwargs):
        calls.append(argv)
        return FakeResult()

    monkeypatch.setattr(subprocess, "run", fake_run)
    scan.run_scan("musefs", "/db.sqlite", ["/a.flac", "/b.flac"])

    assert len(calls) == 1
    argv = calls[0]
    assert argv == ["musefs", "scan", "/a.flac", "/b.flac", "--db", "/db.sqlite"]
    # All targets precede the --db flag.
    assert argv.index("/b.flac") < argv.index("--db")


def test_run_scan_force_appends_force(monkeypatch):
    import subprocess

    import musefs_common.scan as scan

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

    import musefs_common.scan as scan

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


def test_run_scan_rejects_incompatible_flags(monkeypatch):
    import subprocess

    def fail(*a, **k):
        raise AssertionError("subprocess must not run on invalid flags")

    monkeypatch.setattr(subprocess, "run", fail)
    with pytest.raises(ValueError):
        run_scan("musefs", "/db.sqlite", "/only.flac", revalidate=True, force=True)
    with pytest.raises(ValueError):
        run_scan("musefs", "/db.sqlite", "/only.flac", prune=True)  # prune without revalidate
    with pytest.raises(ValueError):
        run_scan("musefs", "/db.sqlite", [])


def test_run_scan_single_path_still_works(monkeypatch):
    import subprocess

    import musefs_common.scan as scan

    seen = {}

    class FakeResult:
        returncode = 0
        stderr = b""

    monkeypatch.setattr(
        subprocess, "run", lambda argv, **kw: seen.update(argv=argv) or FakeResult()
    )
    scan.run_scan("musefs", "/db.sqlite", "/only.flac")
    assert seen["argv"] == ["musefs", "scan", "/only.flac", "--db", "/db.sqlite"]


def test_run_scan_hard_failure_error_names_count(monkeypatch):
    import subprocess

    import musefs_common.scan as scan
    from musefs_common import ScanError

    class FakeResult:
        returncode = 1
        stderr = b"boom"

    monkeypatch.setattr(subprocess, "run", lambda argv, **kw: FakeResult())
    try:
        scan.run_scan("musefs", "/db.sqlite", ["/a.flac", "/b.flac"])
    except ScanError as exc:
        assert exc.kind == "failed"
        assert "2" in str(exc.target)  # "2 target(s)"
    else:
        raise AssertionError("expected ScanError")


# --- the three-state exit contract (#647) ---------------------------------
#
# 0 = success, 2 = partial success (the batch committed; some files failed to
# ingest), anything else = hard failure.


def _fake_exit(monkeypatch, code, stderr=b""):
    """Make `subprocess.run` report ``code``/``stderr`` for a musefs invocation."""
    import subprocess
    from types import SimpleNamespace

    monkeypatch.setattr(
        subprocess, "run", lambda argv, **kw: SimpleNamespace(returncode=code, stderr=stderr)
    )


def test_run_scan_success_returns_non_partial_result(monkeypatch):
    _fake_exit(monkeypatch, 0)
    result = run_scan("musefs", "/db.sqlite", "/a.flac")
    assert result.partial is False
    assert result.returncode == 0
    assert result.verb == "scan"
    assert result.warning() is None


def test_run_scan_partial_returns_result_instead_of_raising(monkeypatch):
    _fake_exit(monkeypatch, 2, b"skipping /b.flac: no parseable audio metadata")
    result = run_scan("musefs", "/db.sqlite", ["/a.flac", "/b.flac"])
    assert result.partial is True
    assert result.returncode == 2
    assert result.target == "2 target(s)"
    assert "no parseable audio metadata" in result.stderr


def test_run_scan_partial_warning_names_binary_target_and_stderr(monkeypatch):
    _fake_exit(monkeypatch, 2, b"skipping /b.flac: no parseable audio metadata")
    warning = run_scan("musefs", "/db.sqlite", "/b.flac").warning()
    assert "musefs scan" in warning
    assert "/b.flac" in warning
    assert "continuing" in warning
    assert "no parseable audio metadata" in warning


def test_run_scan_partial_revalidate_names_the_verb(monkeypatch):
    _fake_exit(monkeypatch, 2)
    result = run_scan("musefs", "/db.sqlite", "/a.flac", revalidate=True)
    assert result.verb == "revalidate"
    assert "musefs revalidate" in result.warning()


def test_run_scan_other_nonzero_is_still_a_hard_failure(monkeypatch):
    from musefs_common import ScanError

    _fake_exit(monkeypatch, 1, b"opening database: permission denied")
    with pytest.raises(ScanError) as ei:
        run_scan("musefs", "/db.sqlite", "/a.flac")
    assert ei.value.kind == "failed"
    assert ei.value.returncode == 1
