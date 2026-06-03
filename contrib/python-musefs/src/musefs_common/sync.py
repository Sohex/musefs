from __future__ import annotations

from dataclasses import dataclass, field

from .constants import MAX_ART_BYTES
from .store import replace_tags, replace_track_art, track_id_for_path, upsert_art


@dataclass
class Record:
    """One file's sync inputs: the realpath key, the (key, value) tag pairs, and
    pre-resolved cover art as an ``(bytes, mime)`` tuple or ``None``."""

    key: str
    pairs: list = field(default_factory=list)
    art: object = None  # tuple[bytes, str] | None


@dataclass
class SyncStats:
    synced: int = 0
    skipped: int = 0  # path had no matching track row
    art_linked: int = 0
    skipped_art: int = 0  # art over the size cap (or, in the beets adapter, unreadable)

    def summary(self):
        return (
            f"synced={self.synced} skipped={self.skipped} "
            f"art_linked={self.art_linked} skipped_art={self.skipped_art}"
        )


def sync_one(conn, record, stats, *, dry_run=False):
    """Sync one ``Record`` into the DB, mutating ``stats``. Caller owns the
    transaction. Tags are always fully replaced (scanner-written binary tags
    survive — see ``replace_tags``). Art is replaced only when present and within
    ``MAX_ART_BYTES``; an over-cap image bumps ``skipped_art`` and leaves any
    scan-seeded ``track_art`` untouched."""
    track_id = track_id_for_path(conn, record.key)
    if track_id is None:
        stats.skipped += 1
        return

    will_link_art = False
    if record.art is not None:
        data, _mime = record.art
        if len(data) > MAX_ART_BYTES:
            stats.skipped_art += 1
        else:
            will_link_art = True

    if not dry_run:
        replace_tags(conn, track_id, record.pairs)
        if will_link_art:
            data, mime = record.art
            art_id = upsert_art(conn, data, mime)
            replace_track_art(conn, track_id, art_id)

    if will_link_art:
        stats.art_linked += 1
    stats.synced += 1


def sync_files(conn, records, *, dry_run=False, stats=None):
    """Sync an iterable of ``Record``s, returning the ``SyncStats``. Pass
    ``stats`` to accumulate into a caller-seeded instance (e.g. beets pre-counts
    unreadable art); otherwise a fresh one is created. Caller owns the
    transaction (commit on success, rollback for dry runs)."""
    if stats is None:
        stats = SyncStats()
    for record in records:
        sync_one(conn, record, stats, dry_run=dry_run)
    return stats
