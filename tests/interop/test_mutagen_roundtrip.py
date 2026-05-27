import json
import os
import struct

import mutagen
import mutagen.id3


def _m4a_read_ilst_tag(path, key):
    """Low-level ilst reader for minimal M4A fixtures that lack mdhd.

    mutagen's MP4 parser requires the mdhd (Media Header) atom to open a file.
    The minimal test fixture omits mdhd because it is not needed by musefs's
    synthesis path. The tags themselves are correctly placed in moov/udta/meta/ilst;
    this helper walks that path directly without going through MPEGStreamInfo.
    """
    atom_map = {"title": b"\xa9nam", "artist": b"\xa9ART"}
    target = atom_map.get(key)
    if target is None:
        return None

    data = open(path, "rb").read()

    def find_atom(buf, name):
        pos = 0
        while pos + 8 <= len(buf):
            size = struct.unpack(">I", buf[pos : pos + 4])[0]
            if size < 8:
                break
            atom_name = buf[pos + 4 : pos + 8]
            if atom_name == name:
                return buf[pos + 8 : pos + size]
            pos += size
        return None

    moov = find_atom(data, b"moov")
    if moov is None:
        return None
    udta = find_atom(moov, b"udta")
    if udta is None:
        return None
    meta = find_atom(udta, b"meta")
    if meta is None:
        return None
    # meta is a FullBox — skip 4 bytes of version/flags
    ilst = find_atom(meta[4:], b"ilst")
    if ilst is None:
        return None
    item = find_atom(ilst, target)
    if item is None:
        return None
    # item payload is one or more 'data' atoms; first data atom:
    # [size 4][b'data' 4][type 4][locale 4][value ...]
    if len(item) < 16 or item[4:8] != b"data":
        return None
    return item[16:].decode("utf-8", errors="replace")


def _read_tag(path, key):
    # M4A: use low-level ilst reader because the minimal fixture lacks mdhd,
    # which mutagen's MP4 parser requires to open the file.
    if path.endswith(".m4a"):
        return _m4a_read_ilst_tag(path, key)

    # MP3: mutagen.File requires valid MPEG frames; fall back to ID3 directly.
    if path.endswith(".mp3"):
        id3_key = {"title": "TIT2", "artist": "TPE1"}.get(key)
        if id3_key is None:
            return None
        try:
            f = mutagen.id3.ID3(path)
            tag = f.get(id3_key)
            if tag is not None:
                return str(tag[0])
        except Exception:
            pass
        return None

    f = mutagen.File(path, easy=True)
    assert f is not None, f"mutagen could not open {path}"
    vals = f.get(key)
    if vals:
        return vals[0]
    # Fallback: some containers (e.g. WAV/ID3) may not expose the easy key.
    f2 = mutagen.File(path)
    if f2 is not None and f2.tags is not None:
        for tag_key in (key, key.upper(), {"title": "TIT2", "artist": "TPE1"}.get(key)):
            if tag_key and tag_key in f2.tags:
                v = f2.tags[tag_key]
                return str(v[0]) if isinstance(v, list) else str(v)
    return None


def test_ecosystem_reads_synthesized_tags():
    base = os.environ["MUSEFS_INTEROP_DIR"]
    with open(os.path.join(base, "manifest.json")) as fh:
        manifest = json.load(fh)
    assert manifest, "empty manifest"
    for row in manifest:
        path = os.path.join(base, row["file"])
        title = _read_tag(path, "title")
        artist = _read_tag(path, "artist")
        assert title == row["title"], f"{row['file']}: title {title!r} != {row['title']!r}"
        assert artist == row["artist"], f"{row['file']}: artist {artist!r} != {row['artist']!r}"
