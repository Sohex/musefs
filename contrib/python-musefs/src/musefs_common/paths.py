import os


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
