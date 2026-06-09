#!/usr/bin/env bash
# StructureOnly passthrough dd benchmark.
# Usage: sudo benches/passthrough_dd.sh <musefs-binary> <work-dir> [size-mib]
# Mounts a single large backing track StructureOnly and times a sequential
# `dd` read through the mount. StructureOnly serves raw backing bytes, so
# throughput is FUSE-path-bound and format-independent; we use a WAV backing
# file to avoid an external encoder dependency.
set -euo pipefail
BIN="$1"; WORK="$2"; SIZE_MIB="${3:-512}"
mkdir -p "$WORK/backing" "$WORK/mnt"
WAV="$WORK/backing/track.wav"
if [ ! -f "$WAV" ]; then
  # Minimal 44-byte PCM WAV header for SIZE_MIB of 16-bit stereo 44.1k data, then zero-fill.
  DATA=$(( SIZE_MIB * 1024 * 1024 )); RIFF=$(( DATA + 36 ))
  printf 'RIFF' > "$WAV"
  printf "$(printf '\\x%02x\\x%02x\\x%02x\\x%02x' $((RIFF&255)) $((RIFF>>8&255)) $((RIFF>>16&255)) $((RIFF>>24&255)))" >> "$WAV"
  printf 'WAVEfmt ' >> "$WAV"
  printf '\x10\x00\x00\x00\x01\x00\x02\x00\x44\xac\x00\x00\x10\xb1\x02\x00\x04\x00\x10\x00' >> "$WAV"
  printf 'data' >> "$WAV"
  printf "$(printf '\\x%02x\\x%02x\\x%02x\\x%02x' $((DATA&255)) $((DATA>>8&255)) $((DATA>>16&255)) $((DATA>>24&255)))" >> "$WAV"
  dd if=/dev/zero bs=1M count="$SIZE_MIB" >> "$WAV" 2>/dev/null
fi
DB="$WORK/m.db"; rm -f "$DB"
"$BIN" scan "$WORK/backing" --db "$DB" >/dev/null
"$BIN" mount "$WORK/mnt" --db "$DB" --mode structure-only --template '$title' &
MPID=$!
# Poll for mount readiness instead of a fixed sleep (the CLI gives no ready signal).
VIRT=""
for _ in $(seq 1 60); do
  VIRT=$(find "$WORK/mnt" -type f 2>/dev/null | head -1 || true)
  [ -n "$VIRT" ] && break
  sleep 0.5
done
if [ -z "$VIRT" ]; then echo "ERROR: mount never exposed a file" >&2; kill "$MPID" 2>/dev/null || true; exit 1; fi
cat "$VIRT" > /dev/null   # warm backing into page cache
for i in 1 2 3; do dd if="$VIRT" of=/dev/null bs=1M 2>&1 | tail -1; done
fusermount3 -u "$WORK/mnt" 2>/dev/null || umount "$WORK/mnt" 2>/dev/null || true
kill "$MPID" 2>/dev/null || true
