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
# Mirrors scripts/contract_writer.py's CONTRACT_ART, keyed by the backing file
# name contract_emit.rs gives each synthesized file.
CONTRACT_ART = {
    "track.flac": (
        b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x02\x80\x00\x00\x01\xe0"
        b"\x08\x03\x00\x00\x00\x02\x0f,\xd6contract-roundtrip-flac-cover",
        "image/png",
    ),
    "unstated.flac": (b"\xff\xd8\xff\xe0contract-roundtrip-unstated-cover", "image/jpeg"),
    "track.mp3": (b"GIF89acontract-roundtrip-mp3-cover", "image/gif"),
}
CONTRACT_ART_TYPE = 3
CONTRACT_ART_DESCRIPTION = "front cover"
# Mirrors scripts/contract_writer.py's CONTRACT_GEOMETRY: (width, height, depth,
# colors) for the tracks linked with a six-field row.
CONTRACT_GEOMETRY = {"track.flac": (640, 480, 8, 256)}


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


def _flac_picture(name: str) -> mutagen.flac.Picture:
    """The one PICTURE block of the synthesized file for backing file ``name``."""
    path = os.path.join(_interop_dir(), name)
    pictures = mutagen.flac.FLAC(path).pictures
    assert len(pictures) == 1, f"{path}: {len(pictures)} pictures, one was written"
    return pictures[0]


def test_every_scanned_file_was_synthesized() -> None:
    """One file per fixture scripts/contract-roundtrip.sh creates, and nothing
    else — a track the emitter skipped would otherwise drop out of every glob
    below without a failure."""
    assert sorted(os.listdir(_interop_dir())) == sorted(CONTRACT_ART)


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
    picture served from the wrong track fails too. The geometry a FLAC picture
    also carries has its own tests below.
    """
    for path in _audio_files():
        data, mime = CONTRACT_ART[os.path.basename(path)]
        if path.endswith(".flac"):
            pic = _flac_picture(os.path.basename(path))
            assert (pic.data, pic.mime, pic.type, pic.desc) == (
                data,
                mime,
                CONTRACT_ART_TYPE,
                CONTRACT_ART_DESCRIPTION,
            ), f"wrong picture in {path}"
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


def test_stated_geometry_is_served_as_written() -> None:
    """The width and height a six-field row states, and the depth and colour
    count on the same link, are what the served PICTURE block carries (#737).

    The stored rows are tested in python-musefs; this is the served bytes, read
    by a parser musefs does not own. ID3 ``APIC`` has no geometry fields, so
    only FLAC can show it."""
    for name, geometry in CONTRACT_GEOMETRY.items():
        pic = _flac_picture(name)
        assert (pic.width, pic.height, pic.depth, pic.colors) == geometry, name


def test_unstated_geometry_is_served_as_zero() -> None:
    """A four-field row states no geometry: width and height stay NULL, depth
    and colours keep their 0 default, and the PICTURE block spells every one of
    them 0, "not stated"."""
    unstated = [n for n in CONTRACT_ART if n.endswith(".flac") and n not in CONTRACT_GEOMETRY]
    assert unstated, "no FLAC fixture is linked with a four-field row"
    for name in unstated:
        pic = _flac_picture(name)
        assert (pic.width, pic.height, pic.depth, pic.colors) == (0, 0, 0, 0), name
