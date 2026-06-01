# Benchmarks

Before/after measurements for the [2026-05-30 optimization pass](docs/superpowers/specs/2026-05-30-optimization-pass/README.md). Each section is reproducible from the SP0 harness (`bench_ingest`); commands are given inline.

**Machine:** AMD EPYC (6 cores) · 17 GiB RAM · `/dev/sda3` SSD (non-rotational) · Linux 7.0 · rustc 1.96 · release builds.

---

## SP1 — Ingestion scalability

*Measured 2026-05-31.*

- **Before** = `main` @ `16caba4` (pre-SP1): whole-file `fs::read` slurp, single-threaded, per-file commits at `synchronous=FULL`.
- **After** = `sp1-ingestion-scalability`: bounded probing reads + parallel-probe/single-writer pipeline + per-batch transactions at `synchronous=NORMAL` (WAL retained).
- Harness: `cargo test --release -p musefs-core --features metrics --test bench_ingest -- --ignored --nocapture <test>`. `wall_ms` is the `scan_directory` call only; `bytes_read` = `scan_bytes_read` (SP1 metric); `fsyncs` via the SP0b passthrough latency-FS.

### 1. Durable storage, small files — the fsync/batching win

`ci` tier (200 tracks × 4 KiB, no embedded art), corpus + DB on the SSD (`MUSEFS_BENCH_DIR=…`), per-format sweep. This is *not* compute-bound — `main` issues ~4 commits/file at `synchronous=FULL`, so it is dominated by per-file `fsync` latency on durable storage.

| format    | before scan (ms) | after scan (ms) | speedup |
|-----------|-----------------:|----------------:|--------:|
| flac      | 8949 | 17 | **526×** |
| mp3       | 6090 | 21 | 290× |
| m4a       | 10877 | 32 | 340× |
| m4a-last  | 5324 | 25 | 213× |
| ogg       | 2679 | 20 | 134× |
| wav       | 3033 | 96 | 32× |

```bash
MUSEFS_BENCH_TIER=ci MUSEFS_BENCH_DIR=/path/on/ssd \
  cargo test --release -p musefs-core --features metrics --test bench_ingest \
  -- --ignored --nocapture bench_cold_scan_and_revalidate
```

### 2. Durable storage, large files — bounded reads + batching

`bandwidth` tier (1000 tracks × 30 MiB FLAC + 200 KiB art = ~30 GiB), corpus + DB on the SSD.

| metric              | before (slurp) | after (bounded) | delta |
|---------------------|---------------:|----------------:|-------|
| scan wall (ms)      | 170 263 | 8 735 | **19.5× faster** |
| `bytes_read`        | ~30 GiB¹ | 1.05 GiB | **~30× less I/O** |
| peak RSS (KiB)      | 98 724 | 123 016 | comparable² |
| revalidate (ms)     | 329 | 21 | 16× |

¹ `main` has no `scan_bytes_read` counter; it `fs::read`s every 30 MiB file in full, so it reads the whole ~30 GiB corpus. The "after" reads only a 1 MiB metadata window per file (1.05 GiB total).
² `main` holds one 30 MiB file in memory at a time (released per file); the pipeline holds its in-flight art budget + worker buffers. Neither is unbounded. The memory *win* is on M4A moov-last (the seek reader avoids slurping a hundreds-of-MB audiobook to reach a trailing `moov`) — not captured by this FLAC corpus.

```bash
MUSEFS_BENCH_TIER=bandwidth MUSEFS_BENCH_FORMAT_MIX=flac MUSEFS_BENCH_DIR=/path/on/ssd \
  cargo test --release -p musefs-core --features metrics --test bench_ingest \
  -- --ignored --nocapture bench_cold_scan_and_revalidate
```

### 3. fsync count — the mechanism

`ci` tier (200 FLAC) scanned through the SP0b passthrough latency-FS (`ssd` profile), which counts `fsync`/`fsyncdir` at the FUSE layer.

