from __future__ import annotations

import sqlite3
from dataclasses import dataclass, field

from .constants import MAX_ART_BYTES
from .store import (
    _savepoint,
    merge_tags,
    replace_tags,
    replace_track_art,
    track_id_for_path,
    upsert_art,
)


@dataclass(frozen=True)
class ArtImage:
    """One embedded picture to sync: raw bytes, mime, ID3/FLAC picture type
    (3 = front cover), and free-text description."""

    data: bytes
    mime: str
    picture_type: int = 3
    description: str = ""


@dataclass
class Record:
    """One file's sync inputs: the realpath key, the (key, value) tag pairs, and
    pre-resolved art as a list of ``ArtImage``s (``None``/empty list = no art
    from the host)."""

    key: str
    pairs: list = field(default_factory=list)
    art: object = None  # list[ArtImage] | None
    delete_keys: object = None  # list[str] of keys to clear without rewrite (merge mode)


@dataclass
class SyncStats:
    synced: int = 0
    skipped: int = 0  # path had no matching track row
    art_linked: int = 0
    skipped_art: int = 0  # art over the size cap (or, in the beets adapter, unreadable)
    skipped_invalid: int = 0  # record violated a store CHECK constraint
    invalid: list = field(default_factory=list)  # (record key, sqlite error message)

    def summary(self):
        return (
            f"synced={self.synced} skipped={self.skipped} "
            f"art_linked={self.art_linked} skipped_art={self.skipped_art} "
            f"skipped_invalid={self.skipped_invalid}"
        )


def sync_one(conn, record, stats, *, dry_run=False, merge=False):
    """Sync one ``Record`` into the DB, mutating ``stats``. Caller owns the
    transaction. With ``merge=False`` (the default) all plugin-owned text tags are
    replaced; with ``merge=True`` only the keys in ``record.pairs`` and
    ``record.delete_keys`` are touched (see ``merge_tags``). Either way,
    scanner-written binary tags survive. Art is replaced when at least one image is
    within ``MAX_ART_BYTES``; each over-cap image bumps ``skipped_art``, and if
    every provided image is over cap any scan-seeded ``track_art`` is left
    untouched.

    A record whose tags or art violate a store CHECK constraint (key/value/mime
    length, ``picture_type`` range, control chars, ...) is rolled back through its
    own savepoint and skipped -- it bumps ``skipped_invalid`` and appends
    ``(record.key, message)`` to ``invalid`` rather than aborting the whole batch
    with an opaque commit-time ``IntegrityError`` (#420)."""
    track_id = track_id_for_path(conn, record.key)
    if track_id is None:
        stats.skipped += 1
        return

    kept = []
    for img in record.art or []:
        if len(img.data) > MAX_ART_BYTES:
            stats.skipped_art += 1
        else:
            kept.append(img)
    will_link_art = bool(kept)

    if not dry_run:
        try:
            with _savepoint(conn, "musefs_sync_one"):
                if merge:
                    merge_tags(conn, track_id, record.pairs, record.delete_keys or [])
                else:
                    replace_tags(conn, track_id, record.pairs)
                if will_link_art:
                    arts = [
                        (upsert_art(conn, img.data, img.mime), img.picture_type, img.description)
                        for img in kept
                    ]
                    replace_track_art(conn, track_id, arts)
        except sqlite3.IntegrityError as err:
            stats.skipped_invalid += 1
            stats.invalid.append((record.key, str(err)))
            return

    if will_link_art:
        stats.art_linked += 1
    stats.synced += 1


def sync_files(conn, records, *, dry_run=False, stats=None, merge=False):
    """Sync an iterable of ``Record``s, returning the ``SyncStats``. Pass
    ``stats`` to accumulate into a caller-seeded instance (e.g. beets pre-counts
    unreadable art); otherwise a fresh one is created. Caller owns the
    transaction (commit on success, rollback for dry runs)."""
    if stats is None:
        stats = SyncStats()
    for record in records:
        sync_one(conn, record, stats, dry_run=dry_run, merge=merge)
    return stats
