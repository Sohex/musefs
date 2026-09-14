"""Write the tags/art that an external writer owns into a scanned musefs DB.

Part of the #204 contract round trip: `musefs scan` has already created the
track geometry; this sets known tags + cover art via the public python-musefs
store API, which the Rust serve path then synthesizes and an independent reader
verifies. The constants below MUST match tests/contract/test_contract_roundtrip.py.
"""

import os
import sys

from musefs_common import realpath_key
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
# The bytes and the mime differ between the two, so a served picture that
# belongs to the other track fails the read-back instead of passing as "some art
# is present". The mime is declared per link, not per blob (#716).
CONTRACT_ART = {
    "track.flac": (b"\xff\xd8\xff\xe0contract-roundtrip-flac-cover", "image/jpeg"),
    "track.mp3": (b"\x89PNG\r\n\x1a\ncontract-roundtrip-mp3-cover", "image/png"),
}
# picture_type 3 == front cover (valid range 0..=20).
CONTRACT_ART_TYPE = 3
CONTRACT_ART_DESCRIPTION = "front cover"


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
            replace_track_art(
                conn, tid, [(art_id, CONTRACT_ART_TYPE, CONTRACT_ART_DESCRIPTION, mime)]
            )
        conn.commit()
    finally:
        conn.close()


if __name__ == "__main__":
    if len(sys.argv) != 3:
        raise SystemExit("usage: python scripts/contract_writer.py <db_path> <backing_dir>")
    main(sys.argv[1], sys.argv[2])
