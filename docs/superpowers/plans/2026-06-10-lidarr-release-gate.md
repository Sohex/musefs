# Lidarr Real-Instance Release Gate Implementation Plan (#224)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the prose-only Lidarr release gate with an automated CI job that boots a real Lidarr container, seeds it via the Lidarr API (one-time live `api.lidarr.audio` metadata fetch), drives the real Custom Script, and asserts the integration end-to-end — wired as a required gate before the Python package publish.

**Architecture:** The orchestration (ffmpeg fixtures, `docker run linuxserver/lidarr`, API seeding, mounting musefs, running `musefs-lidarr-import`) lives in one POSIX shell harness `scripts/lidarr-smoke.sh`, mirroring the existing `scripts/smoke-binary.sh`. The fiddly, regression-prone pure logic (building the `AlbumDownload` env, parsing ffprobe tag output, sha256 byte-equality) is extracted into `scripts/lidarr_smoke_lib.py` with pytest unit tests so it is TDD-able without Docker. A new reusable workflow `lidarr-smoke.yml` runs the harness; `release-python.yml` gates `publish` on it; `ci.yml` runs it on PRs touching the Lidarr surface.

**Tech Stack:** GitHub Actions, POSIX shell, Docker (`linuxserver/lidarr`), Python 3 (stdlib), pytest, `curl`/`gh` for the Lidarr REST API, `ffmpeg`/`ffprobe`, fuse3.

This plan is Component 2 of the release-process hardening spec:
`docs/superpowers/specs/2026-06-10-release-process-hardening-design.md`.

> **Honesty note for the executor:** the *pure helpers* (Tasks 1–2) are fully
> unit-tested locally. The *full smoke* (Tasks 3–5) requires Docker + `/dev/fuse`
> + network to `api.lidarr.audio` and **cannot be proven on a dev box without
> those**; its acceptance evidence is a green `workflow_dispatch` run (Task 6),
> not a local pass. Do not claim the integration works until that run is green.

---

## File Structure

- Create `scripts/lidarr_smoke_lib.py` — pure helpers: `build_album_download_env`, `parse_ffprobe_tags`, `sha256_file`, `assert_bytes_unchanged`.
- Create `scripts/test_lidarr_smoke_lib.py` — pytest unit tests.
- Create `scripts/lidarr-smoke.sh` — the integration harness (Docker + API seed + mount + assert).
- Create `.github/workflows/lidarr-smoke.yml` — reusable (`workflow_call`) + dispatchable (`workflow_dispatch`) workflow that runs the harness.
- Modify `.github/workflows/release-python.yml:137-138` — add a `lidarr-smoke` job calling the reusable workflow and add it to `publish.needs`.
- Modify `.github/workflows/ci.yml` — add a `lidarr-smoke` job (calling the reusable workflow) gated to PRs that touch `contrib/lidarr/**` or the Rust binary.
- Modify `.github/workflows/ci.yml:146-148` — run the new helper unit tests.

---

## Background the executor needs

- The Custom Script ships two console entry points (`contrib/lidarr/pyproject.toml:32-33`): `musefs-lidarr-import` and `musefs-lidarr-sync`. Lidarr invokes `musefs-lidarr-import` with `Lidarr_*` environment variables.
- On an `AlbumDownload` event the script reads `Lidarr_Album_Id` and `Lidarr_AddedTrackPaths` (pipe-joined), then queries Lidarr's REST API (`contrib/lidarr/src/musefs_lidarr/api.py`: `/api/v1/trackfile`, `/api/v1/track`, `/api/v1/album/{id}`, `/api/v1/artist/{id}`) for the metadata it writes into the musefs store. **Tags come from Lidarr's DB, not the env.**
- Before syncing, `run_preflight`/`check_safe_settings` (`api.py:134-154`) require `config/metadataprovider` `writeAudioTags == no` and `config/mediamanagement` `fileDate == none` (+ Linux permission-setting off). The seed MUST set these or the script aborts at preflight.
- `mapping.match_track_file` compares the env's `AddedTrackPaths` against Lidarr's `trackfile.path` via `realpath_key()` on **both** sides, so paths must resolve identically inside the container's namespace.
- The musefs cardinal invariant: backing audio bytes are never modified. The smoke asserts this with sha256 before/after.
- Required `MUSEFS_*` env for the script: `MUSEFS_DB` (store path), `MUSEFS_LIDARR_URL` + `MUSEFS_LIDARR_API_KEY` (Lidarr API), `MUSEFS_BIN` (musefs binary for autoscan).

