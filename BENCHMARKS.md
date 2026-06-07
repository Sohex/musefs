# Benchmarks

Before/after measurements for the [optimization pass](docs/superpowers/specs/2026-05/2026-05-26-optimization-pass-design.md). Each section is reproducible from the SP0 harness (`bench_ingest`); commands are given inline.

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

## SP4 — Storage-aware serving (backwards-scan + algebraic CRC)

Replaced the eager whole-region Ogg page index (`build_index`/`OggPageIndex`,
built once and cached on the resolved file) with a stateless per-request
backwards-scan: `find_page_start` locates the containing page from a ~65 KB window
(CRC-validated entry-page guard), and `serve_ogg_window` patches each page header
algebraically (`crc_shift_zeros`, no payload I/O) and serves payload via exact
`pread`. No O(whole-file) first-read scan. A one-entry `last_page` memo on the
resolved file `(page_rel, total_len, patched_header)` short-circuits
`find_page_start` when the next request lands inside the already-located page —
skipping both the backward scan and the entry CRC guard, without weakening
determinism (the page descended from a CRC-validated entry in a resolved file whose
backing bytes are immutable for its life; a content change rebuilds it → fresh memo;
on a memo miss the full scan + CRC guard runs).

### Why the `sequential_read` "regression" was a benchmark artifact

`sequential_read` re-reads **one cached file in a tight loop** — the single workload
where the old eager index's amortization helps and which no real client performs (a
player reads a track ~once; the kernel page cache absorbs re-reads of one offset).
The first stateless implementation (no memo) re-ran the entry CRC guard — a full
~65 KB page read + crc32 — on every chunk; an `MUSEFS_DISABLE_OGG_CRC_GUARD`
experiment measured that guard at **72–79%** of cold/warm ogg cost. Amortizing it
through the memo (validate once per page, not once per chunk) closed the gap.

### `ci` tier, 4 MiB single-track ogg, apples-to-apples (release, same box)

| workload                               | main (eager index) | SP4    | result        |
|----------------------------------------|-------------------:|-------:|---------------|
| `sequential_read` (warm repeat-read)   | 0.93 ms            | 0.93 ms| **parity**     |
| `cold_first_read` (play a track once)  | 13.2 ms            | 1.61 ms| **SP4 ~8×**    |
| `seek_read` (one 128 KiB read @ 3.5 MiB)| 12.7 ms           | 0.83 ms| **SP4 ~15×**   |

SP4 matches or beats the eager index on every workload. The cold/seek wins come
from never building the whole-file index up front (old code reads the entire prefix
to serve even one chunk near EOF; SP4 scans ~65 KB backward, then the memo carries
the validated page forward). `main` numbers are the median of 60 fresh-mount runs;
the regression-gate evolution per implementation:

| ogg bench        | SP4 linear crc | SP4 +matrix | SP4 +matrix +memo-amortized guard |
|------------------|---------------:|------------:|----------------------------------:|
| `sequential_read`| 17.6 ms        | 6.40 ms     | **0.93 ms**                        |
| `cold_first_read`| ~17 ms         | 7.42 ms     | **1.61 ms**                        |
| `seek_read`      | —              | 821 µs      | **829 µs**                         |

### Other formats (unaffected by SP4)

`cold_first_read`/`seek_read` (fresh mount per iteration): flac 1.43 ms / 520 µs,
mp3 1.40 ms / 506 µs, m4a 1.43 ms / 513 µs, m4a-last 1.46 ms / 519 µs, wav ~1.4 ms.
`sequential_read` for non-ogg stayed within noise run-over-run (no page index
involved).

### `crc_shift_zeros`: hybrid (per-step loop ↔ GF(2) matrix)

