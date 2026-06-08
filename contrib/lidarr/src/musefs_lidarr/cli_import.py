from __future__ import annotations

import os
import sys

from .env import lidarr_get
from .errors import MusefsLidarrError
from .import_link import ensure_link, parse_import_env


def run(environ: dict[str, str] | None = None) -> int:
    env = os.environ if environ is None else environ
    if lidarr_get(env, "Lidarr_EventType") == "Test":
        print("musefs-lidarr-import: test ok")
        return 0

    try:
        request = parse_import_env(env)
        ensure_link(request.source, request.destination, request.mode)
        print(
            f"musefs-lidarr-import: {request.mode.value} {request.source} -> {request.destination}"
        )
        return 0
    except MusefsLidarrError as exc:
        print(f"musefs-lidarr-import: {exc}", file=sys.stderr)
        return 1


def main() -> int:
    return run()
