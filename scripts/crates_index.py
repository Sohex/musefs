"""Check whether a crate version is resolvable from the crates.io sparse index.

Used by the release workflow to (a) skip crates already published — so a
whole-workflow re-run after a partial failure is idempotent — and (b) wait for
index propagation between dependency-ordered publishes (#163).
"""

from __future__ import annotations

import argparse
import json
import urllib.error
import urllib.request


def index_path(name: str) -> str:
    """Return the sparse-index path for ``name`` (crates.io's layout)."""
    n = len(name)
    if n == 1:
        return f"1/{name}"
    if n == 2:
        return f"2/{name}"
    if n == 3:
        return f"3/{name[0]}/{name}"
    return f"{name[0:2]}/{name[2:4]}/{name}"


def _http_fetch(url: str) -> str:
    req = urllib.request.Request(url, headers={"User-Agent": "musefs-release"})
    try:
        with urllib.request.urlopen(req, timeout=15) as resp:
            return resp.read().decode("utf-8")
    except urllib.error.HTTPError as exc:
        if exc.code == 404:
            raise FileNotFoundError(url) from exc
        raise


def is_published(name: str, version: str, *, fetch=_http_fetch) -> bool:
    """True if ``name@version`` appears in the sparse index."""
    url = f"https://index.crates.io/{index_path(name)}"
    try:
        body = fetch(url)
    except FileNotFoundError:
        return False
    for line in body.splitlines():
        line = line.strip()
        if not line:
            continue
        if json.loads(line).get("vers") == version:
            return True
    return False


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("name")
    parser.add_argument("version")
    args = parser.parse_args(argv)
    if is_published(args.name, args.version):
        print(f"{args.name}@{args.version} is in the index.")
        return 0
    print(f"{args.name}@{args.version} not in the index yet.")
    return 3


if __name__ == "__main__":  # pragma: no cover
    raise SystemExit(main())
