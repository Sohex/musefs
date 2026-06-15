"""Assert the musefs store received Lidarr's tags and art (no vacuous pass).

Counts distinct tracks carrying a non-empty `artist` tag (and, with --min-art,
distinct tracks linked to cover art). A host/path mismatch that skips every
track leaves 0 — requiring the counts to meet the track count makes that fail
loud instead of passing green.
"""

from __future__ import annotations

import argparse
import sqlite3


def count_artist_tagged_tracks(db_path: str) -> int:
    con = sqlite3.connect(db_path)
    try:
        return con.execute(
            "SELECT COUNT(DISTINCT track_id) FROM tags WHERE key = 'artist' AND value <> ''"
        ).fetchone()[0]
    finally:
        con.close()


def count_arted_tracks(db_path: str) -> int:
    con = sqlite3.connect(db_path)
    try:
        return con.execute("SELECT COUNT(DISTINCT track_id) FROM track_art").fetchone()[0]
    finally:
        con.close()


def main(argv=None) -> int:
    p = argparse.ArgumentParser()
    p.add_argument("--db", required=True)
    p.add_argument("--min-records", type=int, default=1)
    p.add_argument("--min-art", type=int, default=0)
    a = p.parse_args(argv)
    n = count_artist_tagged_tracks(a.db)
    if n < a.min_records:
        print(f"::error::store has {n} artist-tagged tracks, expected >= {a.min_records}")
        return 1
    print(f"store records OK: {n} artist-tagged tracks")
    if a.min_art:
        arted = count_arted_tracks(a.db)
        if arted < a.min_art:
            print(f"::error::store has {arted} arted tracks, expected >= {a.min_art}")
            return 1
        print(f"store art OK: {arted} arted tracks")
    return 0


if __name__ == "__main__":  # pragma: no cover
    raise SystemExit(main())
