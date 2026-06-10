# Lidarr Release Gate Implementation Plan (#224)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the prose-only Lidarr release gate with an automated CI job that (a) boots a real Lidarr and fires its Test event to prove the real process execs the installed Custom Script with lowercased env vars, and (b) drives the integration end-to-end against a local mock Lidarr API — asserting tag-writes, the symlink, unchanged backing bytes, and served-mount tags — wired as a required gate before the Python publish.

**Architecture:** A real Lidarr container proves only the **Test-event exec path** (it cannot deterministically import synthetic files — its MusicBrainz matcher rejects them). The **content** leg runs against a **local mock Lidarr API** (a stdlib `http.server` returning fixed JSON for the endpoints `musefs-lidarr-sync` calls), so it is deterministic and network-free. The fiddly pure logic (env builders, fixture JSON, ffprobe-tag parse, byte equality, store query) is extracted into unit-tested Python helpers under `scripts/`; the orchestration is one POSIX harness `scripts/lidarr-smoke.sh`. A reusable workflow `lidarr-smoke.yml` runs it; `release-python.yml` gates `publish` on it.

**Tech Stack:** GitHub Actions, POSIX shell, Docker (`linuxserver/lidarr`), Python 3 (stdlib `http.server`/`sqlite3`/`urllib`), pytest, `ffmpeg`/`ffprobe`, fuse3.

This plan is Component 2 of the release-process hardening spec:
`docs/superpowers/specs/2026-06-10-release-process-hardening-design.md`.

> **Honesty note for the executor:** the *pure helpers* (Tasks 1–4) are fully
> unit-tested locally. The *full smoke* (Tasks 5–7) needs Docker + `/dev/fuse`
> and **cannot be proven on a dev box without them**; its acceptance evidence is
> a green `workflow_dispatch` run (Task 7). Do not claim the integration works
> until that run is green.

---

## Background the executor needs (verified against the code)

- Two console scripts (`contrib/lidarr/pyproject.toml:32-33`):
  - `musefs-lidarr-import` (`cli_import.py`): for a non-Test event it **requires**
    `Lidarr_SourcePath` + `Lidarr_DestinationPath` (`import_link.py:parse_import_env`)
    and makes a **symlink**. It never calls the API and never writes tags.
  - `musefs-lidarr-sync` (`cli_sync.py`): on an `AlbumDownload` event it reads
    `Lidarr_Album_Id`/`Lidarr_AddedTrackPaths`, runs preflight, queries the
    Lidarr API, autoscans the paths via `MUSEFS_BIN`, and writes tags to the
    store. **This is the tag-writer.**
- `musefs-lidarr-sync`'s AlbumDownload path calls exactly these endpoints
  (`api.py` + `sync.py:collect_event_payloads`):
  - `GET /api/v1/config/mediamanagement` → preflight needs `fileDate == "none"`,
    `setPermissionsLinux` falsy.
  - `GET /api/v1/config/metadataprovider` → preflight needs `writeAudioTags == "no"`.
  - `GET /api/v1/trackfile?albumId=<id>` → `[{"id", "path", "albumId", "artistId"}, …]`
  - `GET /api/v1/track?albumId=<id>` → `[{"trackFileId", "title", "trackNumber"}, …]`
  - `GET /api/v1/album/<id>` → `{"id", "title", "artistId", "releaseDate", "genres", "foreignAlbumId"}`
  - `GET /api/v1/artist/<id>` → `{"id", "artistName", "foreignArtistId"}`
- `mapping.records_for_paths` links: a path matches a trackfile by `realpath_key`;
  the trackfile's `id` links to tracks by `trackFileId`; `albumId`/`artistId`
  key the album/artist. To emit a non-empty `artist` tag, the artist must have
  `artistName` (`mapping.build_pairs:44-46`, lowercase key `artist`).
- musefs store schema (`musefs-db/src/schema.rs`): `tags(track_id, key, value,
  ordinal, value_blob)`. There is no `artist` column — tags are key/value rows.
- musefs CLI (`musefs-cli/src/lib.rs`): mount is `musefs mount <mountpoint>
  --db <db>`; scan is `musefs scan <targets…> --db <db>` (both take `--db` as a
  flag; the path is positional).
