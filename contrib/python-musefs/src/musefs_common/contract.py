"""Canonical tag-row contract both plugins must satisfy.

Each plugin's test builds an equivalent host object (a beets ``Item`` from the
list fields, a Picard ``Metadata`` from ``getall``) carrying ``CONTRACT_VALUES``
and asserts its ``map_fields`` output, normalized, equals
``normalize_rows(CONTRACT_EXPECTED)``. This guards #84/#86 against future
divergence between the two mappers.

Scope: the genuinely-shared multi-value fields (``genre``, ``composer``). beets
has no multi-artist field, so ``artist``/``albumartist`` are single-valued here;
Picard's multi-artist expansion is tested in its own unit tests.
"""

from collections import defaultdict

CONTRACT_VALUES = {
    "title": "Song",
    "artist": "Alice",
    "albumartist": "Alice",
    "album": "Greatest Hits",
    "genre": ["Rock", "Pop"],
    "composer": ["Carol", "Dave"],
}

CONTRACT_EXPECTED = [
    ("title", "Song"),
    ("artist", "Alice"),
    ("albumartist", "Alice"),
    ("album", "Greatest Hits"),
    ("genre", "Rock"),
    ("genre", "Pop"),
    ("composer", "Carol"),
    ("composer", "Dave"),
]


def normalize_rows(rows):
    """Group ``(key, value)`` rows by key into a comparison-stable dict. All
    contract keys use set semantics (the store treats multi-values as a set), so
    each key's values are returned sorted."""
    grouped = defaultdict(list)
    for key, value in rows:
        grouped[key].append(value)
    return {key: sorted(values) for key, values in grouped.items()}
