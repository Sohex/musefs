#!/usr/bin/env bash
# Full real-instance Lidarr e2e (issue #224, the download-client OnReleaseImport
# gap). Boots a real Lidarr against three local mocks (metadata/indexer/qbit),
# drives a genuine grab -> download-complete -> import of a real CC0 album as a
# NewDownload, and lets Lidarr exec the REAL musefs scripts:
#   * import script  (musefs-lidarr-import) symlinks the library entry to the
#     untouched backing download, and
#   * OnReleaseImport (musefs-lidarr-sync) writes Lidarr's metadata into the
#     musefs store.
# Then it serves the store through the real musefs binary and proves the served
# file carries Lidarr's tags (overriding the backing file's deliberately-wrong
# artist) with the backing audio bytes unchanged.
#
# Deterministic and network-free (the metadata server is mocked too). Needs
# docker/podman + /dev/fuse + ffmpeg/ffprobe + fuse3 + a built musefs binary.
#
# Usage: scripts/lidarr-e2e/run-e2e.sh /path/to/musefs
set -uo pipefail
MUSEFS="${1:?usage: run-e2e.sh /path/to/musefs}"
: "${DOCKER:=docker}"
: "${LIDARR_HOST_PORT:=8686}"
: "${LIDARR_IMAGE:?set LIDARR_IMAGE to a pinned linuxserver/lidarr@sha256:... digest}"
HERE="$(cd "$(dirname "$0")" && pwd)"; REPO="$(cd "$HERE/../.." && pwd)"
KEY="musefse2e000000000000000000000000"; MBID=fca800c1-6fc3-4bfb-a5de-8c2398c27bc0
BTIH=0123456789abcdef0123456789abcdef01234567
B="http://localhost:${LIDARR_HOST_PORT}/api/v1"; HC=host.containers.internal
ART_ID=1
W="$(mktemp -d)"; CID=lidarr-e2e-$$; M1=""; M2=""; M3=""
cleanup(){ for p in $M1 $M2 $M3; do kill "$p" 2>/dev/null||true; done
  [ -n "${MOUNT_PID:-}" ] && kill "$MOUNT_PID" 2>/dev/null||true; fusermount3 -u "$W/mnt" 2>/dev/null||true
  "$DOCKER" rm -f "$CID" >/dev/null 2>&1||true; reown "$W"; rm -rf "$W"; }
trap cleanup EXIT
fail(){ echo "FAIL: $*" >&2; exit 1; }
# Rootful Docker writes bind-mounted files as real root; chown them back to the
# runner so the host can edit/read/delete them. No-op under rootless podman
# (already mapped to the host user) and where passwordless sudo is unavailable.
reown(){ sudo -n chown -R "$(id -u):$(id -g)" "$@" 2>/dev/null || true; }
api(){ curl -sS -H "X-Api-Key: $KEY" -H "Content-Type: application/json" "$@"; }
jq1(){ python3 -c "import json,sys;d=json.load(sys.stdin);print($1)"; }

DL="$W/downloads/Komiku"; LIB="$W/library"; mkdir -p "$DL" "$LIB" "$W/config" "$W/store" "$W/bin" "$W/mnt"
# Staged "download": the vendored CC0 clip, tagged with the real MBIDs + title so
# Lidarr matches and imports it — but deliberately WITHOUT albumartist or date.
# Those are fields Lidarr's metadata supplies, so finding them on the *served*
# file proves musefs-lidarr-sync applied Lidarr's metadata (not the file's tags).
ffmpeg -hide_banner -loglevel error -y -i "$HERE/fixtures/komiku-the-calling.flac" -c copy \
  -metadata ARTIST=Komiku -metadata ALBUM="The adventure goes on, vol.1" \
  -metadata TITLE="The calling" -metadata track=1 \
  -metadata MUSICBRAINZ_ARTISTID=$MBID -metadata MUSICBRAINZ_ALBUMID=59d2fc91-8a6a-45da-9e12-51b01d2a50d7 \
  -metadata MUSICBRAINZ_RELEASETRACKID=7fe0e7ac-d373-4c34-9a47-f4582ed345ef \
  -metadata MUSICBRAINZ_TRACKID=c6c102b4-755a-4fff-ab96-f2bbd6d39deb \
  "$DL/01 - The calling.flac" || fail "ffmpeg tag"
