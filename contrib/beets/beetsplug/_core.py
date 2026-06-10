"""beets-specific mapping for the musefs sync plugin: no beets imports here.

The shared store/scan/sync contract lives in the ``musefs_common`` package
(python-musefs); this module only maps beets items to musefs tag pairs and reads
album cover art into ``Record``s. ``musefs.py`` holds the BeetsPlugin adapter.
"""

import os

from musefs_common import MAX_ART_BYTES, ArtImage, Record, realpath_key, sniff_mime

MANAGED_FLEXATTR = "musefs_managed"


def read_managed(item):
    """Parse the per-item ``musefs_managed`` flexattr into a list of keys."""
    raw = getattr(item, MANAGED_FLEXATTR, None)
    if not raw:
        return []
    return [k for k in str(raw).split(",") if k]


def format_managed(keys):
    """Serialize a managed key set: sorted, de-duplicated, comma-joined."""
    return ",".join(sorted(set(keys)))


def persist_managed(writes):
    """Persist each ``(item, managed_keys)`` pair into the beets DB via
    ``item.store()``. Never calls ``item.write()`` — that writes the audio file and
    fires ``after_write``, which would re-enter the plugin's reconcile loop."""
    for item, keys in writes:
        item[MANAGED_FLEXATTR] = format_managed(keys)
        item.store()


# beets field name -> canonical musefs (Vorbis-lowercase) key, where they differ.
RENAME = {
    "track": "tracknumber",
    "disc": "discnumber",
    "comments": "comment",
    "rg_track_gain": "replaygain_track_gain",
    "rg_album_gain": "replaygain_album_gain",
    "rg_track_peak": "replaygain_track_peak",
    "rg_album_peak": "replaygain_album_peak",
    "mb_trackid": "musicbrainz_trackid",
    "mb_albumid": "musicbrainz_albumid",
    "mb_artistid": "musicbrainz_artistid",
    "mb_albumartistid": "musicbrainz_albumartistid",
    "mb_releasegroupid": "musicbrainz_releasegroupid",
    "mb_releasetrackid": "musicbrainz_releasetrackid",
    "mb_workid": "musicbrainz_workid",
}

# plural beets list field -> the singular beets field it collapses onto. The
# plural wins when present; both resolve to one output key (via RENAME below) and
# are emitted once so a value is never written twice.
TWINS = {
    "artists": "artist",
    "albumartists": "albumartist",
    "genres": "genre",
    "composers": "composer",
    "lyricists": "lyricist",
    "arrangers": "arranger",
    "remixers": "remixer",
    "artists_sort": "artist_sort",
    "albumartists_sort": "albumartist_sort",
    "artists_credit": "artist_credit",
    "albumartists_credit": "albumartist_credit",
}

# Assembled into `date`; never emitted under their own names.
_DATE_PARTS = ("year", "month", "day")

# Output keys dropped when their numeric value is zero, because beets uses 0 as
# the "unset" sentinel for these integer fields, so a stored 0 is noise rather
# than a real value. `comp` is a 0/1 int, so this covers both its int and bool
# forms. (ReplayGain is exempt — beets defaults it to None, not 0.0, so a stored
# 0.00 dB is a real measured value and must survive.)
_DROP_IF_ZERO = {
    "tracknumber",
    "discnumber",
    "tracktotal",
    "disctotal",
    "comp",
    "bpm",
    "original_year",
    "original_month",
    "original_day",
}

# Used when an item has no _media_tag_fields (older beets / non-beets test stubs).
FALLBACK_TAG_FIELDS = (
    "title",
    "artist",
    "artists",
    "albumartist",
    "albumartists",
    "album",
    "genre",
    "genres",
    "composer",
    "composers",
    "track",
    "disc",
    "year",
    "month",
    "day",
)


def _fmt_db(value):
    return f"{float(value):.2f} dB"


def _fmt_peak(value):
    return f"{float(value):.6f}"


# Per-output-key value formatters: the explicit exceptions to default stringify.
FORMATTERS = {
    "replaygain_track_gain": _fmt_db,
    "replaygain_album_gain": _fmt_db,
    "replaygain_track_peak": _fmt_peak,
    "replaygain_album_peak": _fmt_peak,
}


def _output_key(field):
    """beets field name -> canonical musefs store key (collapse twin, then rename)."""
    base = TWINS.get(field, field)
    return RENAME.get(base, base.lower())


def _stringify(value, output_key):
    """Render one beets value to a store string. Formatter exceptions first, then
    the default: bool -> '1'/'0', integral float -> no trailing '.0', int -> str,
    else str().strip()."""
    formatter = FORMATTERS.get(output_key)
    if formatter is not None:
        return formatter(value)
    if isinstance(value, bool):
        return "1" if value else "0"
    if isinstance(value, float):
        return str(int(value)) if value.is_integer() else str(value)
    if isinstance(value, int):
        return str(value)
    return str(value).strip()


def _iter_values(value):
    """A beets field value as a list of raw (un-stringified) elements."""
    if value is None:
        return []
    return list(value) if isinstance(value, (list, tuple)) else [value]


