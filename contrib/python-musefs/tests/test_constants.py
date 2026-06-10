from musefs_common import constants


def test_expected_user_version_matches_rust_migrations():
    assert constants.EXPECTED_USER_VERSION == 4


def test_max_art_bytes_is_16mib_minus_64kib():
    assert constants.MAX_ART_BYTES == 16 * 1024 * 1024 - 64 * 1024


def test_scan_timeout_seconds_present():
    from musefs_common import SCAN_TIMEOUT_SECONDS
    from musefs_common.constants import SCAN_TIMEOUT_SECONDS as CONST_SCAN_TIMEOUT_SECONDS

    assert SCAN_TIMEOUT_SECONDS == CONST_SCAN_TIMEOUT_SECONDS == 120
