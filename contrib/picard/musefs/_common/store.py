# GENERATED from python-musefs/src/musefs_common/store.py — do not edit.
# Run contrib/python-musefs/vendor_to_picard.py after changing the library.
#
import contextlib
import hashlib
import os
import sqlite3
import struct
from dataclasses import dataclass

from .constants import EXPECTED_USER_VERSION
from .errors import ArtDigestMismatch, SchemaMismatch

# SQLite caps a statement's host parameters (SQLITE_MAX_VARIABLE_NUMBER: 999 on
# the <3.32 floor). Chunk bulk IN-lists below it so large lookups never trip it.
_MAX_SQL_VARS = 900


@dataclass(frozen=True)
class TagRow:
    """One tag row read back from the store: the key, the text value, and the
    raw ``value_blob``. Plugin-owned text tags have ``value_blob is None``;
    scanner-written binary tags have ``value == ""`` and ``value_blob`` bytes."""

    key: str
    value: str
    value_blob: object = None  # bytes | None


# sqlite3.LEGACY_TRANSACTION_CONTROL is 3.12+; it is == -1. Use getattr so this
# module still imports on the 3.8 floor (where the constant does not exist).
_LEGACY = getattr(sqlite3, "LEGACY_TRANSACTION_CONTROL", -1)


def _is_autocommit(conn):
    """True if the connection auto-commits each statement (no caller-owned
    transaction will be committed for us)."""
    ac = getattr(conn, "autocommit", _LEGACY)  # 3.12+ attribute; _LEGACY on <3.12
    if ac is True:
        return True
    if ac is False:
        return False
    return conn.isolation_level is None  # legacy transaction control


def _is_legacy(conn):
    """True if the connection uses legacy transaction control (the <3.12 default
    and the 3.12+ LEGACY_TRANSACTION_CONTROL mode)."""
    return getattr(conn, "autocommit", _LEGACY) == _LEGACY


@contextlib.contextmanager
def _savepoint(conn, name):
    """Make a DELETE+INSERT block atomic regardless of the connection's
    transaction mode. On a caller-managed connection it nests via SAVEPOINT and
    never commits the enclosing transaction; on an autocommit connection the
    outermost call owns a transaction for the block (commit on success, rollback
    on failure). Nested calls only nest -- they never BEGIN or commit -- so a
    sync_one savepoint may wrap these per-function savepoints safely.

    ``name`` must be a hardcoded SQL identifier (it is interpolated into the SQL,
    so never pass caller-controlled text)."""
    autocommit = _is_autocommit(conn)
    owns = not conn.in_transaction  # outermost call: it opens & owns the txn
    # Legacy mode never auto-BEGINs before SAVEPOINT, so a savepoint opened as
    # the first statement of a batch would become the outermost transaction and
    # commit durably on RELEASE. Force a nesting BEGIN there. PEP-249 modes
    # auto-begin before any statement, so they need no nudge.
    if owns and _is_legacy(conn):
        conn.execute("BEGIN")
    conn.execute(f"SAVEPOINT {name}")
    try:
        yield
    except BaseException:
        try:
            conn.execute(f"ROLLBACK TO {name}")
            conn.execute(f"RELEASE {name}")
            if owns and autocommit:
                conn.rollback()
        except sqlite3.Error:
            pass  # never mask the original exception with a cleanup failure
        raise
    else:
        conn.execute(f"RELEASE {name}")
        if owns and autocommit:
            conn.commit()


def connect(db_path):
    """Open the musefs DB with a busy timeout and foreign keys enabled."""
    conn = sqlite3.connect(db_path)
    # 5s busy timeout so a brief write doesn't fail while the FUSE mount reads.
    conn.execute("PRAGMA busy_timeout = 5000")
    conn.execute("PRAGMA foreign_keys = ON")
    return conn


def check_schema_version(conn):
    """Raise ``SchemaMismatch`` unless the DB's ``user_version`` matches the
    version this library targets. Call on an open connection from ``connect``."""
    found = conn.execute("PRAGMA user_version").fetchone()[0]
    if found != EXPECTED_USER_VERSION:
        raise SchemaMismatch(found)


