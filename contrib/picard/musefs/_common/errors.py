# GENERATED from python-musefs/src/musefs_common/errors.py — do not edit.
# Run contrib/python-musefs/vendor_to_picard.py after changing the library.
#
import sqlite3

from .constants import EXPECTED_USER_VERSION


class SchemaMismatch(Exception):  # noqa: N818
    """Raised when the musefs DB schema version differs from what this library
    targets (``EXPECTED_USER_VERSION``). Both hosts surface the message
    verbatim, so it names which side is behind and the action that fixes it
    (#654)."""

    def __init__(self, found):
        self.found = found
        super().__init__(self._message(found))

    @staticmethod
    def _message(found):
        head = f"musefs DB user_version is {found}, plugin targets {EXPECTED_USER_VERSION}"
        if found > EXPECTED_USER_VERSION:
            return (
                f"{head}; the store was written by a newer musefs than this plugin "
                f"knows — upgrade the plugin (beets: upgrade python-musefs; Picard: "
                f"re-install the plugin folder) to write this store"
            )
        if found < EXPECTED_USER_VERSION:
            return (
                f"{head}; the store predates this plugin — upgrade musefs and run "
                f"`musefs migrate --db <store>`, which upgrades the store in place"
            )
        return head


class ScanError(Exception):  # noqa: N818
    """A `musefs scan` invocation failed. ``kind`` is one of ``"not_found"``,
    ``"timeout"``, ``"failed"``; the remaining attributes carry enough context
    for a host adapter to format its own user-facing message."""

    def __init__(self, kind, *, binary, target, timeout=None, returncode=None, stderr=""):
        self.kind = kind
        self.binary = binary
        self.target = target
        self.timeout = timeout
        self.returncode = returncode
        self.stderr = stderr
        super().__init__(self._default_message())

    def _default_message(self):
        if self.kind == "not_found":
            return f"musefs binary '{self.binary}' not found"
        if self.kind == "timeout":
            return f"`{self.binary} scan` for {self.target} timed out after {self.timeout}s"
        return (
            f"`{self.binary} scan` failed for {self.target} (exit {self.returncode}): {self.stderr}"
        )


class ArtDigestMismatch(sqlite3.IntegrityError):  # noqa: N818
    """An ``art`` row is filed under a ``sha256`` its own bytes do not hash to.

    Nothing in the schema ties the digest column to the data column, so a
    crafted or corrupt store can hold such a row, and linking it would give a
    track some other image's bytes (musefs #724). ``upsert_art`` raises this
    instead of returning the row's id.

    A subclass of ``sqlite3.IntegrityError`` on purpose: the store is refusing
    this one record's art, so ``sync_one`` skips the record through the same
    path it takes for a CHECK violation, rather than aborting the whole sync —
    mirroring how ``musefs scan`` fails only the file that would link it."""

    def __init__(self, art_id, sha256):
        self.art_id = art_id
        self.sha256 = sha256
        super().__init__(
            f"art {art_id} is stored under sha256 {sha256} but holds different bytes "
            f"(crafted or corrupt DB)"
        )
