from __future__ import annotations

import os
from dataclasses import dataclass
from enum import Enum
from pathlib import Path

from .env import lidarr_get
from .errors import ConfigError, ImportLinkError


class LinkMode(Enum):
    SYMLINK = "symlink"
    HARDLINK = "hardlink"


@dataclass(frozen=True)
class ImportRequest:
    source: Path
    destination: Path
    mode: LinkMode


def parse_link_mode(environ: dict[str, str]) -> LinkMode:
    raw = environ.get("MUSEFS_LIDARR_LINK_MODE", "symlink").strip().lower()
    try:
        return LinkMode(raw)
    except ValueError as exc:
        raise ConfigError(
            f"MUSEFS_LIDARR_LINK_MODE must be 'symlink' or 'hardlink', got {raw!r}"
        ) from exc


def _required_path(environ: dict[str, str], name: str) -> Path:
    value = lidarr_get(environ, name)
    if not value:
        raise ConfigError(f"{name} is required")
    return Path(value)


def parse_import_env(environ: dict[str, str] | None = None) -> ImportRequest:
    env = os.environ if environ is None else environ
    return ImportRequest(
        source=_required_path(env, "Lidarr_SourcePath"),
        destination=_required_path(env, "Lidarr_DestinationPath"),
        mode=parse_link_mode(env),
    )


def _same_symlink(source: Path, destination: Path) -> bool:
    return destination.is_symlink() and destination.readlink() == source


def _same_inode(source: Path, destination: Path) -> bool:
    try:
        source_stat = source.stat()
        destination_stat = destination.stat()
    except FileNotFoundError:
        return False
    return (
        source_stat.st_ino == destination_stat.st_ino
        and source_stat.st_dev == destination_stat.st_dev
    )


def ensure_link(source: Path, destination: Path, mode: LinkMode) -> None:
    """Create a symlink/hardlink at ``destination`` pointing to ``source``.

    Idempotent when the destination already links to the same source; raises
    ``ImportLinkError`` if the source is missing or the destination exists but
    does not match. Never copies bytes.
    """
    if not source.exists():
        raise ImportLinkError(f"source does not exist: {source}")

    destination.parent.mkdir(parents=True, exist_ok=True)

    if destination.exists() or destination.is_symlink():
        if mode is LinkMode.SYMLINK and _same_symlink(source, destination):
            return
        if mode is LinkMode.HARDLINK and _same_inode(source, destination):
            return
        raise ImportLinkError(
            f"destination already exists and does not match source: {destination}"
        )

    try:
        if mode is LinkMode.SYMLINK:
            destination.symlink_to(source)
        else:
            os.link(source, destination)
    except OSError as exc:
        raise ImportLinkError(
            f"failed to create {mode.value}: {source} -> {destination}: {exc}"
        ) from exc
