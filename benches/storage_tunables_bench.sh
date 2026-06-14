#!/usr/bin/env bash
# Storage-tunables bench: measures whether --max-readahead-kib / --max-background /
# --keep-cache actually help musefs on slow backing storage. Backs the discussion in
# BENCHMARKS.md (#storage-tunables). Negative result: only --keep-cache helps.
#
# These knobs are kernel<->FUSE parameters, so they only matter through a REAL kernel
# mount with a real reader (not the in-process Criterion benches). Mode MUST be
# synthesis: structure-only triggers kernel passthrough when privileged and bypasses
# the daemon read path entirely.
#
# Usage (run as root; reads drop the page cache between samples):
#   storage_tunables_bench.sh local <backing-dir> [size_mib] [streams]
#   storage_tunables_bench.sh nfs   <export-dir>  <netem_ms_per_way> [size_mib] [streams]
#
#   local: <backing-dir> is a real disk (e.g. an HDD) holding the corpus.
#   nfs:   <export-dir> is exported via loopback NFSv4 and `tc netem` adds
#          <netem_ms_per_way> per packet (~2x that as RPC RTT). Backing it on tmpfs
#          isolates the RPC tax; on HDD adds real seeks. Needs nfs-kernel-server + tc.
set -euo pipefail

MODE="${1:?usage: $0 local|nfs <dir> ...}"
BIN="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)/target/release/musefs"
[ -x "$BIN" ] || { echo "build first: cargo build --release -p musefs" >&2; exit 1; }

NFSMNT=/tmp/sp-nfsmnt
MMNT=/tmp/sp-musefs-mnt
DB=/tmp/sp-tunables.db
MP=""; NETEM=0; NFS_EXP=""

cleanup() {
  [ -n "$MP" ] && kill "$MP" 2>/dev/null || true
  sleep 0.5
  [ "$NETEM" = 1 ] && tc qdisc del dev lo root 2>/dev/null || true
  mountpoint -q "$MMNT" && { fusermount3 -u "$MMNT" 2>/dev/null || umount -l "$MMNT" 2>/dev/null; } || true
  if [ -n "$NFS_EXP" ]; then
    mountpoint -q "$NFSMNT" && umount -l "$NFSMNT" 2>/dev/null || true
    exportfs -u localhost:"$NFS_EXP" 2>/dev/null || true
  fi
}
trap cleanup EXIT

make_wav() { local p="$1" m="$2" d r; [ -f "$p" ] && return 0
  d=$(( m*1024*1024 )); r=$(( d+36 ))
  # shellcheck disable=SC2059  # generated hex-escape format string, by design
  { printf 'RIFF'; printf "$(printf '\\x%02x\\x%02x\\x%02x\\x%02x' $((r&255)) $((r>>8&255)) $((r>>16&255)) $((r>>24&255)))"
    printf 'WAVEfmt \x10\x00\x00\x00\x01\x00\x02\x00\x44\xac\x00\x00\x10\xb1\x02\x00\x04\x00\x10\x00data'
    printf "$(printf '\\x%02x\\x%02x\\x%02x\\x%02x' $((d&255)) $((d>>8&255)) $((d>>16&255)) $((d>>24&255)))"
    # Incompressible payload: a /dev/zero corpus on a compressing fs (btrfs
    # compress=zstd) collapses to a tiny cached extent, so cold reads never hit
    # the platter and the "HDD" rows measure decompression, not backing latency.
    dd if=/dev/urandom bs=1M count="$m" 2>/dev/null; } >> "$p"; }

gen_corpus() { # $1=backing-dir $2=size_mib $3=streams
  mkdir -p "$1"
  # Real-file corpus: with MUSEFS_BENCH_CORPUS_SRC set, copy the largest audio
  # files from that tree (biggest first, streams+1 of them) instead of synthetic
  # WAVs. Real already-compressed audio is incompressible and representative, so
  # a compressing backing fs can't collapse it into a cached extent. `cp -n` makes
  # reruns reuse the copy.
  if [ -n "${MUSEFS_BENCH_CORPUS_SRC:-}" ]; then
    # Already populated (a prior run)? Skip the rescan of the (large) source tree.
    if find "$1" -maxdepth 1 -type f \( -iname '*.flac' -o -iname '*.mp3' \) 2>/dev/null | grep -q .; then
      return 0
    fi
    local n=$(( $3 + 1 ))
    # Optional size cap (MiB): pick the biggest real tracks UNDER it. High-RTT NFS
    # rows need a normal ~50 MiB track (a 1 GiB file would take ~15 min/sample);
    # local rows leave it unset to get one large file for a long, stable read.
    local maxb=0
    [ -n "${MUSEFS_BENCH_CORPUS_MAX_MIB:-}" ] && maxb=$(( MUSEFS_BENCH_CORPUS_MAX_MIB * 1024 * 1024 ))
    # Capture-then-copy: `head` closing the pipe early would SIGPIPE `find`/`sort`
    # and trip `pipefail`, so collect the list in a substitution (|| true) first.
    local list
    list=$(find "$MUSEFS_BENCH_CORPUS_SRC" -type f \
      \( -iname '*.flac' -o -iname '*.mp3' -o -iname '*.m4a' -o -iname '*.ogg' \) \
      -printf '%s\t%p\n' 2>/dev/null | awk -v m="$maxb" -F'\t' 'm==0 || $1<=m' |
      sort -rn | head -n "$n" | cut -f2-) || true
    while IFS= read -r f; do
      [ -n "$f" ] && { cp -n "$f" "$1/" 2>/dev/null || true; }
    done <<< "$list"
    return 0
  fi
  make_wav "$1/big.wav" "$2"
  local i; for i in $(seq 1 "$3"); do make_wav "$1/t$i.wav" 32; done
}

