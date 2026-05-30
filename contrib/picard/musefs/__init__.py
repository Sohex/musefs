"""musefs Picard plugin: sync Picard metadata into the musefs SQLite store.

The Picard-coupled glue is filled in during implementation; until then this
guarded block is a no-op so the package imports cleanly without Picard (the
test suite only exercises ``musefs._core``).
"""

PLUGIN_NAME = "musefs sync"
PLUGIN_AUTHOR = "musefs contributors"
PLUGIN_DESCRIPTION = (
    "Right-click a file/album → 'Sync to musefs' to push Picard's tags and "
    "front cover into a musefs SQLite store, without rewriting the audio file."
)
PLUGIN_VERSION = "0.1.0"
PLUGIN_API_VERSIONS = ["2.2", "2.6", "2.7", "2.8", "2.9", "2.10", "2.11", "2.12"]
PLUGIN_LICENSE = "MIT"
PLUGIN_LICENSE_URL = "https://opensource.org/licenses/MIT"

try:
    from picard.ui.itemviews import BaseAction  # noqa: F401
except ImportError:
    # Picard not present (e.g. running the unit tests). Glue is registered in
    # the full __init__.py; _core imports remain available regardless.
    pass
