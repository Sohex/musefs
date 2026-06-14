#!/usr/bin/env bash
# Run the full read bench plus the ci-tier ingest/refresh benches and capture
# all output to a single artifact file. Record-only; never gates a release.
#
# Usage: scripts/perf-release-bench.sh <out-file>
set -euo pipefail
OUT="${1:?output file required}"

{
  echo "# musefs release benchmark snapshot"
  echo "commit: $(git rev-parse HEAD)"
  echo

  echo "## read_throughput (criterion)"
  cargo bench -p musefs-core --bench read_throughput -- 2>&1 || true

  echo "## bench_ingest (ci tier)"
  MUSEFS_BENCH_TIER=ci cargo test --release -p musefs-core --features metrics \
    --test bench_ingest -- --ignored --nocapture 2>&1 || true

  echo "## bench_refresh (ci tier)"
  MUSEFS_BENCH_TIER=ci cargo test --release -p musefs-core \
    --test bench_refresh -- --ignored --nocapture 2>&1 || true
} | tee "$OUT"