case "$MODE" in
  local)
    BACKING="${2:?need backing dir}"; SIZE="${3:-512}"; STREAMS="${4:-16}"
    gen_corpus "$BACKING/backing" "$SIZE" "$STREAMS"
    SCANDIR="$BACKING/backing"
    echo "local backing=$BACKING ($(stat -f -c %T "$BACKING"))  size=${SIZE}MiB  streams=$STREAMS" ;;
  nfs)
    NFS_EXP="${2:?need export dir}"; NETEM_MS="${3:?need netem ms/way}"; SIZE="${4:-96}"; STREAMS="${5:-16}"
    gen_corpus "$NFS_EXP/backing" "$SIZE" "$STREAMS"
    mkdir -p "$NFSMNT"
    systemctl start nfs-server 2>/dev/null || true
    exportfs -o rw,sync,no_subtree_check,insecure,no_root_squash localhost:"$NFS_EXP"
    mount -t nfs -o vers=4.2 localhost:"$NFS_EXP" "$NFSMNT"
    SCANDIR="$NFSMNT/backing"
    echo "nfs export=$NFS_EXP ($(stat -f -c %T "$NFS_EXP"))  netem=${NETEM_MS}ms/way  size=${SIZE}MiB  streams=$STREAMS" ;;
  *) echo "unknown mode: $MODE" >&2; exit 2 ;;
esac

mkdir -p "$MMNT"
rm -f "$DB"; "$BIN" scan "$SCANDIR" --db "$DB" >/dev/null   # scan before adding netem
if [ "$MODE" = nfs ]; then tc qdisc add dev lo root netem delay "${NETEM_MS}ms"; NETEM=1; fi

# shellcheck disable=SC2016  # '$title' is a musefs output-template literal, not a shell var
mount_mode() { local m="$1"; shift; "$BIN" mount "$MMNT" --db "$DB" --mode "$m" --template '$title' "$@" >/dev/null 2>&1 & MP=$!
  local f=""; for _ in $(seq 1 60); do f=$(find "$MMNT" -type f 2>/dev/null|head -1); [ -n "$f" ] && break; sleep 0.25; done; }
mount_m() { mount_mode synthesis "$@"; }
umount_m() { kill "$MP" 2>/dev/null||true; for _ in $(seq 1 20); do kill -0 "$MP" 2>/dev/null||break; sleep 0.25; done; MP=""; }
drop() { sync; echo 3 > /proc/sys/vm/drop_caches; }
biggest() { find "$MMNT" -type f -printf '%s\t%p\n'|sort -rn|head -1|cut -f2-; }
secs() { dd if="$1" of=/dev/null bs=1M 2>&1|tail -1|awk '{for(i=1;i<=NF;i++) if($i=="copied,") print $(i+1)}'; }
cold_mbps() { local v; v="$(biggest)"; local o
  o=$(for _ in 1 2 3; do drop
    dd if="$v" of=/dev/null bs=1M 2>&1|tail -1|awk '{b=$1}{for(i=1;i<=NF;i++) if($i=="copied,")s=$(i+1)}END{if(s>0)printf "%.1f\n",b/1e6/s}'
  done|sort -n|sed -n 2p); echo "${o:-0}"; }

echo "## read_ahead_budget (cold single-stream MB/s; issue #255)"
printf '%-24s %10s\n' config MBps
# off (ra=0) vs the default Phase-1 read amplification (ra=64, prefetch off).
for rb in 0 64; do mount_m --read-ahead-budget-mib "$rb"; printf '%-24s %10s\n' "synthesis ra=${rb}MiB" "$(cold_mbps)"; umount_m; done
# Phase 1+2: opt into the background prefetch threads (off by default; #255 found
# they add overhead without benefit, so this row should not beat ra=64 alone).
mount_m --read-ahead-budget-mib 64 --read-ahead-prefetch
printf '%-24s %10s\n' "synthesis ra=64 +prefetch" "$(cold_mbps)"; umount_m
# Reference ceiling: structure-only triggers kernel FUSE passthrough when
# privileged, serving the backing fd directly (no daemon, no daemon read-ahead).
mount_mode structure-only; printf '%-24s %10s\n' "passthrough (kernel)" "$(cold_mbps)"; umount_m

echo "## max_readahead-kib (cold single-stream MB/s)"
printf '%-16s %10s\n' readahead MBps
for ra in 512 2048 4096; do mount_m --max-readahead-kib "$ra"; printf '%-16s %10s\n' "$ra" "$(cold_mbps)"; umount_m; done

echo "## max_background ($STREAMS concurrent cold streams, wall s)"
printf '%-16s %10s\n' max_background wall_s
for mb in 64 128; do
  mount_m --max-background "$mb"
  mapfile -t files < <(find "$MMNT" -type f ! -path "$(biggest)")
  drop; t0=$(date +%s.%N); pids=()
  for f in "${files[@]:0:$STREAMS}"; do dd if="$f" of=/dev/null bs=1M 2>/dev/null & pids+=("$!"); done
  wait "${pids[@]}"; t1=$(date +%s.%N); umount_m
  printf '%-16s %10s\n' "$mb" "$(awk -v a="$t0" -v b="$t1" 'BEGIN{printf "%.2f",b-a}')"
done

echo "## keep_cache (cold then reopen, s)"
printf '%-16s %10s %10s\n' keep_cache cold_s reopen_s
for kc in false true; do
  mount_m --keep-cache "$kc"; v="$(biggest)"; drop
  c=$(secs "$v"); r=$(secs "$v"); umount_m
  printf '%-16s %10s %10s\n' "$kc" "$c" "$r"
done
