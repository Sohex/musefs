from __future__ import annotations

import os
import sys
from dataclasses import dataclass

from musefs_common import (
    SCAN_TIMEOUT_SECONDS,
    ArtImage,
    SyncStats,
    check_schema_version,
    connect,
    delete_tracks,
    prune_missing,
    realpath_key,
    run_scan,
    sniff_mime,
    sync_files,
    track_id_for_path,
    track_ids_by_tag,
    track_ids_for_paths,
)

from .errors import ConfigError, LidarrApiError
from .events import EventType, LidarrEvent
from .import_link import LinkMode, parse_link_mode
from .mapping import _album_cover_url, records_for_paths


@dataclass(frozen=True)
class SyncConfig:
    """musefs-side sync settings: DB path, link mode, autoscan, scanner binary."""

    db_path: str
    link_mode: LinkMode
    autoscan: bool = True
    musefs_bin: str = "musefs"


@dataclass(frozen=True)
class EventPayloads:
    """Lidarr API data for an event: paths plus track/album/artist lookups."""

    paths: list[str]
    track_files: list[dict]
    tracks: list[dict]
    albums_by_id: dict[int, dict]
    artists_by_id: dict[int, dict]
    art_by_album_id: dict[int, ArtImage]


def _env_bool(value: str | None, *, default: bool) -> bool:
    if value is None:
        return default
    return value.strip().lower() not in {"0", "false", "no", "off"}


def config_from_env(environ: dict[str, str] | None = None) -> SyncConfig:
    """Build a :class:`SyncConfig` from ``MUSEFS_*`` env vars.

    Raises ``ConfigError`` if ``MUSEFS_DB`` is unset.
    """
    env = os.environ if environ is None else environ
    db_path = env.get("MUSEFS_DB")
    if not db_path:
        raise ConfigError("MUSEFS_DB is required")
    return SyncConfig(
        db_path=db_path,
        link_mode=parse_link_mode(env),
        autoscan=_env_bool(env.get("MUSEFS_LIDARR_AUTOSCAN"), default=True),
        musefs_bin=env.get("MUSEFS_BIN") or "musefs",
    )


def scan_if_enabled(*, config: SyncConfig, paths: list[str], runner=run_scan) -> None:
    """Run ``musefs scan`` over ``paths`` when autoscan is on and paths exist."""
    if not config.autoscan or not paths:
        return
    runner(config.musefs_bin, config.db_path, paths, timeout=SCAN_TIMEOUT_SECONDS)


def _log_skipped(skipped, *, warning_printer) -> None:
    for item in skipped:
        warning_printer(
            f"musefs-lidarr-sync: skipped {item.path}: {item.reason}",
            file=sys.stderr,
        )


def _log_invalid(invalid, *, warning_printer) -> None:
    for key, message in invalid:
        warning_printer(
            f"musefs-lidarr-sync: skipped {key}: invalid record: {message}",
            file=sys.stderr,
        )


def _collect_album_art(client, albums_by_id, *, warning_printer=print) -> dict[int, ArtImage]:
    """Fetch each album's cover art via ``client``; return ``{album_id: ArtImage}``.

    Albums with no cover image are skipped silently. A failed fetch is logged and
    skipped rather than aborting the sync (Lidarr custom scripts are
    fire-and-forget, so a partial sync beats none).
    """
    art_by_album_id: dict[int, ArtImage] = {}
    for album_id, album in albums_by_id.items():
        url = _album_cover_url(album)
        if not url:
            continue
        try:
            data = client.media_cover(url)
        except LidarrApiError as exc:
            warning_printer(
                f"musefs-lidarr-sync: art fetch failed for album {album_id}: {exc}",
                file=sys.stderr,
            )
            continue
        art_by_album_id[album_id] = ArtImage(data=data, mime=sniff_mime(data, url))
    return art_by_album_id


