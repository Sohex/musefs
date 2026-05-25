"""Pure logic for the musefs beets plugin: no beets imports live here.

Everything beets-specific (the BeetsPlugin subclass, commands, event
listeners) is in ``musefs.py``; this module is unit-testable on its own.
"""

import os
import sqlite3

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
