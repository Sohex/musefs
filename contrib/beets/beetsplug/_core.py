"""Pure logic for the musefs beets plugin: no beets imports live here.

Everything beets-specific (the BeetsPlugin subclass, commands, event
listeners) is in ``musefs.py``; this module is unit-testable on its own.
"""

import hashlib
import os
import sqlite3
from dataclasses import dataclass

# Schema version this plugin was written against (musefs schema.rs MIGRATIONS
# length). The plugin refuses to run against any other version.
EXPECTED_USER_VERSION = 1

# Mirror of musefs-core scan.rs MAX_ART_BYTES: 16 MiB minus 64 KiB headroom.
MAX_ART_BYTES = 16 * 1024 * 1024 - 64 * 1024

# beets field name -> musefs (Vorbis-lowercase) tag key, for direct copies.
DIRECT_FIELDS = {
    "title": "title",
    "artist": "artist",
    "albumartist": "albumartist",
    "album": "album",
    "genre": "genre",
    "composer": "composer",
}


def _to_int(value):
    """Coerce a beets field to int, tolerating None and non-numeric strings
    (e.g. a malformed ``"1/12"`` track-of-total) so a bad tag can't abort sync."""
    try:
        return int(value or 0)
    except (ValueError, TypeError):
        return 0


def _format_date(item):
    year = _to_int(getattr(item, "year", 0))
    if not year:
        return None
    month = _to_int(getattr(item, "month", 0))
    day = _to_int(getattr(item, "day", 0))
    if month and day:
        return f"{year:04d}-{month:02d}-{day:02d}"
    return f"{year:04d}"


def map_fields(item, extra_fields=None):
    """Map a beets item to a list of (musefs_key, value) pairs.

    Empty strings and zero numerics are omitted. ``extra_fields`` merges into
    (and can override) the direct-copy table.
    """
    fields = dict(DIRECT_FIELDS)
    if extra_fields:
        fields.update(extra_fields)

    pairs = []
    for beets_field, key in fields.items():
        value = getattr(item, beets_field, None)
        if value is None:
            continue
        text = str(value).strip()
        if text:
            pairs.append((key, text))

    track = _to_int(getattr(item, "track", 0))
    if track:
        pairs.append(("tracknumber", str(track)))
    disc = _to_int(getattr(item, "disc", 0))
    if disc:
        pairs.append(("discnumber", str(disc)))
    date = _format_date(item)
    if date:
        pairs.append(("date", date))

    return pairs


def realpath_key(path):
    """Canonical absolute path string matching musefs scan's stored
    ``backing_path`` (``std::fs::canonicalize`` + ``to_string_lossy``).

    Accepts ``str`` or ``bytes`` (beets stores ``item.path`` as bytes) and
    always returns ``str`` via the filesystem encoding.
    """
    real = os.path.realpath(path)
    if isinstance(real, bytes):
        real = os.fsdecode(real)
    # os.fsdecode uses surrogateescape; Rust's to_string_lossy uses U+FFFD for
    # undecodable bytes. Normalize so a non-UTF-8 path component produces the
    # same key string on both sides instead of silently mismatching.
    return real.encode("utf-8", "surrogateescape").decode("utf-8", "replace")


class SchemaMismatch(Exception):
    """Raised when the musefs DB schema version differs from what the plugin
    targets (``EXPECTED_USER_VERSION``)."""

    def __init__(self, found):
        self.found = found
        super().__init__(
            f"musefs DB user_version is {found}, plugin targets "
            f"{EXPECTED_USER_VERSION}; the musefs and plugin versions have "
            f"diverged."
        )


def connect(db_path):
    """Open the musefs DB with a busy timeout and foreign keys enabled."""
    conn = sqlite3.connect(db_path)
    # 5s busy timeout so a brief write doesn't fail while the FUSE mount reads.
    conn.execute("PRAGMA busy_timeout = 5000")
    conn.execute("PRAGMA foreign_keys = ON")
    return conn


def check_schema_version(conn):
    """Raise ``SchemaMismatch`` unless the DB's ``user_version`` matches the
    version this plugin targets. Call on an open connection from ``connect``."""
    found = conn.execute("PRAGMA user_version").fetchone()[0]
    if found != EXPECTED_USER_VERSION:
        raise SchemaMismatch(found)


def track_id_for_path(conn, key):
    """Return the track id whose backing_path equals ``key``, or None."""
    row = conn.execute(
        "SELECT id FROM tracks WHERE backing_path = ?", (key,)
    ).fetchone()
    return row[0] if row else None


def replace_tags(conn, track_id, pairs):
    """Replace all tags for a track. Duplicate keys get incrementing ordinals
    (mirroring musefs scan ingest)."""
    conn.execute("DELETE FROM tags WHERE track_id = ?", (track_id,))
    ordinals = {}
    rows = []
    for key, value in pairs:
        ordinal = ordinals.get(key, 0)
        ordinals[key] = ordinal + 1
        rows.append((track_id, key, value, ordinal))
    conn.executemany(
        "INSERT INTO tags (track_id, key, value, ordinal) VALUES (?, ?, ?, ?)",
        rows,
    )


