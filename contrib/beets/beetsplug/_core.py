"""beets-specific mapping for the musefs sync plugin: no beets imports here.

The shared store/scan/sync contract lives in the ``musefs_common`` package
(python-musefs); this module only maps beets items to musefs tag pairs and reads
album cover art into ``Record``s. ``musefs.py`` holds the BeetsPlugin adapter.
"""

import os

from musefs_common import MAX_ART_BYTES, ArtImage, Record, realpath_key, sniff_mime

# beets field name -> musefs (Vorbis-lowercase) tag key, for direct copies.
# beets 2.x exposes genre/composer as the multi-valued `genres`/`composers`
# (lists); the singular keys are kept for simpler/older items. List values are
# expanded into one tag per element by _values().
DIRECT_FIELDS = {
    "title": "title",
    "artist": "artist",
    "albumartist": "albumartist",
    "album": "album",
}

# (list_field, scalar_field, store_key): beets carries some tags as both a list
# (genres/composers, beets 2.x) and a joined scalar (genre/composer). Emitting
# both duplicates rows, so prefer the list when present, else the scalar. The
# per-twin dedup below is scoped to one twin: if a user maps an extra_field onto
# the "genre"/"composer" store key, that row is emitted in the direct-fields
# loop and won't be deduped against these — an unlikely config we don't guard.
TWIN_FIELDS = (
    ("genres", "genre", "genre"),
    ("composers", "composer", "composer"),
)


def _values(value):
    """Normalize a beets field value to a list of non-empty string values.
    Multi-valued beets fields (genres, composers) arrive as lists; scalars
    become a single-element list. Avoids stringifying a list as ``['Rock']``."""
    if value is None:
        return []
    items = value if isinstance(value, (list, tuple)) else [value]
    return [text for v in items if (text := str(v).strip())]


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
        for text in _values(getattr(item, beets_field, None)):
            pairs.append((key, text))

    for list_field, scalar_field, key in TWIN_FIELDS:
        values = _values(getattr(item, list_field, None)) or _values(
            getattr(item, scalar_field, None)
        )
        seen = set()
        for text in values:
            if text not in seen:
                seen.add(text)
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


def _album_art_path(item):
    """Return the album cover path (bytes/str) for an item, or None."""
    get_album = getattr(item, "get_album", None)
    album = get_album() if get_album else None
    if album is None:
        return None
    artpath = getattr(album, "artpath", None)
    return artpath or None


def _read_album_art(item, cache, stats):
    """Return ``(data, mime)`` for the item's album cover, or None. Reads each
    distinct cover once (cached by realpath). An unreadable or over-cap cover is
    counted into ``stats.skipped_art`` once and cached as None (matches the
    legacy ``_prepare_art`` counting before the python-musefs split).

    Also size-capped here (not only in sync_one) so a shared over-cap cover is
    counted once per distinct file — the double enforcement is intentional, not
    dead code."""
    artpath = _album_art_path(item)
    if not artpath:
        return None
    key = realpath_key(artpath)
    if key in cache:
        return cache[key]
    try:
        # Open the raw realpath, not realpath_key's lossy U+FFFD form: the file
        # is only opened, not matched against the DB.
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
    art = (data, sniff_mime(data, key))
    cache[key] = art
    return art


def build_records(items, *, fields=None, stats):
    """Build ``Record``s for beets items: map tags and resolve album art (with a
    per-run cache; unreadable/over-cap covers counted into ``stats.skipped_art``).
    ``stats`` is mutated and must be the same instance passed to ``sync_files``."""
    records = []
    art_cache = {}
    for item in items:
        cover = _read_album_art(item, art_cache, stats)
        records.append(
            Record(
                key=realpath_key(item.path),
                pairs=map_fields(item, fields),
                art=[ArtImage(*cover)] if cover else None,
            )
        )
    return records