def sync_records(
    *,
    config: SyncConfig,
    event: LidarrEvent,
    track_files: list[dict],
    tracks: list[dict],
    albums_by_id: dict[int, dict],
    artists_by_id: dict[int, dict],
    art_by_album_id: dict[int, ArtImage] | None = None,
    warning_printer=print,
) -> SyncStats:
    """Map the event's paths to records and write their tags into the store.

    Paths with no matching track row (e.g. unscanned) are skipped and counted;
    the write runs in a single transaction that rolls back on error.
    """
    records, skipped_paths = records_for_paths(
        paths=event.paths,
        track_files=track_files,
        tracks=tracks,
        albums_by_id=albums_by_id,
        artists_by_id=artists_by_id,
        art_by_album_id=art_by_album_id,
    )
    _log_skipped(skipped_paths, warning_printer=warning_printer)

    stats = SyncStats(skipped=len(skipped_paths))
    conn = connect(config.db_path)
    try:
        check_schema_version(conn)
        present_records = []
        for record in records:
            if track_id_for_path(conn, record.key) is None:
                stats.skipped += 1
                warning_printer(
                    (
                        "musefs-lidarr-sync: skipped "
                        f"{record.key}: no matching musefs track row after scan"
                    ),
                    file=sys.stderr,
                )
                continue
            present_records.append(record)

        sync_files(conn, present_records, stats=stats)
        _log_invalid(stats.invalid, warning_printer=warning_printer)
        conn.commit()
        return stats
    except Exception:
        conn.rollback()
        raise
    finally:
        conn.close()


def sync_rename_prune(*, config: SyncConfig, previous_paths: list[str]) -> int:
    """Prune store rows for a rename's old paths; return the count pruned.

    No-op in symlink mode (the backing path is the unchanged real file).
    """
    if config.link_mode is LinkMode.SYMLINK or not previous_paths:
        return 0

    previous_keys = [realpath_key(path) for path in previous_paths]
    conn = connect(config.db_path)
    try:
        ids = track_ids_for_paths(conn, previous_keys)
        pruned = prune_missing(conn, list(ids.values()))
        conn.commit()
        return pruned
    except Exception:
        conn.rollback()
        raise
    finally:
        conn.close()


def prune_deleted(*, config: SyncConfig, event: LidarrEvent) -> int:
    """Delete store rows for a Lidarr album/artist deletion, mapped by MusicBrainz id.

    Lidarr never touches the backing files (it only unlinks its own symlink
    tree), so this is intent-based, not existence-based: rows are removed by
    matching the stored ``musicbrainz_albumid`` / ``musicbrainz_artistid`` tag
    against the id Lidarr reports in the delete event. Returns the count deleted.

    The caller guarantees the relevant MBID is present (see ``cli_sync``); an
    album event matches ``musicbrainz_albumid``, an artist event
    ``musicbrainz_artistid``.
    """
    if event.event_type is EventType.ALBUM_DELETED:
        key, value = "musicbrainz_albumid", event.album_mbid
    else:
        key, value = "musicbrainz_artistid", event.artist_mbid

    conn = connect(config.db_path)
    try:
        deleted = delete_tracks(conn, track_ids_by_tag(conn, key, value))
        conn.commit()
        return deleted
    except Exception:
        conn.rollback()
        raise
    finally:
        conn.close()


def _int_or_none(value) -> int | None:
    try:
        return int(value)
    except (TypeError, ValueError):
        return None


def _dedupe_ints(values) -> list[int]:
    seen = set()
    out = []
    for value in values:
        ident = _int_or_none(value)
        if ident is None or ident in seen:
            continue
        seen.add(ident)
        out.append(ident)
    return out


def _album_ids(track_files: list[dict]) -> list[int]:
    return _dedupe_ints(track_file.get("albumId") for track_file in track_files)


def _artist_ids(track_files: list[dict], *fallback_ids) -> list[int]:
    values = [track_file.get("artistId") for track_file in track_files]
    values.extend(fallback_ids)
    return _dedupe_ints(values)


