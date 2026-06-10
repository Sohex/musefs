"""Write the tags/art that an external writer owns into a scanned musefs DB.

Part of the #204 contract round trip: `musefs scan` has already created the
track geometry; this sets known tags + cover art via the public python-musefs
store API, which the Rust serve path then synthesizes and an independent reader
verifies. The constants below MUST match tests/contract/test_contract_roundtrip.py.
"""

import sys

from musefs_common.store import (
    connect,
    replace_tags,
    replace_track_art,
    upsert_art,
)

CONTRACT_TITLE = "Contract Roundtrip Title"
CONTRACT_ARTIST = "Contract Roundtrip Artist"
# A small payload; content is opaque to the contract (we only assert art exists).
CONTRACT_ART = b"\xff\xd8\xff\xe0contract-roundtrip-cover-bytes"


def main(db_path):
    conn = connect(db_path)
    try:
        track_ids = [row[0] for row in conn.execute("SELECT id FROM tracks").fetchall()]
        if not track_ids:
            raise SystemExit("contract_writer: no tracks in DB (did scan run?)")
        for tid in track_ids:
            replace_tags(conn, tid, [("title", CONTRACT_TITLE), ("artist", CONTRACT_ARTIST)])
            art_id = upsert_art(conn, CONTRACT_ART, "image/jpeg")
            # picture_type 3 == front cover (valid range 0..=20).
            replace_track_art(conn, tid, [(art_id, 3, "front cover")])
        conn.commit()
    finally:
        conn.close()


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit("usage: python scripts/contract_writer.py <db_path>")
    main(sys.argv[1])
