from musefs_common import constants


def test_expected_user_version_matches_rust_migrations():
    assert constants.EXPECTED_USER_VERSION == 2


def test_max_art_bytes_is_16mib_minus_64kib():
    assert constants.MAX_ART_BYTES == 16 * 1024 * 1024 - 64 * 1024
