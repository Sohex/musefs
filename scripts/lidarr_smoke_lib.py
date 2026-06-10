"""Pure helpers for the Lidarr real-instance smoke (scripts/lidarr-smoke.sh).

Kept free of Docker/network so the regression-prone bits are unit-tested.
"""

from __future__ import annotations

import hashlib
import json


def build_album_download_env(
    *,
    album_id: int,
    track_paths: list[str],
    db_path: str,
    lidarr_url: str,
    api_key: str,
    musefs_bin: str,
) -> dict[str, str]:
    """Env Lidarr would pass for an AlbumDownload import (drives musefs-lidarr-sync).

    ``track_paths`` must equal the mock's ``trackfile.path`` values
    (realpath-compared by mapping.match_track_file).
    """
    return {
        "Lidarr_EventType": "AlbumDownload",
        "Lidarr_Album_Id": str(album_id),
        "Lidarr_AddedTrackPaths": "|".join(track_paths),
        "MUSEFS_DB": db_path,
        "MUSEFS_LIDARR_URL": lidarr_url,
        "MUSEFS_LIDARR_API_KEY": api_key,
        "MUSEFS_BIN": musefs_bin,
    }


def build_import_env(*, source: str, destination: str) -> dict[str, str]:
    """Env for the symlink path (drives musefs-lidarr-import)."""
    return {
        "Lidarr_EventType": "Download",
        "Lidarr_SourcePath": source,
        "Lidarr_DestinationPath": destination,
    }


def parse_ffprobe_tags(ffprobe_json: str) -> dict[str, str]:
    """Extract the format-level tag map from ``ffprobe -show_format -of json``."""
    data = json.loads(ffprobe_json)
    tags = data.get("format", {}).get("tags", {})
    return {str(k).lower(): str(v) for k, v in tags.items()}


def sha256_file(path: str) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(65536), b""):
            h.update(chunk)
    return h.hexdigest()


def assert_bytes_unchanged(before: dict[str, str], after: dict[str, str]) -> None:
    """Raise AssertionError if any file's sha256 changed (cardinal invariant)."""
    if before != after:
        changed = [k for k in before if before.get(k) != after.get(k)]
        raise AssertionError(f"backing audio bytes changed for: {changed}")
