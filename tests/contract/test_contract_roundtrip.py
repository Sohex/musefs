"""Independent-reader (mutagen) assertions for the #204 contract round trip.

Reads the files synthesized by `cargo test --test contract_emit` from
MUSEFS_INTEROP_DIR and confirms the tags/art that scripts/contract_writer.py
wrote via the python-musefs store survived Python -> DB -> Rust synthesis.
"""

import glob
import os

import mutagen

CONTRACT_TITLE = "Contract Roundtrip Title"
CONTRACT_ARTIST = "Contract Roundtrip Artist"


def _audio_files() -> list[str]:
    """Sorted synthesized flac/mp3 paths from MUSEFS_INTEROP_DIR."""
    d = os.environ.get("MUSEFS_INTEROP_DIR")
    if not d:
        raise RuntimeError("MUSEFS_INTEROP_DIR must be set (see scripts/contract-roundtrip.sh)")
    return sorted(glob.glob(os.path.join(d, "*.flac")) + glob.glob(os.path.join(d, "*.mp3")))


def test_python_written_tags_survive_synthesis() -> None:
    """The title/artist written via the python store read back via mutagen."""
    files = _audio_files()
    assert files, "no synthesized contract files found in MUSEFS_INTEROP_DIR"
    for path in files:
        f = mutagen.File(path, easy=True)
        assert f is not None, f"mutagen could not open {path}"
        assert f.get("title", [None])[0] == CONTRACT_TITLE, f"title wrong in {path}"
        assert f.get("artist", [None])[0] == CONTRACT_ARTIST, f"artist wrong in {path}"


def test_synthesized_files_carry_embedded_art() -> None:
    """Cover art written via the python store survives into the synthesized file."""
    for path in _audio_files():
        f = mutagen.File(path)
        has_art = bool(getattr(f, "pictures", None)) or (
            f.tags is not None and any(str(k).startswith("APIC") for k in f.tags.keys())
        )
        assert has_art, f"no embedded art in {path}"
