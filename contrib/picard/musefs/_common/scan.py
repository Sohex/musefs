# GENERATED from python-musefs/src/musefs_common/scan.py — do not edit.
# Run contrib/python-musefs/vendor_to_picard.py after changing the library.
#
import os
import subprocess

from .errors import ScanError


def run_scan(binary, db_path, target, *, revalidate=False, force=False, prune=False, timeout=None):
    """Run musefs once for ``target`` (a path or iterable of paths).

    - default: ``<binary> scan <targets...> --db <db_path>`` (additive)
    - ``force``: appends ``--force`` to rescan existing rows from disk
    - ``revalidate``: ``<binary> revalidate <targets...> --db <db_path>``
      with ``prune`` appending ``--prune``

    All targets precede the ``--db`` flag and are scanned under one process
    (one DB open). Creates the DB if absent and fills the structural columns a
    plugin can't compute. Raises ``ScanError`` (with ``kind`` in
    ``"not_found" | "timeout" | "failed"``) on failure; the caller formats its
    own user-facing message from the exception attributes."""
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
    if revalidate:
        argv = [binary, "revalidate", *(str(t) for t in targets), "--db", str(db_path)]
        if prune:
            argv.append("--prune")
    else:
        argv = [binary, "scan", *(str(t) for t in targets), "--db", str(db_path)]
        if force:
            argv.append("--force")
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
