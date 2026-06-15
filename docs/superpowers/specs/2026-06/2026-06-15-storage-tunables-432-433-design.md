# Storage tunables follow-ups: #432 (keep-cache default) + #433 (window-cap HDD bench)

**Date:** 2026-06-15
**Branch:** `tunables`
**Issues:** [#432](https://github.com/Sohex/musefs/issues/432), [#433](https://github.com/Sohex/musefs/issues/433)
**Lands as:** two commits, one PR.

These are sibling follow-ups to the storage-tunables investigation (#256): #432
acts on its one measured win, #433 closes a gap in what it measured.

---

## #432 — Default `keep_cache` to `true`

### Problem

`FuseConfig::keep_cache` defaults to `false` (`musefs-fuse/src/lib.rs:84`), so
`FOPEN_KEEP_CACHE` is omitted and the kernel drops the page cache on every close,
re-reading from the daemon on the next open. BENCHMARKS.md (#256) measured
`--keep-cache` as the one real storage win (~3× faster repeat-open on HDD/NFS),
and the inode-invalidation machinery that keeps it consistent under external
retags already exists (`poll_refresh_notify` → `inval_inode`). The conservative
default is no longer justified.

### Change

1. `musefs-fuse/src/lib.rs:84` — `keep_cache: false` → `true` in
   `Default for FuseConfig`.
2. `musefs-cli/src/lib.rs:111` — make the flag value-taking with a `true`
   default while keeping the bare flag working:
   ```rust
   #[arg(long, env = "MUSEFS_KEEP_CACHE", default_value_t = true,
         num_args = 0..=1, default_missing_value = "true",
         value_parser = clap::builder::BoolishValueParser::new())]
   pub keep_cache: bool,
   ```
   This gives three working forms — default `true`, bare `--keep-cache`
   (still `true`, backward-compatible), and `--keep-cache false` to opt out.
   It also fixes the latently-broken `--keep-cache false` call in
   `storage_tunables_bench.sh:157` (which errors against today's presence-only
   flag).

### Why `num_args = 0..=1` over `action = Set`

`action = Set` (the `case_insensitive` convention) would require a value and turn
a bare `--keep-cache` into a parse error — a needless break for existing scripts.
`num_args = 0..=1` + `default_missing_value` is strictly more compatible and
costs nothing.

### Tests (TDD: adjust assertions to the new default first, watch them fail, then flip)

- `musefs-fuse/src/lib.rs:1052` — `assert!(!c.keep_cache)` → assert default `true`.
- `musefs-cli/src/lib.rs:568` — flip the default assertion; add a case proving
  `--keep-cache false` parses to `keep_cache == false`, and that bare
  `--keep-cache` still yields `true`.
- `musefs-fuse/tests/keep_cache.rs` — audit for any default-off assumption.

### Docs

- `README.md:417` — flip the table row from "disabled" to "enabled by default";
  reword the prose at `README.md:409` if it implies the knobs are off.
- `keep_cache` doc comments in both crates — note it is now on by default and how
  to disable.
- BENCHMARKS.md — one-line note that the measured win is now the default.

### Validation

Run the diff through the local in-diff mutation gate (SP convention: `-j2`,
`--output` on `/tmp`, `TMPDIR=$HOME/.cache/musefs-mutants-tmp`, sanity-check the
diff is non-empty). `0 missed` is the gate. Also run the `metrics`-feature core
tests if the open/read path is touched (it is not, but the assertion counts are
fragile) — N/A here since only defaults/flags change.

---

## #433 — Benchmark the 8 MiB internal window cap on HDD

### Problem

The per-stream read-amplification window doubles per sequential read up to
`WINDOW_ABS_CAP = 8 MiB` (`musefs-core/src/readahead.rs:12`), a **compile-time
const**. The #256 investigation found large read-ahead hurts on HDD, but that
measured the FUSE kernel `max_readahead` knob, not this internal amplification
window. BENCHMARKS.md shows phase-1 amplification (at the 8 MiB cap) is *neutral*
on local HDD (~62 vs ~60 MB/s), but the cap itself was never swept on spinning
media, so its effect is unmeasured.

### Approach — measure, change only if warranted

Production `readahead.rs` stays untouched unless the data shows 8 MiB hurts.

**Mechanism.** Extend `benches/storage_tunables_bench.sh` with a `window-cap`
sweep: for each cap value it patches `WINDOW_ABS_CAP` in `readahead.rs`, rebuilds
`--release`, measures cold single-stream MB/s (and one concurrent-streams row) on
the real-FLAC HDD corpus, then restores the source. The patch/restore is
`git`-based and `trap`-protected so an interrupted run cannot leave the tree
modified or half-built.

**Corpus.** Real FLAC at `/data/torrents/completed/music` (btrfs HDD, 4389
files). The single-stream row uses the 1112 MiB FLAC for a long, stable cold
read; concurrent rows use the next-largest distinct tracks. Provision via
`cp --reflink=auto` into a writable backing dir (no byte copy — extents are
shared, cold reads still hit the platter) **or** point the scan at the tree
directly. The corpus must be incompressible real audio, not `/dev/zero` (a
compressing fs collapses zero-fill to a cached extent and silently inverts HDD
numbers — see BENCHMARKS.md methodology).

**Run conditions.** Mountpoint + DB on `/tmp` (AppArmor allows FUSE mounts
there; `/data` is denied). Cold samples via `echo 3 > /proc/sys/vm/drop_caches`
(needs `sudo`). Mode **synthesis** (structure-only triggers kernel passthrough
when privileged and bypasses the daemon read path). Cap sweep: 1 / 2 / 4 / 8 /
16 MiB, median of 3.

### Outcome

A new subsection under "Backing read-ahead (#255)" in BENCHMARKS.md with the
sweep table, methodology, and reproduce command.

- **If 8 MiB is within run-to-run noise / optimal:** no code change. Document and
  close #433 with the evidence.
- **If a smaller cap clearly wins on HDD:** stop and bring the data back before
  touching the const or adding a runtime knob (a permanent tunable is out of
  scope for this pass per the agreed "change only if warranted" decision).

### Validation

Bench + docs only in the no-change case (no Rust source change → no mutation
gate). The new bench sweep is committed in-tree as a runnable script section
(per the harness-in-tree convention).

---

## Out of scope

- A permanent `--read-ahead-window-cap-*` runtime tunable (only if #433 data
  warrants it; would be a separate decision).
- Re-touching `max_readahead` / `max_background` — empirically disproven in #256,
  do not re-propose without new evidence.
- Per-medium `--storage-profile` presets — proposed and dropped in #256.
