import subprocess

from .errors import ScanError


def run_scan(binary, db_path, target, *, timeout=None):
    """Run ``<binary> scan <target> --db <db_path>``. Creates the DB if absent
    and fills the structural columns a plugin can't compute. Raises ``ScanError``
    (with ``kind`` in ``"not_found" | "timeout" | "failed"``) on failure; the
    caller formats its own user-facing message from the exception attributes."""
    try:
        result = subprocess.run(
            [binary, "scan", target, "--db", db_path],
            capture_output=True,
            timeout=timeout,
        )
    except FileNotFoundError as exc:
        raise ScanError("not_found", binary=binary, target=target) from exc
    except subprocess.TimeoutExpired as exc:
        raise ScanError("timeout", binary=binary, target=target, timeout=timeout) from exc
    if result.returncode != 0:
        raise ScanError(
            "failed",
            binary=binary,
            target=target,
            returncode=result.returncode,
            stderr=result.stderr.decode(errors="replace").strip(),
        )
