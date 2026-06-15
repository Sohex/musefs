#!/usr/bin/env bash
# Diff two exported critcmp baselines into a warn-only markdown report. The
# baselines may come from one machine (perf-ab.sh) or two separate CI runners
# (perf-bench-one.sh); see those for context.
#
# Usage: scripts/perf-ab-compare.sh <base-json> <pr-json> <base-sha> <head-sha> <out-md> [note]
# Requires: critcmp on PATH.
set -euo pipefail

BASE_JSON="${1:?base baseline json required}"
PR_JSON="${2:?pr baseline json required}"
BASE_SHA="${3:?base sha required}"
HEAD_SHA="${4:?head sha required}"
OUT="${5:?output markdown path required}"
NOTE="${6:-Treat <10% moves as noise.}"

{
  echo "### Read-path A/B (warn-only)"
  echo
  echo "Base \`${BASE_SHA:0:12}\` vs PR \`${HEAD_SHA:0:12}\`. $NOTE"
  echo
  base_n="$(critcmp "$BASE_JSON" 2>/dev/null | tail -n +2 | grep -c . || true)"
  pr_n="$(critcmp "$PR_JSON" 2>/dev/null | tail -n +2 | grep -c . || true)"
  common="$(critcmp "$BASE_JSON" "$PR_JSON" 2>/dev/null | tail -n +2 | grep -c . || true)"
  if [ "$common" -eq 0 ]; then
    echo "> ⚠️ No comparable benchmarks (benchmark IDs differ between base and PR"
    echo "> — a harness/bench rename?). Nothing to compare."
  else
    echo '```'
    critcmp "$BASE_JSON" "$PR_JSON"
    echo '```'
    echo
    echo "_base benches: ${base_n}, pr benches: ${pr_n}, compared: ${common}._"
  fi
} > "$OUT"

echo "Wrote $OUT" >&2