- Because the mock API and the `-sync` process both run **on the host** (only
  the real Lidarr runs in the container, and it touches no paths), the
  host/container path-namespace problem does not arise for the content leg.

---

## File Structure

- Create `scripts/lidarr_smoke_lib.py` — pure: `build_album_download_env`, `build_import_env`, `parse_ffprobe_tags`, `sha256_file`, `assert_bytes_unchanged`.
- Create `scripts/test_lidarr_smoke_lib.py` — pytest.
- Create `scripts/mock_lidarr.py` — `build_fixture(...)` (pure, returns path→JSON map) + an `http.server` runner.
- Create `scripts/test_mock_lidarr.py` — pytest for `build_fixture`.
- Create `scripts/store_assert.py` — assert the store received tags.
- Create `scripts/test_store_assert.py` — pytest against a temp sqlite.
- Create `scripts/configure_connection.py` — register the Custom Script connection + fire the Test event (real Lidarr).
- Create `scripts/lidarr-smoke.sh` — the integration harness.
- Create `.github/workflows/lidarr-smoke.yml` — reusable + dispatchable workflow.
- Modify `.github/workflows/release-python.yml` — add a `lidarr-smoke` job, add it to `publish.needs`.
- Modify `.github/workflows/ci.yml` — add a `lidarr-smoke` job (PRs touching the Lidarr surface) + a `changes` output + run the helper unit tests.

---

## Task 1: Pure env builders (TDD)

**Files:**
- Create: `scripts/lidarr_smoke_lib.py`
- Test: `scripts/test_lidarr_smoke_lib.py`

- [ ] **Step 1: Write the failing tests**

Create `scripts/test_lidarr_smoke_lib.py`:

```python
from lidarr_smoke_lib import build_album_download_env, build_import_env


def test_build_album_download_env_for_sync():
    env = build_album_download_env(
        album_id=34,
        track_paths=["/music/Artist/Album/01.flac", "/music/Artist/Album/02.flac"],
        db_path="/work/store.db",
        lidarr_url="http://127.0.0.1:9999",
        api_key="KEY",
        musefs_bin="/usr/local/bin/musefs",
    )
    assert env["Lidarr_EventType"] == "AlbumDownload"
    assert env["Lidarr_Album_Id"] == "34"
    assert env["Lidarr_AddedTrackPaths"] == "/music/Artist/Album/01.flac|/music/Artist/Album/02.flac"
    assert env["MUSEFS_DB"] == "/work/store.db"
    assert env["MUSEFS_LIDARR_URL"] == "http://127.0.0.1:9999"
    assert env["MUSEFS_LIDARR_API_KEY"] == "KEY"
    assert env["MUSEFS_BIN"] == "/usr/local/bin/musefs"


def test_build_import_env_for_symlink():
    env = build_import_env(source="/music/Artist/Album/01.flac", destination="/links/01.flac")
    assert env["Lidarr_EventType"] == "Download"
    assert env["Lidarr_SourcePath"] == "/music/Artist/Album/01.flac"
    assert env["Lidarr_DestinationPath"] == "/links/01.flac"
```

- [ ] **Step 2: Run to verify failure**

Run: `python -m pytest scripts/test_lidarr_smoke_lib.py -v`
Expected: FAIL — `ModuleNotFoundError: No module named 'lidarr_smoke_lib'`.

- [ ] **Step 3: Write the implementation**

Create `scripts/lidarr_smoke_lib.py`:

