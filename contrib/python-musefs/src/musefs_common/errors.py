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
                f"`musefs scan` against the library, which migrates the store in place"
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