def path_param(key):
    """Encode a ``backing_path`` key for binding.

    The column is a ``BLOB`` from schema v4 on: a filesystem path is bytes, and
    the lossy ``str`` round-trip collapsed two distinct files onto one row.
    SQLite never compares a ``TEXT`` value equal to a ``BLOB``, so a ``str``
    bound as-is matches nothing at all rather than failing — which is why this
    is a helper and not four inline ``.encode()`` calls.

    The library's own type is ``str``, carrying undecodable bytes as surrogates
    the way Python spells an OS path everywhere else. ``os.fsencode`` and
    ``os.fsdecode`` are exact inverses, so the round trip is lossless and — the
    part that matters — injective: two distinct files cannot become one key.
    Using them rather than a hardcoded ``utf-8`` keeps both directions on the
    same codec, so a filesystem encoding that is not UTF-8 cannot split them.
    """
    return os.fsencode(key)


def path_value(raw):
    """Decode a ``backing_path`` read back out. The inverse of `path_param`."""
    return os.fsdecode(bytes(raw) if isinstance(raw, (bytes, bytearray)) else raw)


def track_id_for_path(conn, key):
    """Return the track id whose backing_path equals ``key``, or None."""
    row = conn.execute(
        "SELECT id FROM tracks WHERE backing_path = ?", (path_param(key),)
    ).fetchone()
    return row[0] if row else None


def track_ids_for_paths(conn, keys):
    """Resolve many ``backing_path`` keys to track ids in one pass, returning a
    ``{key: id}`` dict that omits keys with no matching track row. The IN-list is
    chunked under SQLite's host-parameter cap so arbitrarily large lookups work
    (the bulk counterpart to ``track_id_for_path``)."""
    # Deduplicate while preserving first-seen order: a key repeated across chunk
    # boundaries would re-fetch its row in a later chunk and trip the duplicate
    # guard below even on a conformant DB.
    keys = list(dict.fromkeys(keys))
    out = {}
    for start in range(0, len(keys), _MAX_SQL_VARS):
        chunk = keys[start : start + _MAX_SQL_VARS]
        placeholders = ",".join("?" for _ in chunk)
        rows = conn.execute(
            f"SELECT backing_path, id FROM tracks WHERE backing_path IN ({placeholders})",
            [path_param(k) for k in chunk],
        )
        for raw_path, track_id in rows:
            backing_path = path_value(raw_path)
            if backing_path in out:
                # backing_path is UNIQUE in the schema, so a duplicate means a
                # non-conformant DB; collapsing it would silently hide a track
                # from prune (#478). Fail loudly instead.
                raise ValueError(
                    f"duplicate backing_path {backing_path!r} in tracks "
                    f"(ids {out[backing_path]} and {track_id})"
                )
            out[backing_path] = track_id
    return out


def track_ids_by_tag(conn, key, value):
    """Return a list of track ids whose plugin-owned text tag ``(key, value)``
    matches (order unspecified, possibly empty).

    Scoped to text rows (``value_blob IS NULL``); scanner-written binary tags
    never match. The intent-based counterpart to ``prune_missing``'s
    existence-based scoping: used to map a source's "I deleted this album/artist"
    signal back to the rows it tagged.
    """
    rows = conn.execute(
        "SELECT track_id FROM tags WHERE key = ? AND value = ? AND value_blob IS NULL",
        (key, value),
    )
    return [track_id for (track_id,) in rows]


def delete_tracks(conn, track_ids):
    """Unconditionally delete the given track rows; return the count actually
    deleted (an already-gone id contributes 0).

    The intent-based delete: unlike ``prune_missing`` it does not check on-disk
    existence. ``tags`` and ``track_art`` rows cascade away via the schema's
    ``ON DELETE CASCADE`` (``connect`` enables ``foreign_keys = ON``).
    """
    deleted = 0
    for track_id in track_ids:
        deleted += conn.execute("DELETE FROM tracks WHERE id = ?", (track_id,)).rowcount
    return deleted


