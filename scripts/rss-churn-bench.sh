#!/usr/bin/env bash
# Steady-state RSS churn benchmark for the musefs daemon (issue #360 part A).
# Drives concurrent open/read/release churn against a mounted store and reports
# the MEDIAN VmRSS over the flattened tail (steady state, NOT peak RSS), for the
# system-malloc and jemalloc builds, then prints a ship/investigate decision.
#
# Linux-only: VmRSS comes from /proc/<pid>/status.
#
# Env knobs (defaults in parens):
#   DB           musefs store path                         (required)
#   MOUNT        mountpoint under $HOME                     ($HOME/.musefs-rss-mnt)
#   WORKERS      concurrent reader threads                 (nproc)
#   FILES        distinct files to churn                   (500)
#   CYCLES       1-second RSS samples per variant          (200)
#   WARMUP       leading samples discarded                 (20)
#   REFRESH_CMD  shell command run every REFRESH_SECS      (none)
#   REFRESH_SECS refresh cadence in seconds                (30)
#   BINARIES     override builds: "sysmalloc=/p jemalloc=/p" (built from repo)
set -euo pipefail

if [ "$(uname -s)" != "Linux" ]; then
  echo "rss-churn-bench: Linux-only (needs /proc/<pid>/status VmRSS)" >&2
  exit 1
fi

: "${DB:?set DB to a musefs store path}"
MOUNT="${MOUNT:-$HOME/.musefs-rss-mnt}"
WORKERS="${WORKERS:-$(nproc)}"
FILES="${FILES:-500}"
CYCLES="${CYCLES:-200}"
WARMUP="${WARMUP:-20}"
REFRESH_SECS="${REFRESH_SECS:-30}"

build_variants() {
  echo "building system-malloc and jemalloc release binaries..." >&2
  cargo build --release -p musefs --no-default-features >&2
  cp target/release/musefs /tmp/musefs-sysmalloc
  cargo build --release -p musefs >&2
  cp target/release/musefs /tmp/musefs-jemalloc
  echo "sysmalloc=/tmp/musefs-sysmalloc jemalloc=/tmp/musefs-jemalloc"
}

# stdin: one integer per line -> median of the last 25% of lines.
median_tail() {
  local n tail_start
  mapfile -t vals
  n="${#vals[@]}"
  tail_start=$(( n - n / 4 ))
  printf '%s\n' "${vals[@]:tail_start}" | sort -n | awk '
    { a[NR] = $1 }
    END { if (NR % 2) print a[(NR + 1) / 2]; else print (a[NR / 2] + a[NR / 2 + 1]) / 2 }'
}

run_variant() {
  local bin="$1"
  mkdir -p "$MOUNT"
  "$bin" mount --db "$DB" "$MOUNT" &
  local mpid=$!
  for _ in $(seq 1 50); do
    mountpoint -q "$MOUNT" && break
    sleep 0.1
  done
  local targets=()
  mapfile -t targets < <(find "$MOUNT" -type f | head -n "$FILES")
  if [ "${#targets[@]}" -eq 0 ]; then
    echo "no files found under $MOUNT" >&2
    fusermount3 -u "$MOUNT" 2>/dev/null || true
    return 1
  fi
  local stop
  stop="$(mktemp)"
  rm -f "$stop"
  local pids=()
  for _ in $(seq 1 "$WORKERS"); do
    (
      while [ ! -e "$stop" ]; do
        for f in "${targets[@]}"; do
          [ -e "$stop" ] && break
          cat "$f" >/dev/null 2>&1 || true
        done
      done
    ) &
    pids+=("$!")
  done
  local rpid=""
  if [ -n "${REFRESH_CMD:-}" ]; then
    (
      while [ ! -e "$stop" ]; do
        sleep "$REFRESH_SECS"
        # shellcheck disable=SC2086
        eval $REFRESH_CMD >/dev/null 2>&1 || true
      done
    ) &
    rpid=$!
  fi
  local samples=()
  for _ in $(seq 1 "$CYCLES"); do
    sleep 1
    samples+=("$(awk '/^VmRSS:/ { print $2 }' "/proc/$mpid/status" 2>/dev/null || echo 0)")
  done
  : > "$stop"
  wait "${pids[@]}" 2>/dev/null || true
  [ -n "$rpid" ] && { wait "$rpid" 2>/dev/null || true; }
  rm -f "$stop"
  fusermount3 -u "$MOUNT" 2>/dev/null || true
  wait "$mpid" 2>/dev/null || true
  printf '%s\n' "${samples[@]:$WARMUP}" | median_tail
}

main() {
  local spec="${BINARIES:-$(build_variants)}"
  echo "label,steady_state_rss_kib"
  local sys_rss="" jem_rss="" pair label bin rss
  # shellcheck disable=SC2086
  for pair in $spec; do
    label="${pair%%=*}"
    bin="${pair#*=}"
    rss="$(run_variant "$bin")"
    echo "$label,$rss"
    case "$label" in
      *sys*) sys_rss="$rss" ;;
      *jem*) jem_rss="$rss" ;;
    esac
  done
  if [ -n "$sys_rss" ] && [ -n "$jem_rss" ]; then
    if [ "$jem_rss" -le "$sys_rss" ]; then
      echo "decision: SHIP jemalloc (steady-state ${jem_rss} kiB <= sysmalloc ${sys_rss} kiB)"
    else
      echo "decision: INVESTIGATE — jemalloc ${jem_rss} kiB > sysmalloc ${sys_rss} kiB"
    fi
  fi
}

main "$@"
