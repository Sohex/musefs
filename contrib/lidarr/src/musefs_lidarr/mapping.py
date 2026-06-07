from __future__ import annotations

from collections import defaultdict
from dataclasses import dataclass

from musefs_common import Record, realpath_key

from .errors import MappingError


@dataclass(frozen=True)
class SkippedPath:
    path: str
    reason: str


def _text(value) -> str | None:
    if value is None:
        return None
    out = str(value).strip()
    return out or None


def _date(value) -> str | None:
    text = _text(value)
    if not text:
        return None
    return text[:10] if len(text) >= 10 else text


def _append(pairs: list[tuple[str, str]], key: str, value) -> None:
    text = _text(value)
    if text:
        pairs.append((key, text))


def build_pairs(*, track: dict, album: dict, artist: dict) -> list[tuple[str, str]]:
    pairs: list[tuple[str, str]] = []
    artist_name = artist.get("artistName") or artist.get("name")
    _append(pairs, "title", track.get("title"))
    _append(pairs, "artist", artist_name)
    _append(pairs, "albumartist", artist_name)
    _append(pairs, "album", album.get("title"))
    _append(pairs, "tracknumber", track.get("trackNumber") or track.get("absoluteTrackNumber"))
    _append(pairs, "discnumber", track.get("mediumNumber"))
    _append(pairs, "date", _date(album.get("releaseDate")))
    _append(pairs, "musicbrainz_artistid", artist.get("foreignArtistId") or artist.get("mbId"))
    _append(pairs, "musicbrainz_albumid", album.get("foreignAlbumId"))
    _append(pairs, "musicbrainz_trackid", track.get("foreignTrackId"))
    _append(pairs, "musicbrainz_releasetrackid", track.get("foreignRecordingId"))

    seen_genres = set()
    for genre in list(album.get("genres") or []) + list(artist.get("genres") or []):
        text = _text(genre)
        if text and text not in seen_genres:
            seen_genres.add(text)
            pairs.append(("genre", text))
    return pairs


def match_track_file(path_key: str, track_files: list[dict]) -> dict | None:
    matches = [tf for tf in track_files if realpath_key(tf["path"]) == path_key]
    if len(matches) > 1:
        ids = ", ".join(str(tf.get("id")) for tf in matches)
        raise MappingError(f"multiple Lidarr track files match {path_key}: {ids}")
    return matches[0] if matches else None


def _tracks_by_file(tracks: list[dict]) -> dict[int, list[dict]]:
    grouped: dict[int, list[dict]] = defaultdict(list)
    for track in tracks:
        grouped[int(track["trackFileId"])].append(track)
    return grouped


def records_for_paths(
    *,
    paths: list[str],
    track_files: list[dict],
    tracks: list[dict],
    albums_by_id: dict[int, dict],
    artists_by_id: dict[int, dict],
) -> tuple[list[Record], list[SkippedPath]]:
    tracks_by_file = _tracks_by_file(tracks)
    records = []
    skipped = []
    for path in paths:
        key = realpath_key(path)
        track_file = match_track_file(key, track_files)
        if track_file is None:
            skipped.append(SkippedPath(path=path, reason="no matching Lidarr track file"))
            continue
        linked = tracks_by_file.get(int(track_file["id"]), [])
        if not linked:
            skipped.append(SkippedPath(path=path, reason="multi-track metadata unavailable"))
            continue
        album = albums_by_id.get(int(track_file["albumId"]))
        artist = artists_by_id.get(int(track_file["artistId"]))
        if album is None or artist is None:
            skipped.append(SkippedPath(path=path, reason="album or artist metadata unavailable"))
            continue
        pairs = []
        for track in linked:
            pairs.extend(build_pairs(track=track, album=album, artist=artist))
        records.append(Record(key=key, pairs=pairs, art=None))
    return records, skipped
