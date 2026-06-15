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
# Overridable for local runs (e.g. a host already running Lidarr on 8686, or
# podman instead of docker); CI uses the defaults.
: "${DOCKER:=docker}"
: "${LIDARR_HOST_PORT:=8686}"
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/.." && pwd)"
WORK="$(mktemp -d)"
CID=""; MOCK_PID=""; MOUNT_PID=""
API_KEY="musefssmoke0000000000000000000000"
PORT=9678
ALBUM_ID=34; ARTIST_ID=7

cleanup() {
  [ -n "$MOUNT_PID" ] && kill "$MOUNT_PID" 2>/dev/null || true
  fusermount3 -u "$WORK/mnt" 2>/dev/null || true
  [ -n "$MOCK_PID" ] && kill "$MOCK_PID" 2>/dev/null || true
  [ -n "$CID" ] && "$DOCKER" rm -f "$CID" >/dev/null 2>&1 || true
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
# Boot real Lidarr with the plugin source bind-mounted and the wrapper as its
# Custom Script. The Alpine/.NET image has no python3, so install it in the
# container (~2s from Alpine's repos); the wrapper then runs the REAL
# musefs-lidarr-import. Firing the Test event proves Lidarr execs the real
# script and that lidarr_get resolves Lidarr's lowercased env keys end to end.
BASE="http://localhost:${LIDARR_HOST_PORT}"
CID="$("$DOCKER" run -d --rm -e PUID=0 -e PGID=0 -e TZ=UTC \
  -e LIDARR__AUTH__APIKEY="$API_KEY" -p "${LIDARR_HOST_PORT}:8686" \
  -v "$REPO/contrib/lidarr/src":/musefs/lidarr-src:ro \
  -v "$REPO/contrib/python-musefs/src":/musefs/common-src:ro \
  -v "$HERE/lidarr_import_wrapper.sh":/musefs/bin/import.sh:ro \
  "$LIDARR_IMAGE")"
for _ in $(seq 1 60); do
  curl -fsS -H "X-Api-Key: $API_KEY" "$BASE/api/v1/system/status" >/dev/null 2>&1 && break
  sleep 2
done
curl -fsS -H "X-Api-Key: $API_KEY" "$BASE/api/v1/system/status" >/dev/null
"$DOCKER" exec --user 0 "$CID" apk add --no-cache python3 >/dev/null
python3 "$HERE/configure_connection.py" --url "$BASE" --api-key "$API_KEY" \
  --script /musefs/bin/import.sh

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

# Store received tags AND the album cover for both tracks (loud-fails a vacuous
# 0-record pass).
python3 "$HERE/store_assert.py" --db "$MUSEFS_DB" --min-records 2 --min-art 2

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

# Served mount carries the embedded cover (Lidarr's art spliced into the view).
ffprobe -hide_banner -loglevel error -show_streams -of json "$SERVED" > "$WORK/streams.json"
python3 - "$WORK/streams.json" <<'PY'
import sys
sys.path.insert(0, "scripts")
from lidarr_smoke_lib import has_attached_picture
assert has_attached_picture(open(sys.argv[1]).read()), "served file has no embedded cover art"
print("served art: OK")
PY

echo "lidarr-smoke: PASS"