def tags_for_track(conn, track_id):
    """Read back a track's tag rows as an ordered ``list[TagRow]`` (by key, then
    ordinal). Includes both plugin-owned text tags (``value_blob is None``) and
    scanner-written binary tags (``value == ""``, ``value_blob`` bytes)."""
    rows = conn.execute(
        "SELECT key, value, value_blob FROM tags WHERE track_id = ? ORDER BY key, ordinal",
        (track_id,),
    )
    return [TagRow(key, value, value_blob) for key, value, value_blob in rows]


def _rows_to_prune(conn, track_ids):
    """Yield ``(track_id, backing_path)`` for the rows a prune should consider:
    every track, or just ``track_ids`` (ids with no row are skipped). A repeated
    id is considered once, in first-seen order, so a caller that passes
    duplicates neither over-counts the prune nor reports one path twice."""
    if track_ids is None:
        for track_id, raw_path in conn.execute("SELECT id, backing_path FROM tracks"):
            yield track_id, path_value(raw_path)
        return
    for track_id in dict.fromkeys(track_ids):
        row = conn.execute("SELECT backing_path FROM tracks WHERE id=?", (track_id,)).fetchone()
        if row is not None:
            yield track_id, path_value(row[0])


def _backing_is_gone(track_id, path, unreadable):
    """True only if ``path`` is confirmed absent. A path that cannot be stat'd
    for any other reason is not a deletion: the row is kept and, when
    ``unreadable`` is a list, recorded there as ``(track_id, path, message)``."""
    try:
        os.stat(path)
    except FileNotFoundError:
        return True
    except OSError as exc:
        if unreadable is not None:
            unreadable.append((track_id, path, str(exc)))
    return False


def prune_missing(conn, track_ids=None, *, unreadable=None):
    """Delete track rows whose backing file is confirmed gone; return the count.

    "Confirmed" is the whole point: a path counts as missing only when
    ``os.stat`` raises ``FileNotFoundError``. Every other ``OSError`` -- a
    permission change on a parent directory, a network or removable mount that
    is momentarily unreachable -- means "cannot tell", and the row survives.
    ``ON DELETE CASCADE`` would take that track's plugin-written ``tags`` and
    ``track_art`` rows with it, so a path we merely failed to stat must never
    read as a deletion (#692). ``os.path.exists`` is deliberately not used: it
    collapses "absent" and "cannot stat" into one ``False``. This mirrors
    ``musefs revalidate --prune``, which deletes only on ``ErrorKind::NotFound``.

    When ``track_ids`` is provided, only those tracks are checked and
    potentially pruned. Otherwise, every track in the database is checked.

    Pass a list as ``unreadable`` to collect ``(track_id, backing_path,
    message)`` for each row kept because its path could not be stat'd; a pass
    that pruned nothing can then tell an intact library from an unreachable one.
    """
    gone = [
        (track_id,)
        for track_id, path in _rows_to_prune(conn, track_ids)
        if _backing_is_gone(track_id, path, unreadable)
    ]
    conn.executemany("DELETE FROM tracks WHERE id = ?", gone)
    return len(gone)


def replace_tags(conn, track_id, pairs):
    """Replace all tags for a track. Duplicate keys get incrementing ordinals
    (mirroring musefs scan ingest).

    Atomic via an internal savepoint (see ``_savepoint``), so a crash between the
    DELETE and the INSERT can never leave the track's text tags wiped -- safe
    even when called on an autocommit connection."""
    # Scope to the plugin-owned text rows: scanner-written binary tags
    # (value_blob NOT NULL) must survive a sync (#82).
    with _savepoint(conn, "musefs_replace_tags"):
        conn.execute("DELETE FROM tags WHERE track_id = ? AND value_blob IS NULL", (track_id,))
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


