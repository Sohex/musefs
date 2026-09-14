"""Independent-reader (mutagen) assertions for the #204 contract round trip.

Reads the files synthesized by `cargo test --test contract_emit` from
MUSEFS_INTEROP_DIR and confirms the tags/art that scripts/contract_writer.py
wrote via the python-musefs store survived Python -> DB -> Rust synthesis.
"""

import glob
import os

import mutagen
import mutagen.flac
import mutagen.id3

CONTRACT_TITLE = "Contract Roundtrip Title"
CONTRACT_ARTIST = "Contract Roundtrip Artist"
# Mirrors scripts/contract_writer.py's CONTRACT_ART, keyed by the extension
# contract_emit.rs names each synthesized file with.
CONTRACT_ART = {
    ".flac": (b"\xff\xd8\xff\xe0contract-roundtrip-flac-cover", "image/jpeg"),
    ".mp3": (b"\x89PNG\r\n\x1a\ncontract-roundtrip-mp3-cover", "image/png"),
}
CONTRACT_ART_TYPE = 3
CONTRACT_ART_DESCRIPTION = "front cover"


def _interop_dir() -> str:
    d = os.environ.get("MUSEFS_INTEROP_DIR")
    if not d:
        raise RuntimeError("MUSEFS_INTEROP_DIR must be set (see scripts/contract-roundtrip.sh)")
    return d


def _audio_files() -> list[str]:
    """Sorted synthesized flac/mp3 paths from MUSEFS_INTEROP_DIR.

    Never empty: every test iterates this, and an empty directory would pass
    each of them vacuously.
    """
    d = _interop_dir()
    files = sorted(glob.glob(os.path.join(d, "*.flac")) + glob.glob(os.path.join(d, "*.mp3")))
    assert files, "no synthesized contract files found in MUSEFS_INTEROP_DIR"
    return files


def test_every_scanned_format_was_synthesized() -> None:
    """One file per fixture scripts/contract-roundtrip.sh creates, and nothing
    else — a format the emitter could not name (it falls back to `.bin`) would
    otherwise drop out of every glob below without a failure."""
    extensions = sorted(os.path.splitext(name)[1] for name in os.listdir(_interop_dir()))
    assert extensions == sorted(CONTRACT_ART), f"synthesized files: {extensions}"


def test_python_written_tags_survive_synthesis() -> None:
    """The title/artist written via the python store read back via mutagen."""
    for path in _audio_files():
        f = mutagen.File(path, easy=True)
        assert f is not None, f"mutagen could not open {path}"
        assert f.get("title") == [CONTRACT_TITLE], f"title wrong in {path}"
        assert f.get("artist") == [CONTRACT_ARTIST], f"artist wrong in {path}"


def test_synthesized_files_carry_the_art_that_was_written() -> None:
    """Cover art written via the python store survives into the synthesized file.

    All of it, not just its presence: the bytes, and every field the link
    declares. The mime lives on the ``track_art`` link from schema v4 on and
    synthesis writes that value into the picture block, so a writer that stores
    a link without one produces art whose type is the empty string — which an
    "art is present" check does not notice. Each track gets different art, so a
    picture served from the wrong track fails too.
    """
    for path in _audio_files():
        ext = os.path.splitext(path)[1]
        data, mime = CONTRACT_ART[ext]
        if ext == ".flac":
            pictures = mutagen.flac.FLAC(path).pictures
            assert len(pictures) == 1, f"{path}: {len(pictures)} pictures, one was written"
            pic = pictures[0]
            assert (pic.data, pic.mime, pic.type, pic.desc) == (
                data,
                mime,
                CONTRACT_ART_TYPE,
                CONTRACT_ART_DESCRIPTION,
            ), f"wrong picture in {path}"
            # The writer states no geometry, so every field reads "not stated".
            assert (pic.width, pic.height, pic.depth, pic.colors) == (0, 0, 0, 0), (
                f"{path}: geometry nobody wrote"
            )
        else:
            apics = mutagen.id3.ID3(path).getall("APIC")
            assert len(apics) == 1, f"{path}: {len(apics)} APIC frames, one was written"
            apic = apics[0]
            assert (apic.data, apic.mime, apic.type, apic.desc) == (
                data,
                mime,
                CONTRACT_ART_TYPE,
                CONTRACT_ART_DESCRIPTION,
            ), f"wrong APIC in {path}"
