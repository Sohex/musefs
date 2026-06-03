"""Picard-specific logic for the musefs sync plugin: no Picard imports here.

The shared store/scan/sync contract lives in the vendored ``musefs._common``
package (python-musefs); this module only maps Picard metadata to musefs tag
pairs, extracts the front cover, and resolves plugin options. ``__init__.py``
holds the Picard adapter (actions, options page, registration).
"""

from __future__ import annotations

from dataclasses import dataclass, field

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



