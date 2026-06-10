"""Picard-specific logic for the musefs sync plugin: no Picard imports here.

The shared store/scan/sync contract lives in the vendored ``musefs._common``
package (python-musefs); this module only maps Picard metadata to musefs tag
pairs, extracts the cover images, and resolves plugin options. ``__init__.py``
holds the Picard adapter (actions, options page, registration).
"""

from __future__ import annotations

from dataclasses import dataclass, field

from ._common.sync import ArtImage

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
    "comment": "comment",
    "lyrics": "lyrics",
    "grouping": "grouping",
    "isrc": "isrc",
    "replaygain_track_gain": "replaygain_track_gain",
    "replaygain_album_gain": "replaygain_album_gain",
    "replaygain_track_peak": "replaygain_track_peak",
    "replaygain_album_peak": "replaygain_album_peak",
    "musicbrainz_albumid": "musicbrainz_albumid",
    "musicbrainz_artistid": "musicbrainz_artistid",
}

# Keys whose value is dropped when it normalizes to zero (a 0 track/disc is noise).
_NUMERIC_KEYS = {"tracknumber", "discnumber"}


class MusefsError(Exception):  # noqa: N818
    """A user-facing failure (binary missing, scan failed, DB absent)."""


def _to_int(value):
    """Coerce to int, tolerating None and non-numeric strings so a bad tag
    can't abort sync."""
    try:
        return int(value or 0)
    except (ValueError, TypeError):
        return 0


def _values(metadata, field_name):
    """All non-empty, stripped string values of a Picard metadata field. Reads
    ``metadata.getall(field)`` when available (Picard's multi-valued accessor),
    else falls back to a plain ``.get``."""
    getall = getattr(metadata, "getall", None)
    if getall is not None:
        values = getall(field_name)
    else:
        v = metadata.get(field_name) if hasattr(metadata, "get") else None
        values = v if isinstance(v, (list, tuple)) else ([] if v is None else [v])
    return [text for v in values if (text := str(v).strip())]


def _first_value(metadata, field_name):
    """First non-empty, stripped string value of a Picard metadata field, or
    ``""`` if none."""
    values = _values(metadata, field_name)
    return values[0] if values else ""


# Keys whose Picard values may legitimately be multi-valued (one store row each).
# Everything else (title, tracknumber, discnumber, date) stays a single scalar.
_MULTI_VALUE_KEYS = {"artist", "albumartist", "genre", "composer"}


def map_fields(metadata, extra_fields=None):
    """Map a Picard Metadata (dict-like) to a list of (musefs_key, value) pairs.

    Keys in ``_MULTI_VALUE_KEYS`` emit one row per non-empty value (preserving
    Picard's order); every other key emits a single row (the first non-empty).
    Empty strings are omitted, as is a zero tracknumber/discnumber.
    ``extra_fields`` merges into (and can override) the direct-copy table.
    """
    fields = dict(DIRECT_FIELDS)
    if extra_fields:
        fields.update(extra_fields)

    pairs = []
    for pic_field, key in fields.items():
        if key in _MULTI_VALUE_KEYS:
            for text in _values(metadata, pic_field):
                pairs.append((key, text))
            continue
        text = _first_value(metadata, pic_field)
        if not text:
            continue
        if key in _NUMERIC_KEYS and _to_int(text) == 0:
            continue
        pairs.append((key, text))
    return pairs


# Picard maintype → ID3 picture type (mirrors Picard's own ID3 image-type
# map). An unrecognized maintype falls back to front-image detection (the more
# reliable signal), then to 0 (Other).
_ID3_PICTURE_TYPES = {
    "front": 3,
    "back": 4,
    "booklet": 5,
    "medium": 6,
}


def _picture_type(img):
    maintype = getattr(img, "maintype", None)
    if maintype in _ID3_PICTURE_TYPES:
        return _ID3_PICTURE_TYPES[maintype]
    is_front = getattr(img, "is_front_image", None)
    if is_front is not None and is_front():
        return 3
    return 0


def images(metadata):
    """Return an ``ArtImage`` per syncable image in a Picard Metadata, in
    Picard order. Duck-typed: images expose ``data`` and ``mimetype``, and
    optionally ``maintype``, ``comment``, ``can_be_saved_to_tags``, and
    ``is_front_image()``."""
    out = []
    for img in getattr(metadata, "images", None) or []:
        if not getattr(img, "can_be_saved_to_tags", True):
            continue
        out.append(
            ArtImage(
                data=img.data,
                mime=img.mimetype,
                picture_type=_picture_type(img),
                description=getattr(img, "comment", "") or "",
            )
        )
    return out


@dataclass
class Opts:
    db: "str | None"
    bin: str
    autoscan: bool
    fields: dict = field(default_factory=dict)


def parse_field_map(text):
    """Parse a ``key=value`` field map (from the options page) into a dict.
    One entry per line; blank lines and lines without ``=`` are ignored. A
    value may contain commas — they are kept literally."""
    result = {}
    if not text:
        return result
    for line in str(text).splitlines():
        line = line.strip()
        if not line or "=" not in line:
            continue
        k, v = line.split("=", 1)
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
