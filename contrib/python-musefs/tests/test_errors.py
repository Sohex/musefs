import pytest

from musefs_common.errors import ScanError, SchemaMismatch


def test_schema_mismatch_message_and_found():
    exc = SchemaMismatch(5)
    assert exc.found == 5
    assert "user_version is 5" in str(exc)
    assert "diverged" in str(exc)


def test_scan_error_not_found():
    exc = ScanError("not_found", binary="musefs", target="/x.flac")
    assert exc.kind == "not_found"
    assert exc.binary == "musefs"
    assert "not found" in str(exc)


def test_scan_error_timeout_carries_timeout():
    exc = ScanError("timeout", binary="musefs", target="/x.flac", timeout=120)
    assert exc.kind == "timeout"
    assert exc.timeout == 120
    assert "timed out" in str(exc)


def test_scan_error_failed_carries_returncode_and_stderr():
    exc = ScanError("failed", binary="musefs", target="/x.flac", returncode=2, stderr="boom")
    assert exc.kind == "failed"
    assert exc.returncode == 2
    assert exc.stderr == "boom"
    assert "exit 2" in str(exc)


def test_scan_error_is_an_exception():
    with pytest.raises(ScanError):
        raise ScanError("not_found", binary="m", target="/x")
