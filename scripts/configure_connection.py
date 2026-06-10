"""Register a Custom Script connection in a real Lidarr and fire its Test event.

The Test event makes the REAL Lidarr exec the script, proving connection wiring
and the lowercased-env resolution (Lidarr stores env keys in a StringDictionary
that lowercases them). It carries no Album_Id, so it proves exec only.
"""

from __future__ import annotations

import argparse
import json
import urllib.request


def _req(method, url, api_key, body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method)
    req.add_header("X-Api-Key", api_key)
    req.add_header("Content-Type", "application/json")
    with urllib.request.urlopen(req, timeout=60) as resp:
        text = resp.read().decode()
        return json.loads(text) if text else None


def main(argv=None):
    p = argparse.ArgumentParser()
    p.add_argument("--url", required=True)
    p.add_argument("--api-key", required=True)
    p.add_argument("--script", required=True, help="path to musefs-lidarr-import")
    a = p.parse_args(argv)
    base = a.url.rstrip("/")
    body = {
        "name": "musefs-smoke",
        "implementation": "CustomScript",
        "configContract": "CustomScriptSettings",
        "onReleaseImport": True,
        "fields": [{"name": "path", "value": a.script}],
    }
    _req("POST", f"{base}/api/v1/notification/test", a.api_key, body)
    print("Test event fired; Lidarr execed the script.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