BACKING="$DL/01 - The calling.flac"
SHA_BEFORE="$(sha256sum "$BACKING" | awk '{print $1}')" || fail "sha256 of backing file"
[ -n "$SHA_BEFORE" ] || fail "empty sha256 for backing file"

# wrappers + env into the bind dir
cp "$HERE/import_wrapper.sh" "$HERE/sync_wrapper.sh" "$W/bin/"; chmod +x "$W/bin/"*.sh
printf 'export MUSEFS_LIDARR_API_KEY=%s\n' "$KEY" > "$W/bin/env.sh"

# Host pre-registers the backing track so the in-container sync (autoscan off)
# finds it by realpath; downloads are bind-mounted at the same path in-container.
"$MUSEFS" scan "$W/downloads" --db "$W/store/store.db" >/dev/null 2>&1 || fail "host scan"

echo "=== mocks ==="
python3 "$HERE/mock_metadata.py" --port 9701 >/tmp/m1.log 2>&1 & M1=$!
python3 "$HERE/mock_indexer.py"  --port 9702 >/tmp/m2.log 2>&1 & M2=$!
python3 "$HERE/mock_qbit.py" --port 9703 --hash $BTIH --category music --save-path "$W/downloads" --content-path "$DL" >/tmp/m3.log 2>&1 & M3=$!
sleep 1

echo "=== boot Lidarr ==="
"$DOCKER" run -d --name "$CID" -e PUID=0 -e PGID=0 -e TZ=UTC -e LIDARR__AUTH__APIKEY="$KEY" \
  --add-host=host.containers.internal:host-gateway \
  -p "${LIDARR_HOST_PORT}:8686" -v "$W/config":/config \
  -v "$W/downloads":"$W/downloads" -v "$LIB":"$LIB" -v "$W/store":/musefs/store -v "$W/bin":/musefs/bin \
  -v "$REPO/contrib/lidarr/src":/musefs/lidarr-src:ro -v "$REPO/contrib/python-musefs/src":/musefs/common-src:ro \
  "$LIDARR_IMAGE" >/dev/null || fail "docker run"
for _ in $(seq 1 90); do curl -fsS -H "X-Api-Key: $KEY" "$B/system/status" >/dev/null 2>&1 && break; sleep 2; done
"$DOCKER" exec --user 0 "$CID" apk add --no-cache python3 >/dev/null 2>&1 || fail "apk python3"
DB=$(find "$W/config" -name lidarr.db|head -1)
"$DOCKER" stop -t 20 "$CID" >/dev/null
reown "$W/config"   # lidarr.db is root-owned under rootful Docker; make it host-writable
python3 -c "import sqlite3;c=sqlite3.connect('$DB');c.execute(\"INSERT OR REPLACE INTO Config (Key,Value) VALUES ('metadatasource',?)\",('http://$HC:9701/api/v0.4',));c.commit();c.close()" || fail "metadatasource"
"$DOCKER" start "$CID" >/dev/null
for _ in $(seq 1 90); do curl -fsS -H "X-Api-Key: $KEY" "$B/system/status" >/dev/null 2>&1 && break; sleep 2; done