def _album_artist_id(album: dict) -> int | None:
    artist = album.get("artist") or {}
    return _int_or_none(
        album.get("artistId")
        or album.get("artist_id")
        or artist.get("artistId")
        or artist.get("id")
    )


def collect_event_payloads(*, client, event: LidarrEvent) -> EventPayloads:
    """Fetch the track/album/artist data an event needs from the Lidarr API.

    Scopes the queries by album id when present, else by artist id; raises
    ``ConfigError`` if the event carries neither.
    """
    if event.album_id is not None:
        track_files = client.track_files(album_id=event.album_id)
        tracks = client.tracks(album_id=event.album_id)
        album = client.album(event.album_id)
        album_id = _int_or_none(album.get("id"))
        if album_id is None:
            raise ConfigError("Lidarr album payload is missing an id")
        album_artist_id = _album_artist_id(album)
        artists_by_id = {
            artist_id: client.artist(artist_id)
            for artist_id in _artist_ids(track_files, event.artist_id, album_artist_id)
        }
        albums_by_id = {album_id: album}
        return EventPayloads(
            paths=[track_file["path"] for track_file in track_files],
            track_files=track_files,
            tracks=tracks,
            albums_by_id=albums_by_id,
            artists_by_id=artists_by_id,
            art_by_album_id=_collect_album_art(client, albums_by_id),
        )
    if event.artist_id is not None:
        track_files = client.track_files(artist_id=event.artist_id)
        tracks = client.tracks(artist_id=event.artist_id)
        albums_by_id = {album_id: client.album(album_id) for album_id in _album_ids(track_files)}
        artists_by_id = {
            artist_id: client.artist(artist_id)
            for artist_id in _artist_ids(track_files, event.artist_id)
        }
        return EventPayloads(
            paths=[track_file["path"] for track_file in track_files],
            track_files=track_files,
            tracks=tracks,
            albums_by_id=albums_by_id,
            artists_by_id=artists_by_id,
            art_by_album_id=_collect_album_art(client, albums_by_id),
        )
    raise ConfigError("Lidarr event must include Lidarr_Artist_Id or Lidarr_Album_Id")


def collect_all_payloads(*, client) -> EventPayloads:
    """Fetch every artist's track/album data for a full ``--all`` backfill."""
    artists = client.artists()
    track_files = []
    tracks = []
    albums_by_id: dict[int, dict] = {}
    artists_by_id: dict[int, dict] = {}

    for artist in artists:
        artist_id = _int_or_none(artist.get("id"))
        if artist_id is None:
            continue
        artists_by_id[artist_id] = artist
        artist_track_files = client.track_files(artist_id=artist_id)
        artist_tracks = client.tracks(artist_id=artist_id)
        track_files.extend(artist_track_files)
        tracks.extend(artist_tracks)
        for album_id in _album_ids(artist_track_files):
            albums_by_id.setdefault(album_id, client.album(album_id))

    return EventPayloads(
        paths=[track_file["path"] for track_file in track_files],
        track_files=track_files,
        tracks=tracks,
        albums_by_id=albums_by_id,
        artists_by_id=artists_by_id,
        art_by_album_id=_collect_album_art(client, albums_by_id),
    )


def sync_event_with_payloads(
    *,
    config: SyncConfig,
    event: LidarrEvent,
    track_files: list[dict],
    tracks: list[dict],
    albums_by_id: dict[int, dict],
    artists_by_id: dict[int, dict],
    art_by_album_id: dict[int, ArtImage] | None = None,
    scanner=run_scan,
) -> SyncStats:
    """Scan, write tags, then prune renames for one event; return its stats."""
    scan_if_enabled(config=config, paths=event.paths, runner=scanner)
    stats = sync_records(
        config=config,
        event=event,
        track_files=track_files,
        tracks=tracks,
        albums_by_id=albums_by_id,
        artists_by_id=artists_by_id,
        art_by_album_id=art_by_album_id,
    )
    sync_rename_prune(config=config, previous_paths=event.previous_paths)
    return stats
