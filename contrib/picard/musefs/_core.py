"""Pure logic for the musefs Picard plugin: no Picard imports live here.

Everything Picard-specific (the BaseAction, the options page, registration)
is in ``__init__.py``; this module is unit-testable on its own.
"""

from __future__ import annotations

import hashlib
import os
import sqlite3
import subprocess
from dataclasses import dataclass, field

# Schema version this plugin was written against (musefs schema.rs MIGRATIONS
# length). The plugin refuses to run against any other version.
EXPECTED_USER_VERSION = 2

# Mirror of musefs-core scan.rs MAX_ART_BYTES: 16 MiB minus 64 KiB headroom.
MAX_ART_BYTES = 16 * 1024 * 1024 - 64 * 1024

# Upper bound on a single-file `musefs scan` autoscan. A scan probes one file,
# so this only fires on a genuine hang (e.g. a wedged binary or stuck DB lock);
# without it a hung scan would block the Picard worker thread forever.
SCAN_TIMEOUT_SECONDS = 120

# Picard internal tag name -> musefs (Vorbis-lowercase) key. Picard's internal
# names already match musefs keys, so this is mostly identity.
DIRECT_FIELDS = {
    "title": "title",
    "artist": "artist",
    "albumartist": "albumartist",
    "album": "album",
    "genre": "genre",
    "composer": "composer",
    "tracknumber": "tracknumber",
    "discnumber": "discnumber",
    "date": "date",
}

# Keys whose value is dropped when it normalizes to zero (a 0 track/disc is noise).
_NUMERIC_KEYS = {"tracknumber", "discnumber"}


class SchemaMismatch(Exception):  # noqa: N818
    """Raised when the musefs DB schema version differs from what the plugin
    targets (``EXPECTED_USER_VERSION``)."""

    def __init__(self, found):
        self.found = found
        super().__init__(
            f"musefs DB user_version is {found}, plugin targets "
            f"{EXPECTED_USER_VERSION}; the musefs and plugin versions have "
            f"diverged."
        )


class MusefsError(Exception):  # noqa: N818
    """A user-facing failure (binary missing, scan failed, DB absent)."""


def _to_int(value):
    """Coerce to int, tolerating None and non-numeric strings so a bad tag
    can't abort sync."""
    try:
        return int(value or 0)
    except (ValueError, TypeError):
        return 0


def _first_value(metadata, field_name):
    """First non-empty, stripped string value of a Picard metadata field.
    Reads ``metadata.getall(field)`` when available (Picard's multi-valued
    accessor), else falls back to a plain ``.get``."""
    getall = getattr(metadata, "getall", None)
    if getall is not None:
        values = getall(field_name)
    else:
        v = metadata.get(field_name) if hasattr(metadata, "get") else None
        values = v if isinstance(v, (list, tuple)) else ([] if v is None else [v])
    for v in values:
        text = str(v).strip()
        if text:
            return text
    return ""


def map_fields(metadata, extra_fields=None):
    """Map a Picard Metadata (dict-like) to a list of (musefs_key, value) pairs.

    One value per key (the first non-empty), empty strings omitted, and a zero
    tracknumber/discnumber omitted. ``extra_fields`` merges into (and can
    override) the direct-copy table.
    """
    fields = dict(DIRECT_FIELDS)
    if extra_fields:
        fields.update(extra_fields)

    pairs = []
    for pic_field, key in fields.items():
        text = _first_value(metadata, pic_field)
        if not text:
            continue
        if key in _NUMERIC_KEYS and _to_int(text) == 0:
            continue
        pairs.append((key, text))
    return pairs


def front_cover(metadata):
    """Return ``(data, mime)`` for the first front-cover image in a Picard
    Metadata, or ``None``. Duck-typed: images expose ``is_front_image()``,
    ``data``, and ``mimetype``."""
    images = getattr(metadata, "images", None) or []
    for img in images:
        is_front = getattr(img, "is_front_image", None)
        if is_front is not None and is_front():
            return (img.data, img.mimetype)
    return None


def realpath_key(path):
    """Canonical absolute path string matching musefs scan's stored
    ``backing_path`` (``std::fs::canonicalize`` + ``to_string_lossy``).

    Accepts ``str`` or ``bytes`` and always returns ``str``.
    """
    real = os.path.realpath(path)
    if isinstance(real, bytes):
        real = os.fsdecode(real)
    # os.fsdecode uses surrogateescape; Rust's to_string_lossy uses U+FFFD for
    # undecodable bytes. Normalize so a non-UTF-8 path component produces the
    # same key string on both sides instead of silently mismatching.
    return real.encode("utf-8", "surrogateescape").decode("utf-8", "replace")


@dataclass
class Opts:
    db: "str | None"
    bin: str
    autoscan: bool
    fields: dict = field(default_factory=dict)


def parse_field_map(text):
    """Parse a ``key=value`` field map (from the options page) into a dict.
    Entries are separated by commas or newlines; blank/invalid entries ignored."""
    result = {}
    if not text:
        return result
    for entry in str(text).replace("\n", ",").split(","):
        entry = entry.strip()
        if not entry or "=" not in entry:
            continue
        k, v = entry.split("=", 1)
        k, v = k.strip(), v.strip()
        if k and v:
            result[k] = v
    return result


