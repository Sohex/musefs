"""A local mock of the Lidarr REST API for the release smoke.

Returns fixed JSON for exactly the endpoints musefs-lidarr-sync calls on an
AlbumDownload event, so the content assertions are deterministic and need no
real MusicBrainz/Lidarr metadata. ``?albumId=`` query strings are ignored --
the fixture describes a single album.
"""

from __future__ import annotations

import argparse
import json
from http.server import BaseHTTPRequestHandler, HTTPServer
from urllib.parse import urlparse


def build_fixture(*, album_id, artist_id, artist_name, album_title, tracks):
    """Return a {path: response} map. ``tracks`` = [(tf_id, path, title, no), ...]."""
    trackfiles = [
        {"id": tf_id, "path": path, "albumId": album_id, "artistId": artist_id}
        for (tf_id, path, _title, _no) in tracks
    ]
    track_rows = [
        {"trackFileId": tf_id, "title": title, "trackNumber": no}
        for (tf_id, _path, title, no) in tracks
    ]
    return {
        "/api/v1/config/mediamanagement": {"fileDate": "none", "setPermissionsLinux": False},
        "/api/v1/config/metadataprovider": {"writeAudioTags": "no"},
        "/api/v1/trackfile": trackfiles,
        "/api/v1/track": track_rows,
        f"/api/v1/album/{album_id}": {
            "id": album_id,
            "title": album_title,
            "artistId": artist_id,
            "releaseDate": "2020-01-01T00:00:00Z",
            "genres": ["Test"],
            "foreignAlbumId": "00000000-0000-0000-0000-0000000000a1",
        },
        f"/api/v1/artist/{artist_id}": {
            "id": artist_id,
            "artistName": artist_name,
            "foreignArtistId": "00000000-0000-0000-0000-0000000000b2",
        },
    }


def make_handler(fixture):
    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):  # noqa: N802
            path = urlparse(self.path).path
            if path in fixture:
                body = json.dumps(fixture[path]).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.end_headers()
                self.wfile.write(body)
            else:
                self.send_response(404)
                self.end_headers()

        def log_message(self, *args):  # silence
            pass

    return Handler


def main(argv=None):
    p = argparse.ArgumentParser()
    p.add_argument("--port", type=int, required=True)
    p.add_argument("--album-id", type=int, required=True)
    p.add_argument("--artist-id", type=int, required=True)
    p.add_argument("--artist-name", required=True)
    p.add_argument("--album-title", required=True)
    # repeatable: --track TF_ID:PATH:TITLE:NO
    p.add_argument("--track", action="append", default=[])
    a = p.parse_args(argv)
    tracks = []
    for spec in a.track:
        tf_id, path, title, no = spec.split(":", 3)
        tracks.append((int(tf_id), path, title, int(no)))
    fixture = build_fixture(
        album_id=a.album_id,
        artist_id=a.artist_id,
        artist_name=a.artist_name,
        album_title=a.album_title,
        tracks=tracks,
    )
    HTTPServer(("127.0.0.1", a.port), make_handler(fixture)).serve_forever()


if __name__ == "__main__":  # pragma: no cover
    main()