def merge_tags(conn, track_id, managed_pairs, delete_keys):
    """Per-key replace of the plugin-managed text tags, leaving unmanaged text
    rows (the scan-seeded baseline) intact. ``managed_pairs`` is an ordered list
    of (key, value); every key it names is cleared and rewritten with contiguous
    ordinals. ``delete_keys`` names keys to clear without rewriting (tags the
    plugin previously managed and the user has now removed). Both deletes are
    scoped to ``value_blob IS NULL`` so scanner-written binary tags survive.

    Atomic via an internal savepoint (see ``_savepoint``): the per-key deletes
    and the rewrite either all land or none do, even on an autocommit
    connection."""
    with _savepoint(conn, "musefs_merge_tags"):
        by_key = {}
        for key, value in managed_pairs:
            by_key.setdefault(key, []).append(value)

        # Case-fold the key match: a scan seeds an unmapped tag in the file's
        # native case (e.g. Vorbis ``LABEL``) while the plugin canonicalises to
        # lowercase (``label``). Vorbis keys render case-insensitively, so an
        # exact-case delete would leave the scan row and render a duplicate (#407).
        for key in set(by_key) | set(delete_keys or ()):
            conn.execute(
                "DELETE FROM tags WHERE track_id = ? AND lower(key) = lower(?) "
                "AND value_blob IS NULL",
                (track_id, key),
            )

        rows = [
            (track_id, key, value, ordinal)
            for key, values in by_key.items()
            for ordinal, value in enumerate(values)
        ]
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
    # WebP: 'RIFF' <4-byte size> 'WEBP'.
    if data[:4] == b"RIFF" and data[8:12] == b"WEBP":
        return "image/webp"
    ext = os.path.splitext(path)[1].lower()
    return _EXT_MIME.get(ext, "application/octet-stream")


_PNG_SIGNATURE = b"\x89PNG\r\n\x1a\n"
# The start-of-frame markers, which carry the frame's dimensions: every C0-CF
# except DHT (C4), JPG (C8) and DAC (CC), which share the range and do not.
_JPEG_SOF = frozenset((
    0xC0,
    0xC1,
    0xC2,
    0xC3,
    0xC5,
    0xC6,
    0xC7,
    0xC9,
    0xCA,
    0xCB,
    0xCD,
    0xCE,
    0xCF,
))
# Markers that stand alone, with no length field: RST0-RST7 and TEM.
_JPEG_STANDALONE = frozenset(range(0xD0, 0xD8)) | {0x01}
# PNG caps a dimension at 2**31 - 1; the same bound keeps a JPEG's inside the
# column's range.
_MAX_DIMENSION = 2**31 - 1


def image_dimensions(data):
    """``(width, height)`` from a PNG or JPEG header, or ``None``.

    Reads header bytes only, with no decoder and no dependency: PNG's ``IHDR``
    chunk, which the format requires to come first, and a JPEG's start-of-frame
    segment, found by walking the marker segments ahead of the image data.
    Anything else — WebP, an unrecognised format, a truncated or malformed
    header, a zero dimension — is ``None``, which is how ``track_art`` spells
    "not stated" (musefs #737). Bit depth and colour count are not read."""
    if data[:8] == _PNG_SIGNATURE:
        return _png_dimensions(data)
    if data[:2] == b"\xff\xd8":
        return _jpeg_dimensions(data)
    return None


def _checked_dimensions(width, height):
    if 0 < width <= _MAX_DIMENSION and 0 < height <= _MAX_DIMENSION:
        return (width, height)
    return None


def _png_dimensions(data):
    # Signature (8), then the IHDR chunk: length (4) = 13, type (4), width (4),
    # height (4), big-endian.
    if len(data) < 24 or data[8:12] != struct.pack(">I", 13) or data[12:16] != b"IHDR":
        return None
    width, height = struct.unpack(">II", data[16:24])
    return _checked_dimensions(width, height)


