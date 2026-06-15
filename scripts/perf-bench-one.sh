#!/usr/bin/env bash
# Bench one ref's read_throughput criterion baseline and export it to JSON.
# In CI each ref benches on its own runner in parallel (base on one, PR on
# another); perf-ab-compare.sh then diffs the two exported baselines. Splitting
# trades same-machine stability for wall-clock — the A/B job is warn-only and
# already treats <10% moves as noise.
#
# Usage: scripts/perf-bench-one.sh <ref> <baseline-name> <out-json>
# Requires: cargo, critcmp on PATH. Run from the repo root with a clean tree.
set -euo pipefail

REF="${1:?ref required}"
NAME="${2:?baseline name required}"
OUT="${3:?output json path required}"
BENCH=(cargo bench -p musefs-core --bench read_throughput --)

echo "Benching $NAME ($REF)…" >&2
git checkout --quiet --detach "$REF"
"${BENCH[@]}" --save-baseline "$NAME" >/dev/null 2>&1
critcmp --export "$NAME" > "$OUT"
echo "Exported $NAME baseline to $OUT" >&2
