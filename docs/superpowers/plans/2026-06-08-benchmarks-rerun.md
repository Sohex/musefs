# BENCHMARKS.md Apples-to-Apples Rerun — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Re-measure every benchmark pass in `BENCHMARKS.md` on this box under one documented methodology (PR-isolated before/after pairs + a cumulative `16caba4`→`main` summary), then rewrite `BENCHMARKS.md` in a layered structure.

**Architecture:** All measurements run in a dedicated scratch git worktree on `/data` (detached HEAD per commit, shared `CARGO_TARGET_DIR`) so the primary worktree — which holds this branch, the spec, and the final `BENCHMARKS.md` — is never disturbed. Raw stdout and a running results table live under `/data/musefs-bench/` (outside the repo) for crash-resumability; nothing is committed until the final rewrite. Each pass is one task; "before" = the pass's parent commit, "after" = its squash-merge commit. For the two passes whose "before" predates the bench they report (SP2, SP4), the after-commit's harness file is overlaid onto the before checkout.

**Tech Stack:** Rust workspace (`cargo test --release` ignored benches `bench_ingest`/`bench_refresh`; Criterion `read_throughput`), `musefs-latencyfs` latency injection, FUSE passthrough via the `musefs` CLI (sudo, kernel 7.0 ✓), git worktrees.

**This is a measurement + documentation plan, not a code-change plan.** No optimization code or existing harness logic is modified. The only file the repo gains is `benches/passthrough_dd.sh` (Task 10) and the rewritten `BENCHMARKS.md` (Task 12). Reading the spec first is required: `docs/superpowers/specs/2026-06/2026-06-08-benchmarks-rerun-design.md`.

**Runtime warning:** This is many hours of machine time (≥22 release rebuilds across commits + a ~30 GiB corpus generation + Criterion sampling). Run bench commands in the **foreground** and let them finish — never poll a background deps-binary. Record each number into `/data/musefs-bench/results.md` as soon as a run finishes so a crash loses at most one run.

---

## Resolved commit anchors (from the spec)

| Pass | After (squash-merge) | Before (`<after>^`) | Harness | Overlay? |
|------|----------------------|---------------------|---------|----------|
| SP1 ingest | `ccbbfaa` | `ccbbfaa^` | `bench_ingest` | no |
| SP2 refresh | `ed5f380` | `ed5f380^` | `bench_refresh` | **yes** |
| SP3 read | `e8d56bd` | `e8d56bd^` | `read_throughput` | no |
| SP4 ogg serve | `a62453b` | `a62453b^` | `read_throughput` (+latency) | **yes** |
| #69 refresh | `e7ae912` | `e7ae912^` | `bench_refresh` | no |
| #114 root fan-out | `0881b31` | `0881b31^` | `bench_refresh` | no |
| PR2 scan pair | `2d4faf3` | `2d4faf3^` | `bench_ingest` | no |
| PR3 serve copies | `32be8f0` | `32be8f0^` | `read_throughput` | no |
| #136 HeaderCache | `2e6674e` | `2e6674e^` | `read_throughput` | no |
| #112 passthrough | `faec017` | `0881b31` (pre) | `dd` script (sudo) | n/a |
| Cumulative | current `main` | `16caba4` | all three (current-`main` harness both ends) | n/a |

---

## Conventions (apply in every measurement task)

All measurement commands run **inside the scratch worktree** `/data/musefs-bench/wt` with these exports already set (Task 0.2 establishes them):

```bash
export CARGO_TARGET_DIR=/data/musefs-bench/target   # build artifacts on /data, reused across commits
export WT=/data/musefs-bench/wt
export RESULTS=/data/musefs-bench/results.md
export RAW=/data/musefs-bench/raw
cd "$WT"
```

**Checkout-before / checkout-after** (no overlay):
```bash
git -C "$WT" checkout --detach <commit> && git -C "$WT" reset --hard <commit> && git -C "$WT" clean -fdx -e target
```
(`clean -fdx -e target` removes stray files but keeps the build dir; `CARGO_TARGET_DIR` is outside `$WT` anyway.)

**Overlay** (SP2, SP4 — measure old code with the new harness):
```bash
git -C "$WT" checkout <after_commit> -- <bench_file>   # after the before-checkout above
```

**Build (release):** `cargo build --release` for the relevant crate, or let the bench/test command build implicitly.

**Storage selection:**
- `bench_ingest` honors `MUSEFS_BENCH_DIR` → set `/data/musefs-bench/corpus` (durable) or `/dev/shm/musefs-bench` (RAM).
- `bench_refresh` / `read_throughput` ignore `MUSEFS_BENCH_DIR`; they follow `TMPDIR` → `export TMPDIR=/dev/shm` for RAM rows.

**Capture:** tee every run to `$RAW/<pass>-<side>-<n>.txt` and transcribe the key number(s) into `$RESULTS` under that pass's heading.

**Run counts:** ignored-test benches → 3 runs, record median (+ spread if noisy). Criterion → its own sampling via `--save-baseline <pass>-before` then `--baseline <pass>-before`; **namespace the baseline name per task** (`sp3-before`, `sp4-before`, …) so a crash mid-plan can't compare against a clobbered baseline (all read_throughput tasks share one `CARGO_TARGET_DIR/criterion`). Record Criterion's reported median + change %/p-value.