def _jpeg_dimensions(data):
    i = 2  # past SOI
    end = len(data)
    while i < end:
        if data[i] != 0xFF:
            return None
        # Any number of 0xFF fill bytes may precede a marker.
        while i < end and data[i] == 0xFF:
            i += 1
        if i >= end:
            return None
        marker = data[i]
        i += 1
        if marker in (0xD9, 0xDA):
            # End of image, or the image data itself: no frame header came first.
            return None
        if marker in _JPEG_STANDALONE:
            continue
        if i + 2 > end:
            return None
        (length,) = struct.unpack(">H", data[i : i + 2])
        if length < 2 or i + length > end:
            return None
        if marker in _JPEG_SOF:
            # Length (2), precision (1), height (2), width (2).
            if length < 7:
                return None
            height, width = struct.unpack(">HH", data[i + 3 : i + 7])
            return _checked_dimensions(width, height)
        i += length
    return None


def upsert_art(conn, data):
    """Content-address ``data`` by sha256 and return its art id, inserting only
    if new (mirrors musefs Db::upsert_art).

    The row is the bytes and nothing else. It used to carry the mime and the
    dimensions, which made the first writer of a given image choose them for
    every track that shared it — so they moved to ``track_art``, and this
    function lost the argument it could not honour: on a sha256 conflict the
    stored row was kept and the passed mime silently ignored.

    A conflict returns the row already filed under the digest, and a store is
    only content-addressed if that row holds these bytes. Nothing in the schema
    ties ``sha256`` to ``data``, so the conflicting row is compared with ``data``
    — in SQL, nothing re-hashed — and :class:`ArtDigestMismatch` is raised
    instead of returning an id that points at another image (musefs #724). A
    fresh insert needs no comparison: it just stored these bytes."""
    sha = hashlib.sha256(data).hexdigest()
    inserted = conn.execute(
        "INSERT INTO art (sha256, byte_len, data) VALUES (?, ?, ?) ON CONFLICT(sha256) DO NOTHING",
        (sha, len(data), data),
    ).rowcount
    art_id = conn.execute("SELECT id FROM art WHERE sha256 = ?", (sha,)).fetchone()[0]
    if inserted == 0:
        (holds_these_bytes,) = conn.execute(
            "SELECT data = ? FROM art WHERE id = ?", (data, art_id)
        ).fetchone()
        if not holds_these_bytes:
            raise ArtDigestMismatch(art_id, sha)
    return art_id


def replace_track_art(conn, track_id, arts):
    """Replace the track's art rows. ``arts`` is an ordered list of
    ``(art_id, picture_type, description, mime)`` or
    ``(art_id, picture_type, description, mime, width, height)``; each row's
    ``ordinal`` is its list index.

    ``mime`` describes *this* link, not the image bytes. From schema v4 it lives
    on ``track_art`` rather than ``art``, because two files can hold
    byte-identical art and declare it differently — and while ``art`` owned it,
    whichever file was ingested first chose it for every track sharing the blob.
    It is what musefs writes into the synthesized picture block, so a link
    without one serves an empty MIME type.

    ``width`` and ``height`` describe the embedding too, and a six-field row
    states them; ``None`` in either, or a four-field row, leaves it unset.
    :func:`image_dimensions` reads them from a PNG or JPEG header without
    decoding, which is how :func:`sync_one` fills them (musefs #737). ``depth``
    and ``colors`` are always left unset: unset is how both the FLAC picture
    block and musefs spell "not stated".

    Atomic via an internal savepoint (see ``_savepoint``): the DELETE and the
    re-insert either both land or neither does, even on an autocommit
    connection."""
    rows = [_track_art_row(track_id, i, art) for i, art in enumerate(arts)]
    with _savepoint(conn, "musefs_replace_track_art"):
        conn.execute("DELETE FROM track_art WHERE track_id = ?", (track_id,))
        conn.executemany(
            "INSERT INTO track_art (track_id, art_id, picture_type, description, "
            "mime, width, height, ordinal) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            rows,
        )


def _track_art_row(track_id, ordinal, art):
    """One ``track_art`` insert from a four- or six-field ``replace_track_art``
    row."""
    if len(art) == 4:
        art_id, picture_type, description, mime = art
        width = height = None
    else:
        art_id, picture_type, description, mime, width, height = art
    return (track_id, art_id, picture_type, description, mime, width, height, ordinal)
