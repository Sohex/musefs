# Spec: Re-run & rewrite BENCHMARKS.md (apples-to-apples, this box)

*2026-06-08*

## Goal

Every number in `BENCHMARKS.md` currently comes from different machines and
conditions, accreted across many sessions (the header machine — AMD EPYC 6-core
/ 17 GiB SSD — is not the machine the repo now lives on, and several sections
were measured "under load"). Re-measure every pass on **this box** under a
single, documented methodology so the numbers are mutually comparable and
self-consistent, then rewrite the file in a layered structure that reads clearly.

This is a measurement + documentation task only. **No code, optimization, or
harness changes.** The benchmark harnesses (`bench_ingest`, `bench_refresh`,
`read_throughput`) and the served-byte invariant are untouched.

## Decisions (settled during brainstorming)

1. **Re-measure before/after pairs**, not current-state-only. The file's value is
   per-pass *attribution* ("this change bought X"), so the before/after story is
   preserved.
2. **After = the pass's own squash-merge commit; before = its parent
   (`<after>^`).** Each pass is isolated against its immediate predecessor — NOT
   against current `main`. Rationale: overlapping code paths (e.g. `read_segments`
   alloc elimination was done in both SP3 and phase6-pr3) would double-count and
   misattribute if every section used current `main` as "after"; and adjacent
   commits share a harness, so each isolated pair is self-consistent (no
   harness-drift confound).
3. **Plus one cumulative summary** `16caba4` → current `main`, clearly labelled as
   cumulative (non-isolating), to show the total journey.
4. **Storage class per section is preserved**, mapped onto this box's equivalents:
   durable → `/data`, RAM-backed → `/dev/shm`.
5. **Layered file structure**: at-a-glance summary → shared methodology → trimmed
   per-pass detail sections.

## This box (new machine header)

- 8-core, **32 GB RAM**, `rustc 1.96`, release builds.
- **Durable storage → `/data`**: a 2-device btrfs spanning the rotational disks
  `/dev/sda3` + `/dev/sdb3` (`compress=zstd:1`, `noatime`), 3.0 TB free, profile
  `Data: single` / `Metadata,System: RAID1` (so metadata fsyncs mirror to both
  spindles). Distinct from the `md` RAID that backs `/home`. No SSD on this box —
  durable benches are rotational, so SP1's fsync-bound "before" absolutes will be
  *larger* than the header SSD's (and the RAID1 metadata mirroring adds to that);
  expected, and still a valid before/after on identical hardware. `zstd:1` is
  irrelevant to the fsync-count tests and the bandwidth-tier FLAC corpus is largely
  incompressible, so it does not materially distort the I/O numbers; noted in the
  header for honesty.
- **RAM-backed → `/dev/shm`** (16 GiB tmpfs) for the compute-isolated / tempfs
  sections.
- One-time setup (requires sudo): `sudo mkdir -p /data/musefs-bench && sudo chown
  "$USER" /data/musefs-bench`.

## Methodology (written once in the rewritten file)

- **Before/after definition** as in decision 2 above. Both sides built fresh on
  this box from a clean checkout of the respective commit.
- **Run convention:**
  - Ignored-test benches (`bench_ingest`, `bench_refresh`): **3 runs, report
    median**; show the spread where runs are noisy.
  - Criterion (`read_throughput`): its own sampling; report Criterion's median and
    change-estimate (p-value) as the file already does.
  - Box quiesced before each run set.
- **Storage class preserved per section** (durable → `/data`, RAM → `/dev/shm`),
  set via `MUSEFS_BENCH_DIR` / the relevant tier env vars.

## Rerun matrix

| Pass | Before | After | Harness | Storage |
|------|--------|-------|---------|---------|
| SP1 ingest (small-files §1, bandwidth §2, fsync-count §3, large-compute §4) | `sp1^` | sp1 merge | `bench_ingest` | durable `/data` (§1–§3) + `/dev/shm` (§4) + latencyfs (§3) |
| SP2 refresh (Stage A→B) | `sp2^` | sp2 merge | `bench_refresh` | `/dev/shm` |
| SP3 read residuals | `sp3^` | sp3 merge | `read_throughput` | `/dev/shm` |
| SP4 ogg serve | `sp4^` | sp4 merge | `read_throughput` (+ latencyfs) | `/dev/shm` |
| Phase 6 PR1 (#69) | `e7ae912^` | `e7ae912` | `bench_refresh` | `/dev/shm` |
| Issue #114 root fan-out | `0881b31^` | `0881b31` | `bench_refresh` | `/dev/shm` |
| Phase 6 PR2 (#67/#68) | `2d4faf3^` | `2d4faf3` | `bench_ingest` | `/dev/shm` |
| Phase 6 PR3 (#70) | `32be8f0^` | `32be8f0` | `read_throughput` | `/dev/shm` |
| HeaderCache (#136) | `#136^` | #136 merge | `read_throughput` | `/dev/shm` |
| Issue #112 passthrough | `0881b31` (pre) | passthrough merge | `dd` on real mount, **sudo** | RAM-cached backing |
| **Cumulative** | `16caba4` | current `main` | all three | matched per-bench |

Merge commits for the pre-SHA-convention passes (SP1, SP2, SP3, SP4, #136, #112)
are pinned during planning via `git log --grep`. Documented anchors already known:
`16caba4` (baseline), `e7ae912` (after-#69 / before-PR2), `2d4faf3` (after-PR2 /
before-PR3), `32be8f0` (after-PR3), `0881b31` (after-#114), `9b49a63`
(intermediate #68).

## Known wrinkles (resolved during planning/execution)

- **SP2 Stage A vs Stage B** is an intra-pass two-commit split. If those two
  commits are not cleanly isolable on `main`, SP2 collapses to a single pre-SP2 →
  post-SP2 pair. Granularity confirmed once the SP2 merge commit is located. Same
  fallback applies to any other multi-stage pass.
- **Old-commit build drift:** adjacent before/after commits share a harness, so
  isolated pairs are self-consistent. The **cumulative** row spans commits whose
  harnesses differ, so cumulative numbers are produced by running **each commit's
  own harness** for the same workload, labelled non-isolating. Lockfile/rustc
  drift on the old commits is low-risk (all May–June 2026, rustc 1.96); build
  failures are caught at run time.
- **`bandwidth` tier (~30 GiB)** lands on durable `/data` only (won't fit RAM);
  it is long-running, so a single run is acceptable unless cheap to repeat.

## Gates removed entirely

The per-pass "Gates" subsections (byte-identical proptests, in-diff mutation) are
**correctness** results, not performance, and are **dropped from `BENCHMARKS.md`
entirely** — not re-run, not kept as notes. Correctness lives in the test suite,
CI, and the per-PR history; this file is about performance only.

## Rewritten file structure (layered)

1. **Results at a glance** — the cumulative `16caba4` → `main` summary (one line
   per subsystem: ingest / refresh / serve), plus one headline number per pass.
2. **Methodology** — the block above, written once (machine, before/after
   definition, run convention, storage mapping).
3. **Per-pass detail sections** — same passes as today, each trimmed to: a
   one-line "what changed", the before/after table with this-box numbers, and the
   reproduce command. No gate notes; repeated methodology boilerplate removed.

## Out of scope

- No code, optimization, or benchmark-harness changes.
- No new benchmarks.
- Old numbers are replaced, not archived in-file (git history preserves them).
