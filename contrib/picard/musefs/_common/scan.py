# GENERATED from python-musefs/src/musefs_common/scan.py — do not edit.
# Run contrib/python-musefs/vendor_to_picard.py after changing the library.
#
import os
import subprocess

from .errors import ScanError


def run_scan(binary, db_path, target, *, revalidate=False, timeout=None):
    """Run ``<binary> scan <target...> --db <db_path> [--revalidate]``. ``target``
    is a single path or an iterable of paths; all targets precede the ``--db``
    flag and are scanned under one process (one DB open). Creates the DB if
    absent and fills the structural columns a plugin can't compute. With
    ``revalidate``, the scanner re-checks stamps, prunes rows whose backing file
    is gone, and GCs orphaned art. Raises ``ScanError`` (with ``kind`` in
    ``"not_found" | "timeout" | "failed"``) on failure; the caller formats its
    own user-facing message from the exception attributes."""
    if isinstance(target, (str, os.PathLike)):
        targets = [target]
    else:
        targets = list(target)
    display = str(targets[0]) if len(targets) == 1 else f"{len(targets)} target(s)"
    argv = [binary, "scan", *(str(t) for t in targets), "--db", str(db_path)]
    if revalidate:
        argv.append("--revalidate")
    try:
        result = subprocess.run(argv, capture_output=True, timeout=timeout)
    except FileNotFoundError as exc:
        raise ScanError("not_found", binary=binary, target=display) from exc
    except subprocess.TimeoutExpired as exc:
        raise ScanError("timeout", binary=binary, target=display, timeout=timeout) from exc
    if result.returncode != 0:
        raise ScanError(
            "failed",
            binary=binary,
            target=display,
            returncode=result.returncode,
            stderr=result.stderr.decode(errors="replace").strip(),
        )
