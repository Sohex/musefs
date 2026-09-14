"""Write the tags/art that an external writer owns into a scanned musefs DB.

Part of the #204 contract round trip: `musefs scan` has already created the
track geometry; this sets known tags + cover art via the public python-musefs
store API, which the Rust serve path then synthesizes and an independent reader
verifies. The constants below MUST match tests/contract/test_contract_roundtrip.py.
"""

import os
import sys

from musefs_common import image_dimensions, realpath_key
from musefs_common.store import (
    connect,
    replace_tags,
    replace_track_art,
    track_id_for_path,
    upsert_art,
)

CONTRACT_TITLE = "Contract Roundtrip Title"
CONTRACT_ARTIST = "Contract Roundtrip Artist"
# Art per backing file, keyed by the name scripts/contract-roundtrip.sh gives it.
# The bytes and the mime differ between all three, so a served picture that
# belongs to another track fails the read-back instead of passing as "some art
# is present". The mime is declared per link, not per blob (#716).
CONTRACT_ART = {
    # A PNG whose IHDR chunk states 640x480, 8-bit indexed colour.
    "track.flac": (
        b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x02\x80\x00\x00\x01\xe0"
        b"\x08\x03\x00\x00\x00\x02\x0f,\xd6contract-roundtrip-flac-cover",
        "image/png",
    ),
    "unstated.flac": (b"\xff\xd8\xff\xe0contract-roundtrip-unstated-cover", "image/jpeg"),
    "track.mp3": (b"GIF89acontract-roundtrip-mp3-cover", "image/gif"),
}
# picture_type 3 == front cover (valid range 0..=20).
CONTRACT_ART_TYPE = 3
CONTRACT_ART_DESCRIPTION = "front cover"
# (width, height, depth, colors) linked with a six-field row, per backing file.
# Every track not named here is linked with a four-field row, which states none.
CONTRACT_GEOMETRY = {"track.flac": (640, 480, 8, 256)}


def _art_row(name, art_id, mime, data):
    """The ``replace_track_art`` row for one track, shaped as a plugin shapes it."""
    if name not in CONTRACT_GEOMETRY:
        return (art_id, CONTRACT_ART_TYPE, CONTRACT_ART_DESCRIPTION, mime)
    width, height, _, _ = CONTRACT_GEOMETRY[name]
    # The dimensions come from the image's own header, as `sync_one` takes them
    # (#737), so the header reader is inside the round trip too.
    if image_dimensions(data) != (width, height):
        raise SystemExit(
            f"contract_writer: {name}'s art header states {image_dimensions(data)}, "
            f"not {(width, height)}"
        )
    return (art_id, CONTRACT_ART_TYPE, CONTRACT_ART_DESCRIPTION, mime, width, height)


def main(db_path, backing_dir):
    conn = connect(db_path)
    try:
        (count,) = conn.execute("SELECT COUNT(*) FROM tracks").fetchone()
        if count != len(CONTRACT_ART):
            raise SystemExit(
                f"contract_writer: expected {len(CONTRACT_ART)} scanned tracks, found {count}"
            )
        for name, (data, mime) in CONTRACT_ART.items():
            # Resolve each track the way a plugin does, from the file on disk,
            # so the path-key encoding is inside the round trip too.
            tid = track_id_for_path(conn, realpath_key(os.path.join(backing_dir, name)))
            if tid is None:
                raise SystemExit(f"contract_writer: no track for {name} (did scan run?)")
            replace_tags(conn, tid, [("title", CONTRACT_TITLE), ("artist", CONTRACT_ARTIST)])
            art_id = upsert_art(conn, data)
            replace_track_art(conn, tid, [_art_row(name, art_id, mime, data)])
            if name in CONTRACT_GEOMETRY:
                # A replace_track_art row has no depth or colour count (no
                # adapter knows them), but both are link columns an external
                # writer owns, so set them the way any other writer would.
                _, _, depth, colors = CONTRACT_GEOMETRY[name]
                conn.execute(
                    "UPDATE track_art SET depth = ?, colors = ? WHERE track_id = ?",
                    (depth, colors, tid),
                )
        conn.commit()
    finally:
        conn.close()


if __name__ == "__main__":
    if len(sys.argv) != 3:
        raise SystemExit("usage: python scripts/contract_writer.py <db_path> <backing_dir>")
    main(sys.argv[1], sys.argv[2])