---

## Task 1: Pure helper — build the AlbumDownload env (TDD)

**Files:**
- Create: `scripts/lidarr_smoke_lib.py`
- Test: `scripts/test_lidarr_smoke_lib.py`

- [ ] **Step 1: Write the failing test**

Create `scripts/test_lidarr_smoke_lib.py`:

```python
from lidarr_smoke_lib import build_album_download_env


def test_build_album_download_env_joins_paths_and_sets_event():
    env = build_album_download_env(
        album_id=34,
        track_paths=["/music/Artist/Album/01.flac", "/music/Artist/Album/02.flac"],
        db_path="/work/store.db",
        lidarr_url="http://localhost:8686",
        api_key="KEY",
        musefs_bin="/usr/local/bin/musefs",
    )
    assert env["Lidarr_EventType"] == "AlbumDownload"
    assert env["Lidarr_Album_Id"] == "34"
    assert env["Lidarr_AddedTrackPaths"] == "/music/Artist/Album/01.flac|/music/Artist/Album/02.flac"
    assert env["MUSEFS_DB"] == "/work/store.db"
    assert env["MUSEFS_LIDARR_URL"] == "http://localhost:8686"
    assert env["MUSEFS_LIDARR_API_KEY"] == "KEY"
    assert env["MUSEFS_BIN"] == "/usr/local/bin/musefs"
```