| config | fsyncs | scan wall (ms) |
|--------|-------:|---------------:|
| before (`synchronous=FULL`, per-file commits) | **403** | 1300 |
| after (`synchronous=NORMAL`, batched commits) | **0** | 494 |

The 403 → 0 fsync collapse is the root cause of §1's durable-storage speedups: SP1 commits one transaction per batch (≤256 files) under `synchronous=NORMAL`, so the WAL is not fsync'd per commit.

```bash
MUSEFS_BENCH_LATENCY_PROFILE=ssd MUSEFS_BENCH_TIER=ci MUSEFS_BENCH_FORMAT_MIX=flac \
  cargo test --release -p musefs-core --features metrics --test bench_ingest \
  bench_scan_under_latency -- --ignored --nocapture
```

### 4. Compute-isolated (tempfs) — the honest cost

`large-compute` tier (100k tracks × ~38 KiB FLAC, including a 30 KiB cover/file) on **tempfs (RAM)**, where `fsync` is essentially free — so the §1/§3 batching win is *neutralized* and only raw compute remains.

| config | scan wall (ms) | peak RSS (KiB) |
|--------|---------------:|---------------:|
| before (slurp, 1 thread) | **24 687** | 28 004 |
| after, `--jobs 1` (sequential pipeline) | 68 436 | 97 704 |
| after, `--jobs 6` (parallel) | 46 077 | 109 964 |

On RAM with tiny files, SP1 is **~1.9× slower** than the simple slurp: the parallel pipeline adds per-file coordination (channel, budget, batch buffering, art held/cloned in flight) that the absent fsync win can't offset, and the single DB writer serializes the 100k inserts (so parallelism only recovers part of it: 68 s → 46 s). `bytes_read` here is ~3.92 GiB for both jobs settings — the 38 KiB files are smaller than the 1 MiB window, so bounded reads don't help at this size.

This is the deliberate trade SP1 makes: **a little extra compute in exchange for eliminating the durable-write (fsync) storm** — a large net win on any real (non-RAM) disk, as §1–§3 show, and a small loss only on RAM-backed storage with sub-window files (not a real music-library deployment).

```bash
MUSEFS_BENCH_TIER=large-compute MUSEFS_BENCH_FORMAT_MIX=flac [MUSEFS_BENCH_JOBS=1] \
  cargo test --release -p musefs-core --features metrics --test bench_ingest \
  -- --ignored --nocapture bench_cold_scan_and_revalidate
```

### Summary

- **On durable storage (the deployment target), SP1 is 20–500× faster at cold scan**, scaling with how fsync-bound the old per-file-commit path was. The win is overwhelmingly from transaction batching + `synchronous=NORMAL` (403 → 0 fsyncs), with bounded reads adding a ~30× I/O reduction on large files.
- **Bounded reads** cut scan I/O from full-file to a ~1 MiB window — negligible below the window size, ~30× at 30 MiB; M4A additionally seeks to `moov` instead of slurping `mdat`.
- **Parallel probing** (`--jobs`) helps when probing dominates (large files / slow storage); the single DB writer caps its benefit on tiny-track-heavy libraries.
- **Honest caveat:** on RAM-backed tempfs with sub-window files, the pipeline overhead makes SP1 slower than the naive slurp — there is no fsync cost there to amortize.

### Follow-up optimization candidates (surfaced by these runs)

- The bounded path issues a 128-byte ID3v1 tail read for *every* front-anchored file, but only MP3 consumes it — gating it to MP3 would drop a syscall/file for FLAC/OGG/WAV.
- `ingest_bulk` clones each picture's bytes (the batch holds `&Probed`); draining owned `Unit`s into the writer would let the art move instead of copy.

---

## SP2 — Incremental tree refresh

*Measured 2026-05-31 (box under load — relative scaling is the signal, not absolute ms).*

