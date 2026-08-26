import re

from musefs_common import constants, schema


def test_expected_user_version_matches_rust_migrations():
    # Checks what the name says instead of pinning a literal: the mirror stamps
    # `PRAGMA user_version = n` once per Rust migration, so the count and the
    # last stamp must both agree with the constant. A hardcoded number here just
    # breaks on every migration without ever verifying the correspondence.
    stamps = [int(n) for n in re.findall(r"PRAGMA user_version = (\d+);", schema.SCHEMA_SQL)]
    assert stamps, "the mirrored schema must stamp a user_version per migration"
    assert stamps == list(range(1, len(stamps) + 1)), stamps
    assert constants.EXPECTED_USER_VERSION == stamps[-1]


def test_max_tag_value_len_matches_the_mirrored_check():
    # The cap the store actually enforces is the one in the LAST migration to
    # rebuild `tags`; earlier migrations carry their own frozen literals.
    caps = re.findall(r"length\(CAST\(value AS BLOB\)\) <= (\d+)", schema.SCHEMA_SQL)
    assert caps, "the mirrored schema must carry a byte-counting tags.value CHECK"
    assert constants.MAX_TAG_VALUE_LEN == int(caps[-1])


def test_max_art_bytes_is_16mib_minus_64kib():
    assert constants.MAX_ART_BYTES == 16 * 1024 * 1024 - 64 * 1024


def test_scan_timeout_seconds_present():
    from musefs_common import SCAN_TIMEOUT_SECONDS
    from musefs_common.constants import SCAN_TIMEOUT_SECONDS as CONST_SCAN_TIMEOUT_SECONDS

    assert SCAN_TIMEOUT_SECONDS == CONST_SCAN_TIMEOUT_SECONDS == 120