- [ ] **Step 2: Run the test to verify it fails**

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
    """Construct the env Lidarr would pass for an AlbumDownload import.

    ``track_paths`` must be the in-container realpaths of the imported tracks,
    matching Lidarr's seeded ``trackfile.path`` values.
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

- [ ] **Step 4: Run the test to verify it passes**

Run: `python -m pytest scripts/test_lidarr_smoke_lib.py -v`
Expected: PASS (1 passed).

- [ ] **Step 5: Commit**

```bash
git add scripts/lidarr_smoke_lib.py scripts/test_lidarr_smoke_lib.py
git commit -m "feat(lidarr-smoke): pure AlbumDownload env builder (#224)"
```

---

## Task 2: Pure helpers — ffprobe tags + byte-equality (TDD)

**Files:**
- Modify: `scripts/test_lidarr_smoke_lib.py`

- [ ] **Step 1: Write the failing tests**

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
    assert_bytes_unchanged({"a": "x"}, {"a": "x"})  # no raise


def test_assert_bytes_unchanged_raises_on_change():
    with pytest.raises(AssertionError, match="a.flac"):
        assert_bytes_unchanged({"a.flac": "x"}, {"a.flac": "y"})
```

- [ ] **Step 2: Run the tests to verify they fail then pass**

Run: `python -m pytest scripts/test_lidarr_smoke_lib.py -v`
Expected: PASS (6 passed total — the helpers from Task 1 Step 3 already implement these; if any fail, the implementation in Task 1 is incomplete — fix it there).

- [ ] **Step 3: Lint**

Run: `ruff check scripts/lidarr_smoke_lib.py scripts/test_lidarr_smoke_lib.py && ruff format --check scripts/lidarr_smoke_lib.py scripts/test_lidarr_smoke_lib.py`
Expected: no errors.

- [ ] **Step 4: Commit**

```bash
git add scripts/test_lidarr_smoke_lib.py
git commit -m "test(lidarr-smoke): cover ffprobe-tag parse and byte-equality helpers"
```

---

## Task 3: The integration harness `scripts/lidarr-smoke.sh`

**Files:**
- Create: `scripts/lidarr-smoke.sh`

This is the orchestration. It is long but each block is mechanical. It assumes `docker`, `ffmpeg`, `ffprobe`, `fusermount3`, `python3`, `curl`, and a musefs binary path passed as `$1`.

- [ ] **Step 1: Write the harness script**

Create `scripts/lidarr-smoke.sh` (make it executable in Step 2):

```sh
#!/usr/bin/env bash
# Lidarr real-instance release smoke (issue #224).
#
# Boots a real linuxserver/lidarr container, seeds one artist/album via Lidarr's
# API (one-time live api.lidarr.audio metadata fetch), drives the real Custom
# Script (musefs-lidarr-import), and asserts: symlinks created, store tags match
# Lidarr's metadata, backing audio bytes unchanged, served mount carries tags.
#
# The download-client AlbumImportedEvent path is OUT OF SCOPE (documented gap):
# it only fires for NewDownload imports and cannot be driven here.
#
# Usage: scripts/lidarr-smoke.sh /path/to/musefs
set -euo pipefail

MUSEFS="${1:?usage: lidarr-smoke.sh /path/to/musefs}"
LIDARR_IMAGE="${LIDARR_IMAGE:?set LIDARR_IMAGE to a pinned linuxserver/lidarr@sha256:... digest}"
HERE="$(cd "$(dirname "$0")" && pwd)"
WORK="$(mktemp -d)"
CID=""
MOUNT_PID=""

cleanup() {
  [ -n "$MOUNT_PID" ] && kill "$MOUNT_PID" 2>/dev/null || true
  fusermount3 -u "$WORK/mnt" 2>/dev/null || true
  [ -n "$CID" ] && docker rm -f "$CID" >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

API_KEY="musefssmoke0000000000000000000000"
ARTIST_MBID="${ARTIST_MBID:-cc197bad-dc9c-440d-a5b5-d52ba2e14234}"  # Coldplay; stable test artist
MUSIC="$WORK/music"
mkdir -p "$MUSIC/Artist/Album" "$WORK/mnt"

# 1) Generate two tagged FLAC fixtures (the "imported" tracks).
for n in 01 02; do
  ffmpeg -hide_banner -loglevel error -f lavfi -i "sine=frequency=440:duration=1" \
    -metadata title="orig-$n" "$MUSIC/Artist/Album/$n.flac"
done

# Record backing byte hashes BEFORE anything touches them.
python3 - "$MUSIC" > "$WORK/before.json" <<'PY'
import json, sys
from pathlib import Path
sys.path.insert(0, "scripts")
from lidarr_smoke_lib import sha256_file
root = Path(sys.argv[1])
print(json.dumps({str(p): sha256_file(str(p)) for p in sorted(root.rglob("*.flac"))}))
PY

# 2) Boot Lidarr with the music dir mounted at the SAME path in-container.
CID="$(docker run -d --rm \
  -e PUID=0 -e PGID=0 -e TZ=UTC \
  -e LIDARR__AUTH__APIKEY="$API_KEY" \
  -p 8686:8686 \
  -v "$MUSIC":"$MUSIC" \
  "$LIDARR_IMAGE")"
LIDARR_URL="http://localhost:8686"

# Wait for the API to come up.
for _ in $(seq 1 60); do
  if curl -fsS -H "X-Api-Key: $API_KEY" "$LIDARR_URL/api/v1/system/status" >/dev/null 2>&1; then
    break
  fi
  sleep 2
done
curl -fsS -H "X-Api-Key: $API_KEY" "$LIDARR_URL/api/v1/system/status" >/dev/null

# 3) Seed via the API. seed_lidarr.py adds the root folder, sets safe-settings,
#    adds the artist by MBID (live api.lidarr.audio fetch, retried), triggers a
#    scan, and prints "ALBUM_ID=<n>" plus "TRACKPATHS=<a>|<b>" for matched files.
SEED_OUT="$WORK/seed.out"
python3 "$HERE/seed_lidarr.py" \
  --url "$LIDARR_URL" --api-key "$API_KEY" \
  --music-root "$MUSIC" --artist-mbid "$ARTIST_MBID" | tee "$SEED_OUT"
ALBUM_ID="$(sed -n 's/^ALBUM_ID=//p' "$SEED_OUT")"
TRACKPATHS="$(sed -n 's/^TRACKPATHS=//p' "$SEED_OUT")"
[ -n "$ALBUM_ID" ] && [ -n "$TRACKPATHS" ] || { echo "FAIL: seeding produced no album/tracks"; exit 1; }

# 4) Fire the Lidarr Test event so the REAL Lidarr execs the script (proves the
#    connection wiring + lowercased-env resolution). Configure a Custom Script
#    connection pointing at musefs-lidarr-import, then POST the test.
#    (configure_connection.py registers the connection and returns its id.)
MUSEFS_DB="$WORK/store.db"
export MUSEFS_DB MUSEFS_LIDARR_URL="$LIDARR_URL" MUSEFS_LIDARR_API_KEY="$API_KEY" MUSEFS_BIN="$MUSEFS"
python3 "$HERE/configure_connection.py" \
  --url "$LIDARR_URL" --api-key "$API_KEY" \
  --script "$(command -v musefs-lidarr-import)" \
  --env MUSEFS_DB="$MUSEFS_DB" --env MUSEFS_LIDARR_URL="$LIDARR_URL" \
  --env MUSEFS_LIDARR_API_KEY="$API_KEY" --env MUSEFS_BIN="$MUSEFS" \
  --fire-test

# 5) Drive the AlbumDownload import directly with the env Lidarr would pass.
ENVFILE="$WORK/env.sh"
python3 - "$ALBUM_ID" "$TRACKPATHS" "$MUSEFS_DB" "$LIDARR_URL" "$API_KEY" "$MUSEFS" > "$ENVFILE" <<'PY'
import shlex, sys
sys.path.insert(0, "scripts")
from lidarr_smoke_lib import build_album_download_env
album_id, paths, db, url, key, binp = sys.argv[1:7]
env = build_album_download_env(
    album_id=int(album_id), track_paths=paths.split("|"),
    db_path=db, lidarr_url=url, api_key=key, musefs_bin=binp,
)
for k, v in env.items():
    print(f"export {k}={shlex.quote(v)}")
PY
# shellcheck disable=SC1090
. "$ENVFILE"
musefs-lidarr-import

# 6) Assertions.
# 6a) Symlinks created for every track (the script links into the store layout).
LINKS="$(find "$WORK" -type l -name '*.flac' | wc -l)"
[ "$LINKS" -ge 2 ] || { echo "FAIL: expected >=2 symlinks, got $LINKS"; exit 1; }

# 6b) Backing bytes unchanged.
python3 - "$MUSIC" "$WORK/before.json" <<'PY'
import json, sys
from pathlib import Path
sys.path.insert(0, "scripts")
from lidarr_smoke_lib import sha256_file, assert_bytes_unchanged
root, before_path = sys.argv[1], sys.argv[2]
before = json.load(open(before_path))
after = {str(p): sha256_file(str(p)) for p in sorted(Path(root).rglob("*.flac"))}
assert_bytes_unchanged(before, after)
print("bytes unchanged: OK")
PY

# 6c) Store has tags AND the import skipped nothing (defends against a vacuous
#     pass from a host/container path mismatch). store_assert.py exits non-zero
#     if records == 0 or skipped > 0, and verifies the seeded artist/album tags.
python3 "$HERE/store_assert.py" --db "$MUSEFS_DB" --min-records 2

# 6d) Serve the store through musefs and confirm a served file carries the tags.
"$MUSEFS" mount "$MUSEFS_DB" "$WORK/mnt" &
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
assert tags.get("artist"), f"served file missing artist tag: {tags}"
print("served tags: OK ->", {k: tags[k] for k in ("artist", "album") if k in tags})
PY

echo "lidarr-smoke: PASS"
```

This harness references three small companion Python helpers — `seed_lidarr.py`, `configure_connection.py`, `store_assert.py` — created in Task 4. (They are thin API/SQLite wrappers; the regression-prone *pure* logic already lives in `lidarr_smoke_lib.py`.)

- [ ] **Step 2: Make it executable and shellcheck it**

```bash
chmod +x scripts/lidarr-smoke.sh
command -v shellcheck >/dev/null && shellcheck scripts/lidarr-smoke.sh || echo "shellcheck not installed; skipping"
```
Expected: no errors, or the skip message.

- [ ] **Step 3: Commit**

```bash
git add scripts/lidarr-smoke.sh
git commit -m "feat(lidarr-smoke): integration harness boots Lidarr and drives the Custom Script (#224)"
```

---

## Task 4: API/SQLite companion helpers

**Files:**
- Create: `scripts/seed_lidarr.py`
- Create: `scripts/configure_connection.py`
- Create: `scripts/store_assert.py`

These talk to the live Lidarr API and the musefs SQLite store. They are integration glue; keep them small and stdlib-only.

- [ ] **Step 1: Write `seed_lidarr.py`**

Create `scripts/seed_lidarr.py`:

```python
"""Seed a running Lidarr with one artist/album whose tracks map to local FLACs.

