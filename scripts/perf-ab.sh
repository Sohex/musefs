#!/usr/bin/env bash
# Same-runner A/B wall-clock comparison of the read_throughput criterion bench.
# Benches the base ref and HEAD back-to-back on ONE machine (robust to
# runner-to-runner variance), then diffs with critcmp. Warn-only: always exits 0.
#
# Usage: scripts/perf-ab.sh <base-sha> <out-markdown-file>
# Requires: cargo, critcmp on PATH. Run from the repo root with a clean tree.
set -euo pipefail

BASE_SHA="${1:?base sha required}"
OUT="${2:?output markdown path required}"
BENCH=(cargo bench -p musefs-core --bench read_throughput --)

head_sha="$(git rev-parse HEAD)"

run_baseline() {
  local name="$1"
  "${BENCH[@]}" --save-baseline "$name" >/dev/null 2>&1
}

echo "Benching base ($BASE_SHA)…" >&2
git checkout --quiet --detach "$BASE_SHA"
run_baseline base

echo "Benching head ($head_sha)…" >&2
git checkout --quiet --detach "$head_sha"
run_baseline pr

{
  echo "### Read-path A/B (same-runner, warn-only)"
  echo
  echo "Base \`${BASE_SHA:0:12}\` vs PR \`${head_sha:0:12}\`. Wall-clock on a"
  echo "shared GH runner — treat <10% moves as noise."
  echo
  base_n="$(critcmp --list 2>/dev/null | grep -c '^base' || true)"
  pr_n="$(critcmp --list 2>/dev/null | grep -c '^pr' || true)"
  common="$(critcmp base pr 2>/dev/null | tail -n +2 | grep -c . || true)"
  if [ "$common" -eq 0 ]; then
    echo "> ⚠️ No comparable benchmarks (benchmark IDs differ between base and PR"
    echo "> — a harness/bench rename?). Nothing to compare."
  else
    echo '```'
    critcmp base pr
    echo '```'
    echo
    echo "_base benches: ${base_n}, pr benches: ${pr_n}, compared: ${common}._"
  fi
} > "$OUT"

echo "Wrote $OUT" >&2
