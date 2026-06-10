"""Mock of Lidarr's metadata server (api.lidarr.audio) for the e2e gate.

Replays captured Komiku artist/album resources so a real Lidarr can add and
monitor the artist with no network dependency. Lidarr is pointed here via its
`metadatasource` Config key; it appends routes like `artist/{mbid}` and
`album/{mbid}` to the configured base (see Lidarr's MetadataRequestBuilder).
"""

from __future__ import annotations

import argparse
import json
import os
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse

FIX = os.path.join(os.path.dirname(__file__), "fixtures", "metadata")


def _load(name):
    with open(os.path.join(FIX, name)) as fh:
        return json.load(fh)


def make_handler():
    artist = _load("artist.json")
    album = _load("album.json")

    class Handler(BaseHTTPRequestHandler):
        def _send(self, obj, code=200):
            body = json.dumps(obj).encode()
            self.send_response(code)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):  # noqa: N802
            path = urlparse(self.path).path
            # strip the /api/v0.4 prefix Lidarr's base URL carries
            route = path.split("/api/v0.4/", 1)[-1] if "/api/v0.4/" in path else path.lstrip("/")
            if route.startswith("artist/"):
                self._send(artist)
            elif route.startswith("album/"):
                self._send(album)
            elif route.startswith("recent/"):
                self._send({"Items": [], "Limited": False})
            elif route.startswith("search"):
                self._send([artist])
            else:
                self._send({"error": f"unmocked route: {route}"}, code=404)

        def log_message(self, *args):  # quiet
            pass

    return Handler


def main(argv=None):
    p = argparse.ArgumentParser()
    p.add_argument("--port", type=int, required=True)
    a = p.parse_args(argv)
    ThreadingHTTPServer(("0.0.0.0", a.port), make_handler()).serve_forever()


if __name__ == "__main__":  # pragma: no cover
    main()