Adds a root folder, sets the safe-settings the Custom Script's preflight
requires, adds the artist by MBID (one-time live api.lidarr.audio fetch, with
retries to distinguish an upstream-metadata flake from an integration break),
triggers an import, and prints `ALBUM_ID=` and `TRACKPATHS=` for the harness.
"""

from __future__ import annotations

import argparse
import json
import sys
import time
import urllib.error
import urllib.request


def _req(method, url, api_key, body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method)
    req.add_header("X-Api-Key", api_key)
    req.add_header("Content-Type", "application/json")
    with urllib.request.urlopen(req, timeout=60) as resp:
        text = resp.read().decode()
        return json.loads(text) if text else None


def _retry(fn, *, attempts=5, delay=10, what="operation"):
    last = None
    for i in range(attempts):
        try:
            return fn()
        except urllib.error.URLError as exc:  # network / upstream metadata
            last = exc
            print(f"::warning::{what} failed (attempt {i + 1}/{attempts}): {exc}", file=sys.stderr)
            time.sleep(delay)
    print(f"::error::{what} failed after {attempts} attempts (likely api.lidarr.audio outage): {last}")
    raise SystemExit(2)


def main(argv=None):
    p = argparse.ArgumentParser()
    p.add_argument("--url", required=True)
    p.add_argument("--api-key", required=True)
    p.add_argument("--music-root", required=True)
    p.add_argument("--artist-mbid", required=True)
    a = p.parse_args(argv)
    base = a.url.rstrip("/")

    # Safe-settings the Custom Script preflight enforces (api.py check_safe_settings).
    mm = _req("GET", f"{base}/api/v1/config/mediamanagement", a.api_key)
    mm["fileDate"] = "none"
    mm["setPermissionsLinux"] = False
    _req("PUT", f"{base}/api/v1/config/mediamanagement", a.api_key, mm)
    mp = _req("GET", f"{base}/api/v1/config/metadataprovider", a.api_key)
    mp["writeAudioTags"] = "no"
    _req("PUT", f"{base}/api/v1/config/metadataprovider", a.api_key, mp)

    # Root folder = the in-container music dir.
    _req("POST", f"{base}/api/v1/rootfolder", a.api_key, {"path": a.music_root})

    # Look up + add the artist (live metadata fetch).
    lookup = _retry(
        lambda: _req("GET", f"{base}/api/v1/artist/lookup?term=lidarr:{a.artist_mbid}", a.api_key),
        what="artist lookup (api.lidarr.audio)",
    )
    if not lookup:
        print("::error::artist lookup returned no results"); raise SystemExit(2)
    artist = lookup[0]
    artist.update({
        "rootFolderPath": a.music_root,
        "qualityProfileId": 1,
        "metadataProfileId": 1,
        "monitored": True,
        "addOptions": {"monitor": "all", "searchForMissingAlbums": False},
    })
    added = _retry(lambda: _req("POST", f"{base}/api/v1/artist", a.api_key, artist),
                   what="add artist")
    artist_id = added["id"]

    # Trigger a disk scan so Lidarr creates trackfile rows for the FLACs.
    _req("POST", f"{base}/api/v1/command", a.api_key,
         {"name": "RescanArtist", "artistId": artist_id})

    # Poll for trackfiles to appear.
    album_id = None
    paths = []
    for _ in range(60):
        tfs = _req("GET", f"{base}/api/v1/trackfile?artistId={artist_id}", a.api_key) or []
        if tfs:
            paths = [tf["path"] for tf in tfs]
            album_id = tfs[0].get("albumId")
            if album_id:
                break
        time.sleep(5)
    if not album_id or not paths:
        print("::error::no trackfiles imported after scan"); raise SystemExit(2)

    print(f"ALBUM_ID={album_id}")
    print("TRACKPATHS=" + "|".join(paths))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
```

- [ ] **Step 2: Write `configure_connection.py`**

Create `scripts/configure_connection.py`:

```python
"""Register a Custom Script connection in Lidarr and (optionally) fire its Test.

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
    p.add_argument("--env", action="append", default=[], help="KEY=VALUE passed to the script")
    p.add_argument("--fire-test", action="store_true")
    a = p.parse_args(argv)
    base = a.url.rstrip("/")

    # Lidarr Custom Script connection definition. On import it runs `script`
    # with Lidarr_* env; our MUSEFS_* vars must be present in the script's
    # environment — provided to the harness process, not the connection, since
    # the harness drives AlbumDownload directly. The connection here exists to
    # validate the Test event exec path.
    body = {
        "name": "musefs-smoke",
        "implementation": "CustomScript",
        "configContract": "CustomScriptSettings",
        "onReleaseImport": True,
        "onUpgrade": True,
        "fields": [{"name": "path", "value": a.script}],
    }
    created = _req("POST", f"{base}/api/v1/notification", a.api_key, body)
    if a.fire_test:
        _req("POST", f"{base}/api/v1/notification/test", a.api_key, body)
        print("Test event fired; Lidarr execed the script.")
    print(f"connection_id={created['id']}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
```

- [ ] **Step 3: Write `store_assert.py`**

Create `scripts/store_assert.py`:

```python
"""Assert the musefs store received Lidarr's tags (no vacuous pass).

Fails (non-zero) unless at least --min-records tracks carry a non-empty artist
tag. A host/container path-namespace mismatch makes the import skip every track,
leaving 0 records; requiring --min-records == the seeded track count (2) makes
that case fail loud instead of passing vacuously.
"""

from __future__ import annotations

import argparse
import sqlite3
import sys


def main(argv=None):
    p = argparse.ArgumentParser()
    p.add_argument("--db", required=True)
    p.add_argument("--min-records", type=int, default=1)
    a = p.parse_args(argv)

    con = sqlite3.connect(a.db)
    try:
        # The musefs store keys tags per track; a populated tags table with an
        # artist value is the evidence Lidarr's API metadata landed.
        rows = con.execute(
            "SELECT COUNT(*) FROM tags WHERE key = 'artist' AND value != ''"
        ).fetchone()[0]
    except sqlite3.OperationalError as exc:
        print(f"::error::store schema not as expected ({exc}); "
              f"adjust store_assert.py to the musefs-db schema")
        return 2
    finally:
        con.close()

    if rows < a.min_records:
        print(f"::error::store has {rows} artist-tagged tracks, expected >= {a.min_records}")
        return 1
    print(f"store records OK: {rows} artist-tagged tracks")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
```

> **Executor note:** `store_assert.py`'s `tags` table/column names are a
> best-effort guess at the musefs-db schema. Before relying on it, confirm the
> real schema: `python -c "import sqlite3,sys; print([r[0] for r in
> sqlite3.connect(sys.argv[1]).execute('SELECT name FROM sqlite_master WHERE
> type=\"table\"')])" <store.db>` against a store produced by a real import, and
> correct the query to the actual tag table. This is the one place the plan
> cannot fully pin without a populated store; treat a `::error::store schema`
> message as "fix the query," not "integration broken."

- [ ] **Step 4: Lint the companions**

Run: `ruff check scripts/seed_lidarr.py scripts/configure_connection.py scripts/store_assert.py && ruff format --check scripts/seed_lidarr.py scripts/configure_connection.py scripts/store_assert.py`
Expected: no errors (run `ruff format ...` first if needed).

- [ ] **Step 5: Commit**

```bash
git add scripts/seed_lidarr.py scripts/configure_connection.py scripts/store_assert.py
git commit -m "feat(lidarr-smoke): Lidarr API seed/connection + store assertion helpers (#224)"
```

---

## Task 5: The reusable `lidarr-smoke.yml` workflow

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
      - name: Run the Lidarr real-instance smoke
        env:
          # Pin a linuxserver/lidarr digest. Bump deliberately; the seed uses
          # the API (schema-stable), so a bump is low-risk but should be smoke-run.
          LIDARR_IMAGE: lscr.io/linuxserver/lidarr@sha256:REPLACE_WITH_PINNED_DIGEST
        run: ./scripts/lidarr-smoke.sh ./target/debug/musefs
```

- [ ] **Step 2: Pin the Lidarr image digest**

Resolve a current digest and replace `REPLACE_WITH_PINNED_DIGEST`:

```bash
docker manifest inspect lscr.io/linuxserver/lidarr:latest -v \
  | python3 -c "import json,sys; d=json.load(sys.stdin); print(d['Descriptor']['digest'] if isinstance(d,dict) else d[0]['Descriptor']['digest'])"
```
Paste the `sha256:...` into the `LIDARR_IMAGE` value. (If `docker manifest` is unavailable, copy the digest from the image's GHCR/Docker Hub page.)

- [ ] **Step 3: Validate YAML**

Run: `python -c "import yaml; yaml.safe_load(open('.github/workflows/lidarr-smoke.yml'))" && echo OK`
Expected: `OK`.

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/lidarr-smoke.yml
git commit -m "ci(lidarr-smoke): reusable + dispatchable real-instance smoke workflow (#224)"
```

---

## Task 6: Wire as a gate and run the acceptance dispatch

**Files:**
- Modify: `.github/workflows/release-python.yml:137-138`
- Modify: `.github/workflows/ci.yml`

- [ ] **Step 1: Gate the Python publish on the smoke**

In `.github/workflows/release-python.yml`, add a job that calls the reusable workflow and make `publish` need it. Before the `publish:` job, add:

```yaml
  lidarr-smoke:
    needs: version-gate
    uses: ./.github/workflows/lidarr-smoke.yml
```

Then change `publish`'s needs from:

```yaml
  publish:
    needs: [test-python-musefs, test-beets, test-lidarr, test-picard]
```

to:

```yaml
  publish:
    needs: [test-python-musefs, test-beets, test-lidarr, test-picard, lidarr-smoke]
```

- [ ] **Step 2: Run the smoke on PRs touching the Lidarr surface**

In `.github/workflows/ci.yml`, add a job that calls the reusable workflow only when the Lidarr plugin or the binary changed. Add at the end of the `jobs:` map:

```yaml
  lidarr-smoke:
    needs: changes
    if: needs.changes.outputs.lidarr == 'true'
    uses: ./.github/workflows/lidarr-smoke.yml
```

And extend the `changes` job's path filter to emit a `lidarr` output. In the `changes` job, where it sets outputs, add a `lidarr` filter covering `contrib/lidarr/**`, `scripts/lidarr*`, `scripts/seed_lidarr.py`, `scripts/configure_connection.py`, `scripts/store_assert.py`, and the Rust sources for the binary (`musefs-*/src/**`, `Cargo.*`). Implement it in the same shell style as the existing `src` filter:

```yaml
          if printf '%s\n' "$changed" | grep -qE '^(contrib/lidarr/|scripts/(lidarr|seed_lidarr|configure_connection|store_assert)|musefs-[a-z]+/src/|Cargo\.(toml|lock))'; then
            echo "lidarr=true" >> "$GITHUB_OUTPUT"
          else
            echo "lidarr=false" >> "$GITHUB_OUTPUT"
          fi
```

and declare `lidarr: ${{ steps.filter.outputs.lidarr }}` under the `changes` job `outputs:`.

> **Executor note:** read the actual `changes` job in `ci.yml` first and follow
> its exact output-wiring shape; the snippet above is the pattern, adapt the
> variable names to match.

- [ ] **Step 3: Run the new helper unit tests in CI**

In `.github/workflows/ci.yml`, after the release-gate/crates-index test steps added by the release-graph plan (or after the existing `Test mutant-anchor guard` step if that plan has not merged yet), add:

```yaml
      - name: Test lidarr-smoke helpers
        run: python -m pytest scripts/test_lidarr_smoke_lib.py -v
```

- [ ] **Step 4: Validate YAML**

Run: `python -c "import yaml; yaml.safe_load(open('.github/workflows/release-python.yml')); yaml.safe_load(open('.github/workflows/ci.yml'))" && echo OK`
Expected: `OK`.

- [ ] **Step 5: Commit**

```bash
git add .github/workflows/release-python.yml .github/workflows/ci.yml
git commit -m "ci(lidarr-smoke): gate python publish on the smoke; run on lidarr PRs (#224)"
```

- [ ] **Step 6: Acceptance — dispatch the smoke and confirm green**

Push the branch, then trigger the dispatchable workflow and watch it:

```bash
gh workflow run lidarr-smoke.yml --ref "$(git branch --show-current)"
gh run watch "$(gh run list --workflow lidarr-smoke.yml --limit 1 --json databaseId -q '.[0].databaseId')"
```
Expected: the run ends with `lidarr-smoke: PASS`, including `bytes unchanged: OK`, `store records OK`, and `served tags: OK`. **This run is the acceptance evidence for the whole component** — do not mark the component complete until it is green. If it fails at `store_assert.py` with a schema message, fix the query (see the executor note in Task 4 Step 3) and re-dispatch.