```python
"""Pure helpers for the Lidarr real-instance smoke (scripts/lidarr-smoke.sh).

Kept free of Docker/network so the regression-prone bits are unit-tested.
"""

from __future__ import annotations

import hashlib
import json


def build_album_download_env(
    *,
    album_id: int,
    track_paths: list[str],
    db_path: str,
    lidarr_url: str,
    api_key: str,
    musefs_bin: str,
) -> dict[str, str]:
    """Env Lidarr would pass for an AlbumDownload import (drives musefs-lidarr-sync).

    ``track_paths`` must equal the mock's ``trackfile.path`` values
    (realpath-compared by mapping.match_track_file).
    """
    return {
        "Lidarr_EventType": "AlbumDownload",
        "Lidarr_Album_Id": str(album_id),
        "Lidarr_AddedTrackPaths": "|".join(track_paths),
        "MUSEFS_DB": db_path,
        "MUSEFS_LIDARR_URL": lidarr_url,
        "MUSEFS_LIDARR_API_KEY": api_key,
        "MUSEFS_BIN": musefs_bin,
    }


def build_import_env(*, source: str, destination: str) -> dict[str, str]:
    """Env for the symlink path (drives musefs-lidarr-import)."""
    return {
        "Lidarr_EventType": "Download",
        "Lidarr_SourcePath": source,
        "Lidarr_DestinationPath": destination,
    }


def parse_ffprobe_tags(ffprobe_json: str) -> dict[str, str]:
    """Extract the format-level tag map from `ffprobe -show_format -of json`."""
    data = json.loads(ffprobe_json)
    tags = data.get("format", {}).get("tags", {})
    return {str(k).lower(): str(v) for k, v in tags.items()}


def sha256_file(path: str) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(65536), b""):
            h.update(chunk)
    return h.hexdigest()


def assert_bytes_unchanged(before: dict[str, str], after: dict[str, str]) -> None:
    """Raise AssertionError if any file's sha256 changed (cardinal invariant)."""
    if before != after:
        changed = [k for k in before if before.get(k) != after.get(k)]
        raise AssertionError(f"backing audio bytes changed for: {changed}")
```

- [ ] **Step 4: Run to verify pass**

Run: `python -m pytest scripts/test_lidarr_smoke_lib.py -v`
Expected: PASS (2 passed).

- [ ] **Step 5: Commit**

```bash
git add scripts/lidarr_smoke_lib.py scripts/test_lidarr_smoke_lib.py
git commit -m "feat(lidarr-smoke): pure env builders for sync and import (#224)"
```

---

## Task 2: Pure helpers — ffprobe tags + byte equality (TDD)

**Files:**
- Modify: `scripts/test_lidarr_smoke_lib.py`

- [ ] **Step 1: Append the tests**

Append to `scripts/test_lidarr_smoke_lib.py`:

```python
import pytest

from lidarr_smoke_lib import assert_bytes_unchanged, parse_ffprobe_tags, sha256_file


def test_parse_ffprobe_tags_lowercases_keys():
    payload = '{"format": {"tags": {"ARTIST": "Alice", "album": "Demo"}}}'
    assert parse_ffprobe_tags(payload) == {"artist": "Alice", "album": "Demo"}


def test_parse_ffprobe_tags_empty_when_no_tags():
    assert parse_ffprobe_tags('{"format": {}}') == {}


def test_sha256_file_roundtrip(tmp_path):
    p = tmp_path / "a.bin"
    p.write_bytes(b"hello")
    assert sha256_file(str(p)) == (
        "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
    )


def test_assert_bytes_unchanged_passes_when_equal():
    assert_bytes_unchanged({"a": "x"}, {"a": "x"})


def test_assert_bytes_unchanged_raises_on_change():
    with pytest.raises(AssertionError, match="a.flac"):
        assert_bytes_unchanged({"a.flac": "x"}, {"a.flac": "y"})
```

- [ ] **Step 2: Run to verify pass**

Run: `python -m pytest scripts/test_lidarr_smoke_lib.py -v`
Expected: PASS (7 passed — the helpers are already implemented in Task 1 Step 3).

- [ ] **Step 3: Lint + commit**

```bash
ruff check scripts/lidarr_smoke_lib.py scripts/test_lidarr_smoke_lib.py
ruff format --check scripts/lidarr_smoke_lib.py scripts/test_lidarr_smoke_lib.py
git add scripts/test_lidarr_smoke_lib.py
git commit -m "test(lidarr-smoke): cover ffprobe-tag parse and byte-equality"
```

---

## Task 3: Mock Lidarr API (TDD)

**Files:**
- Create: `scripts/mock_lidarr.py`
- Test: `scripts/test_mock_lidarr.py`

`build_fixture` returns a `{url_path: response_obj}` map covering exactly the
endpoints `collect_event_payloads` calls for an `AlbumDownload`. The HTTP runner
serves them.

- [ ] **Step 1: Write the failing tests**

Create `scripts/test_mock_lidarr.py`:

