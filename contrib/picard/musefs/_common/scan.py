# GENERATED from python-musefs/src/musefs_common/scan.py — do not edit.
# Run contrib/python-musefs/vendor_to_picard.py after changing the library.
#
import os
import subprocess
from dataclasses import dataclass

from .errors import ScanError

# `musefs scan` / `musefs revalidate` exit 2 when the batch itself completed and
# committed but at least one file could not be ingested — the documented
# partial-success signal (docs/src/guide/scanning.md), and the only
# machine-detectable one: per-file failures otherwise appear on stderr alone.
# It is deliberately not a hard failure: everything parseable is in the store,
# so a host adapter warns and goes on to sync rather than aborting (#647).
PARTIAL_EXIT_CODE = 2


@dataclass(frozen=True)
class ScanResult:
    """What a completed ``run_scan`` invocation did.

    ``partial`` is True for the exit-2 partial success: the batch committed, and
    ``stderr`` names the file(s) that failed to ingest. A hard failure never
    yields a ``ScanResult`` — it raises ``ScanError``."""

    binary: str
    target: str
    verb: str = "scan"
    returncode: int = 0
    partial: bool = False
    stderr: str = ""

    def warning(self):
        """A one-line, non-fatal message for a partial run, else ``None``. Host
        adapters prefix it with their own tag and surface it without aborting."""
        if not self.partial:
            return None
        detail = f":\n{self.stderr}" if self.stderr else ""
        return (
            f"`{self.binary} {self.verb}` could not ingest some file(s) of "
            f"{self.target} (exit {self.returncode}); everything parseable was "
            f"stored, continuing{detail}"
        )


def run_scan(binary, db_path, target, *, revalidate=False, force=False, prune=False, timeout=None):
    """Run musefs once for ``target`` (a path or iterable of paths).

    - default: ``<binary> scan <targets...> --db <db_path>`` (additive)
    - ``force``: appends ``--force`` to rescan existing rows from disk
    - ``revalidate``: ``<binary> revalidate <targets...> --db <db_path>``
      with ``prune`` appending ``--prune``

    All targets precede the ``--db`` flag and are scanned under one process
    (one DB open). Creates the DB if absent and fills the structural columns a
    plugin can't compute.

    Exit codes are three-state: ``0`` success, ``PARTIAL_EXIT_CODE`` (2) partial
    success, anything else a hard failure. Returns a ``ScanResult`` for the first
    two — ``result.partial`` distinguishes them, and ``result.warning()`` renders
    the non-fatal message a caller should log before proceeding. Raises
    ``ScanError`` (with ``kind`` in ``"not_found" | "timeout" | "failed"``) on a
    hard failure; the caller formats its own user-facing message from the
    exception attributes."""
    if isinstance(target, (str, os.PathLike)):
        targets = [target]
    else:
        targets = list(target)
    if not targets:
        raise ValueError("run_scan: at least one target is required")
    if revalidate and force:
        raise ValueError("run_scan: force is incompatible with revalidate")
    if prune and not revalidate:
        raise ValueError("run_scan: prune requires revalidate")
    display = str(targets[0]) if len(targets) == 1 else f"{len(targets)} target(s)"
    verb = "revalidate" if revalidate else "scan"
    argv = [binary, verb, *(str(t) for t in targets), "--db", str(db_path)]
    if revalidate and prune:
        argv.append("--prune")
    if force:
        argv.append("--force")
    try:
        result = subprocess.run(argv, capture_output=True, timeout=timeout)
    except FileNotFoundError as exc:
        raise ScanError("not_found", binary=binary, target=display) from exc
    except subprocess.TimeoutExpired as exc:
        raise ScanError("timeout", binary=binary, target=display, timeout=timeout) from exc
    stderr = result.stderr.decode(errors="replace").strip()
    if result.returncode not in (0, PARTIAL_EXIT_CODE):
        raise ScanError(
            "failed",
            binary=binary,
            target=display,
            returncode=result.returncode,
            stderr=stderr,
        )
    return ScanResult(
        binary=binary,
        target=display,
        verb=verb,
        returncode=result.returncode,
        partial=result.returncode == PARTIAL_EXIT_CODE,
        stderr=stderr,
    )