---

## Phase 0 — Environment setup

### Task 0.1: Create the durable bench directory on /data

**Files:** none (filesystem setup).

- [ ] **Step 1: Create and own the dir (sudo).**

Run:
```bash
sudo mkdir -p /data/musefs-bench && sudo chown "$USER" /data/musefs-bench
mkdir -p /data/musefs-bench/raw /data/musefs-bench/corpus
```
Expected: `/data/musefs-bench` writable by you (`test -w /data/musefs-bench && echo ok` → `ok`).

- [ ] **Step 2: Verify free space.**

Run: `df -h /data | tail -1`
Expected: ≥ ~60 GiB free (bandwidth corpus ~30 GiB + DB + headroom). Abort and tell the user if not.

### Task 0.2: Create the scratch worktree and results scaffold

**Files:**
- Create: `/data/musefs-bench/wt` (git worktree)
- Create: `/data/musefs-bench/results.md`

- [ ] **Step 1: Add a detached worktree off the primary repo.**

Run:
```bash
git -C /home/cfutro/git/musefs worktree add --detach /data/musefs-bench/wt main
```
Expected: `Preparing worktree (detached HEAD ...)`. Verify: `git -C /data/musefs-bench/wt rev-parse --short HEAD`.

- [ ] **Step 2: Persist the exports for the session.**

Run:
```bash
cat > /data/musefs-bench/env.sh <<'EOF'
export CARGO_TARGET_DIR=/data/musefs-bench/target
export WT=/data/musefs-bench/wt
export RESULTS=/data/musefs-bench/results.md
export RAW=/data/musefs-bench/raw
EOF
```
Source it (`source /data/musefs-bench/env.sh`) at the start of every measurement task.

- [ ] **Step 3: Seed the results file.**

Run:
```bash
printf '# Benchmark rerun raw results (%s)\n\n' "$(git -C /home/cfutro/git/musefs rev-parse --short HEAD)" > /data/musefs-bench/results.md
```
Expected: file exists. All later tasks append to it.

### Task 0.3: Record the machine header facts

**Files:**
- Modify: `/data/musefs-bench/results.md` (append a `## Machine` block)

- [ ] **Step 1: Capture the header values verbatim for the rewrite.**

Run and paste output under `## Machine` in `$RESULTS`:
```bash
{ echo '## Machine'; nproc; free -h | awk '/^Mem:/{print "RAM total "$2}'; rustc --version; findmnt -no FSTYPE,SOURCE,TARGET /data; findmnt -no FSTYPE,TARGET /dev/shm; uname -sr; } 
```
Expected: 8 cores, ~32 GB, rustc 1.96, `/data` btrfs (2-device span, Data:single/Metadata:RAID1 — already known from the spec), `/dev/shm` tmpfs, Linux 7.0. These populate the rewritten **Methodology → Machine** block.

---

## Phase 1 — Measurements

### Task 1: SP1 — Ingestion scalability (`ccbbfaa^` → `ccbbfaa`)

Four sub-measurements. `bench_ingest`, `--features metrics`. No overlay.

**Files:** read-only runs; appends to `$RESULTS`.

- [ ] **Step 1: Source env and checkout BEFORE.**
```bash
source /data/musefs-bench/env.sh; cd "$WT"
git -C "$WT" checkout --detach ccbbfaa^ && git -C "$WT" reset --hard ccbbfaa^ && git -C "$WT" clean -fdx -e target
```
Expected: detached at `ccbbfaa^`.

- [ ] **Step 2: §1 — durable small-files, per-format sweep (BEFORE), 3 runs.**

`corpus.rs::generate()` does `create_dir_all` but never clears the target dir, so a tier left behind would pollute the next scan. Clear before each tier:
```bash
rm -rf /data/musefs-bench/corpus/*
for n in 1 2 3; do MUSEFS_BENCH_TIER=ci MUSEFS_BENCH_DIR=/data/musefs-bench/corpus \
  cargo test --release -p musefs-core --features metrics --test bench_ingest \
  -- --ignored --nocapture bench_cold_scan_and_revalidate 2>&1 | tee "$RAW/sp1-s1-before-$n.txt"; done
```
Expected: per-format `wall_ms` / `scan_bytes_read` lines. Record per-format median scan ms under `## SP1 §1` in `$RESULTS` (before column).

- [ ] **Step 3: §2 — durable bandwidth tier (BEFORE), 1 run (~30 GiB, long).**
```bash
rm -rf /data/musefs-bench/corpus/*
MUSEFS_BENCH_TIER=bandwidth MUSEFS_BENCH_FORMAT_MIX=flac MUSEFS_BENCH_DIR=/data/musefs-bench/corpus \
  cargo test --release -p musefs-core --features metrics --test bench_ingest \
  -- --ignored --nocapture bench_cold_scan_and_revalidate 2>&1 | tee "$RAW/sp1-s2-before.txt"
```
Expected: scan wall ms, `bytes_read`, peak RSS, revalidate ms. Record under `## SP1 §2`. Then `rm -rf /data/musefs-bench/corpus/*` to reclaim space.