echo "=== configure ==="
api -X POST "$B/rootfolder" -d "{\"name\":\"library\",\"path\":\"$LIB\",\"defaultQualityProfileId\":1,\"defaultMetadataProfileId\":1}" >/dev/null
MM=$(api "$B/config/mediamanagement"|jq1 "json.dumps({**d,'allowFingerprinting':'never','useScriptImport':True,'scriptImportPath':'/musefs/bin/import_wrapper.sh'})")
api -X PUT "$B/config/mediamanagement" -d "$MM" >/dev/null
api -X POST "$B/downloadclient?forceSave=true" -d "{\"enable\":true,\"protocol\":\"torrent\",\"priority\":1,\"name\":\"qb\",\"implementation\":\"QBittorrent\",\"configContract\":\"QBittorrentSettings\",\"fields\":[{\"name\":\"host\",\"value\":\"$HC\"},{\"name\":\"port\",\"value\":9703},{\"name\":\"useSsl\",\"value\":false},{\"name\":\"username\",\"value\":\"x\"},{\"name\":\"password\",\"value\":\"x\"},{\"name\":\"musicCategory\",\"value\":\"music\"}]}" >/dev/null
api -X POST "$B/indexer?forceSave=true" -d "{\"enable\":true,\"enableRss\":false,\"enableAutomaticSearch\":true,\"enableInteractiveSearch\":true,\"protocol\":\"torrent\",\"priority\":25,\"name\":\"idx\",\"implementation\":\"Torznab\",\"configContract\":\"TorznabSettings\",\"fields\":[{\"name\":\"baseUrl\",\"value\":\"http://$HC:9702\"},{\"name\":\"apiPath\",\"value\":\"/api\"},{\"name\":\"categories\",\"value\":[3040]},{\"name\":\"minimumSeeders\",\"value\":1}]}" >/dev/null
api -X POST "$B/notification?forceSave=true" -d '{"onReleaseImport":true,"name":"musefs","implementation":"CustomScript","configContract":"CustomScriptSettings","fields":[{"name":"path","value":"/musefs/bin/sync_wrapper.sh"}]}' >/dev/null

echo "=== add Komiku ==="
LK=$(api "$B/artist/lookup?term=lidarr:$MBID")
ADD=$(echo "$LK"|jq1 "json.dumps({**d[0],'rootFolderPath':'$LIB','qualityProfileId':1,'metadataProfileId':1,'monitored':True,'addOptions':{'monitor':'all','searchForMissingAlbums':False}})")
api -X POST "$B/artist" -d "$ADD" >/dev/null
ALBID=""; for _ in $(seq 1 40); do ALBID=$(api "$B/album?artistId=$ART_ID" 2>/dev/null|jq1 "d[0]['id'] if d else ''" 2>/dev/null); [ -n "$ALBID" ] && break; sleep 2; done
[ -n "$ALBID" ] || fail "album not created"

echo "=== search + import ==="
api -X POST "$B/command" -d "{\"name\":\"AlbumSearch\",\"albumIds\":[$ALBID]}" >/dev/null; sleep 10
IMPORTED=""
for _ in $(seq 1 24); do
  api -X POST "$B/command" -d '{"name":"RefreshMonitoredDownloads"}' >/dev/null
  [ -L "$LIB/Komiku/01 - The calling.flac" ] || [ -f "$LIB/Komiku/01 - The calling.flac" ] && { IMPORTED=1; break; }
  sleep 5
done
[ -n "$IMPORTED" ] || { echo "--- lidarr log ---"; "$DOCKER" exec "$CID" sh -c 'tail -n 40 /config/logs/lidarr.txt'|grep -iE 'import|reject|script|sync'|tail -15; fail "import did not complete"; }

