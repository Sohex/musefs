import pytest

from musefs_common.constants import EXPECTED_USER_VERSION
from musefs_common.errors import ScanError, SchemaMismatch


def test_schema_mismatch_message_and_found():
    exc = SchemaMismatch(5)
    assert exc.found == 5
    assert "user_version is 5" in str(exc)
    # Both versions stay in the text whichever way the skew runs.
    assert str(EXPECTED_USER_VERSION) in str(exc)


def test_schema_mismatch_newer_store_says_upgrade_the_plugin():
    """A store ahead of the plugin: the plugin is the side that must move."""
    message = str(SchemaMismatch(EXPECTED_USER_VERSION + 1))
    assert f"user_version is {EXPECTED_USER_VERSION + 1}" in message
    assert "newer musefs" in message
    assert "upgrade the plugin" in message


def test_schema_mismatch_older_store_says_rescan_to_migrate():
    """A store behind the plugin: `musefs scan` migrates it in place."""
    message = str(SchemaMismatch(EXPECTED_USER_VERSION - 1))
    assert f"user_version is {EXPECTED_USER_VERSION - 1}" in message
    assert "predates this plugin" in message
    assert "`musefs scan`" in message


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