- [ ] **Step 4: §3 — fsync count under latencyfs (BEFORE), 3 runs.**
```bash
for n in 1 2 3; do MUSEFS_BENCH_LATENCY_PROFILE=ssd MUSEFS_BENCH_TIER=ci MUSEFS_BENCH_FORMAT_MIX=flac \
  cargo test --release -p musefs-core --features metrics --test bench_ingest \
  bench_scan_under_latency -- --ignored --nocapture 2>&1 | tee "$RAW/sp1-s3-before-$n.txt"; done
```
Expected: `fsyncs` count + scan wall ms. Record median under `## SP1 §3`. Note in `$RESULTS`: wall-ms is box-relative (rotational `/data`), the **fsync count** is the portable signal.

- [ ] **Step 5: §4 — compute-isolated large-compute on RAM (BEFORE), 3 runs, both jobs settings.**
```bash
for n in 1 2 3; do MUSEFS_BENCH_TIER=large-compute MUSEFS_BENCH_FORMAT_MIX=flac MUSEFS_BENCH_DIR=/dev/shm/musefs-bench \
  cargo test --release -p musefs-core --features metrics --test bench_ingest \
  -- --ignored --nocapture bench_cold_scan_and_revalidate 2>&1 | tee "$RAW/sp1-s4-before-$n.txt"; rm -rf /dev/shm/musefs-bench; done
```
Expected: scan wall ms + peak RSS (default `--jobs`). Record under `## SP1 §4`. (The harness reads `MUSEFS_BENCH_JOBS`; for the §4 `--jobs 1` row, repeat this step with `MUSEFS_BENCH_JOBS=1` prefixed and record separately.)

- [ ] **Step 6: Checkout AFTER and repeat Steps 2–5.**
```bash
git -C "$WT" checkout --detach ccbbfaa && git -C "$WT" reset --hard ccbbfaa && git -C "$WT" clean -fdx -e target
```
Re-run Steps 2–5 writing `-after-` raw files; record the after columns in `$RESULTS`. Compute speedups (before/after) per row.

- [ ] **Step 7: Sanity-check the direction.**
Confirm in `$RESULTS`: §1 durable shows large speedups (after ≪ before), §3 shows after fsyncs → 0, §4 RAM shows after *slower* (the honest trade). If any direction is inverted vs the old file, note it as a finding rather than silently recording.

### Task 2: SP2 — Incremental tree refresh (`ed5f380^` → `ed5f380`, OVERLAY)

`bench_refresh`, RAM (`TMPDIR=/dev/shm`). The `bench_refresh_one_across_library_sizes` sweep does not exist at `ed5f380^`; overlay the after harness.

- [ ] **Step 1: Checkout BEFORE + overlay the after harness.**
```bash
source /data/musefs-bench/env.sh; cd "$WT"; export TMPDIR=/dev/shm
git -C "$WT" checkout --detach 'ed5f380^' && git -C "$WT" reset --hard 'ed5f380^' && git -C "$WT" clean -fdx -e target
git -C "$WT" checkout ed5f380 -- musefs-core/tests/bench_refresh.rs
```
Expected: `bench_refresh.rs` now contains `bench_refresh_one_across_library_sizes`.

- [ ] **Step 2: Verify the overlaid harness compiles against the old code.**
```bash
cargo test --release -p musefs-core --test bench_refresh --no-run 2>&1 | tee "$RAW/sp2-overlay-build.txt"
```
Expected: PASS (compiles). **If it fails to compile** (the sweep calls an API absent at `ed5f380^`): record the failure in `$RESULTS`, fall back to reporting SP2's before column as "not reproducible — see git history" and proceed with only the after numbers + the cumulative comparison. Do not fabricate.

- [ ] **Step 3: Run the sweep (BEFORE), 3 runs.**
```bash
for n in 1 2 3; do cargo test -p musefs-core --release --test bench_refresh \
  bench_refresh_one_across_library_sizes -- --ignored --nocapture 2>&1 | tee "$RAW/sp2-before-$n.txt"; done
```
Expected: refresh-1 wall ms per library size (100/1000/5000). Record medians under `## SP2`.

- [ ] **Step 4: Checkout AFTER (no overlay needed — harness is native there) and run 3×.**
```bash
git -C "$WT" checkout --detach ed5f380 && git -C "$WT" reset --hard ed5f380 && git -C "$WT" clean -fdx -e target
for n in 1 2 3; do cargo test -p musefs-core --release --test bench_refresh \
  bench_refresh_one_across_library_sizes -- --ignored --nocapture 2>&1 | tee "$RAW/sp2-after-$n.txt"; done
```
Record the after column. Keep the Stage-A/B "why" note (spec: SP2 detail retained).

### Task 3: SP3 — Read/serve residuals (`e8d56bd^` → `e8d56bd`)

Criterion `read_throughput`, RAM. Benches: `sequential_read`, `concurrent_read_walk` (both exist at `e8d56bd^` — no overlay).

