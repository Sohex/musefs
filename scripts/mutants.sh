#!/usr/bin/env bash
# Run cargo-mutants over the three logic-bearing crates with a disk budget that
# fits a small VPS. Known-good cargo-mutants version: 27.0.0.
#
# musefs-cli and musefs-fuse are intentionally out of scope (thin glue / e2e-only;
# see the remediation tracking doc).
#
# Usage: scripts/mutants.sh [crate ...]   (default: all three in-scope crates)
# Env:   MUTANTS_TMP  scratch PARENT dir off the /tmp tmpfs (default: ./.mutants-tmp).
#                     cargo-mutants builds inside a unique child we create here;
#                     a caller-provided parent is never deleted, only our children.
#        MUTANTS_LIST set to 1 to only enumerate mutants (no build/run)
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# Scratch parent. If the caller supplied MUTANTS_TMP we treat it as a shared
# parent and must NOT remove it on exit (could be /tmp or another shared dir);
# we only ever remove the unique children we mktemp inside it. Our own default
# repo-local parent we do clean up.
if [ -n "${MUTANTS_TMP:-}" ]; then
  SCRATCH_PARENT="$MUTANTS_TMP"; OWN_PARENT=0
else
  SCRATCH_PARENT="$ROOT/.mutants-tmp"; OWN_PARENT=1
fi
mkdir -p "$SCRATCH_PARENT"
CREATED_TMPS=()
cleanup() {
  # Remove every per-crate scratch child we created, even on abnormal exit and
  # even when the caller owns SCRATCH_PARENT (OWN_PARENT=0).
  [ "${#CREATED_TMPS[@]}" -gt 0 ] && rm -rf "${CREATED_TMPS[@]}"
  # Remove the parent only if we created it.
  [ "$OWN_PARENT" = 1 ] && rm -rf "$SCRATCH_PARENT"
}
trap cleanup EXIT

OUT_ROOT="$ROOT/mutants-out"

# Per-crate args. --test-workspace=true for musefs-db: its dependents' tests are
# cheap to build, so workspace-wide checking buys stronger mutant detection.
# =false for core/format: workspace mode pulls in criterion/proptest scratch
# builds that blew the disk/time budget in the audit; crate-local tests suffice.
#
# cargo-mutants 27.0.0 has no --target-dir; it builds inside a copy of the tree
# under TMPDIR. We point TMPDIR at a unique per-crate child so peak disk is one
# build tree at a time, removed before the next crate.
run_crate() {
  local crate="$1"; shift
  local out="$OUT_ROOT/$crate"
  local tmp
  tmp="$(mktemp -d "$SCRATCH_PARENT/${crate}.XXXXXX")" || {
    echo "mutants: mktemp -d failed under $SCRATCH_PARENT" >&2
    return 1
  }
  CREATED_TMPS+=("$tmp")
  echo "== mutants: $crate (scratch: $tmp) =="
  local args=(-p "$crate" --jobs 1 --output "$out")
  [ "${MUTANTS_LIST:-0}" = "1" ] && args+=(--list)
  args+=("$@")
  TMPDIR="$tmp" cargo mutants "${args[@]}"
  local rc=$?
  rm -rf "$tmp"
  return "$rc"
}

status=0

crates=("$@")
[ "${#crates[@]}" -eq 0 ] && crates=(musefs-db musefs-core musefs-format)

# Collect every crate's result; do NOT abort on the first failure so the
# inventory stays complete. Exit non-zero at the end if any crate failed.
for crate in "${crates[@]}"; do
  case "$crate" in
    musefs-db)
      run_crate musefs-db --test-workspace=true \
        --file musefs-db/src/schema.rs \
        --file musefs-db/src/lib.rs \
        --file musefs-db/src/tracks.rs \
        --file musefs-db/src/art.rs \
        --file musefs-db/src/tags.rs
      ;;
    musefs-core)
      run_crate musefs-core --test-workspace=false \
        --file musefs-core/src/reader.rs \
        --file musefs-core/src/tree.rs \
        --file musefs-core/src/scan.rs \
        --file musefs-core/src/facade.rs \
        --file musefs-core/src/ogg_index.rs
      ;;
    musefs-format)
      run_crate musefs-format --test-workspace=false --features fuzzing \
        --file musefs-format/src/flac.rs \
        --file musefs-format/src/mp3.rs \
        --file musefs-format/src/mp4.rs \
        --file musefs-format/src/wav.rs \
        --file musefs-format/src/ogg/mod.rs \
        --file musefs-format/src/ogg/page.rs \
        --file musefs-format/src/ogg/crc.rs \
        --file musefs-format/src/ogg/b64.rs
      ;;
    *)
      echo "unknown crate: $crate" >&2; status=1; continue
      ;;
  esac
  # cargo-mutants exits non-zero when mutants survive (2) or on error (>2).
  rc=$?
  [ "$rc" -ne 0 ] && status=$rc
done

exit "$status"