`patch_page_header_algebraic` advances the CRC past a page's payload via
`crc_shift_zeros`. The per-step loop is O(n); for the max-size 65 KB pages a
single-giant-packet file produces it dominated (linear `sequential_read/ogg`
17.6 ms). A GF(2) matrix-power method is O(log n) but has a fixed ~32-matmul cost,
so it is *slower* for the small pages real Opus/Vorbis streams carry. Shipped as a
hybrid: per-step loop below n=16384, matrix at/above. Differential test
(`crc_shift_zeros_matches_appending_zeros`) covers both paths + the boundary.

### Latency-injected reads (`bench_read_under_latency`, nfs-hdd, SP4 / Phase 5)

`read_whole_cold` 30 ms (2 preads, 4378 bytes), `read_seek_cold` 29 ms
(2 preads, 4378 bytes). Earlier recordings showed 0 in the pread columns
because the Ogg serve path was uninstrumented (#71) — the zeros meant
"uncounted", not "free". Since Phase 5 every Ogg backing read (index scan,
CRC probe, header, payload) counts attempt-based preads/bytes, and
`MUSEFS_FAULT_PREAD_US`/latency injection applies to them, so `wall_ms` and
the round-trip columns are all meaningful for Ogg.
The near-equal whole/seek wall time still indicates per-file open+resolve
latency dominates under nfs-hdd; the local cold/seek benches above are the
clean signal.

### Gates

- Byte-identical: `proptest_read_fidelity` (16) + `musefs-format --features
  fuzzing` (283) green; FUSE e2e (`all_supported_formats_decode_to_same_pcm_sha_as_source`,
  `end_to_end_read_through_mount`) — 9 passed.
- In-diff mutation (CI parity, `cargo mutants --in-diff` over changed `.rs` lines,
  excluding `musefs-latencyfs/**`): **0 missed.** The new-code survivors were
  resolved as (a) killing tests for the genuinely-killable ones — `find_page_start`
  memo-range boundaries + the load-bearing cheap-filter `&&`, the `< 27` /
  `< header_len` / `< total_len` header guards in `page_crc_ok`,
  `patch_page_header_algebraic`, and `verify_page_crc` (a 0-segment / truncated
  header was never exercised by the format-layer tests) — and (b) documented
  exclusions in `.cargo/mutants.toml` for proven-equivalent mutants (the
  `crc_shift_zeros` loop↔matrix dispatch and `poly_step` over disjoint basis
  vectors; `serve_ogg_window`'s empty-range overlap guards, verified byte-identical)
  and two non-terminating loop mutants (`i /= 1`, `pos *= …`).

```bash
# Representative read benches (the SP4 regression gate):
cargo bench -p musefs-core --bench read_throughput -- cold_first_read seek_read
# In-diff mutation gate (TMPDIR on a roomy fs; per-job tree copies are large):
TMPDIR=/path/with/space cargo mutants --in-diff sp4.diff -j4 --exclude 'musefs-latencyfs/**'
```

---

## Phase 6 PR 1 — Refresh O(changed) (#69)

*Measured 2026-06-03 (box under load — relative scaling is the signal, not absolute ms).*

- **Before** = `main` @ `16caba4` (pre-#69): `rebuild_incremental` scans all N render keys to build a full HashMap snapshot, then calls `apply_changes` — O(N) even for a single-track change.
- **After** = `phase6-pr1-incremental-refresh`: two stacked changes.
  1. Changelog-driven change detection (`changelog_since` + `render_keys_for` on just the changed ids) replaces the O(N) render-key scan; the snapshot is mutated in place.
  2. Collision-gated `apply_changes` dirtying. The changelog change alone did **not** flatten the sweep (intermediate column below): the bench touch (`replace_tags` with a lone COMMENT) wipes artist/album/title, so the track's rendered path moves to `Unknown/…` — and `apply_changes` dirtied the old parent chain unconditionally, rebuilding the single 20000-sibling album dir twice (a depth tie-break between `""` and depth-0 dirs rebuilt both root and the artist dir). Instrumentation: 131 ms of the 138 ms refresh was `apply_changes`; the DB phase was 0.7 ms. Gating dirty-marking on actual rendered-name collisions (O(log) probes of the deterministic ` (k)` disambiguation candidates) removed the rebuilds outright — `apply_changes` fell to 64 µs at 20k.
- Harness: `cargo test -p musefs-core --release --test bench_refresh -- --ignored --nocapture`. Each library size gets its own tempdir + cold DB (no cross-size collision).

### Sweep — single-track refresh vs library size

A single-track re-tag (its rendered path moves out of the shared album dir, the structural worst case for a flat corpus) triggers `poll_refresh`. `ci` tier, FLAC, on tempfs. Three independent runs per stage; the intermediate stage (changelog only, ungated `apply_changes`) is kept to show where the time actually was.

| library size (N tracks) | before (ms, 3 runs) | changelog only (ms, 3 runs) | final (ms, 3 runs) |
|------------------------:|--------------------:|----------------------------:|-------------------:|
| 100   | 0 / 0 / 0       | 0 / 0 / 0      | 0 / 0 / 0 |
| 1000  | 4 / 6 / 6       | 3 / 5 / 6      | 0 / 0 / 0 |
| 5000  | 43 / 25 / 34    | 16 / 29 / 17   | 0 / 0 / 0 |
| 20000 | 162 / 120 / 108 | 138 / 89 / 134 | 1 / 1 / 1 |

**The final sweep is flat**: refresh-1@20000 is within 1 ms of refresh-1@100 (the #69 acceptance), against a linear ~160 ms slope on main. The corpus is deliberately pathological — one artist / one album, 20000 sibling files — so the gated path is exercised at maximum fan-out; collision-free moves and adds (including a new top-level artist dir, which previously dirtied root and forced a full-tree rebuild) no longer touch unrelated siblings. Trees with real rendered-name collisions still pay an O(subtree) rebuild of the affected dir only, by design.

### One-vs-many on the branch

Both measurements run on the same `Musefs` instance (200-track `ci` tier, FLAC, tempfs). After the first `poll_refresh` the tree is freshly built; the second call starts from that warm state. `many` = 100 tracks (half the corpus, clamped to 1000).

| label | wall (ms) |
|-------|----------:|
| refresh-1 | 0 |
| refresh-N | 3 |

`touched_many=100`

refresh-N scales with the touched set (100 moved tracks), not the library: changelog read + 100 renders + 100 gated tree edits.

```bash
# Before (main — apply 4-point sweep edit first):
git checkout main
sed -i 's/\[100usize, 1000, 5000\]/[100usize, 1000, 5000, 20000]/' musefs-core/tests/bench_refresh.rs
cargo test -p musefs-core --release --test bench_refresh \
  bench_refresh_one_across_library_sizes -- --ignored --nocapture
git checkout -- musefs-core/tests/bench_refresh.rs

# After (branch):
git checkout phase6-pr1-incremental-refresh
cargo test -p musefs-core --release --test bench_refresh \
  bench_refresh_one_across_library_sizes -- --ignored --nocapture
cargo test -p musefs-core --release --test bench_refresh \
  bench_refresh_one_vs_many -- --ignored --nocapture
```

---

## Issue #114 — Rendered Child Lookup Root Fan-Out

*Measured 2026-06-06 (same machine as implementation run; release build; ignored harness).*

Harness:

```bash
cargo test -p musefs-core --release --test bench_refresh \
  bench_refresh_root_fanout_one_across_library_sizes -- --ignored --nocapture
```

The corpus uses `CorpusParams::single(Format::Flac, n, 1)`, so `$artist/$album/$title`
creates `n` top-level artist directories. The timed update retags one track with
only `COMMENT`, moving it to fallback `Unknown/Unknown/...`; this exercises an
absent rendered-name lookup at root in `deepest_existing_ancestor`.

| library size (top-level artists) | refresh-root-fanout-1 wall (ms) |
|---------------------------------:|--------------------------------:|
| 100 | 0 |
| 1000 | 0 |
| 5000 | 1 |
| 20000 | 1 |

The rendered-name child index turns the root lookup into an indexed miss, so the
tree-side lookup no longer scans unrelated artists. Overall wall time may still
include SQLite changelog reads, changed-track rendering, and test harness setup,
but the root sibling scan from issue #114 is removed.

---

## Phase 6 PR 2 — Scan pair (#67, #68)

*Measured 2026-06-04 (same box, lightly loaded · tempfs · `ci` tier).*

Two stacked scan-path optimizations, neither touching served bytes:

1. **#67 — Lazy ID3v1 tail.** The bounded probe path issued a 128-byte
   `read_exact` at the file tail for *every* front-anchored file, but only MP3
   consumes the ID3v1 frame. Gating the tail read to `.mp3` files drops one
   `read_exact` per non-MP3 file, saving exactly 128 bytes of scan I/O each.
2. **#68 — Move-not-clone ingest.** `ingest_bulk` previously cloned each
   picture's bytes (`Vec::clone`) because the batch held `&Probed` borrows.
   The writer now drains the owned `Unit` batch by value, moving the payload
   into the DB structs instead of copying it.

- **Before** = `main` @ `e7ae912` (post-#69): reads ID3v1 tail for all formats; clones picture bytes.
- **After** = `phase6-pr2-scan-pair` @ `9b49a63`: tail gated to `.mp3`; picture bytes moved.
- Harness: `cargo test -p musefs-core --release --features metrics --test bench_ingest -- --ignored --nocapture bench_cold_scan_and_revalidate`. Three independent runs; `bytes_read` = `scan_bytes_read`.

### Scan — `ci` tier (200 tracks × 4 KiB, no embedded art), tempfs

| format    | before wall (ms, 3 runs) | after wall (ms, 3 runs) | before bytes_read | after bytes_read | Δ bytes/file |
|-----------|-------------------------:|------------------------:|------------------:|-----------------:|-------------:|
| flac      | 32 / 30 / 34            | 30 / 29 / 28           | 870 600           | 845 000          | **−128 B**   |
| mp3       | 21 / 21 / 20            | 23 / 23 / 22           | 847 200           | 847 200          | 0 (tail still read) |
| m4a       | 27 / 29 / 24            | 25 / 28 / 28           | 0                 | 0                | n/a          |
| m4a-last  | 28 / 27 / 25            | 28 / 34 / 25           | 0                 | 0                | n/a          |
| ogg       | 20 / 22 / 22            | 23 / 22 / 20           | 873 000           | 847 400          | **−128 B**   |
| wav       | 24 / 23 / 20            | 23 / 22 / 23           | 853 600           | 828 000          | **−128 B**   |

**#67 signal:** Non-MP3 formats (flac, ogg, wav) show exactly −128 bytes/file in
`bytes_read` — the 128-byte ID3v1 tail read is no longer issued for those formats.
MP3 is unchanged (it still consumes the tail). M4A is 0 in both directions (it
uses a seek-reader, not the front-anchored probe path).

**#68 signal:** Structural (move vs clone) — wall times are within run-to-run noise
on the `ci` tier because the 4 KiB test files have no embedded art, so there is no
picture payload to move. The win appears on art-bearing corpora (the `bandwidth`
tier from SP1 §2 or real libraries) where the clone was O(art-size) per file.

**Wall time:** held or improved across all formats; no >10% rise.

`opens`/`preads` stay at 0 on the scan path (they are serve-path counters),
matching the documented expectation.

```bash
# Before (main):
git checkout main
MUSEFS_BENCH_TIER=ci \
  cargo test -p musefs-core --release --features metrics --test bench_ingest \
  -- --ignored --nocapture bench_cold_scan_and_revalidate

# After (branch):
git checkout phase6-pr2-scan-pair
MUSEFS_BENCH_TIER=ci \
  cargo test -p musefs-core --release --features metrics --test bench_ingest \
  -- --ignored --nocapture bench_cold_scan_and_revalidate
```

---

## Phase 6 PR 3 — Serve-path copies (#70)

*Measured 2026-06-04 (same box, lightly loaded · tempfs · Criterion `ci` tier).*

Four stacked serve-path optimizations, none touching synthesis or layout (served
audio stays byte-identical by construction):

1. **DB chunk readers write directly into caller buffers** (`read_at_exact`
   fills the caller's `&mut [u8]` slice instead of allocating a throwaway
   `Vec<u8>` + `extend_from_slice`).
2. **`read_segments` fills the caller's buffer** — the `ArtImage`,
   `BinaryTag`, and raw `OggArtSlice` arms write into the output `Vec`'s
   resized tail instead of a temporary alloc + memcpy per chunk
   (`BackingAudio`/`OggAudio`/`Inline` already wrote into `out`).
3. **`Musefs::read_into` serves into a caller buffer** — the FUSE layer passes
   a `&mut Vec<u8>` destination and the core fills it in place, avoiding an
   intermediate `Vec` allocation for the entire read.
4. **FUSE per-worker thread-local scratch buffer** — each worker reuses a
   single `Vec<u8>` across reads instead of allocating a fresh one per `read()`
   syscall.

Note: fuser 0.17 already sends a borrowed iovec (`read` receives `&mut [u8]`);
the win here is chunk direct-write + allocation elimination, not a fuser-layer
copy.

- **Before** = `main` @ `2d4faf3` (pre-#70): throwaway allocs per splice, fresh
  `Vec` per FUSE read.
- **After** = `phase6-pr3-serve-copies` @ `32be8f0`: direct-write into caller
  buffers, thread-local scratch.

### `sequential_read` — per-format median (the >10%-rise regression gate)

`ci` tier, 4 MiB single-track files, 128 KiB reads, `fh=0` (no-handle path →
each read resolves via the header cache). The regression gate is a **>10% rise**
run-over-run.

| format    | before (µs) | after (µs) | Δ        | note |
|-----------|------------:|-----------:|---------:|------|
| flac      | 940         | 837        | −10.9%   | improved |
| mp3       | 919         | 808        | −12.1%   | improved |
| m4a       | 921         | 827        | −10.2%   | improved |
| m4a-last  | 950         | 828        | −12.8%   | improved |
| ogg       | 1123        | 969        | −13.7%   | improved |
| wav       | 934         | 819        | −12.3%   | improved |

No format breaches the >10% *rise* gate. All formats improve 10–14% from
eliminating per-splice alloc+copy and per-read `Vec` allocation.

### `concurrent_read_walk/m16_plus_walker` — contention signal

16 reader threads streaming distinct files + one metadata walker, sharing one
`Arc<Musefs>`. Burst-concurrency wall time (includes thread spawn/join):

| | before (ms) | after (ms) | Δ |
|---|---:|---:|---:|
| m16_plus_walker | 7.92 | 8.32 | +5.0% (p=0.17, no change detected) |

Within noise; the thread-local scratch buffer is per-worker, so contention on
the shared state is unchanged.

### Ogg representative benches (SP4 regression gate)

`cold_first_read` and `seek_read` (fresh mount per iteration):

| workload | before | after | Δ |
|----------|-------:|------:|--:|
| `cold_first_read/ogg` | 1.857 ms | 1.719 ms | −7.4% |
| `seek_read/ogg` | 783 µs | 776 µs | −0.9% |

Both held or improved.

### Gates

- Byte-identical: `proptest_read_fidelity` (17) + `musefs-format --features
  fuzzing` (317) green.

```bash
# Both benches (Criterion records its own before/after baseline):
cargo bench -p musefs-core --bench read_throughput

# Byte-identical gates:
cargo test -p musefs-core --test proptest_read_fidelity
cargo test -p musefs-format --features fuzzing
```

---

## 2026-06-05 — HeaderCache: hand-rolled sharded LRU → quick_cache (#136)

S3-FIFO byte-weighted cache replaces the 16-shard Mutex LRU; the serve
path's last shared std lock is gone. `read_throughput` before/after:

| workload | before | after | Δ |
|----------|-------:|------:|--:|
| `sequential_read/flac` | 817.12 µs | 809.48 µs | −0.9% |
| `sequential_read/mp3` | 815.81 µs | 823.02 µs | +0.9% |
| `sequential_read/m4a` | 821.52 µs | 791.77 µs | −3.6% |
| `sequential_read/m4a-last` | 825.11 µs | 809.40 µs | −1.9% |
| `sequential_read/ogg` | 944.80 µs | 981.06 µs | +3.8% |
| `sequential_read/wav` | 831.54 µs | 814.88 µs | −2.0% |
| `cold_first_read/flac` | 1.5476 ms | 1.5769 ms | +1.9% |
| `cold_first_read/mp3` | 1.5412 ms | 1.5501 ms | +0.6% |
| `cold_first_read/m4a` | 1.5360 ms | 1.5296 ms | −0.4% |
| `cold_first_read/m4a-last` | 1.5496 ms | 1.5300 ms | −1.3% |
| `cold_first_read/ogg` | 1.7133 ms | 1.7151 ms | +0.1% |
| `cold_first_read/wav` | 1.5937 ms | 1.5466 ms | −3.0% |
| `seek_read/flac` | 553.30 µs | 543.93 µs | −1.7% |
| `seek_read/mp3` | 539.68 µs | 523.98 µs | −2.9% |
| `seek_read/m4a` | 559.38 µs | 544.93 µs | −2.6% |
| `seek_read/m4a-last` | 561.40 µs | 528.78 µs | −5.8% |
| `seek_read/ogg` | 798.29 µs | 754.28 µs | −5.5% |
| `seek_read/wav` | 561.63 µs | 567.63 µs | +1.1% |
| `concurrent_read_walk/m16_plus_walker` | 5.5074 ms | 5.4036 ms | −1.9% |

No workload shows a regression outside noise. `seek_read` formats trend 2–6%
faster (cache lookup path no longer behind a Mutex). `sequential_read/ogg`
(+3.8%) and `cold_first_read/flac` (+1.9%) are within Criterion's noise
threshold (p>0.05, "No change in performance detected").

---

## Issue #112 — StructureOnly kernel passthrough

*Measured 2026-06-06.*

- **Before** = `main` @ `0881b31`: every read round-trips kernel → daemon → positioned read → copy back.
- **After** = `issue-112-passthrough`: backing fd registered at open (FUSE passthrough, kernel 6.9+); the kernel serves reads directly from the backing inode, bypassing the daemon entirely.
- Harness: 512 MiB single-track FLAC StructureOnly mount, `dd bs=1M` sequential read, fresh mount per binary with 3 runs inside it, RAM-cached backing file (isolates FUSE-path overhead). Both binaries mounted via `sudo` (passthrough requires `CAP_SYS_ADMIN` for `FUSE_DEV_IOC_BACKING_OPEN`).

| | run 1 | run 2 | run 3 | median |
|---|---|---|---|---|
| Before (daemon reads) | 2.7 GB/s | 2.8 GB/s | 2.8 GB/s | 2.8 GB/s |
| After (passthrough) | 9.3 GB/s | 9.3 GB/s | 9.4 GB/s | 9.3 GB/s |

Passthrough is **~3.3× faster** on this RAM-cached sequential-read workload: the before path round-trips every ~128 KiB chunk through the daemon (wakeup + positioned read into the reply buffer + copy back through `/dev/fuse`), while the after path reads straight from the backing inode's page cache like a native file.