```python
from mock_lidarr import build_fixture


def test_fixture_has_preflight_safe_settings():
    fx = build_fixture(album_id=34, artist_id=7, artist_name="Alice", album_title="Demo",
                       tracks=[(100, "/m/01.flac", "One", 1)])
    assert fx["/api/v1/config/mediamanagement"]["fileDate"] == "none"
    assert fx["/api/v1/config/mediamanagement"]["setPermissionsLinux"] is False
    assert fx["/api/v1/config/metadataprovider"]["writeAudioTags"] == "no"


def test_fixture_trackfiles_carry_album_and_artist_ids():
    fx = build_fixture(album_id=34, artist_id=7, artist_name="Alice", album_title="Demo",
                       tracks=[(100, "/m/01.flac", "One", 1), (101, "/m/02.flac", "Two", 2)])
    tfs = fx["/api/v1/trackfile"]
    assert {tf["path"] for tf in tfs} == {"/m/01.flac", "/m/02.flac"}
    assert all(tf["albumId"] == 34 and tf["artistId"] == 7 for tf in tfs)
    assert {tf["id"] for tf in tfs} == {100, 101}


def test_fixture_tracks_link_to_trackfiles():
    fx = build_fixture(album_id=34, artist_id=7, artist_name="Alice", album_title="Demo",
                       tracks=[(100, "/m/01.flac", "One", 1)])
    tracks = fx["/api/v1/track"]
    assert tracks[0]["trackFileId"] == 100
    assert tracks[0]["title"] == "One"


def test_fixture_album_and_artist_have_required_fields():
    fx = build_fixture(album_id=34, artist_id=7, artist_name="Alice", album_title="Demo",
                       tracks=[(100, "/m/01.flac", "One", 1)])
    assert fx["/api/v1/album/34"]["id"] == 34
    assert fx["/api/v1/album/34"]["title"] == "Demo"
    assert fx["/api/v1/artist/7"]["artistName"] == "Alice"
    assert fx["/api/v1/artist/7"]["id"] == 7
```

- [ ] **Step 2: Run to verify failure**

Run: `python -m pytest scripts/test_mock_lidarr.py -v`
Expected: FAIL — `ModuleNotFoundError: No module named 'mock_lidarr'`.

- [ ] **Step 3: Write the implementation**

Create `scripts/mock_lidarr.py`:

```python
"""A local mock of the Lidarr REST API for the release smoke.

Returns fixed JSON for exactly the endpoints musefs-lidarr-sync calls on an
AlbumDownload event, so the content assertions are deterministic and need no
real MusicBrainz/Lidarr metadata. `?albumId=` query strings are ignored — the
fixture describes a single album.
"""

from __future__ import annotations

import argparse
import json
from http.server import BaseHTTPRequestHandler, HTTPServer
from urllib.parse import urlparse


def build_fixture(*, album_id, artist_id, artist_name, album_title, tracks):
    """Return a {path: response} map. ``tracks`` = [(tf_id, path, title, no), …]."""
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
        album_id=a.album_id, artist_id=a.artist_id,
        artist_name=a.artist_name, album_title=a.album_title, tracks=tracks,
    )
    HTTPServer(("127.0.0.1", a.port), make_handler(fixture)).serve_forever()


if __name__ == "__main__":  # pragma: no cover
    main()
```

- [ ] **Step 4: Run to verify pass**

Run: `python -m pytest scripts/test_mock_lidarr.py -v`
Expected: PASS (4 passed).

- [ ] **Step 5: Lint + commit**

```bash
ruff check scripts/mock_lidarr.py scripts/test_mock_lidarr.py
ruff format --check scripts/mock_lidarr.py scripts/test_mock_lidarr.py
git add scripts/mock_lidarr.py scripts/test_mock_lidarr.py
git commit -m "feat(lidarr-smoke): local mock Lidarr API for the content leg (#224)"
```

---

## Task 4: Store assertion (TDD, correct schema)

**Files:**
- Create: `scripts/store_assert.py`
- Test: `scripts/test_store_assert.py`

The musefs store keys tags per track in `tags(track_id, key, value, ordinal,
value_blob)`. A populated `artist` key is the evidence Lidarr's metadata landed.

