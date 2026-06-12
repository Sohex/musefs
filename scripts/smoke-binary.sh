#!/bin/sh
# Smoke-test a built musefs binary end-to-end: generate a tagged FLAC, scan it,
# mount the binary, read the synthesized file through the mount, then SIGTERM the
# daemon and assert the mount unmounts cleanly.
#
# POSIX sh (runs under bash and busybox ash). Requires on PATH: ffmpeg,
# fusermount3 (fuse3 package), and /dev/fuse present.
#
# Usage: scripts/smoke-binary.sh /path/to/musefs
set -eu

if [ $# -lt 1 ]; then echo "Usage: $0 /path/to/musefs"; exit 1; fi

MUSEFS="$1"
WORK="$(mktemp -d)"
cleanup() { fusermount3 -u "$WORK/mnt" 2>/dev/null || true; rm -rf "$WORK"; }
trap cleanup EXIT

echo "smoke: musefs = $MUSEFS"

# Validate the binary actually runs before doing any FUSE work.
if ! "$MUSEFS" scan --help >/dev/null 2>&1; then
  echo "FAIL: $MUSEFS did not run"; exit 1
fi

mkdir -p "$WORK/backing" "$WORK/mnt"

# 1s tagged FLAC fixture. Tags must cover the default virtual-tree template
# ($albumartist/$album/$title) or the served path falls back to "Unknown ...".
ffmpeg -hide_banner -loglevel error -f lavfi -i "sine=frequency=440:duration=1" \
  -metadata album_artist=Alice -metadata album=Greatest -metadata title=Song \
  "$WORK/backing/a.flac"

"$MUSEFS" scan "$WORK/backing" --db "$WORK/smoke.db"

"$MUSEFS" mount "$WORK/mnt" --db "$WORK/smoke.db" &
PID=$!

SONG="$WORK/mnt/Alice/Greatest/Song.flac"
i=0
while [ ! -f "$SONG" ]; do
  i=$((i + 1))
  if [ "$i" -gt 30 ]; then echo "FAIL: mount did not come up"; exit 1; fi
  sleep 1
done

# Served file must be a real, non-empty FLAC (magic 'fLaC').
MAGIC="$(head -c 4 "$SONG")"
if [ "$MAGIC" != "fLaC" ]; then echo "FAIL: served file is not FLAC (magic='$MAGIC')"; exit 1; fi
BYTES="$(wc -c < "$SONG")"
if [ "$BYTES" -le 0 ]; then echo "FAIL: served file is empty"; exit 1; fi
echo "smoke: read $BYTES bytes from $SONG (fLaC OK)"

# Exercise the SIGTERM graceful-unmount handler on the real binary.
kill -TERM "$PID"
i=0
while kill -0 "$PID" 2>/dev/null; do
  i=$((i + 1))
  if [ "$i" -gt 30 ]; then echo "FAIL: daemon did not exit after SIGTERM"; exit 1; fi
  sleep 1
done
wait "$PID" 2>/dev/null || true

if [ -f "$SONG" ]; then echo "FAIL: mount still present after SIGTERM"; exit 1; fi
echo "smoke: SIGTERM unmounted cleanly — PASS"
