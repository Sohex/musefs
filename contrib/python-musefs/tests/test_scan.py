import os
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
