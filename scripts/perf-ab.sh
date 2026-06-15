#!/usr/bin/env bash
# Same-runner A/B wall-clock comparison of the read_throughput criterion bench.
# Benches the base ref and HEAD back-to-back on ONE machine (robust to
# runner-to-runner variance), then diffs with critcmp. This is the local-dev /
# single-machine entry point; CI instead splits the two bench runs across
# separate runners (perf-bench-one.sh) for wall-clock and joins them with the
# same perf-ab-compare.sh. The A/B job is informational (excluded from the ci-ok
# gate); a build/bench failure surfaces as a red job rather than being swallowed.
#
# Usage: scripts/perf-ab.sh <base-sha> <out-markdown-file>
# Requires: cargo, critcmp on PATH. Run from the repo root with a clean tree.
set -euo pipefail

BASE_SHA="${1:?base sha required}"
OUT="${2:?output markdown path required}"
here="$(dirname "$0")"

head_sha="$(git rev-parse HEAD)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

"$here/perf-bench-one.sh" "$BASE_SHA" base "$tmp/base.json"
"$here/perf-bench-one.sh" "$head_sha" pr "$tmp/pr.json"

"$here/perf-ab-compare.sh" "$tmp/base.json" "$tmp/pr.json" "$BASE_SHA" "$head_sha" "$OUT" \
  "Benched back-to-back on one machine. Treat <10% moves as noise."
