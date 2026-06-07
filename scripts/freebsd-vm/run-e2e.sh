#!/bin/sh
# Build the workspace and run the FUSE end-to-end suite on FreeBSD.
# Run from the repo root after provision.sh. Requires the fusefs kernel module
# (loaded by provision.sh), /dev/fuse, and ffmpeg.
set -eu

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

# Fail loudly if ffmpeg is missing: the playback/ogg e2e tests skip silently
# without it, which would otherwise turn a missing dependency into a vacuous
# green run. (provision.sh installs it.)
command -v ffmpeg >/dev/null 2>&1 || {
    echo "error: ffmpeg not found — playback_pcm/ogg_read_through e2e would" >&2
    echo "       silently skip. Run scripts/freebsd-vm/provision.sh first." >&2
    exit 1
}

# Full workspace (unit + integration, excludes the #[ignore]d FUSE e2e).
cargo test --workspace

# The FUSE end-to-end tests (mount/read-through + ffmpeg decode/encode
# fidelity). Passthrough-specific e2e (the `metrics`-gated tests) are Linux-only
# and intentionally NOT run here: FreeBSD has no kernel passthrough, so
# StructureOnly falls back to daemon serving (verified by the standard suite).
cargo test -p musefs-fuse -- --ignored
