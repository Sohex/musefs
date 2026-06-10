#!/usr/bin/env bash
# Python -> Rust external-writer contract round trip (#204). Single source of
# truth: CI's `contract` job and local runs invoke this same script (mirrors
# scripts/freebsd-vm/). Ownership split is enforced by construction: `musefs
# scan` writes the track geometry; python-musefs writes the tags/art; Rust
# synthesizes; mutagen reads back.
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo_root"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
backing="$work/backing"; mkdir -p "$backing"
db="$work/musefs.db"
out="$work/synth"; mkdir -p "$out"

bin="${MUSEFS_BIN:-$repo_root/target/debug/musefs}"
if [ ! -x "$bin" ]; then
  echo "building musefs binary..."
  cargo build
  bin="$repo_root/target/debug/musefs"
fi

# 1. Real audio fixtures (ffmpeg is installed on this CI tier).
ffmpeg -nostdin -loglevel error -f lavfi -i "sine=frequency=440:duration=1" -c:a flac "$backing/track.flac"
ffmpeg -nostdin -loglevel error -f lavfi -i "sine=frequency=660:duration=1" -c:a libmp3lame "$backing/track.mp3"

# 2. Scan owns the track geometry (an external writer cannot create it).
"$bin" scan "$backing" --db "$db"

# 3. python-musefs writes the tags/art it owns.
python scripts/contract_writer.py "$db"

# 4. Rust synthesizes the served bytes from the externally-written DB.
MUSEFS_DB="$db" MUSEFS_INTEROP_DIR="$out" \
  cargo test -p musefs-core --test contract_emit -- --ignored emit_contract_fixtures

# 5. An independent reader confirms the Python tags/art survived synthesis.
MUSEFS_INTEROP_DIR="$out" python -m pytest tests/contract -v

echo "contract round trip OK"
