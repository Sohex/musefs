"""Picard-specific logic for the musefs sync plugin: no Picard imports here.

The shared store/scan/sync contract lives in the vendored ``musefs._common``
package (python-musefs); this module only maps Picard metadata to musefs tag
pairs, extracts the cover images, and resolves plugin options. ``__init__.py``
holds the Picard adapter (actions, options page, registration).
"""

from __future__ import annotations

from dataclasses import dataclass, field

from ._common.sync import ArtImage

# Picard internal tag name -> canonical musefs store key, for the few names whose
# on-disk form differs from Picard's variable name. Mirrors the format-agnostic
# subset of Picard's own var->tag translation (its vorbis.py ``__rtranslate``):
# the MusicBrainz recording/track id swap, the movement name/number swap, and the
# track/disc totals. Every other Picard name already equals its on-disk key and
# passes through verbatim.
RENAME = {
    "musicbrainz_recordingid": "musicbrainz_trackid",
    "musicbrainz_trackid": "musicbrainz_releasetrackid",
    "movementnumber": "movement",
    "movement": "movementname",
    "totaltracks": "tracktotal",
    "totaldiscs": "disctotal",
}

# Output keys whose value is dropped when it normalizes to zero (a 0
# number/total/compilation flag is noise, not data).
_DROP_IF_ZERO = {
    "tracknumber",
    "discnumber",
    "tracktotal",
    "disctotal",
    "movement",
    "compilation",
    "bpm",
}


class MusefsError(Exception):  # noqa: N818
    """A user-facing failure (binary missing, scan failed, DB absent)."""


def _is_zero(text):
    """True when ``text`` represents a numeric zero, so a 0 placeholder is
    dropped. Float-aware (matches the beets sibling's check): non-numeric and
    fractional non-zero values are not zero and pass through unharmed."""
    try:
        return float(text) == 0.0
    except ValueError:
        return False


def _output_key(name):
    """Map a Picard tag name to a ``(store_key, performer_role)`` pair.

    ``performer_role`` is ``None`` for ordinary keys; for ``performer`` /
    ``performer:<role>`` it is the role string ("" when bare) so the caller folds
    it into the value in Picard's own ``Name (Role)`` form. ``comment`` /
    ``lyrics`` (bare or with a description) collapse to their base key, dropping
    the description. Everything else is renamed via ``RENAME`` or passes through
    verbatim. Picard's ``normalize_tag`` strips trailing colons, so a key only
    carries a ``:`` when it has a non-empty description/role.
    """
    base, _, desc = name.partition(":")
    if base == "performer":
        return "performer", desc
    if base in ("comment", "lyrics"):
        return base, None
    return RENAME.get(name, name), None


def map_fields(metadata, extra_fields=None):
    """Map a Picard ``Metadata`` to a list of ``(store_key, value)`` pairs.

    Enumerates every populated tag via ``rawitems``, skipping Picard's hidden
    ``~``-prefixed internals. Each non-empty value becomes its own row (the store
    has set semantics); values sharing an output key accumulate, so multi-role
    performers and multi-description comments all survive. Keys are canonicalized
    by :func:`_output_key`; ``_DROP_IF_ZERO`` keys drop a zero value.
    ``extra_fields`` (the options-page ``picard_field -> store_key`` map) is a
    final override layer applied verbatim — no role-fold, no zero-drop.
    """
    emitted = {}  # store_key -> list[str], insertion-ordered, accumulating
    for name, values in metadata.rawitems():
        if name.startswith("~"):
            continue
        key, role = _output_key(name)
        for raw in values:
            text = str(raw).strip()
            if not text:
                continue
            if role:
                text = f"{text} ({role})"
            if key in _DROP_IF_ZERO and _is_zero(text):
                continue
            emitted.setdefault(key, []).append(text)

    if extra_fields:
        for pic_field, store_key in extra_fields.items():
            values = [t for v in metadata.getall(pic_field) if (t := str(v).strip())]
            if values:
                emitted[store_key] = values

    return [(key, value) for key, values in emitted.items() for value in values]


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
