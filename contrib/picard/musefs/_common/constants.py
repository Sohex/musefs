# GENERATED from python-musefs/src/musefs_common/constants.py — do not edit.
# Run contrib/python-musefs/vendor_to_picard.py after changing the library.
#
from .schema import USER_VERSION

EXPECTED_USER_VERSION = USER_VERSION

MAX_ART_BYTES = 16 * 1024 * 1024 - 64 * 1024

# Wall-clock cap (seconds) for a single `musefs scan` shell-out; a wedged scan
# (stuck disk, DB lock) raises ScanError(kind="timeout") rather than hanging.
SCAN_TIMEOUT_SECONDS = 120
