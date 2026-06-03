# GENERATED from python-musefs/src/musefs_common/errors.py — do not edit.
# Run contrib/python-musefs/vendor_to_picard.py after changing the library.
#
from .constants import EXPECTED_USER_VERSION


class SchemaMismatch(Exception):  # noqa: N818
    """Raised when the musefs DB schema version differs from what this library
    targets (``EXPECTED_USER_VERSION``)."""

    def __init__(self, found):
        self.found = found
        super().__init__(
            f"musefs DB user_version is {found}, plugin targets "
            f"{EXPECTED_USER_VERSION}; the musefs and plugin versions have "
            f"diverged."
        )


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
