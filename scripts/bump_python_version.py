#!/usr/bin/env python3
"""Bump the shared version of all contrib/ Python packages.

Single source of truth for the unified Python package version (decoupled from
the Rust workspace version). Rewrites every pyproject.toml version, the
__version__ in the packages that carry one, the python-musefs dependency floor
in the dependents, and re-vendors python-musefs into the Picard plugin so the
vendored copy's __version__ stays in lockstep. Does not commit or tag.

Usage: python scripts/bump_python_version.py <version>
"""

from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent

PYPROJECTS = [
    "contrib/python-musefs/pyproject.toml",
    "contrib/beets/pyproject.toml",
    "contrib/lidarr/pyproject.toml",
    "contrib/picard/pyproject.toml",
]
INIT_FILES = [
    "contrib/python-musefs/src/musefs_common/__init__.py",
    "contrib/lidarr/src/musefs_lidarr/__init__.py",
]
DEPENDENTS = [
    "contrib/beets/pyproject.toml",
    "contrib/lidarr/pyproject.toml",
]
VENDOR_SCRIPT = "contrib/python-musefs/vendor_to_picard.py"

_VERSION_RE = re.compile(r'(?m)^version = "[^"]*"')
_INIT_VERSION_RE = re.compile(r'(?m)^__version__ = "[^"]*"')
_DEP_FLOOR_RE = re.compile(r"python-musefs>=[^\"]*")
_PEP440_RE = re.compile(r"^[0-9]+(\.[0-9]+)*((?:a|b|c|rc)[0-9]+|\.[a-z0-9.]+)?$")


def is_valid_version(version: str) -> bool:
    """Return True if ``version`` is an accepted release version string."""
    return bool(_PEP440_RE.match(version))


def set_project_version(text: str, version: str) -> str:
    """Replace the ``version`` key within the ``[project]`` table.

    Scoped to the ``[project]`` section (up to the next table header) via
    ``_VERSION_RE`` so a ``version`` key in any other table is never touched.
    Raises ``ValueError`` if the ``[project]`` table or its version line is
    missing.
    """
    header = re.search(r"(?m)^\[project\]\s*$", text)
    if header is None:
        raise ValueError("no [project] table found")
    start = header.end()
    nxt = re.search(r"(?m)^\[", text[start:])
    end = start + nxt.start() if nxt else len(text)
    section, n = _VERSION_RE.subn(f'version = "{version}"', text[start:end], count=1)
    if n != 1:
        raise ValueError("no [project] version line found")
    return text[:start] + section + text[end:]


def set_init_version(text: str, version: str) -> str:
    """Replace the module-level ``__version__`` string via ``_INIT_VERSION_RE``.

    Raises ``ValueError`` if no ``__version__`` line is present.
    """
    new, n = _INIT_VERSION_RE.subn(f'__version__ = "{version}"', text, count=1)
    if n != 1:
        raise ValueError("no __version__ line found")
    return new


def set_dep_floor(text: str, version: str) -> str:
    """Bump every ``python-musefs>=`` dependency floor to ``version``.

    Replaces all matches of ``_DEP_FLOOR_RE`` (the pin appears once per file
    today, but all occurrences are updated). Raises ``ValueError`` if none are
    present.
    """
    new, n = _DEP_FLOOR_RE.subn(f"python-musefs>={version}", text)
    if n < 1:
        raise ValueError("no python-musefs>= dependency found")
    return new


def bump(version: str, root: Path = REPO_ROOT, run_vendor: bool = True) -> None:
    for rel in PYPROJECTS:
        p = root / rel
        p.write_text(set_project_version(p.read_text(), version))
    for rel in INIT_FILES:
        p = root / rel
        p.write_text(set_init_version(p.read_text(), version))
    for rel in DEPENDENTS:
        p = root / rel
        p.write_text(set_dep_floor(p.read_text(), version))
    if run_vendor:
        subprocess.run([sys.executable, str(root / VENDOR_SCRIPT)], check=True)


def main(argv: list[str]) -> int:
    if len(argv) != 1 or not is_valid_version(argv[0]):
        print("usage: bump_python_version.py <version>", file=sys.stderr)
        return 2
    try:
        bump(argv[0])
    except (ValueError, OSError, subprocess.CalledProcessError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1
    print(f"bumped contrib/ Python packages to {argv[0]}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