# The in-container sync/import (root under rootful Docker) wrote the store +
# library symlinks; chown those back so the host musefs read/mount/assert path
# works. Deliberately NOT $W/downloads: the backing file is host-created and only
# read in-container, so chowning it would bump its ctime after the store recorded
# its freshness stamp — tripping the (size, mtime, ctime) backing-changed guard
# (musefs-core freshness, #276) and failing the serve with an I/O error.
reown "$W/store" "$LIB" "$W/config"
echo "=== assertions ==="
LIBFILE="$LIB/Komiku/01 - The calling.flac"
[ -L "$LIBFILE" ] || fail "library entry is not a symlink (import script didn't run): $(ls -l "$LIBFILE")"
echo "  [ok] library entry is a symlink -> $(readlink "$LIBFILE")"
# store got Lidarr's artist (the sync wrote Lidarr's API metadata)
python3 - "$W/store/store.db" <<'PY' || fail "store does not carry Lidarr artist=Komiku"
import sqlite3,sys
c=sqlite3.connect(sys.argv[1])
vals=[r[0] for r in c.execute("SELECT value FROM tags WHERE key='artist'")]
print("  [ok] store artist tags:", vals)
assert "Komiku" in vals, vals
PY
# store got the album cover (the sync fetched Lidarr's MediaCover art)
python3 - "$W/store/store.db" <<'PY' || fail "store carries no album art"
import sqlite3,sys
c=sqlite3.connect(sys.argv[1])
n=c.execute("SELECT COUNT(*) FROM track_art").fetchone()[0]
print("  [ok] store track_art rows:", n)
assert n>=1, n
PY
SHA_AFTER="$(sha256sum "$BACKING" | awk '{print $1}')"
[ "$SHA_BEFORE" = "$SHA_AFTER" ] || fail "backing audio bytes changed"
echo "  [ok] backing bytes unchanged"
# serve through musefs and prove the served file shows Lidarr's metadata
"$MUSEFS" mount "$W/mnt" --db "$W/store/store.db" & MOUNT_PID=$!
for _ in $(seq 1 30); do mountpoint -q "$W/mnt" && break; sleep 1; done
SERVED="$(find "$W/mnt" -name '*.flac' | head -n1)"; [ -n "$SERVED" ] || fail "no served file"
# the backing file must NOT carry the fields we attribute to Lidarr
ffprobe -hide_banner -loglevel error -show_format -of json "$BACKING" > "$W/backing.json"
python3 - "$W/backing.json" <<'PY' || fail "backing file unexpectedly already had albumartist/date"
import json,sys
t={k.lower():v for k,v in json.load(open(sys.argv[1]))["format"].get("tags",{}).items()}
assert not t.get("album_artist") and not t.get("albumartist"), f"backing has albumartist: {t}"
assert not t.get("date"), f"backing has date: {t}"
print("  [ok] backing file has no albumartist/date (so the served ones must be Lidarr's)")
PY
ffprobe -hide_banner -loglevel error -show_format -of json "$SERVED" > "$W/served.json"
python3 - "$W/served.json" <<'PY' || fail "served file does not carry Lidarr's metadata"
import json,sys
t={k.lower():v for k,v in json.load(open(sys.argv[1]))["format"].get("tags",{}).items()}
aa=t.get("album_artist") or t.get("albumartist")
assert t.get("artist")=="Komiku", f"served artist={t.get('artist')!r}"
assert aa=="Komiku", f"served albumartist={aa!r} (expected Komiku, supplied by Lidarr)"
assert (t.get("date") or "").startswith("2020-05-03"), f"served date={t.get('date')!r} (expected Lidarr's 2020-05-03)"
print("  [ok] served file carries Lidarr-supplied metadata absent from the backing file:")
print("       artist=%r albumartist=%r date=%r title=%r" % (t.get("artist"), aa, t.get("date"), t.get("title")))
PY
# served file carries the embedded cover (Lidarr's art spliced into the view);
# the backing file has none, so an attached_pic stream must be musefs-generated.
ffprobe -hide_banner -loglevel error -show_streams -of json "$SERVED" > "$W/served-streams.json"
python3 - "$W/served-streams.json" <<'PY' || fail "served file has no embedded cover art"
import json,sys
streams=json.load(open(sys.argv[1])).get("streams",[])
assert any(s.get("disposition",{}).get("attached_pic") for s in streams), "no attached_pic stream"
print("  [ok] served file carries embedded cover art")
PY
echo "lidarr-e2e: PASS"
