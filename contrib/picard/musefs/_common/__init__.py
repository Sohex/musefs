# GENERATED from python-musefs/src/musefs_common/__init__.py — do not edit.
# Run contrib/python-musefs/vendor_to_picard.py after changing the library.
#
"""python-musefs: the shared musefs SQLite-store contract.

Single source of truth for the schema-version check, the tags/art/track_art
writes, art content-addressing, path-key normalization, the `musefs scan`
shell-out, and the per-file sync write-loop. Consumed by the beets plugin (as a
pip dependency) and by the Picard plugin (vendored into ``musefs/_common``).
"""

from .constants import EXPECTED_USER_VERSION, MAX_ART_BYTES, SCAN_TIMEOUT_SECONDS
from .errors import ScanError, SchemaMismatch
from .paths import realpath_key
from .scan import run_scan
from .store import (
    check_schema_version,
    connect,
    merge_tags,
    prune_missing,
    replace_tags,
    replace_track_art,
    sniff_mime,
    track_id_for_path,
    upsert_art,
)
from .sync import ArtImage, Record, SyncStats, sync_files, sync_one

__version__ = "1.0.0"

__all__ = [
    "EXPECTED_USER_VERSION",
    "MAX_ART_BYTES",
    "SCAN_TIMEOUT_SECONDS",
    "SchemaMismatch",
    "ScanError",
    "realpath_key",
    "run_scan",
    "connect",
    "check_schema_version",
    "track_id_for_path",
    "prune_missing",
    "merge_tags",
    "replace_tags",
    "upsert_art",
    "replace_track_art",
    "sniff_mime",
    "ArtImage",
    "Record",
    "SyncStats",
    "sync_one",
    "sync_files",
    "__version__",
]
