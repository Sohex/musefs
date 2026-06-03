"""Guard: the vendored musefs/_common must match python-musefs byte-for-byte
(after the 3-line generated header). Run vendor_to_picard.py to refresh it."""

from pathlib import Path

CANON = Path(__file__).parents[2] / "python-musefs" / "src" / "musefs_common"
VENDORED = Path(__file__).parents[1] / "musefs" / "_common"


def test_vendored_file_set_matches_canonical():
    canon = {p.name for p in CANON.glob("*.py")}
    vend = {p.name for p in VENDORED.glob("*.py")}
    assert vend == canon, f"vendored set {vend} != canonical {canon}; re-run vendor_to_picard.py"


def test_vendored_bodies_are_byte_identical():
    for src in sorted(CANON.glob("*.py")):
        vend = VENDORED / src.name
        # Drop exactly the 3 generated header lines, compare the rest verbatim.
        body = vend.read_bytes().split(b"\n", 3)[3]
        assert body == src.read_bytes(), f"{src.name} drifted; re-run vendor_to_picard.py"