def resolve_config(settings, environ):
    """Resolve plugin options from Picard settings (a dict-like) with env
    overrides. ``MUSEFS_DB``/``MUSEFS_BIN`` take precedence over the page;
    autoscan and the field map are page-only."""
    db = environ.get("MUSEFS_DB") or (settings.get("musefs_db") or None)
    binary = environ.get("MUSEFS_BIN") or (settings.get("musefs_bin") or "musefs")
    autoscan = bool(settings.get("musefs_autoscan", True))
    fields = settings.get("musefs_fields") or {}
    if isinstance(fields, str):
        fields = parse_field_map(fields)
    return Opts(db=db, bin=binary, autoscan=autoscan, fields=fields)


def connect(db_path):
    """Open the musefs DB with a busy timeout and foreign keys enabled."""
    conn = sqlite3.connect(db_path)
    # 5s busy timeout so a brief write doesn't fail while the FUSE mount reads.
    conn.execute("PRAGMA busy_timeout = 5000")
    conn.execute("PRAGMA foreign_keys = ON")
    return conn


def check_schema_version(conn):
    """Raise ``SchemaMismatch`` unless the DB's ``user_version`` matches the
    version this plugin targets."""
    found = conn.execute("PRAGMA user_version").fetchone()[0]
    if found != EXPECTED_USER_VERSION:
        raise SchemaMismatch(found)


def track_id_for_path(conn, key):
    """Return the track id whose backing_path equals ``key``, or None."""
    row = conn.execute("SELECT id FROM tracks WHERE backing_path = ?", (key,)).fetchone()
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
    if data[:4] == b"RIFF" and data[8:12] == b"WEBP":
        return "image/webp"
    ext = os.path.splitext(path)[1].lower()
    return _EXT_MIME.get(ext, "application/octet-stream")


def upsert_art(conn, data, mime):
    """Content-address ``data`` by sha256 and return its art id, inserting only
    if new (mirrors musefs Db::upsert_art)."""
    sha = hashlib.sha256(data).hexdigest()
    conn.execute(
        "INSERT INTO art (sha256, mime, width, height, byte_len, data) "
        "VALUES (?, ?, NULL, NULL, ?, ?) ON CONFLICT(sha256) DO NOTHING",
        (sha, mime, len(data), data),
    )
    return conn.execute("SELECT id FROM art WHERE sha256 = ?", (sha,)).fetchone()[0]


def replace_track_art(conn, track_id, art_id):
    """Set the track's single front-cover art (picture_type 3, ordinal 0)."""
    conn.execute("DELETE FROM track_art WHERE track_id = ?", (track_id,))
    conn.execute(
        "INSERT INTO track_art (track_id, art_id, picture_type, description, "
        "ordinal) VALUES (?, ?, 3, '', 0)",
        (track_id, art_id),
    )


@dataclass
class SyncStats:
    synced: int = 0
    skipped: int = 0  # path had no matching track row
    art_linked: int = 0
    skipped_art: int = 0  # front cover oversized

    def summary(self):
        return (
            f"synced={self.synced} skipped={self.skipped} "
            f"art_linked={self.art_linked} skipped_art={self.skipped_art}"
        )


def sync_one(conn, key, pairs, art, stats, *, dry_run=False):
    """Sync one file's primitives into the DB. ``key`` is the realpath key,
    ``pairs`` the tag list, ``art`` an ``(bytes, mime)`` tuple or ``None``.
    Mutates ``stats``. Caller owns the transaction.

    Tags are always fully replaced. Art is replaced **only** when present and
    within the size cap (conditional replacement, spec §7): no front cover, or
    an oversized one, leaves any scan-seeded ``track_art`` untouched.
    """
    track_id = track_id_for_path(conn, key)
    if track_id is None:
        stats.skipped += 1
        return

    will_link_art = False
    if art is not None:
        data, _mime = art
        if len(data) > MAX_ART_BYTES:
            stats.skipped_art += 1
        else:
            will_link_art = True

    if not dry_run:
        replace_tags(conn, track_id, pairs)
        if will_link_art:
            data, mime = art
            art_id = upsert_art(conn, data, mime)
            replace_track_art(conn, track_id, art_id)

    if will_link_art:
        stats.art_linked += 1
    stats.synced += 1


def run_scan(binary, db_path, target):
    """Run ``<binary> scan <target> --db <db_path>``. Creates the DB if absent
    and fills the structural columns the plugin can't compute. Raises
    ``MusefsError`` on a missing binary, a timeout, or a non-zero exit."""
    try:
        result = subprocess.run(
            [binary, "scan", target, "--db", db_path],
            capture_output=True,
            timeout=SCAN_TIMEOUT_SECONDS,
        )
    except FileNotFoundError:
        raise MusefsError(
            f"musefs binary '{binary}' not found; set the binary path in the musefs options"
        )
    except subprocess.TimeoutExpired:
        raise MusefsError(
            f"`{binary} scan` for {target} timed out after {SCAN_TIMEOUT_SECONDS}s; "
            f"the scan may be stuck — check the binary and DB."
        )
    if result.returncode != 0:
        raise MusefsError(
            f"`{binary} scan` failed for {target} (exit {result.returncode}): "
            f"{result.stderr.decode(errors='replace').strip()}"
        )
