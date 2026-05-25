"""Pure logic for the musefs beets plugin: no beets imports live here.

Everything beets-specific (the BeetsPlugin subclass, commands, event
listeners) is in ``musefs.py``; this module is unit-testable on its own.
"""

# Schema version this plugin was written against (musefs schema.rs MIGRATIONS
# length). The plugin refuses to run against any other version.
EXPECTED_USER_VERSION = 1

# Mirror of musefs-core scan.rs MAX_ART_BYTES: 16 MiB minus 64 KiB headroom.
MAX_ART_BYTES = 16 * 1024 * 1024 - 64 * 1024
