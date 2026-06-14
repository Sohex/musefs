#!/usr/bin/env bash
# Runs the .musefs-metrics e2e mount test (#394). Requires /dev/fuse + libfuse.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo test -p musefs-fuse --features metrics --test metrics_e2e -- --ignored --nocapture