_EXT_MIME = {
    ".jpg": "image/jpeg",
    ".jpeg": "image/jpeg",
    ".png": "image/png",
    ".webp": "image/webp",
}


def sniff_mime(data, path):
    """Detect image mime from magic bytes, falling back to file extension."""
    if data[:3] == b"\xff\xd8\xff":
        return "image/jpeg"
    if data[:8] == b"\x89PNG\r\n\x1a\n":
        return "image/png"
    # WebP: 'RIFF' <4-byte size> 'WEBP' (common in modern fetchart/exports).
    if data[:4] == b"RIFF" and data[8:12] == b"WEBP":
        return "image/webp"
    ext = os.path.splitext(path)[1].lower()
    return _EXT_MIME.get(ext, "application/octet-stream")


def upsert_art(conn, data, mime):
    """Content-address ``data`` by sha256 and return its art id, inserting only
    if new (mirrors musefs Db::upsert_art). If the sha256 already exists, the
    stored row (and its mime) is kept and the ``mime`` argument is ignored."""
    sha = hashlib.sha256(data).hexdigest()
    conn.execute(
        "INSERT INTO art (sha256, mime, width, height, byte_len, data) "
        "VALUES (?, ?, NULL, NULL, ?, ?) ON CONFLICT(sha256) DO NOTHING",
        (sha, mime, len(data), data),
    )
    return conn.execute(
        "SELECT id FROM art WHERE sha256 = ?", (sha,)
    ).fetchone()[0]


def replace_track_art(conn, track_id, art_id):
    """Set the track's single front-cover art (picture_type 3, ordinal 0)."""
    conn.execute("DELETE FROM track_art WHERE track_id = ?", (track_id,))
    conn.execute(
        "INSERT INTO track_art (track_id, art_id, picture_type, description, "
        "ordinal) VALUES (?, ?, 3, '', 0)",
        (track_id, art_id),
    )


# Sentinel returned by _prepare_art under dry_run: "would link, but not written".
_WOULD_LINK = object()


@dataclass
class SyncStats:
    synced: int = 0
    skipped: int = 0       # item path had no matching track row
    art_linked: int = 0
    skipped_art: int = 0   # art file oversized / unreadable

    def summary(self):
        return (
            f"synced={self.synced} skipped={self.skipped} "
            f"art_linked={self.art_linked} skipped_art={self.skipped_art}"
        )


def _album_art_path(item):
    """Return the album cover path (bytes/str) for an item, or None."""
    get_album = getattr(item, "get_album", None)
    album = get_album() if get_album else None
    if album is None:
        return None
    artpath = getattr(album, "artpath", None)
    return artpath or None


def _prepare_art(conn, artpath, cache, stats, dry_run):
    """Upsert the cover at ``artpath`` and return its art id (cached per run).
    Returns None if unreadable/oversized, or under dry_run a non-None sentinel
    when the art would be linked."""
    # Cache key is the normalized realpath, but open the raw realpath: the art
    # file only needs to be opened, not matched against the DB, so we must not
    # apply realpath_key's lossy U+FFFD normalization to the bytes we open (it
    # would turn a non-UTF-8 path into a different, nonexistent path).
    key = realpath_key(artpath)
    if key in cache:
        return cache[key]

    try:
        with open(os.path.realpath(artpath), "rb") as fh:
            data = fh.read()
    except OSError:
        stats.skipped_art += 1
        cache[key] = None
        return None

    if len(data) > MAX_ART_BYTES:
        stats.skipped_art += 1
        cache[key] = None
        return None

    if dry_run:
        cache[key] = _WOULD_LINK
        return _WOULD_LINK

    art_id = upsert_art(conn, data, sniff_mime(data, key))
    cache[key] = art_id
    return art_id


def sync_items(conn, items, *, fields=None, dry_run=False):
    """Sync beets items into the musefs DB. Caller controls the transaction
    (commit on success, rollback for dry runs)."""
    stats = SyncStats()
    art_cache = {}
    for item in items:
        key = realpath_key(item.path)
        track_id = track_id_for_path(conn, key)
        if track_id is None:
            stats.skipped += 1
            continue

        pairs = map_fields(item, fields)
        artpath = _album_art_path(item)
        art_id = _prepare_art(conn, artpath, art_cache, stats, dry_run) if artpath else None

        if not dry_run:
            replace_tags(conn, track_id, pairs)
            # In the live path art_id is an int or None (never _WOULD_LINK).
            if art_id is not None:
                replace_track_art(conn, track_id, art_id)

        if art_id is not None:
            stats.art_linked += 1
        stats.synced += 1

    return stats
