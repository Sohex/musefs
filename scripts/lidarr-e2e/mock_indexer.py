"""Mock Torznab indexer: returns one grabbable release for the Komiku album.

Lidarr is configured with a Torznab indexer pointing here. On an album search it
gets back a single release whose magnet carries a fixed btih; Lidarr uses that
infohash as the download id and hands the magnet to the (mock) download client.
"""

from __future__ import annotations

import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

# Fixed infohash the qBittorrent mock will report complete (lowercase hex).
BTIH = "0123456789abcdef0123456789abcdef01234567"
TITLE = "Komiku - The adventure goes on, vol.1 [FLAC]"

CAPS = """<?xml version="1.0" encoding="UTF-8"?>
<caps>
  <server title="musefs-mock-indexer"/>
  <limits max="100" default="50"/>
  <searching>
    <search available="yes" supportedParams="q"/>
    <music-search available="yes" supportedParams="q,artist,album"/>
  </searching>
  <categories>
    <category id="3000" name="Audio">
      <subcat id="3040" name="Audio/Lossless"/>
    </category>
  </categories>
</caps>
"""


def _feed():
    # `&` must be XML-escaped inside the document or Lidarr's RSS parser rejects it.
    magnet = f"magnet:?xt=urn:btih:{BTIH}&amp;dn=Komiku"
    return f"""<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:torznab="http://torznab.com/schemas/2015/feed">
  <channel>
    <title>musefs-mock-indexer</title>
    <item>
      <title>{TITLE}</title>
      <guid>musefs-komiku-0001</guid>
      <pubDate>Mon, 28 Jun 2021 00:00:00 +0000</pubDate>
      <size>30000</size>
      <link>{magnet}</link>
      <enclosure url="{magnet}" length="30000" type="application/x-bittorrent"/>
      <torznab:attr name="category" value="3040"/>
      <torznab:attr name="size" value="30000"/>
      <torznab:attr name="seeders" value="20"/>
      <torznab:attr name="peers" value="20"/>
    </item>
  </channel>
</rss>
"""


def make_handler():
    class Handler(BaseHTTPRequestHandler):
        def _xml(self, body):
            data = body.encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/xml")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def do_GET(self):  # noqa: N802
            q = parse_qs(urlparse(self.path).query)
            t = (q.get("t") or [""])[0]
            if t == "caps":
                self._xml(CAPS)
            else:  # search / music / album / tvsearch -> the one release
                self._xml(_feed())

        def log_message(self, *args):
            pass

    return Handler


def main(argv=None):
    p = argparse.ArgumentParser()
    p.add_argument("--port", type=int, required=True)
    a = p.parse_args(argv)
    ThreadingHTTPServer(("0.0.0.0", a.port), make_handler()).serve_forever()


if __name__ == "__main__":  # pragma: no cover
    main()