def _is_zero(text):
    try:
        return float(text) == 0.0
    except ValueError:
        return False


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
    """Map a beets item to a list of (musefs_key, value) pairs covering every tag
    beets writes to a file (``item._media_tag_fields``), renamed to canonical
    keys, multi-values expanded, file facts excluded automatically. ``extra_fields``
    (the ``fields:`` config) is a final ``beets_field -> store_key`` override layer."""
    field_names = list(getattr(item, "_media_tag_fields", FALLBACK_TAG_FIELDS))
    # Process plural twins before singulars so the plural list wins its key.
    ordered = sorted(field_names, key=lambda f: (f not in TWINS, f))

    emitted = {}  # output_key -> list[str], insertion-ordered
    for field in ordered:
        if field in _DATE_PARTS:
            continue
        key = _output_key(field)
        if key in emitted:
            continue  # already claimed (plural beat singular, or a duplicate)
        values = []
        for raw in _iter_values(getattr(item, field, None)):
            if raw is None:
                continue
            if isinstance(raw, str) and not raw.strip():
                continue
            if isinstance(raw, bool) and not raw:
                continue  # comp=False etc. carry no info -> drop
            text = _stringify(raw, key)
            if not text:
                continue
            if key in _DROP_IF_ZERO and _is_zero(text):
                continue
            values.append(text)
        if values:
            emitted[key] = values

    date = _format_date(item)
    if date:
        emitted.setdefault("date", [date])

    if extra_fields:
        for beets_field, store_key in extra_fields.items():
            values = []
            for raw in _iter_values(getattr(item, beets_field, None)):
                if raw is None or (isinstance(raw, str) and not raw.strip()):
                    continue
                values.append(_stringify(raw, store_key))
            if values:
                emitted[store_key] = values  # override (last wins)

    return [(key, value) for key, values in emitted.items() for value in values]


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
    # Use the raw realpath, not realpath_key's lossy U+FFFD form: the file is
    # only opened and extension-sniffed, not matched against the DB.
    real = os.path.realpath(artpath)
    try:
        with open(real, "rb") as fh:
            data = fh.read()
    except OSError:
        stats.skipped_art += 1
        cache[key] = None
        return None
    if len(data) > MAX_ART_BYTES:
        stats.skipped_art += 1
        cache[key] = None
        return None
    art = (data, sniff_mime(data, os.fsdecode(real)))
    cache[key] = art
    return art


def _computed_path(item):
    """Beets' library-relative path for ``item``, decoded to a SQLite-safe str
    with the file extension removed (musefs re-appends it at render time).

    Mirrors ``realpath_key``'s lossy normalization (U+FFFD for undecodable
    bytes) so the value is always valid UTF-8, but without realpath's on-disk
    resolution. Returns "" when beets yields no usable path.
    """
    raw = item.destination(relative_to_libdir=True)
    decoded = os.fsdecode(raw)
    safe = decoded.encode("utf-8", "surrogateescape").decode("utf-8", "replace")
    return os.path.splitext(safe)[0].lstrip("/")


def _computed_path_or_skip(item, log):
    """``_computed_path`` guarded so a bad destination never aborts a sync.

    Returns "" (skip the tag) on any failure, warning through ``log`` if given.
    """
    try:
        return _computed_path(item)
    except Exception as exc:
        if log is not None:
            # beets' plugin logger is a StrFormatLogger ({}-style, not %-style).
            log.warning("musefs: skipping beets_path for {!r}: {}", item.path, exc)
        return ""


def build_records(items, *, fields=None, stats, write_path=True, restore_backing=False, log=None):
    """Build ``Record``s for beets items and the parallel managed-key writes.

    Returns ``(records, managed_writes)`` where ``managed_writes`` is a list of
    ``(item, managed_keys)`` the caller persists *after a successful commit* via
    ``persist_managed``.

    ``musefs_managed`` is an *accumulating* set (keys ever managed): each record's
    ``delete_keys`` is ``prev - keys(M)`` and the persisted set is the union
    ``prev | keys(M)``, so a key dropped from M stays a tombstone and keeps getting
    re-deleted on every sync until it re-enters M or ``restore_backing`` clears it.
    Under ``restore_backing`` no keys are deleted and the set is reset to ``keys(M)``
    (tombstones forgotten), so restored backing values stay visible."""
    records = []
    managed_writes = []
    art_cache = {}
    for item in items:
        cover = _read_album_art(item, art_cache, stats)
        pairs = map_fields(item, fields)
        if write_path:
            path = _computed_path_or_skip(item, log)
            if path:
                pairs.append(("beets_path", path))
        keys_now = {key for key, _ in pairs}
        prev = set(read_managed(item))
        if restore_backing:
            delete_keys = []
            managed = sorted(keys_now)
        else:
            delete_keys = sorted(prev - keys_now)
            managed = sorted(prev | keys_now)
        records.append(
            Record(
                key=realpath_key(item.path),
                pairs=pairs,
                art=[ArtImage(*cover)] if cover else None,
                delete_keys=delete_keys,
            )
        )
        managed_writes.append((item, managed))
    return records, managed_writes
