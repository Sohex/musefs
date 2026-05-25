"""Pure logic for the musefs beets plugin: no beets imports live here.

Everything beets-specific (the BeetsPlugin subclass, commands, event
listeners) is in ``musefs.py``; this module is unit-testable on its own.
"""

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


def _format_date(item):
    year = int(getattr(item, "year", 0) or 0)
    if not year:
        return None
    month = int(getattr(item, "month", 0) or 0)
    day = int(getattr(item, "day", 0) or 0)
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

    track = int(getattr(item, "track", 0) or 0)
    if track:
        pairs.append(("tracknumber", str(track)))
    disc = int(getattr(item, "disc", 0) or 0)
    if disc:
        pairs.append(("discnumber", str(disc)))
    date = _format_date(item)
    if date:
        pairs.append(("date", date))

    return pairs
