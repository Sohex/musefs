# GENERATED from python-musefs/src/musefs_common/sync.py — do not edit.
# Run contrib/python-musefs/vendor_to_picard.py after changing the library.
#
from __future__ import annotations

from dataclasses import dataclass, field

from .constants import MAX_ART_BYTES
from .store import merge_tags, replace_tags, replace_track_art, track_id_for_path, upsert_art


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

    def summary(self):
        return (
            f"synced={self.synced} skipped={self.skipped} "
            f"art_linked={self.art_linked} skipped_art={self.skipped_art}"
        )


def sync_one(conn, record, stats, *, dry_run=False, merge=False):
    """Sync one ``Record`` into the DB, mutating ``stats``. Caller owns the
    transaction. Tags are always fully replaced (scanner-written binary tags
    survive — see ``replace_tags``). Art is replaced when at least one image is
    within ``MAX_ART_BYTES``; each over-cap image bumps ``skipped_art``, and if
    every provided image is over cap any scan-seeded ``track_art`` is left
    untouched."""
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