### Stage A baseline — single-track refresh vs library size

A single-track re-tag triggers `poll_refresh`. At Stage A the rebuild *already renders incrementally* — only the changed track is re-rendered (O(changed)) — but the subsequent `VirtualTree::build_with` reconstructs the *whole* tree from scratch (O(N)). That full tree-construction step is the remaining linear cost; Stage B eliminates it (in-place tree mutation). The sweep times one-track refresh across three library sizes to capture the Stage A baseline.

`ci` tier, FLAC, on tempfs. Each library size gets its own tempdir + cold DB (no cross-size collision).

| library size (N tracks) | refresh-1 wall (ms) |
|------------------------:|--------------------:|
| 100   | 0 |
| 1000  | 4 |
| 5000  | 41 |

Refresh-1 wall time grows roughly linearly with N: the single-track render is O(changed), but the `VirtualTree::build_with` full reconstruction it feeds is O(N). This is the expected Stage A baseline; **Stage B targets flat** refresh-1 by mutating the tree in place instead of rebuilding it, so cost scales with the changed set, not the library.

Caveat: the sweep corpus is single-album (one artist / one album, no path collisions or disambiguation), so the `build_with` time here is slightly optimistic versus a real multi-album library.

```bash
cargo test -p musefs-core --release --test bench_refresh \
  bench_refresh_one_across_library_sizes -- --ignored --nocapture
```

### Stage B — in-place tree mutation

Stage B replaces the O(N) `VirtualTree::build_with` with `apply_changes` (in-place im-backed tree mutation): only nodes whose id appears in the changed/added/removed sets are touched. The same one-track-retag sweep measures whether the O(N) tree-construction cost is eliminated.

`ci` tier, FLAC, on tempfs (box under load — relative scaling is the signal, not absolute ms). Each library size gets its own tempdir + cold DB.

| library size (N tracks) | refresh-1 wall (ms) |
|------------------------:|--------------------:|
| 100   | ~1–6 |
| 1000  | ~10–22 |
| 5000  | ~38–94 |

Rerun 2026-06-01 (same box, lightly loaded), refresh-1 wall (ms):

| library size (N tracks) | refresh-1 wall (ms) |
|------------------------:|--------------------:|
| 100   | 0 |
| 1000  | 5 |
| 5000  | 24 |

The fresh run lands at the fast end of the prior loaded-box ranges (the box was less contended), confirming the Stage B profile: still a residual linear slope (~24 ms at 5000) from the O(N) render-key scan, not a flat O(changed) curve. Relative scaling is the signal, not absolute ms.

Ranges reflect run-to-run variation on a loaded box (three independent runs). The tree-mutation itself (the `apply_changes` path) is O(changed), but `rebuild_incremental` still iterates all N tracks to build the `new_snapshot` HashMap before calling `apply_changes` — that O(N) scan of `list_render_keys` results accounts for the residual linear slope. The `VirtualTree::build_with` full reconstruction (the dominant O(N) cost at Stage A) is eliminated; the remaining cost is a lighter O(N) DB-row iteration + HashMap insert. Improvement over Stage A: comparable to slightly faster at N=5000 (41 ms vs ~38–94 ms, noisy and overlapping on a loaded box, so inconclusive in absolute ms). The structural win is removing the full tree reconstruction; the residual linear slope is the lighter render-key scan + HashMap rebuild. A future pass could cache the render-key scan to reach a truly flat profile.

Caveat: same single-album corpus as Stage A (no path collisions or disambiguation).

```bash
cargo test -p musefs-core --release --test bench_refresh \
  bench_refresh_one_across_library_sizes -- --ignored --nocapture
```

---

## SP3 — Read/serve residuals

*Measured 2026-06-01 (same box, lightly loaded · tempfs · Criterion `ci` tier).
Relative deltas are the signal; Criterion's own before/after baseline (pre-SP3
`main`) drives the percentages.*

