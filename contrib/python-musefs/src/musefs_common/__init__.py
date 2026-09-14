"""python-musefs: the shared musefs SQLite-store contract.

Single source of truth for the schema-version check, the tags/art/track_art
writes, art content-addressing, path-key normalization, the `musefs scan`
shell-out, and the per-file sync write-loop. Consumed by the beets plugin (as a
pip dependency) and by the Picard plugin (vendored into ``musefs/_common``).
"""

from .constants import (
    EXPECTED_USER_VERSION,
    MAX_ART_BYTES,
    MAX_TAG_VALUE_LEN,
    SCAN_TIMEOUT_SECONDS,
)
from .errors import ArtDigestMismatch, ScanError, SchemaMismatch
from .paths import realpath_key
from .scan import ScanResult, run_scan
from .store import (
    TagRow,
    check_schema_version,
    connect,
    delete_tracks,
    image_dimensions,
    merge_tags,
    path_param,
    path_value,
    prune_missing,
    replace_tags,
    replace_track_art,
    sniff_mime,
    tags_for_track,
    track_id_for_path,
    track_ids_by_tag,
    track_ids_for_paths,
    upsert_art,
)
from .sync import ArtImage, Record, SyncStats, sync_files, sync_one

__version__ = "2.0.0"

__all__ = [
    "EXPECTED_USER_VERSION",
    "MAX_ART_BYTES",
    "MAX_TAG_VALUE_LEN",
    "SCAN_TIMEOUT_SECONDS",
    "SchemaMismatch",
    "ScanError",
    "ArtDigestMismatch",
    "ScanResult",
    "realpath_key",
    "run_scan",
    "connect",
    "check_schema_version",
    "path_param",
    "path_value",
    "track_id_for_path",
    "track_ids_for_paths",
    "track_ids_by_tag",
    "tags_for_track",
    "TagRow",
    "delete_tracks",
    "prune_missing",
    "merge_tags",
    "replace_tags",
    "upsert_art",
    "replace_track_art",
    "sniff_mime",
    "image_dimensions",
    "ArtImage",
    "Record",
    "SyncStats",
    "sync_one",
    "sync_files",
    "__version__",
]