- [ ] **Step 1: Checkout BEFORE, save Criterion baseline.**
```bash
source /data/musefs-bench/env.sh; cd "$WT"; export TMPDIR=/dev/shm
git -C "$WT" checkout --detach 'e8d56bd^' && git -C "$WT" reset --hard 'e8d56bd^' && git -C "$WT" clean -fdx -e target
cargo bench -p musefs-core --bench read_throughput -- --save-baseline sp3-before sequential_read concurrent_read_walk 2>&1 | tee "$RAW/sp3-before.txt"
```
Expected: per-format `sequential_read` medians + `m16_plus_walker`.

- [ ] **Step 2: Checkout AFTER, compare to baseline.**
```bash
git -C "$WT" checkout --detach e8d56bd && git -C "$WT" reset --hard e8d56bd && git -C "$WT" clean -fdx -e target
cargo bench -p musefs-core --bench read_throughput -- --baseline sp3-before sequential_read concurrent_read_walk 2>&1 | tee "$RAW/sp3-after.txt"
```
Expected: Criterion prints change % + p-value per bench id. Record under `## SP3` (per-format Δ + the >10%-rise gate check + m16 contention Δ).

### Task 4: SP4 — Storage-aware Ogg serving (`a62453b^` → `a62453b`, OVERLAY)

Criterion `read_throughput` (`cold_first_read`/`seek_read` added by SP4 → overlay) + the latency-injected read (`bench_read_under_latency`, nfs-hdd).

- [ ] **Step 1: Checkout BEFORE + overlay the after harness; verify it builds.**
```bash
source /data/musefs-bench/env.sh; cd "$WT"; export TMPDIR=/dev/shm
git -C "$WT" checkout --detach 'a62453b^' && git -C "$WT" reset --hard 'a62453b^' && git -C "$WT" clean -fdx -e target
git -C "$WT" checkout a62453b -- musefs-core/benches/read_throughput.rs
cargo bench -p musefs-core --bench read_throughput --no-run 2>&1 | tee "$RAW/sp4-overlay-build.txt"
```
Expected: compiles. **If not** (new bench calls SP4-only API): record the failure, fall back to before = the eager-index numbers from git history (labelled non-reproduced), proceed with after + cumulative. Do not fabricate.

- [ ] **Step 2: Save BEFORE baseline (ogg-focused, all formats for context).**
```bash
cargo bench -p musefs-core --bench read_throughput -- --save-baseline sp4-before cold_first_read seek_read sequential_read 2>&1 | tee "$RAW/sp4-before.txt"
```

- [ ] **Step 3: Checkout AFTER, compare.**
```bash
git -C "$WT" checkout --detach a62453b && git -C "$WT" reset --hard a62453b && git -C "$WT" clean -fdx -e target
cargo bench -p musefs-core --bench read_throughput -- --baseline sp4-before cold_first_read seek_read sequential_read 2>&1 | tee "$RAW/sp4-after.txt"
```
Expected: ogg `cold_first_read`/`seek_read` big wins; other formats flat. Record under `## SP4`.

- [ ] **Step 4: Latency-injected reads (both sides), nfs-hdd.**
```bash
# AFTER is checked out; run it, then re-checkout before+overlay and run.
MUSEFS_BENCH_LATENCY_PROFILE=nfs-hdd cargo test --release -p musefs-core --features metrics --test bench_ingest \
  bench_read_under_latency -- --ignored --nocapture 2>&1 | tee "$RAW/sp4-lat-after.txt"
git -C "$WT" checkout --detach 'a62453b^' && git -C "$WT" reset --hard 'a62453b^' && git -C "$WT" clean -fdx -e target
MUSEFS_BENCH_LATENCY_PROFILE=nfs-hdd cargo test --release -p musefs-core --features metrics --test bench_ingest \
  bench_read_under_latency -- --ignored --nocapture 2>&1 | tee "$RAW/sp4-lat-before.txt"
```
Expected: `read_whole_cold` / `read_seek_cold` wall ms + pread counts. Record under `## SP4 (latency)`. Keep the crc linear↔matrix↔memo "why" table from the old file (spec: SP4 detail retained — it is narrative, not a remeasured number).

### Task 5: Phase 6 PR1 — Refresh O(changed) #69 (`e7ae912^` → `e7ae912`)

`bench_refresh`, RAM. Both `bench_refresh_one_across_library_sizes` (20000 sweep) and `bench_refresh_one_vs_many` exist at `e7ae912^` (post-SP2) — no overlay.