- [ ] **Step 1: Write the failing tests**

Create `scripts/test_store_assert.py`:

```python
import sqlite3

import pytest

from store_assert import count_artist_tagged_tracks


def _store(tmp_path, rows):
    db = tmp_path / "store.db"
    con = sqlite3.connect(db)
    con.execute(
        "CREATE TABLE tags (track_id INTEGER, key TEXT, value TEXT, ordinal INTEGER DEFAULT 0)"
    )
    con.executemany("INSERT INTO tags (track_id, key, value) VALUES (?, ?, ?)", rows)
    con.commit()
    con.close()
    return str(db)


def test_counts_distinct_artist_tagged_tracks(tmp_path):
    db = _store(tmp_path, [(1, "artist", "Alice"), (1, "album", "Demo"), (2, "artist", "Alice")])
    assert count_artist_tagged_tracks(db) == 2


def test_ignores_empty_artist_values(tmp_path):
    db = _store(tmp_path, [(1, "artist", ""), (2, "artist", "Alice")])
    assert count_artist_tagged_tracks(db) == 1


def test_zero_when_no_artist_tags(tmp_path):
    db = _store(tmp_path, [(1, "title", "One")])
    assert count_artist_tagged_tracks(db) == 0
```

- [ ] **Step 2: Run to verify failure**

Run: `python -m pytest scripts/test_store_assert.py -v`
Expected: FAIL — `ModuleNotFoundError: No module named 'store_assert'`.

- [ ] **Step 3: Write the implementation**

Create `scripts/store_assert.py`:

```python
"""Assert the musefs store received Lidarr's tags (no vacuous pass).

Counts distinct tracks carrying a non-empty `artist` tag. A host/path mismatch
that skips every track leaves 0 — requiring --min-records == the track count
makes that fail loud instead of passing green.
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


def main(argv=None) -> int:
    p = argparse.ArgumentParser()
    p.add_argument("--db", required=True)
    p.add_argument("--min-records", type=int, default=1)
    a = p.parse_args(argv)
    n = count_artist_tagged_tracks(a.db)
    if n < a.min_records:
        print(f"::error::store has {n} artist-tagged tracks, expected >= {a.min_records}")
        return 1
    print(f"store records OK: {n} artist-tagged tracks")
    return 0


if __name__ == "__main__":  # pragma: no cover
    raise SystemExit(main())
```

- [ ] **Step 4: Run to verify pass**

Run: `python -m pytest scripts/test_store_assert.py -v`
Expected: PASS (3 passed).

- [ ] **Step 5: Lint + commit**

```bash
ruff check scripts/store_assert.py scripts/test_store_assert.py
ruff format --check scripts/store_assert.py scripts/test_store_assert.py
git add scripts/store_assert.py scripts/test_store_assert.py
git commit -m "feat(lidarr-smoke): store artist-tag assertion against the real schema (#224)"
```

---

## Task 5: Real-Lidarr Test-event helper + the harness

**Files:**
- Create: `scripts/configure_connection.py`
- Create: `scripts/lidarr-smoke.sh`

- [ ] **Step 1: Write `configure_connection.py`**

Create `scripts/configure_connection.py`:

```python
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
```

> **Executor note:** `/api/v1/notification/test` validates a *transient*
> definition; some Lidarr versions require extra `on*` fields in `body`. If the
> POST 400s, GET `/api/v1/notification/schema`, find the `CustomScript` entry,
> and merge its required default fields into `body`. The assertion is only that
> Lidarr execs the script (HTTP 2xx from the test endpoint).

- [ ] **Step 2: Write the harness `scripts/lidarr-smoke.sh`**

Create `scripts/lidarr-smoke.sh`:

```sh
#!/usr/bin/env bash
# Lidarr release smoke (issue #224).
#
# (A) Real-instance exec proof: boot linuxserver/lidarr, fire its Test event,
#     confirm the real Lidarr execs musefs-lidarr-import (lowercased-env path).
# (B) Content leg against a LOCAL MOCK Lidarr API (deterministic, network-free):
#     musefs-lidarr-sync writes tags, musefs-lidarr-import makes a symlink; then
#     assert store tags, symlink, unchanged backing bytes, and served-mount tags.
#
# Out of scope (documented gap): the download-client AlbumImportedEvent path.
#
# Usage: scripts/lidarr-smoke.sh /path/to/musefs
set -euo pipefail

MUSEFS="${1:?usage: lidarr-smoke.sh /path/to/musefs}"
LIDARR_IMAGE="${LIDARR_IMAGE:?set LIDARR_IMAGE to a pinned linuxserver/lidarr@sha256:... digest}"
HERE="$(cd "$(dirname "$0")" && pwd)"
WORK="$(mktemp -d)"
CID=""; MOCK_PID=""; MOUNT_PID=""
API_KEY="musefssmoke0000000000000000000000"
PORT=9678
ALBUM_ID=34; ARTIST_ID=7

cleanup() {
  [ -n "$MOUNT_PID" ] && kill "$MOUNT_PID" 2>/dev/null || true
  fusermount3 -u "$WORK/mnt" 2>/dev/null || true
  [ -n "$MOCK_PID" ] && kill "$MOCK_PID" 2>/dev/null || true
  [ -n "$CID" ] && docker rm -f "$CID" >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

MUSIC="$WORK/music/Artist/Album"
mkdir -p "$MUSIC" "$WORK/mnt" "$WORK/links"
F1="$MUSIC/01.flac"; F2="$MUSIC/02.flac"
ffmpeg -hide_banner -loglevel error -f lavfi -i "sine=frequency=440:duration=1" "$F1"
ffmpeg -hide_banner -loglevel error -f lavfi -i "sine=frequency=660:duration=1" "$F2"

# Record backing byte hashes BEFORE anything touches them.
python3 - "$F1" "$F2" > "$WORK/before.json" <<'PY'
import json, sys
sys.path.insert(0, "scripts")
from lidarr_smoke_lib import sha256_file
print(json.dumps({p: sha256_file(p) for p in sys.argv[1:]}))
PY

# ---- (A) Real-instance exec proof -----------------------------------------
CID="$(docker run -d --rm -e PUID=0 -e PGID=0 -e TZ=UTC \
  -e LIDARR__AUTH__APIKEY="$API_KEY" -p 8686:8686 "$LIDARR_IMAGE")"
for _ in $(seq 1 60); do
  curl -fsS -H "X-Api-Key: $API_KEY" "http://localhost:8686/api/v1/system/status" >/dev/null 2>&1 && break
  sleep 2
done
curl -fsS -H "X-Api-Key: $API_KEY" "http://localhost:8686/api/v1/system/status" >/dev/null
python3 "$HERE/configure_connection.py" --url "http://localhost:8686" --api-key "$API_KEY" \
  --script "$(command -v musefs-lidarr-import)"

# ---- (B) Content leg against the mock --------------------------------------
python3 "$HERE/mock_lidarr.py" --port "$PORT" \
  --album-id "$ALBUM_ID" --artist-id "$ARTIST_ID" \
  --artist-name "Alice" --album-title "Demo" \
  --track "100:$F1:One:1" --track "101:$F2:Two:2" &
MOCK_PID=$!
for _ in $(seq 1 30); do
  curl -fsS "http://127.0.0.1:$PORT/api/v1/artist/$ARTIST_ID" >/dev/null 2>&1 && break
  sleep 1
done

MUSEFS_DB="$WORK/store.db"
# B1) tags via musefs-lidarr-sync (queries the mock; autoscans via MUSEFS_BIN).
ENVFILE="$WORK/env.sh"
python3 - "$ALBUM_ID" "$F1|$F2" "$MUSEFS_DB" "http://127.0.0.1:$PORT" "$API_KEY" "$MUSEFS" > "$ENVFILE" <<'PY'
import shlex, sys
sys.path.insert(0, "scripts")
from lidarr_smoke_lib import build_album_download_env
album_id, paths, db, url, key, binp = sys.argv[1:7]
env = build_album_download_env(album_id=int(album_id), track_paths=paths.split("|"),
                               db_path=db, lidarr_url=url, api_key=key, musefs_bin=binp)
for k, v in env.items():
    print(f"export {k}={shlex.quote(v)}")
PY
# shellcheck disable=SC1090
. "$ENVFILE"
musefs-lidarr-sync

# B2) symlink via musefs-lidarr-import.
env Lidarr_EventType=Download Lidarr_SourcePath="$F1" Lidarr_DestinationPath="$WORK/links/01.flac" \
  musefs-lidarr-import
[ -L "$WORK/links/01.flac" ] || { echo "FAIL: expected symlink at links/01.flac"; exit 1; }

# ---- Assertions ------------------------------------------------------------
# Backing bytes unchanged.
python3 - "$F1" "$F2" "$WORK/before.json" <<'PY'
import json, sys
sys.path.insert(0, "scripts")
from lidarr_smoke_lib import sha256_file, assert_bytes_unchanged
*files, before_path = sys.argv[1:]
before = json.load(open(before_path))
assert_bytes_unchanged(before, {p: sha256_file(p) for p in files})
print("bytes unchanged: OK")
PY

# Store received tags (>= 2 tracks; loud-fails a vacuous 0-record pass).
python3 "$HERE/store_assert.py" --db "$MUSEFS_DB" --min-records 2

# Served mount carries the tags.
"$MUSEFS" mount "$WORK/mnt" --db "$MUSEFS_DB" &
MOUNT_PID=$!
for _ in $(seq 1 30); do mountpoint -q "$WORK/mnt" && break; sleep 1; done
SERVED="$(find "$WORK/mnt" -name '*.flac' | head -n1)"
[ -n "$SERVED" ] || { echo "FAIL: no served FLAC in mount"; exit 1; }
ffprobe -hide_banner -loglevel error -show_format -of json "$SERVED" > "$WORK/served.json"
python3 - "$WORK/served.json" <<'PY'
import sys
sys.path.insert(0, "scripts")
from lidarr_smoke_lib import parse_ffprobe_tags
tags = parse_ffprobe_tags(open(sys.argv[1]).read())
assert tags.get("artist") == "Alice", f"served file artist tag wrong: {tags}"
print("served tags: OK")
PY

echo "lidarr-smoke: PASS"
```

