from __future__ import annotations


def lidarr_get(environ, key, default=None):
    """Look up a Lidarr custom-script environment variable case-insensitively.

    Lidarr documents these variables in PascalCase (e.g. ``Lidarr_EventType``),
    but at runtime it builds the child environment from a .NET
    ``StringDictionary``, which lowercases every key. A Linux script therefore
    actually receives ``lidarr_eventtype``. Resolve the documented name
    regardless of the case Lidarr emits.
    """
    if key in environ:
        return environ[key]
    lowered = key.lower()
    for name, value in environ.items():
        if name.lower() == lowered:
            return value
    return default
