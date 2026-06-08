from __future__ import annotations

import argparse
import os
import sys

from .api import LidarrApiError, LidarrClient, LidarrConfig, run_preflight
from .errors import ConfigError, MusefsLidarrError
from .events import EventType, LidarrEvent, parse_event
from .sync import (
    collect_all_payloads,
    collect_event_payloads,
    config_from_env,
    sync_event_with_payloads,
)


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="musefs-lidarr-sync")
    parser.add_argument("--doctor", action="store_true", help="check Lidarr settings and exit")
    parser.add_argument("--all", action="store_true", help="sync every known Lidarr track file")
    parser.add_argument("--skip-lidarr-preflight", action="store_true")
    return parser


def _doctor(client) -> int:
    result = run_preflight(client)
    if result.ok:
        print("musefs-lidarr-sync: doctor ok")
        return 0
    for error in result.errors:
        print(f"musefs-lidarr-sync: unsafe Lidarr setting: {error}", file=sys.stderr)
    return 1


def run(
    argv: list[str] | None = None,
    environ: dict[str, str] | None = None,
    *,
    client_factory=LidarrClient,
    sync_runner=sync_event_with_payloads,
) -> int:
    """Run the sync CLI; return a process exit code.

    Dispatches on flags (``--doctor``, ``--all``) and otherwise on the Lidarr
    event type, syncing API metadata into the store. Test/unsupported/TrackRetag
    events are no-op successes.
    """
    env = os.environ if environ is None else environ
    args = _parser().parse_args([] if argv is None else argv)
    try:
        if args.doctor:
            config = LidarrConfig.from_env(env)
            if not config.enabled:
                raise ConfigError(
                    "MUSEFS_LIDARR_URL and MUSEFS_LIDARR_API_KEY are required for doctor"
                )
            return _doctor(client_factory(config))

        if args.all:
            config = LidarrConfig.from_env(env)
            if not config.enabled:
                raise ConfigError(
                    "MUSEFS_LIDARR_URL and MUSEFS_LIDARR_API_KEY are required for --all"
                )
            client = client_factory(config)
            sync_config = config_from_env(env)
            if not args.skip_lidarr_preflight:
                doctor_rc = _doctor(client)
                if doctor_rc != 0:
                    return doctor_rc
            payloads = collect_all_payloads(client=client)
            event = LidarrEvent(
                event_type=EventType.ALBUM_DOWNLOAD,
                raw_type="ManualAll",
                paths=payloads.paths,
            )
            stats = sync_runner(
                config=sync_config,
                event=event,
                track_files=payloads.track_files,
                tracks=payloads.tracks,
                albums_by_id=payloads.albums_by_id,
                artists_by_id=payloads.artists_by_id,
            )
            print(f"musefs-lidarr-sync: {stats.summary()}")
            return 0

        event = parse_event(env)
        if event.event_type is EventType.TEST:
            print("musefs-lidarr-sync: test ok")
            return 0
        if event.event_type is EventType.UNSUPPORTED:
            print(f"musefs-lidarr-sync: unsupported event {event.raw_type!r}; skipping")
            return 0
        if event.event_type is EventType.TRACK_RETAG:
            print(
                "musefs-lidarr-sync: TrackRetag fires after Lidarr writes tags; skipping",
                file=sys.stderr,
            )
            return 0

        config = LidarrConfig.from_env(env)
        if not config.enabled:
            raise ConfigError(
                "MUSEFS_LIDARR_URL and MUSEFS_LIDARR_API_KEY are required for v1 event sync"
            )
        client = client_factory(config)
        sync_config = config_from_env(env)
        if not args.skip_lidarr_preflight:
            doctor_rc = _doctor(client)
            if doctor_rc != 0:
                return doctor_rc

        payloads = collect_event_payloads(client=client, event=event)
        stats = sync_runner(
            config=sync_config,
            event=event,
            track_files=payloads.track_files,
            tracks=payloads.tracks,
            albums_by_id=payloads.albums_by_id,
            artists_by_id=payloads.artists_by_id,
        )
        print(f"musefs-lidarr-sync: {stats.summary()}")
        return 0
    except (MusefsLidarrError, LidarrApiError) as exc:
        print(f"musefs-lidarr-sync: {exc}", file=sys.stderr)
        return 1


def main() -> int:
    return run(sys.argv[1:])
