from __future__ import annotations

import os
from dataclasses import dataclass, field
from enum import Enum


class EventType(Enum):
    TEST = "Test"
    ALBUM_DOWNLOAD = "AlbumDownload"
    RENAME = "Rename"
    TRACK_RETAG = "TrackRetag"
    UNSUPPORTED = "Unsupported"


@dataclass(frozen=True)
class LidarrEvent:
    event_type: EventType
    raw_type: str
    paths: list[str] = field(default_factory=list)
    previous_paths: list[str] = field(default_factory=list)
    artist_id: int | None = None
    album_id: int | None = None


def split_paths(value: str | None) -> list[str]:
    if not value:
        return []
    return [part for part in value.split("|") if part]


def _int_or_none(value: str | None) -> int | None:
    if not value:
        return None
    try:
        return int(value)
    except ValueError:
        return None


def parse_event(environ: dict[str, str] | None = None) -> LidarrEvent:
    env = os.environ if environ is None else environ
    raw = env.get("Lidarr_EventType", "")

    if raw == EventType.TEST.value:
        event_type = EventType.TEST
    elif raw == EventType.ALBUM_DOWNLOAD.value:
        event_type = EventType.ALBUM_DOWNLOAD
    elif raw == EventType.RENAME.value:
        event_type = EventType.RENAME
    elif raw == EventType.TRACK_RETAG.value:
        event_type = EventType.TRACK_RETAG
    else:
        event_type = EventType.UNSUPPORTED

    paths = []
    previous_paths = []
    if event_type is EventType.ALBUM_DOWNLOAD:
        paths = split_paths(env.get("Lidarr_AddedTrackPaths"))
    elif event_type is EventType.RENAME:
        paths = split_paths(env.get("Lidarr_TrackFile_Paths"))
        previous_paths = split_paths(env.get("Lidarr_TrackFile_PreviousPaths"))

    return LidarrEvent(
        event_type=event_type,
        raw_type=raw,
        paths=paths,
        previous_paths=previous_paths,
        artist_id=_int_or_none(env.get("Lidarr_Artist_Id")),
        album_id=_int_or_none(env.get("Lidarr_Album_Id")),
    )
