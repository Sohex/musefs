import os


def realpath_key(path):
    """Canonical absolute path matching musefs scan's stored ``backing_path``.

    Accepts ``str`` or ``bytes`` and always returns ``str``.

    The resolution is done on *bytes* and decoded with ``os.fsdecode``, so the
    key round-trips back to the exact path on disk through ``os.fsencode`` —
    which is what `path_param` binds. Those two are exact inverses whatever the
    filesystem encoding is, which is the point of using them rather than a
    hardcoded codec: undecodable bytes escape to surrogates under UTF-8 or
    ASCII, and a total codec like Latin-1 decodes them outright, and both
    round-trip. A filename on Unix is an arbitrary byte
    string, and from schema v4 the store holds those bytes verbatim (#680).

    This helper used to normalize undecodable bytes to ``U+FFFD``, matching what
    the scanner itself stored back when it wrote ``to_string_lossy()``. Both
    sides agreed, and both were wrong: that mapping is not injective, so two
    distinct files collapsed onto one key — and onto one row. Now that the
    scanner stores real bytes, reproducing the old form would simply fail to
    match, and a plugin would skip such a file without saying so.
    """
    return os.fsdecode(os.path.realpath(os.fsencode(path)))