- [ ] **Step 3: Make executable + shellcheck + lint**

```bash
chmod +x scripts/lidarr-smoke.sh
command -v shellcheck >/dev/null && shellcheck scripts/lidarr-smoke.sh || echo "shellcheck not installed; skipping"
ruff check scripts/configure_connection.py && ruff format --check scripts/configure_connection.py
```
Expected: no errors, or the skip message.

- [ ] **Step 4: Commit**

```bash
git add scripts/configure_connection.py scripts/lidarr-smoke.sh
git commit -m "feat(lidarr-smoke): Test-event proof + content harness against mock API (#224)"
```

---

## Task 6: The reusable `lidarr-smoke.yml` workflow

**Files:**
- Create: `.github/workflows/lidarr-smoke.yml`

- [ ] **Step 1: Write the workflow**

Create `.github/workflows/lidarr-smoke.yml`:

```yaml
name: lidarr-smoke

on:
  workflow_call:
  workflow_dispatch:

permissions:
  contents: read

jobs:
  lidarr-smoke:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@df4cb1c069e1874edd31b4311f1884172cec0e10
        with:
          persist-credentials: false
      - uses: dtolnay/rust-toolchain@29eef336d9b2848a0b548edc03f92a220660cdb8
      - uses: Swatinem/rust-cache@e18b497796c12c097a38f9edb9d0641fb99eee32
      - uses: actions/setup-python@a309ff8b426b58ec0e2a45f0f869d46889d02405
        with:
          python-version: '3.x'
      - name: Install system deps
        run: sudo apt-get update && sudo apt-get install -y fuse3 ffmpeg
      - name: Build musefs (debug) and install the Lidarr plugin
        run: |
          set -euo pipefail
          cargo build -p musefs
          pip install -e contrib/python-musefs
          pip install -e contrib/lidarr
      - name: Run the Lidarr smoke
        env:
          # Pin a linuxserver/lidarr digest; bump deliberately and re-dispatch.
          LIDARR_IMAGE: lscr.io/linuxserver/lidarr@sha256:REPLACE_WITH_PINNED_DIGEST
        run: ./scripts/lidarr-smoke.sh ./target/debug/musefs
```

- [ ] **Step 2: Resolve and pin the Lidarr image digest**

