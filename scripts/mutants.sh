#!/usr/bin/env bash
# Run cargo-mutants over the three logic-bearing crates with a disk budget that
# fits a small VPS. Known-good cargo-mutants version: 27.0.0.
#
# musefs-cli and musefs-fuse are intentionally out of scope (thin glue / e2e-only;
# see the remediation tracking doc).
#
# Usage: scripts/mutants.sh [crate ...]   (default: all three in-scope crates)
# Env:   MUTANTS_TMP  scratch PARENT dir, MUST be OUTSIDE this repo (default: the
#                     system temp dir). cargo-mutants copies the source tree into
#                     a build dir under TMPDIR, so a scratch dir inside the repo
#                     gets copied into itself recursively ("File name too long").
#                     On a host whose /tmp is too small (e.g. a tmpfs), point this
#                     at a roomy directory outside the repo.
#        MUTANTS_LIST set to 1 to only enumerate mutants (no build/run)
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# Scratch parent. cargo-mutants copies the source tree into a build dir under
# TMPDIR, so the scratch parent MUST live OUTSIDE this repo — a dir inside the
# tree gets copied into itself recursively until the path overflows
# ("File name too long"). Default to the system temp dir; honor MUTANTS_TMP only
# if it points outside the repo. We never delete the parent (it may be shared,
# e.g. /tmp) — only the unique per-crate children we mktemp inside it.
SCRATCH_PARENT="${MUTANTS_TMP:-${TMPDIR:-/tmp}}"
case "$SCRATCH_PARENT" in
  "$ROOT" | "$ROOT"/*)
    echo "mutants: scratch dir must be outside the repo ($ROOT); cargo-mutants" \
         "copies the tree into it. Set MUTANTS_TMP to a roomy path outside the repo." >&2
    exit 1
    ;;
esac
mkdir -p "$SCRATCH_PARENT"
CREATED_TMPS=()
cleanup() {
  # Remove only the unique per-crate scratch children we created (never the
  # possibly-shared parent), even on abnormal exit.
  [ "${#CREATED_TMPS[@]}" -gt 0 ] && rm -rf "${CREATED_TMPS[@]}"
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
  # cargo-mutants' --output does a plain mkdir of the leaf, not mkdir -p, so the
  # parent must already exist (a real run fails with "create output parent
  # directory ... No such file or directory" otherwise; --list never creates it,
  # which is why the list-only smoke test missed this).
  mkdir -p "$out"
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
  # Discovery run: surviving/timed-out mutants are the expected output, not a
  # failure. If cargo-mutants produced its report dir the crate ran fine, so
  # return success regardless of exit code; only a genuine error (no report,
  # e.g. a baseline build failure) propagates. The PR --in-diff gate calls
  # cargo-mutants directly and still fails on survivors.
  if [ -d "$out/mutants.out" ]; then
    return 0
  fi
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
