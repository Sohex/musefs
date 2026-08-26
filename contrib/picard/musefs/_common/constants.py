# GENERATED from python-musefs/src/musefs_common/constants.py — do not edit.
# Run contrib/python-musefs/vendor_to_picard.py after changing the library.
#
from .schema import MAX_TAG_VALUE_LEN as _MAX_TAG_VALUE_LEN
from .schema import USER_VERSION

EXPECTED_USER_VERSION = USER_VERSION

# Byte cap on a `tags.value`, re-exported from the generated schema mirror so a
# writer can reject an oversize value itself instead of taking an IntegrityError
# from the store's CHECK. Bytes, not characters: the CHECK counts
# `length(CAST(value AS BLOB))`.
MAX_TAG_VALUE_LEN = _MAX_TAG_VALUE_LEN

MAX_ART_BYTES = 16 * 1024 * 1024 - 64 * 1024

# Wall-clock cap (seconds) for a single `musefs scan` shell-out; a wedged scan
# (stuck disk, DB lock) raises ScanError(kind="timeout") rather than hanging.
SCAN_TIMEOUT_SECONDS = 120