```bash
docker manifest inspect lscr.io/linuxserver/lidarr:latest \
  | python3 -c "import json,sys; print(json.load(sys.stdin).get('manifests',[{}])[0].get('digest') or '')"
```
Replace `REPLACE_WITH_PINNED_DIGEST` with the resolved `sha256:...` (or copy it
from the image's GHCR page). The harness fails fast if `LIDARR_IMAGE` is unset.

- [ ] **Step 3: Validate YAML + commit**

```bash
python -c "import yaml; yaml.safe_load(open('.github/workflows/lidarr-smoke.yml'))" && echo OK
git add .github/workflows/lidarr-smoke.yml
git commit -m "ci(lidarr-smoke): reusable + dispatchable smoke workflow (#224)"
```

---

## Task 7: Wire as a gate, run helper tests, dispatch acceptance

**Files:**
- Modify: `.github/workflows/release-python.yml`
- Modify: `.github/workflows/ci.yml`

> **Sequencing:** the release-graph plan must merge first (it adds the
> `Test crates-index probe` step in `ci.yml`'s `python-musefs` job). Re-anchor
> Step 2 below on the most recent test step present.

- [ ] **Step 1: Gate the Python publish on the smoke**

In `.github/workflows/release-python.yml`, add before the `publish:` job:

```yaml
  lidarr-smoke:
    needs: version-gate
    uses: ./.github/workflows/lidarr-smoke.yml
```

Change `publish`'s needs from:

```yaml
  publish:
    needs: [test-python-musefs, test-beets, test-lidarr, test-picard]
```

to:

```yaml
  publish:
    needs: [test-python-musefs, test-beets, test-lidarr, test-picard, lidarr-smoke]
```

- [ ] **Step 2: Run the helper unit tests on PRs**

In `.github/workflows/ci.yml`, in the `python-musefs` job, after the last
`scripts/test_*.py` step (e.g. `Test crates-index probe` from the release-graph
plan), add:

```yaml
      - name: Test lidarr-smoke helpers
        run: python -m pytest scripts/test_lidarr_smoke_lib.py scripts/test_mock_lidarr.py scripts/test_store_assert.py -v
```

- [ ] **Step 3: Run the full smoke on PRs touching the Lidarr surface**

Read the real `changes` job in `ci.yml` and follow its output-wiring shape. Add
a `lidarr` output computed from the same `$changed` file list used for `src`:

```yaml
          if printf '%s\n' "$changed" | grep -qE '^(contrib/lidarr/|scripts/(lidarr|mock_lidarr|store_assert|configure_connection)|musefs-[a-z]+/src/|Cargo\.(toml|lock))'; then
            echo "lidarr=true" >> "$GITHUB_OUTPUT"
          else
            echo "lidarr=false" >> "$GITHUB_OUTPUT"
          fi
```

Declare `lidarr: ${{ steps.filter.outputs.lidarr }}` under the `changes` job's
`outputs:`, then add at the end of the `jobs:` map:

```yaml
  lidarr-smoke:
    needs: changes
    if: needs.changes.outputs.lidarr == 'true'
    uses: ./.github/workflows/lidarr-smoke.yml
```

- [ ] **Step 4: Validate YAML + commit**

```bash
python -c "import yaml; yaml.safe_load(open('.github/workflows/release-python.yml')); yaml.safe_load(open('.github/workflows/ci.yml'))" && echo OK
git add .github/workflows/release-python.yml .github/workflows/ci.yml
git commit -m "ci(lidarr-smoke): gate python publish; run on lidarr PRs + unit tests (#224)"
```

- [ ] **Step 5: Acceptance — dispatch the smoke and confirm green**

```bash
gh workflow run lidarr-smoke.yml --ref "$(git branch --show-current)"
gh run watch "$(gh run list --workflow lidarr-smoke.yml --limit 1 --json databaseId -q '.[0].databaseId')"
```
Expected: the run ends with `lidarr-smoke: PASS` (including `bytes unchanged: OK`,
`store records OK: 2 …`, `served tags: OK`). **This run is the acceptance
evidence for the whole component** — do not mark it complete until green. If the
`configure_connection.py` Test POST 400s, follow its executor note (merge the
`CustomScript` schema's required fields).