- [ ] **Step 1: BEFORE — apply the 4-point sweep edit (matches the old file's recipe).**
```bash
source /data/musefs-bench/env.sh; cd "$WT"; export TMPDIR=/dev/shm
git -C "$WT" checkout --detach 'e7ae912^' && git -C "$WT" reset --hard 'e7ae912^' && git -C "$WT" clean -fdx -e target
sed -i 's/\[100usize, 1000, 5000\]/[100usize, 1000, 5000, 20000]/' musefs-core/tests/bench_refresh.rs
for n in 1 2 3; do cargo test -p musefs-core --release --test bench_refresh \
  bench_refresh_one_across_library_sizes -- --ignored --nocapture 2>&1 | tee "$RAW/pr1-before-$n.txt"; done
```
Expected: refresh-1 wall ms at 100/1000/5000/20000 (linear ~160 ms at 20000). Record under `## #69`.

- [ ] **Step 2: AFTER — sweep (already 20000 on branch) + one-vs-many, 3 runs.**
```bash
git -C "$WT" checkout --detach e7ae912 && git -C "$WT" reset --hard e7ae912 && git -C "$WT" clean -fdx -e target
for n in 1 2 3; do cargo test -p musefs-core --release --test bench_refresh \
  bench_refresh_one_across_library_sizes -- --ignored --nocapture 2>&1 | tee "$RAW/pr1-after-$n.txt"; done
cargo test -p musefs-core --release --test bench_refresh bench_refresh_one_vs_many -- --ignored --nocapture 2>&1 | tee "$RAW/pr1-onevsmany.txt"
```
Expected: flat sweep (refresh-1@20000 within ~1 ms of @100); refresh-1 vs refresh-N. Record under `## #69`.

### Task 6: Issue #114 — Rendered child lookup root fan-out (`0881b31^` → `0881b31`)

`bench_refresh`, RAM. `bench_refresh_root_fanout_one_across_library_sizes` exists at `0881b31^`? It was added by #114 — **check**; if absent, overlay `0881b31`'s `bench_refresh.rs`.

- [ ] **Step 1: Checkout BEFORE; confirm the fanout bench exists, else overlay.**
```bash
source /data/musefs-bench/env.sh; cd "$WT"; export TMPDIR=/dev/shm
git -C "$WT" checkout --detach '0881b31^' && git -C "$WT" reset --hard '0881b31^' && git -C "$WT" clean -fdx -e target
grep -q bench_refresh_root_fanout_one_across_library_sizes musefs-core/tests/bench_refresh.rs \
  || git -C "$WT" checkout 0881b31 -- musefs-core/tests/bench_refresh.rs
cargo test --release -p musefs-core --test bench_refresh --no-run 2>&1 | tee "$RAW/issue114-overlay-build.txt"
```
Expected: compiles (overlay applied only if needed; record in `$RESULTS` whether overlay was used).

- [ ] **Step 2: Run BEFORE 3×, then AFTER 3×.**
```bash
for n in 1 2 3; do cargo test -p musefs-core --release --test bench_refresh \
  bench_refresh_root_fanout_one_across_library_sizes -- --ignored --nocapture 2>&1 | tee "$RAW/issue114-before-$n.txt"; done
git -C "$WT" checkout --detach 0881b31 && git -C "$WT" reset --hard 0881b31 && git -C "$WT" clean -fdx -e target
for n in 1 2 3; do cargo test -p musefs-core --release --test bench_refresh \
  bench_refresh_root_fanout_one_across_library_sizes -- --ignored --nocapture 2>&1 | tee "$RAW/issue114-after-$n.txt"; done
```
Expected: refresh-root-fanout-1 wall ms at 100/1000/5000/20000 (after ≈ flat ≤1 ms). Record under `## #114`.

### Task 7: Phase 6 PR2 — Scan pair #67/#68 (`2d4faf3^` → `2d4faf3`)

`bench_ingest`, `--features metrics`, RAM (`MUSEFS_BENCH_DIR=/dev/shm/musefs-bench`). No overlay.

- [ ] **Step 1: BEFORE — ci tier, 3 runs.**
```bash
source /data/musefs-bench/env.sh; cd "$WT"
git -C "$WT" checkout --detach '2d4faf3^' && git -C "$WT" reset --hard '2d4faf3^' && git -C "$WT" clean -fdx -e target
for n in 1 2 3; do MUSEFS_BENCH_TIER=ci MUSEFS_BENCH_DIR=/dev/shm/musefs-bench \
  cargo test -p musefs-core --release --features metrics --test bench_ingest \
  -- --ignored --nocapture bench_cold_scan_and_revalidate 2>&1 | tee "$RAW/pr2-before-$n.txt"; rm -rf /dev/shm/musefs-bench; done
```
Expected: per-format wall ms + `scan_bytes_read`. Record under `## PR2`.

- [ ] **Step 2: AFTER — same, 3 runs.**
```bash
git -C "$WT" checkout --detach 2d4faf3 && git -C "$WT" reset --hard 2d4faf3 && git -C "$WT" clean -fdx -e target
for n in 1 2 3; do MUSEFS_BENCH_TIER=ci MUSEFS_BENCH_DIR=/dev/shm/musefs-bench \
  cargo test -p musefs-core --release --features metrics --test bench_ingest \
  -- --ignored --nocapture bench_cold_scan_and_revalidate 2>&1 | tee "$RAW/pr2-after-$n.txt"; rm -rf /dev/shm/musefs-bench; done
```
Expected: non-MP3 formats −128 B/file in `scan_bytes_read`; MP3 unchanged; M4A 0/0. Record the Δ bytes/file under `## PR2`.

### Task 8: Phase 6 PR3 — Serve-path copies #70 (`32be8f0^` → `32be8f0`)

Criterion `read_throughput`, RAM. All four benches exist at `32be8f0^` (post-SP4) — no overlay.

- [ ] **Step 1: BEFORE baseline.**
```bash
source /data/musefs-bench/env.sh; cd "$WT"; export TMPDIR=/dev/shm
git -C "$WT" checkout --detach '32be8f0^' && git -C "$WT" reset --hard '32be8f0^' && git -C "$WT" clean -fdx -e target
cargo bench -p musefs-core --bench read_throughput -- --save-baseline pr3-before sequential_read concurrent_read_walk cold_first_read seek_read 2>&1 | tee "$RAW/pr3-before.txt"
```

- [ ] **Step 2: AFTER compare.**
```bash
git -C "$WT" checkout --detach 32be8f0 && git -C "$WT" reset --hard 32be8f0 && git -C "$WT" clean -fdx -e target
cargo bench -p musefs-core --bench read_throughput -- --baseline pr3-before sequential_read concurrent_read_walk cold_first_read seek_read 2>&1 | tee "$RAW/pr3-after.txt"
```
Expected: `sequential_read` all formats improve ~10–14% (no >10% rise); ogg cold/seek held or improved. Record under `## PR3`.

### Task 9: HeaderCache #136 — quick_cache (`2e6674e^` → `2e6674e`)

Criterion `read_throughput`, RAM. No overlay.

- [ ] **Step 1: BEFORE baseline (full bench set).**
```bash
source /data/musefs-bench/env.sh; cd "$WT"; export TMPDIR=/dev/shm
git -C "$WT" checkout --detach '2e6674e^' && git -C "$WT" reset --hard '2e6674e^' && git -C "$WT" clean -fdx -e target
cargo bench -p musefs-core --bench read_throughput -- --save-baseline h136-before 2>&1 | tee "$RAW/h136-before.txt"
```

- [ ] **Step 2: AFTER compare.**
```bash
git -C "$WT" checkout --detach 2e6674e && git -C "$WT" reset --hard 2e6674e && git -C "$WT" clean -fdx -e target
cargo bench -p musefs-core --bench read_throughput -- --baseline h136-before 2>&1 | tee "$RAW/h136-after.txt"
```
Expected: all workloads within noise; `seek_read` trends 2–6% faster. Record under `## #136`. Per the headline rule, this pass's at-a-glance entry is "within noise".

### Task 10: Issue #112 — StructureOnly kernel passthrough (`0881b31` → `faec017`, sudo dd)

Bespoke `dd` harness, captured as a committed script. Kernel 7.0 supports passthrough; needs sudo (CAP_SYS_ADMIN).

**Files:**
- Create (in the PRIMARY worktree, to be committed in Task 12): `/home/cfutro/git/musefs/benches/passthrough_dd.sh`

- [ ] **Step 1: Write the committed harness script.**

Create `/home/cfutro/git/musefs/benches/passthrough_dd.sh`:
```bash
#!/usr/bin/env bash
# StructureOnly passthrough dd benchmark.
# Usage: sudo benches/passthrough_dd.sh <musefs-binary> <work-dir> [size-mib]
# Mounts a single large backing track StructureOnly and times a sequential
# `dd` read through the mount. StructureOnly serves raw backing bytes, so
# throughput is FUSE-path-bound and format-independent; we use a WAV backing
# file to avoid an external encoder dependency.
set -euo pipefail
BIN="$1"; WORK="$2"; SIZE_MIB="${3:-512}"
mkdir -p "$WORK/backing" "$WORK/mnt"
WAV="$WORK/backing/track.wav"
if [ ! -f "$WAV" ]; then
  # Minimal 44-byte PCM WAV header for SIZE_MIB of 16-bit stereo 44.1k data, then zero-fill.
  DATA=$(( SIZE_MIB * 1024 * 1024 )); RIFF=$(( DATA + 36 ))
  printf 'RIFF' > "$WAV"
  printf "$(printf '\\x%02x\\x%02x\\x%02x\\x%02x' $((RIFF&255)) $((RIFF>>8&255)) $((RIFF>>16&255)) $((RIFF>>24&255)))" >> "$WAV"
  printf 'WAVEfmt ' >> "$WAV"
  printf '\x10\x00\x00\x00\x01\x00\x02\x00\x44\xac\x00\x00\x10\xb1\x02\x00\x04\x00\x10\x00' >> "$WAV"
  printf 'data' >> "$WAV"
  printf "$(printf '\\x%02x\\x%02x\\x%02x\\x%02x' $((DATA&255)) $((DATA>>8&255)) $((DATA>>16&255)) $((DATA>>24&255)))" >> "$WAV"
  dd if=/dev/zero bs=1M count="$SIZE_MIB" >> "$WAV" 2>/dev/null
fi
DB="$WORK/m.db"; rm -f "$DB"
"$BIN" scan "$WORK/backing" --db "$DB" >/dev/null
"$BIN" mount "$WORK/mnt" --db "$DB" --mode structure-only --template '$title' &
MPID=$!
# Poll for mount readiness instead of a fixed sleep (the CLI gives no ready signal).
VIRT=""
for _ in $(seq 1 60); do
  VIRT=$(find "$WORK/mnt" -type f 2>/dev/null | head -1 || true)
  [ -n "$VIRT" ] && break
  sleep 0.5
done
if [ -z "$VIRT" ]; then echo "ERROR: mount never exposed a file" >&2; kill "$MPID" 2>/dev/null || true; exit 1; fi
cat "$VIRT" > /dev/null   # warm backing into page cache
for i in 1 2 3; do dd if="$VIRT" of=/dev/null bs=1M 2>&1 | tail -1; done
fusermount3 -u "$WORK/mnt" 2>/dev/null || umount "$WORK/mnt" 2>/dev/null || true
kill "$MPID" 2>/dev/null || true
```
Make executable: `chmod +x /home/cfutro/git/musefs/benches/passthrough_dd.sh`.

- [ ] **Step 2: Build the BEFORE binary (`0881b31`) and run the script (RAM-cached backing on /dev/shm).**
```bash
source /data/musefs-bench/env.sh; cd "$WT"
git -C "$WT" checkout --detach 0881b31 && git -C "$WT" reset --hard 0881b31 && git -C "$WT" clean -fdx -e target
cargo build --release -p musefs --bin musefs
sudo /home/cfutro/git/musefs/benches/passthrough_dd.sh "$CARGO_TARGET_DIR/release/musefs" /dev/shm/pt-before 512 2>&1 | tee "$RAW/issue112-before.txt"
sudo rm -rf /dev/shm/pt-before
```
Expected: 3 `dd` GB/s lines (daemon-read path, ~2.8 GB/s in the old file but box-relative here). Record median under `## #112`.

- [ ] **Step 3: Build the AFTER binary (`faec017`) and run the script.**
```bash
git -C "$WT" checkout --detach faec017 && git -C "$WT" reset --hard faec017 && git -C "$WT" clean -fdx -e target
cargo build --release -p musefs --bin musefs
sudo /home/cfutro/git/musefs/benches/passthrough_dd.sh "$CARGO_TARGET_DIR/release/musefs" /dev/shm/pt-after 512 2>&1 | tee "$RAW/issue112-after.txt"
sudo rm -rf /dev/shm/pt-after
```
Expected: passthrough path faster (old file: ~9.3 GB/s, ~3.3×). Record after median + ratio under `## #112`. **If the mount or scan errors** (e.g. WAV rejected, or passthrough not negotiated), record the exact error and fall back to the FLAC fixture approach from `musefs-fuse/tests/passthrough.rs`; do not report a number you didn't measure.

### Task 11: Cumulative summary (derived — composed deltas + current-`main` absolutes)

**A same-harness `16caba4`→`main` measurement is infeasible and must NOT be attempted** (verified: `MountConfig.case_insensitive` and `scan_directory_with`/`ScanOptions`/`revalidate_with` don't exist at `16caba4`, so main's harnesses can't compile there; and the `16caba4`-era harness omits the now-required `case_insensitive` field, so it can't compile on `main` either). The cumulative summary is therefore **derived**: compose the per-pass isolated deltas already collected, anchored to current-`main` absolutes. No old-commit overlay build.

- [ ] **Step 1: Measure current-`main` absolutes for the three representative benches (one-time, on `main`, native harness — no overlay).**
```bash
source /data/musefs-bench/env.sh; cd "$WT"; export TMPDIR=/dev/shm
git -C "$WT" checkout --detach main && git -C "$WT" reset --hard main && git -C "$WT" clean -fdx -e target
# ingest (durable flac, the SP1 headline tier):
rm -rf /data/musefs-bench/corpus/*
MUSEFS_BENCH_TIER=ci MUSEFS_BENCH_DIR=/data/musefs-bench/corpus \
  cargo test --release -p musefs-core --features metrics --test bench_ingest \
  -- --ignored --nocapture bench_cold_scan_and_revalidate 2>&1 | tee "$RAW/cumulative-ingest-main.txt"
# refresh (the #69/#114 flat-vs-linear story):
cargo test -p musefs-core --release --test bench_refresh \
  bench_refresh_one_across_library_sizes -- --ignored --nocapture 2>&1 | tee "$RAW/cumulative-refresh-main.txt"
# serve:
cargo bench -p musefs-core --bench read_throughput -- sequential_read seek_read cold_first_read 2>&1 | tee "$RAW/cumulative-serve-main.txt"
```
Expected: today's absolute numbers per subsystem. Record under `## Cumulative` as the "current main" anchor.

- [ ] **Step 2: Compose the per-subsystem cumulative delta from the isolated passes.**

No new runs. Using the deltas already in `$RESULTS`, write one derived line per subsystem, naming the contributing passes and which one dominates:
- **ingest** = SP1 (Task 1) ∘ PR2 (Task 7) — dominated by SP1's durable fsync win; PR2 is the −128 B/file + move-not-clone refinement.
- **refresh** = SP2 (Task 2) ∘ #69 (Task 5) ∘ #114 (Task 6) — the O(N)→flat journey; quote refresh-1@largest-N then-vs-now.
- **serve** = SP3 (Task 3) ∘ SP4 (Task 4) ∘ PR3 (Task 8) ∘ #136 (Task 9) — quote the `sequential_read` and ogg `cold_first_read` then-vs-now.

Record under `## Cumulative` labelled **"composed from per-pass isolated deltas; non-isolating"**. Do not multiply unrelated speedups into a single headline number — present the chain and name the dominant pass.

---

## Phase 2 — Rewrite BENCHMARKS.md

### Task 12: Write the layered BENCHMARKS.md and commit

**Files:**
- Modify: `/home/cfutro/git/musefs/BENCHMARKS.md` (full rewrite)
- Create: `/home/cfutro/git/musefs/benches/passthrough_dd.sh` (from Task 10, now committed)

- [ ] **Step 1: Draft the new structure from `$RESULTS`.**

In the PRIMARY worktree (`/home/cfutro/git/musefs`, branch `benchmarks-rerun-this-box`), rewrite `BENCHMARKS.md` with exactly three top-level parts (per spec §"Rewritten file structure"):
  1. **Results at a glance** — a per-subsystem cumulative delta table (ingest / refresh / serve, from Task 11) + a one-line headline per pass. Headline = the pass's single largest *statistically-significant* delta on its deployment-representative tier (SP1 → durable tier; #136 → state "within noise").
  2. **Methodology** — written once: the Task 0.3 machine block (8-core, 32 GB, rustc 1.96, `/data` 2-device btrfs Data:single/Metadata:RAID1 rotational, `/dev/shm` tmpfs, Linux 7.0); the before/after definition (after = squash-merge commit, before = `<after>^`); the overlay rule; run convention (3 runs median for ignored benches, Criterion sampling for `read_throughput`); storage-per-harness (`MUSEFS_BENCH_DIR` for `bench_ingest`, `TMPDIR=/dev/shm` for the others).
  3. **Per-pass detail sections** — one per pass in the Task table order, each: a one-line "what changed", the before/after table with this-box numbers, the reproduce command. **Keep** SP2's Stage-A/B narrative and SP4's crc linear↔matrix↔memo evolution table as "why" context. **No** "Gates" subsections anywhere (dropped per spec). The #112 reproduce command points at `benches/passthrough_dd.sh`.

- [ ] **Step 2: Fill every number from `$RESULTS`.**

Transcribe medians/deltas exactly as recorded. Do not carry over any old number that was not re-measured; where a before side was non-reproducible (overlay build failure), state that explicitly rather than reusing the historical figure as if fresh. Convert each before/after pair to a speedup/Δ consistent with the run convention.

- [ ] **Step 3: Verify the file has no stale machine header or gate text.**

Run: `grep -nE 'AMD EPYC|17 GiB|SSD \(non-rotational\)|## Gates|In-diff mutation|proptest_read_fidelity' /home/cfutro/git/musefs/BENCHMARKS.md`
Expected: **no matches** (old machine header gone, all gate text removed).

- [ ] **Step 4: Commit the rewrite + the harness script.**
```bash
cd /home/cfutro/git/musefs
git add BENCHMARKS.md benches/passthrough_dd.sh
git commit -m "$(cat <<'MSG'
docs: re-run BENCHMARKS.md apples-to-apples on this box; layered rewrite

Re-measured every pass on this box (8-core, 32 GB, rotational /data,
/dev/shm RAM) as PR-isolated before/after pairs (after = squash-merge
commit, before = its parent; after-harness overlaid on the before side
for SP2/SP4), plus a cumulative 16caba4->main per-subsystem summary.
Restructured into at-a-glance / methodology / per-pass detail; dropped
the correctness-gate subsections (perf file only). Adds the #112
passthrough dd harness as a committed script.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
MSG
)"
```
Expected: pre-commit hook (fmt/clippy/full tests/ruff) passes; commit created. The new `.sh` must be `chmod +x` and pass ruff is N/A (bash), but confirm no hook complains about it.

---

## Phase 3 — Verify & clean up

### Task 13: Final verification and scratch teardown

**Files:** none modified (verification + cleanup).

- [ ] **Step 1: Cross-check at-a-glance vs detail.**

Re-read `BENCHMARKS.md`: every headline number in "Results at a glance" must equal a number in that pass's detail table. Every pass in the anchor table has a detail section. #114 is listed once (not double-listed with #112, which shares `0881b31`).

- [ ] **Step 2: Confirm reproduce commands match what was actually run.**

Spot-check 3 reproduce commands against the `$RAW/*.txt` invocations (same tier/env/filter). Fix any drift in the doc.

- [ ] **Step 3: Sanity-check directions one more time.**

SP1 durable = large speedup; SP1 §4 RAM = slower (honest trade); SP1 §3 = fsyncs→0; SP4 ogg cold/seek = big win; #136 = within noise; #112 = ~few× faster. Any inversion is reported in the commit/PR notes, not hidden.

- [ ] **Step 4: Remove the scratch worktree and bench data.**
```bash
git -C /home/cfutro/git/musefs worktree remove --force /data/musefs-bench/wt
sudo rm -rf /data/musefs-bench
```
Expected: `git -C /home/cfutro/git/musefs worktree list` no longer shows the scratch worktree. (Raw logs in `/data/musefs-bench` are intentionally discarded — the numbers now live in `BENCHMARKS.md`; git history holds the old values.)

- [ ] **Step 5: Hand back to the user.**

Report: which passes reproduced cleanly, any overlay build fallbacks taken, any direction inversions vs the old file, and the final `BENCHMARKS.md` diff summary. Offer to open a PR.
