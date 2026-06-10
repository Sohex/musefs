"""Mock qBittorrent WebUI (v2 API) for the Lidarr e2e gate.

Implements just the endpoints Lidarr's QBittorrentProxyV2 calls, and reports a
single torrent (the grabbed infohash) as finished, with content_path pointing at
the staged download folder. That makes Lidarr's completed-download handling
import it as a NewDownload, firing OnReleaseImport.
"""

from __future__ import annotations

import argparse
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse

CFG = {}  # filled by main(): hash, category, save_path, content_path, name


def make_handler():
    def torrent():
        return {
            "hash": CFG["hash"],
            "name": CFG["name"],
            "size": 30000,
            "progress": 1.0,
            "eta": 0,
            "state": "uploading",  # finished + seeding -> Completed (QBittorrent.cs)
            "category": CFG["category"],
            "save_path": CFG["save_path"],
            "content_path": CFG["content_path"],  # != save_path -> OutputPath set
            "ratio": 1.0,
            "ratio_limit": -2,
            "seeding_time": 1,
            "seeding_time_limit": -2,
            "inactive_seeding_time_limit": -2,
            "last_activity": 0,
        }

    class Handler(BaseHTTPRequestHandler):
        def _text(self, s, ctype="text/plain"):
            data = s.encode()
            self.send_response(200)
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(data)))
            # qBittorrent auth: hand out a session cookie on login.
            if urlparse(self.path).path == "/api/v2/auth/login":
                self.send_header("Set-Cookie", "SID=mocksid; path=/")
            self.end_headers()
            self.wfile.write(data)

        def _json(self, obj):
            self._text(json.dumps(obj), "application/json")

        def _route(self):
            path = urlparse(self.path).path
            if path == "/api/v2/auth/login":
                self._text("Ok.")
            elif path == "/api/v2/app/version":
                self._text("v4.6.0")
            elif path == "/api/v2/app/webapiVersion":
                self._text("2.8.19")
            elif path == "/api/v2/app/preferences":
                self._json({
                    "save_path": CFG["save_path"],
                    "max_ratio_enabled": False,
                    "max_ratio": -1,
                    "max_seeding_time_enabled": False,
                    "max_seeding_time": -1,
                    "queueing_enabled": False,
                    "dht": True,
                })
            elif path == "/api/v2/torrents/categories":
                self._json({CFG["category"]: {"name": CFG["category"], "savePath": ""}})
            elif path == "/api/v2/torrents/info":
                self._json([torrent()])
            elif path == "/api/v2/torrents/properties":
                self._json({"hash": CFG["hash"], "save_path": CFG["save_path"], "seeding_time": 1})
            elif path == "/api/v2/torrents/files":
                self._json([{"name": CFG["name"]}])
            else:
                # createCategory/setCategory/add/delete/setShareLimits/topPrio/...
                self._text("Ok.")

        def do_GET(self):  # noqa: N802
            self._route()

        def do_POST(self):  # noqa: N802
            # drain body so the client isn't left hanging
            length = int(self.headers.get("Content-Length", 0) or 0)
            if length:
                self.rfile.read(length)
            self._route()

        def log_message(self, *args):
            pass

    return Handler


def main(argv=None):
    p = argparse.ArgumentParser()
    p.add_argument("--port", type=int, required=True)
    p.add_argument("--hash", required=True)
    p.add_argument("--category", default="music")
    p.add_argument("--save-path", required=True)
    p.add_argument("--content-path", required=True)
    p.add_argument("--name", default="Komiku - The adventure goes on, vol.1")
    a = p.parse_args(argv)
    CFG.update(
        hash=a.hash.lower(),
        category=a.category,
        save_path=a.save_path,
        content_path=a.content_path,
        name=a.name,
    )
    ThreadingHTTPServer(("0.0.0.0", a.port), make_handler()).serve_forever()


if __name__ == "__main__":  # pragma: no cover
    main()
