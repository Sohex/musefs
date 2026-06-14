# Benchmarks

Every optimization pass re-measured **apples-to-apples on one box** as a
PR-isolated before/after pair, plus a cumulative `16caba4`→`main` summary. This
file is **performance only** — correctness gates (byte-identical proptests, FUSE
e2e, in-diff mutation) live in CI and [CONTRIBUTING.md](CONTRIBUTING.md), not
here.

Read it in three layers:

1. [**Results at a glance**](#results-at-a-glance) — the cumulative per-subsystem
   delta and a one-line headline per pass.
2. [**Methodology**](#methodology) — machine, before/after definition, the
   overlay rule, run conventions, storage placement. Written once; every
   detail section assumes it.
3. [**Per-pass detail**](#per-pass-detail) — one section per pass: what changed,
   the before/after table, the reproduce command, and the "why" where it matters.

---

## Results at a glance

### Cumulative — `16caba4` → current `main` (`e02223e`)

Composed from the per-pass isolated deltas below, anchored to current-`main`
absolutes. **Non-isolating**: a same-harness run at both ends is infeasible (API
drift means neither the `16caba4`-era harness nor the `main` harness compiles at
the other commit), so these compose the chain of passes that touched each
subsystem rather than a single end-to-end measurement. See
[Cumulative detail](#cumulative-detail) for the absolutes and the per-pass
composition.

| Subsystem | Headline metric | `16caba4`-era | current `main` | Δ | Dominant pass |
|-----------|-----------------|--------------:|---------------:|---|---------------|
| **Ingest** | fsync count (durable) | 403 | 0 | **eliminated** | SP1 |
| | cold scan, ci flac | 32 206 ms | 47 ms | **~685×** | SP1 |
| **Refresh** | refresh-1 @ 20 000 tracks | 173 ms | 1 ms | **~173×** | #69 |
| **Serve** | sequential_read/flac | 929 µs | 569 µs | **−38.8%** | SP3 + PR3 |
| | cold_first_read/ogg | 14.96 ms | 1.51 ms | **−89.9%** | SP4 |
| | concurrent m16+walker | 8.20 ms | 4.15 ms | **−49.4%** | SP3 + PR3 |

### Per-pass headlines

Each headline is the pass's single largest statistically-significant delta on its
deployment-representative tier.

| Pass | Commit | Headline (this box) |
|------|--------|---------------------|
| [SP1 — ingestion scalability](#sp1--ingestion-scalability) | `ccbbfaa` | durable cold scan **~1150–3600×** faster; fsync storm **403→0** |
| [SP2 — incremental tree refresh](#sp2--incremental-tree-refresh) | `ed5f380` | 5 000-track refresh-1 **1.4×** (32→23 ms) |
| [SP3 — read/serve residuals](#sp3--readserve-residuals) | `e8d56bd` | sequential_read **−8 to −13%** (flac/mp3/m4a/m4a-last) |
| [SP4 — storage-aware Ogg serving](#sp4--storage-aware-ogg-serving) | `a62453b` | ogg cold-read **−88%**, seek **−94%** |
| [#69 — refresh O(changed)](#69--refresh-ochanged) | `e7ae912` | refresh-1 @ 20 000 **~170×** (173→1 ms) |
| [#114 — root fan-out lookup](#114--rendered-child-lookup-root-fan-out) | `0881b31` | root fan-out @ 20 000 **~5×** (5→1 ms) |
| [PR2 — scan pair (#67/#68)](#pr2--scan-pair-6768) | `2d4faf3` | **−128 B/file** scan I/O (flac/ogg/wav); wall within noise |
| [PR3 — serve-path copies (#70)](#pr3--serve-path-copies-70) | `32be8f0` | sequential_read **−7 to −11%** (m4a-last/ogg/wav); concurrent **−19%** |
| [#136 — HeaderCache quick_cache](#136--headercache--quick_cache) | `2e6674e` | **within noise** (marginal m4a/ogg sequential) |
| [#112 — StructureOnly passthrough](#112--structureonly-kernel-passthrough) | `faec017` | passthrough dd **3.36×** (2.5→8.4 GB/s) |

> **One direction inverted vs the historical file:** SP1 §4 (compute-isolated, on
> RAM) is now **faster** after the change, not slower. The old file recorded SP1
> as ~1.9× slower on RAM-backed tempfs (the "honest cost" of the pipeline); on
> this 8-core box the parallel pipeline wins even on RAM (~1.4×), at higher peak
> RSS. See [SP1 §4](#4-compute-isolated-ram--the-trade-now-a-win-on-this-box).

---

## CI regression gating

`BENCHMARKS.md` records hand-run absolute numbers; CI guards against regressions
in three lanes:

1. **Counter gate (every non-doc PR, hard).** `perf_counters.rs` +
   `tree.rs` golden work-counter assertions under `--features metrics`. Catches
   algorithmic regressions (extra copy, whole-file slurp, O(N) tree rebuild).
2. **A/B wall-clock (warn-only, core `src` PRs).** The `perf-ab` job benches the
   base and PR commits back-to-back on one runner and posts a `critcmp` delta as
   a PR comment. Never blocks.
3. **Release record.** The `benchmarks` job runs the full bench suite at the
   `ci` tier on a tag and uploads the numbers as an artifact for curation here.

The fsync-storm (403→0) signal needs a real FUSE mount and lives only in the
release lane / the `#[ignore]` `bench_scan_under_latency`, not the per-PR gate.

The release artifact is named `benchmark-snapshot-<tag>`; download it from the
tag's workflow run and fold the numbers into the per-pass tables here.

## Methodology

### Machine

| | |
|---|---|
| CPU | 8 cores |
| RAM | 32 GB (31 GiB) |
| Durable storage (`/data`) | btrfs, 2-device span (`sda3`+`sdb3`), **rotational**; Data: single, Metadata: RAID1; zstd:1. **No SSD on this box.** |
| RAM storage (`/dev/shm`) | tmpfs |
| Toolchain | rustc 1.96.0 · release builds |
| Kernel | Linux 7.0 (FUSE passthrough requires ≥6.9 + `CAP_SYS_ADMIN`) |

### Before / after definition

History is squash-merged (linear), so each pass is one commit:

- **after** = the pass's own squash-merge commit.
- **before** = its parent, `<after>^` — **PR-isolated**, not current `main`. This
  preserves attribution (each delta is exactly what that PR changed) and avoids
  harness drift from later passes.

### The overlay rule

Two passes (**SP2**, **SP4**) report a bench that did not yet exist at their
`before` commit. For those, the after-commit's harness file is checked out onto
the before checkout (`git checkout <after> -- <bench_file>`) so the old code is
measured with the new harness. Overlay use is called out in each affected
section.

### Run conventions

- **`bench_ingest` / `bench_refresh`** (ignored tests, `cargo test --release
  … -- --ignored`): 3 runs, **median** reported (spread noted where it matters).
  `bench_ingest` needs `--features metrics`.
- **`read_throughput`** (Criterion bench): Criterion's own sampling; before side
  saved with `--save-baseline`, after side compared with `--baseline`. Reported Δ
  is Criterion's change estimate.
- Wall times on `/data` are **box-relative** (rotational disk); where a portable
  signal exists (fsync count, bytes_read, pread count) it is the primary number.

### Storage placement

- **Durable** rows run on `/data` (rotational btrfs). `bench_ingest` honors
  `MUSEFS_BENCH_DIR`.
- **RAM** rows run on `/dev/shm` (tmpfs). `bench_ingest` honors
  `MUSEFS_BENCH_DIR=/dev/shm/…`; `bench_refresh` and `read_throughput` ignore it
  and follow `TMPDIR=/dev/shm`.

---

## Per-pass detail

### SP1 — Ingestion scalability

`ccbbfaa^` → `ccbbfaa`. `bench_ingest`, `--features metrics`. No overlay.

**What changed:** whole-file `fs::read` slurp + per-file commits at
`synchronous=FULL` → bounded probing reads + parallel-probe/single-writer
pipeline + per-batch transactions at `synchronous=NORMAL` (WAL retained).

#### 1. Durable small files — the fsync/batching win

`ci` tier (200 tracks × 4 KiB, no embedded art), corpus + DB on `/data`. Not
compute-bound — the before path is dominated by per-file fsync latency.

| format | before scan (ms) | after scan (ms) | speedup |
|--------|-----------------:|----------------:|--------:|
| flac     | 32 206 | 21 | **1534×** |
| mp3      | 16 124 | 14 | 1152× |
| m4a      | 30 089 | 19 | 1584× |
| m4a-last | 39 592 | 11 | 3599× |
| ogg      | 16 153 | 14 | 1154× |
| wav      | 15 574 | 12 | 1298× |

#### 2. Durable large files — bounded reads + batching

`bandwidth` tier (1000 tracks × 30 MiB FLAC + art ≈ 30 GiB), on `/data`, 1 run.

| metric | before (slurp) | after (bounded) | Δ |
|--------|---------------:|----------------:|---|
| scan wall (ms)  | 378 041 | 15 228 | **24.8× faster** |
| revalidate (ms) | 243 | 14 | 17.4× |
| peak RSS (KiB)  | 98 636 | 132 436 | 0.74× (more) |

The after path reads only a ~1 MiB metadata window per file instead of slurping
each 30 MiB file in full.

#### 3. fsync count — the mechanism

`ci` tier (200 FLAC) scanned through the passthrough latency-FS (`ssd` profile),
which counts fsyncs at the FUSE layer. Wall is box-relative (rotational `/data`);
the **fsync count** is the portable signal.

| config | fsyncs | scan wall (ms, box-relative) |
|--------|-------:|-----------------------------:|
| before (`synchronous=FULL`, per-file commits) | **403** | 79 |
| after (`synchronous=NORMAL`, batched commits)  | **0**   | 21 |

The 403→0 collapse is the root cause of §1's durable speedups.

#### 4. Compute-isolated (RAM) — the trade, now a win on this box

`large-compute` tier (100k tracks × ~38 KiB FLAC) on `/dev/shm` (RAM), where
fsync is free — so the §1/§3 batching win is neutralized and only raw compute
remains. `bytes_read` ≈ 3.92 GiB both sides (the 38 KiB files are below the 1 MiB
window, so bounded reads don't help).

| config | before scan (ms) | after scan (ms) | revalidate before→after (ms) | peak RSS before→after (KiB) |
|--------|-----------------:|----------------:|------------------------------|-----------------------------|
| default jobs | 31 241 | 22 295 | 2239 → 1278 | 27 904 → 96 084 |
| `--jobs 1`   | 31 111 | 23 565 | 2255 → 1283 | 28 024 → 92 200 |

**Finding — direction inverted vs the historical file.** The old file (6-core
EPYC) recorded SP1 as ~1.9× *slower* on RAM — the deliberate "honest cost" of the
pipeline where there is no fsync win to amortize. On this **8-core** box the
parallel pipeline is **~1.4× faster** even on RAM (the extra cores outweigh the
per-file coordination), at the cost of ~3.4× peak RSS (96 MB vs 28 MB). The trade
has shifted from "small RAM loss" to "RAM win for more memory" on wider hardware.

```bash
# durable §1/§2: MUSEFS_BENCH_DIR on /data ; RAM §4: MUSEFS_BENCH_DIR on /dev/shm
MUSEFS_BENCH_TIER=ci MUSEFS_BENCH_DIR=/data/bench \
  cargo test --release -p musefs-core --features metrics --test bench_ingest \
  -- --ignored --nocapture bench_cold_scan_and_revalidate
# §3 fsync count:
MUSEFS_BENCH_LATENCY_PROFILE=ssd MUSEFS_BENCH_TIER=ci MUSEFS_BENCH_FORMAT_MIX=flac \
  cargo test --release -p musefs-core --features metrics --test bench_ingest \
  bench_scan_under_latency -- --ignored --nocapture
```

---

### SP2 — Incremental tree refresh

`ed5f380^` → `ed5f380`. `bench_refresh`, RAM (`TMPDIR=/dev/shm`).
**Overlay:** the `bench_refresh_one_across_library_sizes` sweep didn't exist at
`ed5f380^`, so the after-commit harness is overlaid on the before checkout.

**What changed:** replace the O(N) `VirtualTree::build_with` full reconstruction
with `apply_changes` (in-place `im`-backed tree mutation) — only nodes whose id
appears in the changed/added/removed sets are touched.

`ci` tier, FLAC, single-track re-tag, 3 runs (median):

| library size | before (ms) | after (ms) | speedup |
|-------------:|------------:|-----------:|--------:|
| 100   | 0  | 0  | n/a (sub-granularity) |
| 1000  | 5  | 6  | 0.83× (noise tier) |
| 5000  | 32 | 23 | **1.39×** |

**Why (Stage A → Stage B):** at Stage A the rebuild already rendered
incrementally (only the changed track re-rendered, O(changed)), but the
subsequent `VirtualTree::build_with` reconstructed the *whole* tree from scratch
(O(N)) — the remaining linear cost. Stage B's `apply_changes` removes that full
reconstruction; the residual slope (still ~23 ms at 5000) is the lighter O(N)
render-key scan + HashMap rebuild that feeds `apply_changes`, not a full tree
rebuild. The speedup grows with library size because diff cost is proportional to
changes, not total entries. (Corpus is single-album, so `build_with` time is
slightly optimistic vs a real multi-album library.)

```bash
cargo test -p musefs-core --release --test bench_refresh \
  bench_refresh_one_across_library_sizes -- --ignored --nocapture
```

---

### SP3 — Read/serve residuals

`e8d56bd^` → `e8d56bd`. Criterion `read_throughput`, RAM. No overlay.

**What changed:** (1) `read_segments` writes each `BackingAudio` run directly into
the output buffer's reserved tail (no throwaway `vec![0u8; n]` + copy); (2)
`handles: Mutex<HashMap>` → lock-free `sharded_slab::Slab`; (3) `size_cache:
Mutex<HashMap>` → `dashmap::DashMap`.

#### sequential_read — per-format (4 MiB files, 128 KiB reads)

| format | before (µs) | after (µs) | time Δ | thrpt Δ |
|--------|------------:|-----------:|-------:|--------:|
| flac     | 929.1  | 839.6  | −7.9%  | +8.6%  |
| mp3      | 940.2  | 824.8  | −13.1% | +15.1% |
| m4a      | 939.8  | 824.2  | −10.8% | +12.2% |
| m4a-last | 938.0  | 842.6  | −10.3% | +11.4% |
| ogg      | 966.8  | 1049.4 | +6.3%  | −5.9%  |
| wav      | 935.4  | 912.3  | −2.5%  | +2.5%  |

The metadata-light formats improve 8–13% from dropping the per-splice alloc+copy.
ogg +6.3% is a low-iteration sampling anomaly (Criterion warned "Unable to
complete 100 samples in 5.0s" — only 5050 iterations vs 10k for other formats).

#### concurrent_read_walk/m16_plus_walker

16 reader threads + one metadata walker sharing one `Arc<Musefs>` (includes thread
spawn/join):

| | before (ms) | after (ms) | Δ |
|---|----------:|-----------:|--:|
| m16_plus_walker | 8.20 | 9.48 | +15.7% |

This high-variance burst metric regressed on this run — attributable to
thread spawn/join overhead in the contention path rather than the read path
itself; it is not a sequential-read regression. (The old file recorded this bench
as parity/improved; it swings run-to-run.)

```bash
cargo bench -p musefs-core --bench read_throughput -- sequential_read concurrent_read_walk
```

---

### SP4 — Storage-aware Ogg serving

`a62453b^` → `a62453b`. Criterion `read_throughput` + latency-injected read.
**Overlay:** `cold_first_read`/`seek_read` were added by SP4, so the after-commit
bench is overlaid on the before checkout.

**What changed:** replace the eager whole-region Ogg page index with a stateless
per-request backwards-scan: `find_page_start` locates the containing page from a
~65 KB window (CRC-validated entry guard), `serve_ogg_window` patches each page
header algebraically (`crc_shift_zeros`, no payload I/O), and a one-entry
`last_page` memo short-circuits the scan + CRC guard when the next request lands
inside the already-located page.

#### sequential_read — warm repeat-read (no page-index amortization to win)

| format | before (µs) | after (µs) | Δ |
|--------|------------:|-----------:|--:|
| flac | 856.2 | 880.5 | +2.8% |
| mp3  | 847.7 | 894.5 | +5.5% |
| m4a  | 862.5 | 816.9 | −5.3% |
| m4a-last | 872.7 | 831.6 | −4.7% |
| ogg  | 1037.9 | 1048.2 | +1.0% |
| wav  | 892.6 | 840.8 | −5.8% |

#### cold_first_read / seek_read — the Ogg win

| bench | format | before | after | Δ |
|-------|--------|-------:|------:|--:|
| cold_first_read | ogg | 14.956 ms | 1.799 ms | **−88.0%** |
| seek_read       | ogg | 13.541 ms | 827 µs   | **−93.9%** |

Non-ogg cold/seek stay within ±7% (no page index involved). The wins come from
never building the whole-file index up front — the old code reads the entire
prefix to serve even one chunk near EOF; SP4 scans ~65 KB backward, then the memo
carries the validated page forward. `sequential_read/ogg` is flat (+1.0%) because
it reads the full file linearly regardless — the win is cold-start and seek.

#### Latency-injected reads (`bench_read_under_latency`, nfs-hdd) — AFTER only

This bench was introduced by SP4; no before baseline exists.

| label | format | tier | storage | wall (ms) | opens | preads |
|-------|--------|------|---------|----------:|------:|-------:|
| read_whole_cold | ogg | ci | nfs-hdd | 28 | 1 | 0 |
| read_seek_cold  | ogg | ci | nfs-hdd | 28 | 1 | 0 |

`preads=0`: the backwards-scan reads are served from the layout's inline/generated
segments without reaching the backing file. Near-equal whole/seek wall time
indicates per-file open+resolve latency dominates under nfs-hdd; the local
cold/seek benches above are the clean signal.

#### Why `crc_shift_zeros` is a hybrid

`patch_page_header_algebraic` advances the CRC past a page's payload via
`crc_shift_zeros`. The per-step loop is O(n) and dominated linear `sequential_read`
on max-size 65 KB pages; a GF(2) matrix-power method is O(log n) but carries a
fixed ~32-matmul cost, so it is *slower* for the small pages real Opus/Vorbis
streams carry. The evolution across implementations (ogg benches):

| ogg bench | linear crc | +matrix | +matrix +memo-amortized guard (shipped) |
|-----------|-----------:|--------:|----------------------------------------:|
| sequential_read | 17.6 ms | 6.40 ms | **0.93 ms** |
| cold_first_read | ~17 ms  | 7.42 ms | **1.61 ms** |
| seek_read       | —       | 821 µs  | **829 µs**  |

Shipped as a hybrid: per-step loop below n=16384, matrix at/above; a differential
test covers both paths + the boundary.

```bash
cargo bench -p musefs-core --bench read_throughput -- cold_first_read seek_read sequential_read
MUSEFS_BENCH_LATENCY_PROFILE=nfs-hdd cargo test --release -p musefs-core \
  --features metrics --test bench_ingest bench_read_under_latency -- --ignored --nocapture
```

---

### #69 — Refresh O(changed)

`e7ae912^` → `e7ae912`. `bench_refresh`, RAM. No overlay.

**What changed:** changelog-driven change detection (`changelog_since` +
`render_keys_for` on just the changed ids) replaces the O(N) render-key scan, and
collision-gated `apply_changes` dirtying stops the old parent chain from being
rebuilt unconditionally. Refresh-1 cost becomes O(changed).

#### Single-track refresh vs library size (3 runs, median)

A single-track re-tag moves the track out of its shared album dir — the
structural worst case for a flat corpus (one artist / one album, N siblings).

| library size | before — full rebuild (ms) | after — O(changed) (ms) | factor |
|-------------:|---------------------------:|------------------------:|-------:|
| 100    | 0   | 0 | — |
| 1000   | 6   | 0 | ∞ (sub-ms) |
| 5000   | 33  | 0 | ∞ (sub-ms) |
| 20000  | 173 | 1 | **~170×** |

The after sweep is **flat**: refresh-1 @ 20 000 is within 1 ms of @ 100, against a
linear ~170 ms slope before.

#### One-vs-many (same `Musefs` instance, 200-track ci tier)

| label | wall (ms) |
|-------|----------:|
| refresh-1 | 0 |
| refresh-N (100 touched) | 4 |

refresh-N scales with the touched set, not the library.

```bash
# before (apply the 4-point sweep edit first):
sed -i 's/\[100usize, 1000, 5000\]/[100usize, 1000, 5000, 20000]/' musefs-core/tests/bench_refresh.rs
cargo test -p musefs-core --release --test bench_refresh \
  bench_refresh_one_across_library_sizes -- --ignored --nocapture
cargo test -p musefs-core --release --test bench_refresh \
  bench_refresh_one_vs_many -- --ignored --nocapture
```

---

### #114 — Rendered child lookup (root fan-out)

`0881b31^` → `0881b31`. `bench_refresh`, RAM. **Overlay:** the
`bench_refresh_root_fanout_one_across_library_sizes` bench was added by #114, so
its harness is overlaid on the before checkout.

**What changed:** a rendered-name child index turns the root sibling scan in
`deepest_existing_ancestor` into an indexed miss. The corpus uses N top-level
artist directories; the timed update retags one track to fallback `Unknown/…`,
exercising an absent rendered-name lookup at root.

| library size (top-level artists) | before (ms) | after (ms) |
|---------------------------------:|------------:|-----------:|
| 100   | 0 | 0 |
| 1000  | 0 | 0 |
| 5000  | 2 | 0 |
| 20000 | 5 | 1 |

~5× at the 20 000-artist fan-out; ≤5 k is already ≤2 ms on both sides.

```bash
cargo test -p musefs-core --release --test bench_refresh \
  bench_refresh_root_fanout_one_across_library_sizes -- --ignored --nocapture
```

---

### PR2 — Scan pair (#67/#68)

`2d4faf3^` → `2d4faf3`. `bench_ingest`, `--features metrics`, RAM, 3 runs.
No overlay.

**What changed:** (#67) gate the 128-byte ID3v1 tail read to `.mp3` files — only
MP3 consumes the frame; (#68) `ingest_bulk` drains the owned `Unit` batch by
value, moving picture payloads into the DB structs instead of cloning.

#### Wall time — ci tier (200 tracks × 4 KiB, no art), median of 3

| format | before (ms) | after (ms) |
|--------|------------:|-----------:|
| flac     | 29 | 30 |
| mp3      | 21 | 23 |
| m4a      | 27 | 26 |
| m4a-last | 32 | 26 |
| ogg      | 22 | 24 |
| wav      | 21 | 24 |

Wall time is within run-to-run noise — at ci tier (4 KiB files, no embedded art)
there is no picture payload to move, so #68's win doesn't show here. It appears on
art-bearing corpora (the bandwidth tier / real libraries) where the clone was
O(art-size) per file.

#### Scan I/O — the #67 signal (`scan_bytes_read`)

| format | before (B) | after (B) | Δ total | Δ per file |
|--------|-----------:|----------:|--------:|-----------:|
| flac | 870 600 | 845 000 | −25 600 | **−128 B** |
| mp3  | 847 200 | 847 200 | 0       | 0 (tail still read) |
| m4a  | 0 | 0 | 0 | n/a (seek-reader path) |
| m4a-last | 0 | 0 | 0 | n/a |
| ogg  | 873 000 | 847 400 | −25 600 | **−128 B** |
| wav  | 853 600 | 828 000 | −25 600 | **−128 B** |

Non-MP3 formats drop exactly the 128-byte ID3v1 tail per file (−25 600 B over the
200-track corpus). MP3 is unchanged; M4A uses the seek-reader, not the
front-anchored probe path.

```bash
MUSEFS_BENCH_TIER=ci MUSEFS_BENCH_DIR=/dev/shm/bench \
  cargo test -p musefs-core --release --features metrics --test bench_ingest \
  -- --ignored --nocapture bench_cold_scan_and_revalidate
```

---

### PR3 — Serve-path copies (#70)

`32be8f0^` → `32be8f0`. Criterion `read_throughput`, RAM. No overlay.

**What changed:** four stacked serve-path copy eliminations — DB chunk readers
fill the caller's `&mut [u8]`; `read_segments` writes `ArtImage`/`BinaryTag`/raw
`OggArtSlice` arms into the output buffer's resized tail; `Musefs::read_into`
serves into a caller buffer; and the FUSE layer reuses a per-worker thread-local
scratch buffer. None touches synthesis or layout (served audio stays
byte-identical).

#### sequential_read

| format | before (µs) | after (µs) | Δ | verdict |
|--------|------------:|-----------:|--:|---------|
| flac     | 939.8  | 924.8 | −2.1%  | noise |
| mp3      | 917.2  | 884.1 | −3.1%  | noise |
| m4a      | 904.1  | 877.6 | −3.7%  | noise |
| m4a-last | 909.8  | 860.3 | −7.4%  | improved |
| ogg      | 1080.4 | 963.4 | −9.1%  | improved |
| wav      | 925.6  | 815.7 | −11.1% | improved |

#### cold_first_read / seek_read / concurrent

| bench | before | after | Δ | verdict |
|-------|-------:|------:|--:|---------|
| cold_first_read/flac | 1.652 ms | 1.557 ms | −5.8% | improved |
| cold_first_read/mp3  | 1.590 ms | 1.678 ms | +5.5% | regressed (within 10%) |
| cold_first_read/ogg  | 1.781 ms | 1.694 ms | −4.9% | improved |
| seek_read (all)      | — | — | within ±2.7% | held |
| concurrent_read_walk/m16 | 9.490 ms | 7.642 ms | **−19.5%** | improved |

No format breaches the >10% *rise* gate. The concurrent burst metric improves 19%
here (it is high-variance and swings run-to-run; see SP3).

```bash
cargo bench -p musefs-core --bench read_throughput -- \
  sequential_read concurrent_read_walk cold_first_read seek_read
```

---

### #136 — HeaderCache → quick_cache

`2e6674e^` → `2e6674e`. Criterion `read_throughput`, RAM. No overlay.

**What changed:** an S3-FIFO byte-weighted `quick_cache` replaces the hand-rolled
16-shard Mutex LRU — the serve path's last shared std lock is gone.

**At a glance: within noise.** No workload regresses outside noise; the only
movers are marginal sequential_read improvements on the metadata-light formats.

| bench | before | after | Δ | verdict |
|-------|-------:|------:|--:|---------|
| sequential_read/m4a      | 851.1 µs | 794.7 µs | −6.6% | improved |
| sequential_read/m4a-last | 855.2 µs | 798.3 µs | −6.7% | improved |
| sequential_read/ogg      | 1.043 ms | 962.9 µs | −7.7% | improved |
| sequential_read/flac,mp3,wav | — | — | within noise | held |
| cold_first_read (all)    | — | — | within noise / −3.6% m4a | held |
| seek_read (all)          | — | — | within noise | held |
| concurrent_read_walk/m16 | 5.557 ms | 5.451 ms | −1.9% | held |

```bash
cargo bench -p musefs-core --bench read_throughput
```

---

### #112 — StructureOnly kernel passthrough

`0881b31` → `faec017`. Bespoke `dd` harness (committed:
[`benches/passthrough_dd.sh`](benches/passthrough_dd.sh)), `sudo` (passthrough
needs `CAP_SYS_ADMIN`).

**What changed:** the backing fd is registered at open (FUSE passthrough, kernel
≥6.9); the kernel serves StructureOnly reads directly from the backing inode,
bypassing the daemon round-trip.

512 MiB WAV backing on `/dev/shm` (RAM-cached, isolates FUSE-path overhead),
`dd bs=1M` sequential read, fresh mount per binary, 3 runs each:

| | run 1 | run 2 | run 3 | median |
|---|------:|------:|------:|-------:|
| before (daemon reads) | 2.5 GB/s | 2.5 GB/s | 2.7 GB/s | **2.5 GB/s** |
| after (passthrough)   | 8.4 GB/s | 8.3 GB/s | 8.9 GB/s | **8.4 GB/s** |

**3.36×** on this RAM-cached sequential workload: the before path round-trips
every ~128 KiB chunk through the daemon (wakeup + positioned read + copy back via
`/dev/fuse`); the after path reads straight from the backing inode's page cache.

```bash
sudo benches/passthrough_dd.sh target/release/musefs /dev/shm/pt 512
```

---

## Cumulative detail

`16caba4` → current `main` (`e02223e`). **Derived, non-isolating** — composed from
the per-pass isolated deltas above, anchored to current-`main` absolutes. A
same-harness end-to-end run is infeasible: `MountConfig.case_insensitive` and
`scan_directory_with`/`ScanOptions`/`revalidate_with` don't exist at `16caba4`
(so `main`'s harnesses can't compile there), and the `16caba4`-era harness omits
the now-required `case_insensitive` field (so it can't compile on `main` either).
The deltas below name the contributing passes and the dominant one; unrelated
speedups are **not** multiplied into a single headline.

### Current-`main` absolutes (1 run, native harness)

**Ingest** — ci tier, `/data`, `bench_ingest`:

| format | scan (ms) | revalidate (ms) | RSS (KiB) |
|--------|----------:|----------------:|----------:|
| flac | 47 | 2 | 6900 |
| mp3  | 25 | 2 | 6944 |
| m4a  | 55 | 2 | 6956 |
| m4a-last | 39 | 3 | 6980 |
| ogg  | 20 | 2 | 6980 |
| wav  | 25 | 3 | 6984 |

**Refresh** — RAM, `bench_refresh_one_across_library_sizes`: refresh-1 @ 100 / 1000
/ 5000 = 0 ms; @ 20 000 = 1 ms.

**Serve** — RAM, `read_throughput` (Criterion median): sequential_read flac 569 µs
· mp3 563 µs · m4a 566 µs · m4a-last 568 µs · ogg 737 µs · wav 598 µs;
cold_first_read ogg 1.507 ms; seek_read ogg 806 µs; concurrent m16+walker 4.15 ms.

### Composed per-subsystem deltas

**Ingest** = SP1 ∘ PR2. Dominated by SP1's durable-fsync elimination; PR2 is the
−128 B/file + move-not-clone refinement.

| metric | pre-SP1 | current main | Δ |
|--------|--------:|-------------:|--:|
| fsync count (latencyfs) | 403 | 0 | **eliminated** |
| scan_wall (ci flac) | 32 206 ms | 47 ms | **~685×** |
| scan_wall (bandwidth flac) | 378 041 ms | ~15 228 ms† | **~24.8×** |
| scan_bytes_read (ci flac) | 870 600 B | 845 000 B | **−128 B/file** |

† Bandwidth tier not re-measured at `main`; figure is SP1's after number.

**Refresh** = SP2 ∘ #69 ∘ #114. The O(N)→flat journey; dominant pass is **#69**
(changelog-driven O(changed) rebuild), with #114 shaving the 20 k root fan-out on
top.

| metric | pre-SP2 | current main | Δ |
|--------|--------:|-------------:|--:|
| refresh-1 @ 1000  | 5 ms  | 0 ms | ∞ (sub-ms) |
| refresh-1 @ 5000  | 32 ms | 0 ms | ∞ (sub-ms) |
| refresh-1 @ 20000 | 173 ms | 1 ms | **~173×** |

**Serve** = SP3 ∘ SP4 ∘ PR3 ∘ #136. SP3 + PR3 drive the cross-format
sequential/cold/seek wins (alloc elimination + copy reduction); **SP4** owns the
ogg cold/seek collapse.

| metric | pre-SP3 | current main | Δ |
|--------|--------:|-------------:|--:|
| sequential_read/flac | 929 µs | 569 µs | **−38.8%** |
| sequential_read/mp3  | 940 µs | 563 µs | **−40.1%** |
| sequential_read/m4a  | 940 µs | 566 µs | **−39.8%** |
| sequential_read/ogg  | 967 µs | 737 µs | **−23.8%** |
| sequential_read/wav  | 935 µs | 598 µs | **−36.1%** |
| cold_first_read/ogg  | 14.96 ms | 1.51 ms | **−89.9%** |
| seek_read/ogg        | 13.54 ms | 806 µs  | **−94.0%** |
| concurrent m16+walker | 8.20 ms | 4.15 ms | **−49.4%** |

_Criterion's own `change:` lines compare against the previous on-machine baseline
(itself already optimized); the absolutes above are the reliable end-to-end
signal._

---

## Storage tunables

A proposed `--storage-profile {ssd,hdd,nfs}` preset would have bumped
`--max-readahead-kib` and `--max-background` (and enabled `--keep-cache`) per medium,
on the premise that "larger read-ahead hides HDD/NFS latency." Measured against real
storage, **that premise does not hold** — only `--keep-cache` shows a benefit — so the
preset was dropped and these flags keep their defaults. This section records the
evidence.

### Methodology

Unlike the optimization passes above (tmpfs, in-process Criterion), these run through a
**real kernel mount with a real reader**, because the tunables are kernel↔FUSE
negotiation parameters invisible to an in-process driver:

- **Backing:** real RAID-1 HDD (`/home`, `/dev/md127`) and a btrfs HDD span (`/data`,
  `/dev/sda3`); for NFS, a loopback **NFSv4.2** export (`exportfs` + `mount -t nfs
  localhost:…`) whose backing is tmpfs (isolates the RPC tax) or HDD (RPC + seeks).
- **Latency:** `tc qdisc add dev lo root netem delay <X>ms` adds `X` per packet
  → ≈`2X` RTT per NFS RPC. Tested at 8 ms, 50 ms, and 200 ms RTT (the last ≈ a
  trans-Pacific server).
- **Cold reads:** `sync; echo 3 > /proc/sys/vm/drop_caches` before each measured read —
  without it the page cache serves repeats and hides all backing latency.
- **Mode: `synthesis`, not `structure-only`.** Structure-only triggers kernel FUSE
  passthrough when the process is privileged (these run as root), which serves the
  backing fd directly and **bypasses the daemon read path** — and with it every tunable
  that acts on that path. Synthesis splices `BackingAudio` reads through the daemon,
  the real serving path.
- **Why not the injected `MUSEFS_FAULT_*_US` model:** it cannot show a read-ahead
  effect. FUSE delivers reads to the daemon in fixed ≤256 KiB chunks (`max_pages`,
  already pinned at the kernel's 1 MiB ceiling by `fuser`'s 16 MiB default
  `max_write`), so the per-`pread` count — and thus any per-`pread` injected latency
  total — is independent of `max_readahead`.

Reproduce: `benches/storage_tunables_bench.sh` (needs `/dev/fuse`, root, and for the
NFS rows `nfs-kernel-server` + `tc`). HDD numbers are noisy (±10–15%); the trends, not
the digits, are the signal.

### `--max-readahead-kib` — no benefit anywhere; hurts on HDD

Cold single-stream sequential throughput (MB/s), `synthesis`:

| readahead KiB | HDD /home (RAID1) | HDD /data (btrfs) | NFS 8 ms | NFS-on-HDD 50 ms | NFS-on-HDD 200 ms |
|--------------:|------------------:|------------------:|---------:|-----------------:|------------------:|
| 512 (default) | 248 | 127 | 30.8 | 4.7 | 1.3 |
| 2048          | 191 | 72  | 30.6 | 4.9 | 1.3 |
| 4096          | 153 | 84  | 30.5 | 4.9 | 1.3 |
| 32 (probe)    | 237 | 75  | —    | —   | —   |

(File sizes differ per column — 512 MiB local, 96 MiB at 50 ms, 48 MiB at 200 ms — so
compare *within* a column, not across. The 200 ms column ≈ a trans-Pacific server:
flat to the last digit.)

The window size barely moves throughput, and on HDD values ≥2048 KiB are among the
**slowest** (peak is ~128–512 KiB). The reason is visible on NFS: 512 MiB ÷ 256 KiB ×
8 ms ≈ 16 s ≈ the observed 31 MB/s — a single stream is served **serially**, one
≤256 KiB read at a time, each paying the full RTT, with no prefetch overlap that a
larger window could exploit.

### `--max-background` — no effect on read throughput

Wall time (s) for N concurrent cold streams over distinct tracks:

| max_background | HDD /home (16 streams) | NFS 8 ms (16) | NFS-on-HDD 50 ms (80) | NFS-on-HDD 200 ms (24) |
|---------------:|-----------------------:|--------------:|----------------------:|-----------------------:|
| 64 (default)   | 4.55 | 5.16 | 177.8 | 238.5 |
| 128            | 5.05 | 5.18 | 175.7 | 237.4 |

64 ≈ 128 even with 80 > 64 streams. Expected: musefs's `FuseConfig` notes
`max_background` caps *background* work and that "foreground reads are bounded only by
client concurrency, not by this." The concurrent reads here are foreground.
(Concurrency *does* hide latency — 16 NFS streams reach ~10× single-stream aggregate —
but that is client parallelism, which `max_background` does not gate.)

### `--keep-cache` — the one real win (~3×)

Cold read then immediate reopen (no cache drop between); `reopen_s` is the signal:

| keep_cache | HDD reopen (s) | NFS 8 ms reopen (s) | NFS-on-HDD 50 ms reopen (s) |
|-----------:|---------------:|--------------------:|----------------------------:|
| false      | 0.224 | 0.207 | 0.039 |
| true       | 0.062 | 0.060 | 0.014 |

With `--keep-cache` the kernel retains the page cache across opens, so a re-opened file
is served from RAM instead of re-fetched over slow storage — **~3× faster reopen**,
consistent across HDD and NFS. This is the only tunable worth changing for slow backing
(relevant for players/scanners that re-open files), and it needs no preset.

### Conclusions

- **Drop the `--storage-profile` preset.** Of the four knobs it would have set, three
  (`max_readahead`, `max_background`, and by extension a per-medium combination of them)
  show no benefit; `max_readahead` ≥2048 KiB actively hurts on HDD. The only justified
  change — enable `--keep-cache` on HDD/NFS — does not need an abstraction.
- **Single-stream latency hiding — addressed in #255 (next section).** The serialized
  read path measured above (512 MiB ÷ 256 KiB × RTT) is exactly what backing read-ahead
  now fixes.

---

## Backing read-ahead (#255)

Each `--max-readahead-kib` row above exposed the real bottleneck: a single stream is
served one ≤256 KiB FUSE chunk at a time, each paying the full backing RTT, so a
200 ms-RTT NFS mount tops out at ~1.3 MB/s regardless of the *kernel* read-ahead window.
The fix is **read amplification in the daemon** — `BackingReader` coalesces a stream's
small reads into one large positioned `pread` (geometric window growth, global RAM budget
with LRU eviction), so the backing client can pipeline/parallelize the RPCs behind one
syscall. A background-prefetch-threads layer ("Phase 2") was also built but is **off by
default** (see below).

### Methodology

Two harnesses. **Real kernel mount** (`benches/storage_tunables_bench.sh`): a real reader
(`dd`) over a real FUSE mount, cold (`drop_caches`) each sample, median of 3. Local backing
on a btrfs HDD; NFS via a loopback **NFSv4.2** export plus `tc netem` for RTT. **The corpus
is real FLAC** (`MUSEFS_BENCH_CORPUS_SRC`) — a `/dev/zero` corpus on a compressing fs
(btrfs `compress=zstd`) collapses to a cached extent and never touches the platter, which
silently inverts the HDD numbers; real already-compressed audio is incompressible.
**In-process** (`musefs-core/tests/bench_ingest.rs::bench_read_under_latency`): the core read
path over `musefs-latencyfs` (per-op injected latency), isolating the daemon from the kernel
FUSE layer. `off` = `--read-ahead-budget-mib 0`; `phase1` = the default (amplification only);
`phase1+2` = `--read-ahead-prefetch`.

### Single-stream cold throughput (MB/s)

| backing | off | **phase 1 (default)** | phase 1+2 | passthrough |
|--------:|----:|----------------------:|----------:|------------:|
| local HDD (btrfs, real FLAC) | ~60 | ~62 | ~60 | ~58 |
| NFS, tmpfs-backed, 200 ms RTT | 1.2 | **7.4** | 6.8 | 9.8 |

On NFS read-ahead is a **~6× single-stream win** (1.2 → 7.4 MB/s, 75 % of the kernel-passthrough
ceiling). On a real local HDD all four configs sit within run-to-run noise (~±15 %) — read-ahead
is **neutral**, not a regression. (An earlier `/dev/zero` corpus showed a spurious −35 %; it was
the zstd-compression artifact above, not read-ahead.)

### Concurrent streams (8 × distinct tracks, aggregate MB/s, NFS 200 ms RTT)

| off | **phase 1 (default)** | phase 1+2 | passthrough |
|----:|----------------------:|----------:|------------:|
| 1.6 | **13.6** | 12.1 | 16.3 |

### In-process, per-op latency (16 MiB Ogg whole read; wall ms / backing preads)

| profile | off | phase 1 (default) |
|--------:|----:|------------------:|
| ssd (80 µs/op) | 45 ms / 774 preads | **26 ms / 32 preads** |
| nfs-ssd (600 µs/op) | 138 ms / 774 | **112 ms / 32** |

Amplification collapses 774 backing round-trips to 32; the win scales with per-op latency and
is already material at SSD speeds (1.7×).

### Phase 2 is off by default

Background prefetch threads (Phase 2) **never beat amplification alone** and cost a consistent
~10 %: single-stream NFS 6.8 vs 7.4, concurrent NFS 12.1 vs 13.6, neutral on HDD. A single large
`pread` already lets the NFS client pipeline its RPCs, so the threads add coordination overhead
without overlap to exploit. Phase 2 is therefore opt-in (`--read-ahead-prefetch`), retained for
hypothetical backends where one large read does not self-pipeline.

**Defaults:** read-ahead on at `--read-ahead-budget-mib 64`, Phase-1 amplification only. Set
`0` to disable on local-disk-only setups (no benefit there, though no harm either).

---

## Global allocator — steady-state RSS (#360)

Long-lived high-churn FUSE load fragments glibc malloc, growing daemon RSS over
days without a true leak. The `musefs` binary now defaults to the jemalloc
global allocator with a background purge thread. Measured with
`scripts/rss-churn-bench.sh` (Linux; median `VmRSS` over the flattened tail —
steady state, not peak).

**Parameters:** WORKERS=8 (nproc), FILES=500, CYCLES=200, WARMUP=20, no
REFRESH_CMD. DB = a freshly-scanned 4427-track store on tmpfs (`/tmp`); backing
audio on `/data` (HDD). Concurrent `cat`-to-`/dev/null` churn drives the
open/read/release handle-table and read-synthesis allocation path.

| Allocator      | Steady-state RSS      |
| -------------- | --------------------- |
| system malloc  | ~74.7 MiB (76496 kiB) |
| jemalloc       | ~28.7 MiB (29368 kiB) |

**Decision: SHIP jemalloc.** Steady-state RSS is ~62% lower (jemalloc ≤ system
malloc, the §4 ship rule). Under identical churn glibc retained ~46 MiB of dirty
pages that jemalloc's decay + background purge return to the OS — the #360
fragmentation failure mode, reproduced and fixed. The gap is far outside
run-to-run noise, so no within-noise tie-break was needed.
