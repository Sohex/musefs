#!/usr/bin/env python3
"""Vendor python-musefs into the Picard folder plugin's ``_common`` subpackage.

Picard does not pip-install plugin dependencies, so the shared library is copied
(verbatim, with a generated header) into ``contrib/picard/musefs/_common``. Run
this after any change to ``src/musefs_common``. The Picard test suite's
``test_vendor_sync.py`` fails if the committed copy drifts from canonical.
"""

from pathlib import Path

SRC = Path(__file__).parent / "src" / "musefs_common"
DST = Path(__file__).parent.parent / "picard" / "musefs" / "_common"

HEADER = (
    "# GENERATED from python-musefs/src/musefs_common/{name} — do not edit.\n"
    "# Run contrib/python-musefs/vendor_to_picard.py after changing the library.\n"
    "#\n"
)


def main():
    DST.mkdir(parents=True, exist_ok=True)
    src_names = {p.name for p in SRC.glob("*.py")}
    # Drop vendored files no longer present in the source package.
    for old in DST.glob("*.py"):
        if old.name not in src_names:
            old.unlink()
    for src in sorted(SRC.glob("*.py")):
        header = HEADER.format(name=src.name).encode("utf-8")
        (DST / src.name).write_bytes(header + src.read_bytes())


if __name__ == "__main__":
    main()