Three changes, none touching synthesis, layout, or what is served (served audio
stays byte-identical by construction):

1. `read_segments` reads each `BackingAudio` run directly into the output
   `Vec`'s pre-reserved tail (`resize` + `read_exact_at(&mut out[start..])`)
   instead of a throwaway `vec![0u8; n]` + `extend_from_slice` — removes one heap
   alloc + one memcpy per backing-audio splice (the dominant byte volume of every
   served file).
2. `handles: Mutex<HashMap<u64, Arc<Handle>>>` + `next_fh: AtomicU64` →
   `sharded_slab::Slab<Arc<Handle>>` (lock-free; FUSE `fh` = slab key + 1, so
   `fh` stays non-zero and `next_fh` is gone). Generation-encoded keys give ABA
   safety; at-capacity insert → `CoreError::HandleTableFull` → `ENFILE`.
3. `size_cache: Mutex<HashMap<i64, SizeEntry>>` → `dashmap::DashMap` (per-shard
   locking; the `*e` copy-out drops the read guard before the miss-path insert).

### `sequential_read` — per-format median (the >10%-rise regression gate)

`ci` tier, 4 MiB single-track files, 128 KiB reads, `fh=0` (no-handle path → each
read resolves via the header cache). The regression gate is a **>10% rise**
run-over-run; the alloc fix should hold or improve.

Δ is the ratio of the printed before→after medians; the significance note is
Criterion's own change-estimate p-value (its bootstrap change interval, computed
over the full samples, won't exactly equal the point-median ratio).

| format    | before (µs) | after (µs) | Δ        | note |
|-----------|------------:|-----------:|---------:|------|
| flac      | 925         | 918        | −0.8%    | within noise (p=0.40) |
| mp3       | 958         | 824        | −14.0%   | significant (p<0.05) |
| m4a       | 964         | 780        | −19.1%   | significant (p<0.05) |
| m4a-last  | 954         | 773        | −19.0%   | significant (p<0.05) |
| ogg       | 965         | 948        | −1.8%    | within noise (p=0.54) |
| wav       | 962         | 790        | −17.9%   | significant (p<0.05) |

No format breaches the >10% *rise* gate. The metadata-light formats improve
14–19% from dropping the per-splice alloc+copy; flac/ogg hold flat within noise
(their front-of-file structural-block re-reads dominate, masking the alloc win).

### `concurrent_read_walk/m16_plus_walker` — contention signal (the SP3 target)

16 reader threads streaming distinct files + one metadata walker, sharing one
`Arc<Musefs>` — SP0 named this bench's `handles`/`size_cache` mutex contention as
the SP3 target. Burst-concurrency wall time (includes thread spawn/join):

| | before (ms) | after (ms) | Δ |
|---|---:|---:|---:|
| m16_plus_walker | 8.91 | 8.35 | −6.3% (p=0.26) |

Improvement/parity from removing the two global mutexes; the high-variance burst
metric leaves p>0.05, so the signal is "no contention regression, trending
faster" rather than a precise speedup.

### Gates

- Byte-identical: `proptest_read_fidelity` + `musefs-format --features fuzzing`
  (212 cases) green; FUSE e2e `all_supported_formats_decode_to_same_pcm_sha_as_source`
  + `end_to_end_read_through_mount` green.
- In-diff mutation (CI parity, `cargo mutants --in-diff` over the changed `.rs`
  lines): **25 caught / 2 unviable / 0 missed**.

```bash
# Both benches (Criterion records its own before/after baseline):
cargo bench -p musefs-core --bench read_throughput

# In-diff mutation gate (as .github/workflows/mutants.yml runs it):
git diff "$(git merge-base main HEAD)...HEAD" -- '*.rs' > mutants.diff
cargo mutants --in-diff mutants.diff -j"$(nproc)" --exclude 'musefs-latencyfs/**'
```
