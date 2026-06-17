from __future__ import annotations

from collections import defaultdict
from dataclasses import dataclass

from musefs_common import ArtImage, Record, realpath_key

from .errors import MappingError


@dataclass(frozen=True)
class SkippedPath:
    """A path that could not be mapped, with the reason it was skipped."""

    path: str
    reason: str


# Ownership marker stamped on every track this plugin writes. ``prune_deleted``
# matches a Lidarr album/artist deletion to store rows by MusicBrainz id, but a
# scanner-seeded ``musicbrainz_albumid`` (read from the file's own native tags)
# is an indistinguishable text tag. The marker scopes deletion to rows Lidarr
# actually managed (#546). It is a normal text tag, so it DOES appear in served
# files (e.g. a ``MUSEFS_LIDARR_MANAGED`` Vorbis comment) — see the Lidarr docs.
MANAGED_KEY = "musefs_lidarr_managed"
MANAGED_VALUE = "1"


def _text(value) -> str | None:
    """Stringify and strip ``value``; return None if empty."""
    if value is None:
        return None
    out = str(value).strip()
    return out or None


def _date(value) -> str | None:
    """Return an ``YYYY-MM-DD`` date (the leading 10 chars), or None."""
    text = _text(value)
    if not text:
        return None
    return text[:10] if len(text) >= 10 else text


def _append(pairs: list[tuple[str, str]], key: str, value) -> None:
    text = _text(value)
    if text:
        pairs.append((key, text))


def _track_pairs(track: dict) -> list[tuple[str, str]]:
    """Track-level ``(key, value)`` tags — the fields that legitimately repeat
    once per linked track of a single-file (cue-style) release."""
    pairs: list[tuple[str, str]] = []
    _append(pairs, "title", track.get("title"))
    _append(pairs, "tracknumber", track.get("trackNumber") or track.get("absoluteTrackNumber"))
    _append(pairs, "discnumber", track.get("mediumNumber"))
    _append(pairs, "musicbrainz_trackid", track.get("foreignTrackId"))
    _append(pairs, "musicbrainz_releasetrackid", track.get("foreignRecordingId"))
    return pairs


def _album_artist_pairs(album: dict, artist: dict) -> list[tuple[str, str]]:
    """Album/artist-level ``(key, value)`` tags — emitted once per backing file,
    not once per linked track (#539)."""
    pairs: list[tuple[str, str]] = []
    artist_name = artist.get("artistName") or artist.get("name")
    _append(pairs, "artist", artist_name)
    _append(pairs, "albumartist", artist_name)
    _append(pairs, "album", album.get("title"))
    _append(pairs, "date", _date(album.get("releaseDate")))
    _append(pairs, "musicbrainz_artistid", artist.get("foreignArtistId") or artist.get("mbId"))
    _append(pairs, "musicbrainz_albumid", album.get("foreignAlbumId"))

    seen_genres = set()
    for genre in list(album.get("genres") or []) + list(artist.get("genres") or []):
        text = _text(genre)
        if text and text not in seen_genres:
            seen_genres.add(text)
            pairs.append(("genre", text))
    return pairs


def build_pairs(*, track: dict, album: dict, artist: dict) -> list[tuple[str, str]]:
    """Map one Lidarr track plus its album/artist to musefs ``(key, value)`` tag
    pairs. ``records_for_paths`` does not call this for multi-track files — it
    combines :func:`_album_artist_pairs` (once) with :func:`_track_pairs` (per
    linked track) so album/artist fields are not duplicated (#539)."""
    return _track_pairs(track) + _album_artist_pairs(album, artist)


def match_track_file(path_key: str, track_files: list[dict]) -> dict | None:
    """Return the track file whose real path equals ``path_key``, or None.

    Raises ``MappingError`` if more than one track file matches.
    """
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


def _album_cover_url(album: dict) -> str | None:
    """Return the album's cover-art image URL, or None.

    Prefers the image whose ``coverType`` is ``cover``; otherwise falls back to
    the first image that carries a usable ``url``/``remoteUrl``.
    """
    fallback = None
    for image in album.get("images") or []:
        if not isinstance(image, dict):
            continue
        url = image.get("url") or image.get("remoteUrl")
        if not url:
            continue
        if str(image.get("coverType") or "").lower() == "cover":
            return url
        if fallback is None:
            fallback = url
    return fallback


def records_for_paths(
    *,
    paths: list[str],
    track_files: list[dict],
    tracks: list[dict],
    albums_by_id: dict[int, dict],
    artists_by_id: dict[int, dict],
    art_by_album_id: dict[int, ArtImage] | None = None,
) -> tuple[list[Record], list[SkippedPath]]:
    """Build a store ``Record`` per path; return (records, skipped).

    A path is skipped when no track file matches it or its track/album/artist
    metadata is unavailable. When ``art_by_album_id`` maps the file's album id to
    an ``ArtImage``, that cover is attached to the record.
    """
    art_by_album_id = art_by_album_id or {}
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
        # Album/artist-level tags are emitted once per file; only track-level
        # tags repeat per linked track (a single-file/cue-style release) (#539).
        pairs = _album_artist_pairs(album, artist)
        pairs.append((MANAGED_KEY, MANAGED_VALUE))  # ownership marker (#546)
        for track in linked:
            pairs.extend(_track_pairs(track))
        art = art_by_album_id.get(int(track_file["albumId"]))
        records.append(Record(key=key, pairs=pairs, art=[art] if art else None))
    return records, skipped
