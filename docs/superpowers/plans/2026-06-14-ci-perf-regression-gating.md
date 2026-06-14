# CI Performance-Regression Gating Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give musefs CI a regression signal for the read/serve, ingest, and refresh paths without flaky wall-clock gating.

**Architecture:** Three independently-shippable PRs. PR1 is a hard, zero-flake gate of deterministic work *counters* (reusing the existing `metrics` feature — no production code). PR2 adds a warn-only same-runner A/B wall-clock job that runs only when read/synthesis `src` changes. PR3 records a full bench snapshot at release time as an artifact.

**Tech Stack:** Rust, `cargo test --features metrics`, criterion (`read_throughput` bench), `critcmp`, GitHub Actions, bash.

**Source spec:** `docs/superpowers/specs/2026-06-14-ci-perf-regression-gating-design.md`

---

## File structure

| File | PR | Responsibility |
| ---- | -- | -------------- |
| `musefs-core/tests/perf_counters.rs` | 1 | New `--features metrics` test module: per-format golden read counters + ingest slurp-window counters. |
| `musefs-core/src/tree.rs` | 1 | One new unit test appended to the existing `#[cfg(test)] mod` asserting `apply_changes` rebuild count is size-invariant. |
| `CONTRIBUTING.md`, `BENCHMARKS.md` | 1 | Document the counter gate + (PR2) the A/B job. |
| `scripts/perf-ab.sh` | 2 | Same-runner base-vs-PR criterion A/B + critcmp; emits a markdown table. |
| `.github/workflows/ci.yml` | 2 | New `perf` output on the `changes` job; new `perf-ab` job. |
| `scripts/perf-release-bench.sh` | 3 | Run the full read bench + `ci`-tier ingest/refresh benches, capture output. |
| `.github/workflows/release.yml` | 3 | New `benchmarks` job uploading the snapshot artifact. |

**Key existing APIs (verified, do not re-derive):**
- `common::corpus::{ALL_FORMATS, Format, CorpusParams, prepare_format, format_token, Target}` — `prepare_format(&params, base, fmt) -> Target { corpus_dir, .. }` generates a single-format corpus under `base/<token>/`.
- `musefs_core::{Musefs, MountConfig, Mode, VirtualTree, scan_directory, metrics}`; `metrics::{reset, snapshot, Snapshot}`. `Snapshot` fields: `opens, stats, preads, pread_bytes, art_chunks, binary_tag_chunks, scan_opens, scan_preads, scan_bytes_read`.
- `fs.readdir(ino) -> Vec<(String, u64, bool)>` (name, inode, is_dir); `fs.getattr(ino).size`; `fs.read(ino, None, off, len) -> Vec<u8>`; `VirtualTree::ROOT`.
- In `tree.rs` test module: `VirtualTree::build_with(&[(i64,String)], &mut alloc)`, `InodeAllocator::new(false)`, `trs(path) -> TrackRenderState`, `apply_changes(&new_paths, changed, added, removed, &mut alloc) -> Result<usize, RebuildError>` where `usize` is the rebuild-subtree count.
- `metrics` counters are process-global; serialize cases with a `static METRICS_LOCK: Mutex<()>` and `metrics::reset()` before each measured region (pattern: `musefs-core/tests/metrics.rs`).

---

# PR 1 — Lane 1: deterministic counter gate (no production code)

Runs in the existing `check` job's "Core metrics-feature tests" step
(`cargo test -p musefs-core --features metrics`) — no workflow change. Must land
green (the pre-commit hook runs the full workspace suite).

### Task 1.1: read-counter module scaffold + sequential-read goldens

**Files:**
- Create: `musefs-core/tests/perf_counters.rs`

- [ ] **Step 1: Write the module skeleton with the shared fixture + lock**

```rust
#![cfg(feature = "metrics")]

mod common;
use common::corpus::{ALL_FORMATS, CorpusParams, Format, format_token, prepare_format};
use musefs_core::{Mode, MountConfig, Musefs, VirtualTree, metrics, scan_directory};
use std::collections::BTreeMap;
use std::sync::Mutex;

/// The `metrics` counters are global statics; serialize every measured region.
static METRICS_LOCK: Mutex<()> = Mutex::new(());

/// Audio payload size for every read golden (4 MiB, matching `read_throughput`).
const AUDIO_BYTES: u64 = 4 * 1024 * 1024;
/// 128 KiB read chunk (matching `read_throughput`).
const CHUNK: u64 = 128 * 1024;

fn config() -> MountConfig {
    MountConfig {
        template: "$artist/$album/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode: Mode::Synthesis,
        poll_interval: std::time::Duration::ZERO,
        case_insensitive: false,
    }
}

/// Recursively collect every file inode (non-FLAC corpus tracks render under
/// the `Unknown/` fallback, so we discover by a format-agnostic tree walk).
fn collect_file_inodes(fs: &Musefs, dir: u64, out: &mut Vec<u64>) {
    for (_, ino, is_dir) in fs.readdir(dir).unwrap() {
        if is_dir {
            collect_file_inodes(fs, ino, out);
        } else {
            out.push(ino);
        }
    }
}

/// Generate a single-format corpus (audio-only, fixed seed/size), scan + mount,
/// and return (fs, first-file-inode, tempdir-guard).
fn mount_one(fmt: Format, bytes_per_track: usize, art_bytes_per_track: usize)
    -> (Musefs, u64, tempfile::TempDir)
{
    let base = tempfile::tempdir().unwrap();
    let params = CorpusParams {
        albums: 1,
        tracks_per_album: 1,
        bytes_per_track,
        art_bytes_per_track,
        format_mix: vec![fmt],
        seed: 42,
    };
    let target = prepare_format(&params, base.path(), fmt);
    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, &target.corpus_dir).unwrap();
    let fs = Musefs::open(db, config()).unwrap();
    let mut inodes = Vec::new();
    collect_file_inodes(&fs, VirtualTree::ROOT, &mut inodes);
    assert!(!inodes.is_empty(), "no file inodes for {fmt:?}");
    (fs, inodes[0], base)
}

fn read_whole(fs: &Musefs, inode: u64) {
    let size = fs.getattr(inode).unwrap().size;
    let mut off = 0u64;
    while off < size {
        let got = fs.read(inode, None, off, CHUNK).unwrap();
        if got.is_empty() {
            break;
        }
        off += got.len() as u64;
    }
}
```

- [ ] **Step 2: Add the sequential-read golden test (audio read exactly once, no art/tags)**

```rust
/// Whole-file sequential read of an audio-only file: the audio payload is read
/// exactly once (no slurp / no over-read), and no art or binary-tag chunks are
/// emitted. `pread_bytes` is the load-bearing slurp guard.
#[test]
fn sequential_read_audio_read_exactly_once() {
    let _g = METRICS_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    for &fmt in ALL_FORMATS {
        let (fs, inode, _dir) = mount_one(fmt, AUDIO_BYTES as usize, 0);
        metrics::reset();
        read_whole(&fs, inode);
        let s = metrics::snapshot();
        assert_eq!(
            s.pread_bytes, AUDIO_BYTES,
            "{}: audio body must be read exactly once (slurp/over-read guard)",
            format_token(fmt),
        );
        assert_eq!(s.art_chunks, 0, "{}: audio-only must emit no art chunks", format_token(fmt));
        assert_eq!(
            s.binary_tag_chunks, 0,
            "{}: audio-only must emit no binary-tag chunks", format_token(fmt),
        );
    }
}
```

- [ ] **Step 3: Run the test, expect PASS**

Run: `cargo test -p musefs-core --features metrics --test perf_counters sequential_read_audio_read_exactly_once -- --nocapture`
Expected: PASS. (If `pread_bytes` differs for a format, that is a real finding — investigate before adjusting; the audio payload is read once by construction.)

- [ ] **Step 4: Commit**

```bash
git add musefs-core/tests/perf_counters.rs
git commit -m "test(core): golden sequential-read counters per format (#211)"
```

### Task 1.2: freeze per-format `preads` + seek goldens (characterization)

`preads` (syscall count) and the seek read's `pread_bytes` are deterministic but
format-specific; observe them once and pin them as constants.

**Files:**
- Modify: `musefs-core/tests/perf_counters.rs`

- [ ] **Step 1: Add a per-format expected-constants table and the seek fixture, using sentinel `0`s to be filled in Step 3**

```rust
/// Frozen per-format goldens. `(seq_preads, seek_preads, seek_pread_bytes)`.
/// seek = one 128 KiB read near EOF; it must touch a BOUNDED window, never the
/// whole file/index. Filled by the characterization run in Step 3 — a change
/// here means real read-path work changed; update in the same PR.
fn goldens(fmt: Format) -> (u64, u64, u64) {
    match fmt {
        Format::Flac => (0, 0, 0),
        Format::Mp3 => (0, 0, 0),
        Format::M4aMoovFirst => (0, 0, 0),
        Format::M4aMoovLast => (0, 0, 0),
        Format::Ogg => (0, 0, 0),
        Format::Wav => (0, 0, 0),
    }
}

const SEEK_OFF: u64 = 3_500_000;

#[test]
fn read_preads_and_seek_match_goldens() {
    let _g = METRICS_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    for &fmt in ALL_FORMATS {
        let (exp_seq_preads, exp_seek_preads, exp_seek_bytes) = goldens(fmt);

        let (fs, inode, _dir) = mount_one(fmt, AUDIO_BYTES as usize, 0);
        metrics::reset();
        read_whole(&fs, inode);
        let seq = metrics::snapshot();
        assert_eq!(seq.preads, exp_seq_preads, "{}: sequential preads", format_token(fmt));

        // Fresh mount → cold cache → single deep read.
        let (fs2, inode2, _dir2) = mount_one(fmt, AUDIO_BYTES as usize, 0);
        metrics::reset();
        let _ = fs2.read(inode2, None, SEEK_OFF, CHUNK).unwrap();
        let seek = metrics::snapshot();
        assert_eq!(seek.preads, exp_seek_preads, "{}: seek preads", format_token(fmt));
        assert_eq!(
            seek.pread_bytes, exp_seek_bytes,
            "{}: seek must read a bounded window, not the whole file/index", format_token(fmt),
        );
        assert!(
            seek.pread_bytes < AUDIO_BYTES / 4,
            "{}: seek read {} bytes — not a bounded window", format_token(fmt), seek.pread_bytes,
        );
    }
}
```

- [ ] **Step 2: Run the test to characterize — it will FAIL and print actual vs expected**

Run: `cargo test -p musefs-core --features metrics --test perf_counters read_preads_and_seek_match_goldens -- --nocapture`
Expected: FAIL. Each panic line prints the actual value, e.g. `flac: sequential preads ... left: 33, right: 0`. Record the actual `seq.preads`, `seek.preads`, and `seek.pread_bytes` for every format.

- [ ] **Step 3: Replace the sentinel `0`s in `goldens()` with the observed values**

Edit `goldens()` so each arm holds the `(seq_preads, seek_preads, seek_pread_bytes)` triple you recorded. Example shape (your numbers will differ):

```rust
        Format::Flac => (33, 1, 131072),
```

- [ ] **Step 4: Re-run, expect PASS**

Run: `cargo test -p musefs-core --features metrics --test perf_counters read_preads_and_seek_match_goldens -- --nocapture`
Expected: PASS for all six formats. Confirm every `seek_pread_bytes` is well under `AUDIO_BYTES/4` (the bounded-window invariant).

- [ ] **Step 5: Commit**

```bash
git add musefs-core/tests/perf_counters.rs
git commit -m "test(core): freeze per-format read preads + bounded-seek goldens (#211)"
```

### Task 1.3: ingest slurp-window golden

A corpus whose files exceed the ~1 MiB bounded metadata window, so a reintroduced
whole-file slurp inflates `scan_bytes_read`.

**Files:**
- Modify: `musefs-core/tests/perf_counters.rs`

- [ ] **Step 1: Add the ingest test (FLAC, 2 MiB/track > window), sentinel `0`s**

```rust
/// Ingest of files LARGER than the ~1 MiB bounded metadata window: the scanner
/// reads only a bounded prefix, never the whole file. A reintroduced slurp shows
/// up as `scan_bytes_read` jumping toward `tracks * 2 MiB`. Counts frozen below.
#[test]
fn ingest_reads_bounded_prefix_not_whole_file() {
    let _g = METRICS_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    const TRACKS: usize = 3;
    const BYTES_PER_TRACK: usize = 2 * 1024 * 1024; // > 1 MiB scan window
    // (scan_opens, scan_preads, scan_bytes_read) — filled in Step 3.
    let (exp_opens, exp_preads, exp_bytes): (u64, u64, u64) = (0, 0, 0);

    let base = tempfile::tempdir().unwrap();
    let params = CorpusParams {
        albums: 1,
        tracks_per_album: TRACKS,
        bytes_per_track: BYTES_PER_TRACK,
        art_bytes_per_track: 0,
        format_mix: vec![Format::Flac],
        seed: 42,
    };
    let target = prepare_format(&params, base.path(), Format::Flac);
    let db = musefs_db::Db::open_in_memory().unwrap();
    metrics::reset();
    scan_directory(&db, &target.corpus_dir).unwrap();
    let s = metrics::snapshot();

    assert_eq!(s.scan_opens, exp_opens, "scan_opens");
    assert_eq!(s.scan_preads, exp_preads, "scan_preads");
    assert_eq!(s.scan_bytes_read, exp_bytes, "scan_bytes_read");
    // Hard upper bound independent of the frozen number: must be far below a slurp.
    assert!(
        s.scan_bytes_read < (TRACKS as u64) * BYTES_PER_TRACK as u64 / 2,
        "scan read {} bytes — looks like a whole-file slurp", s.scan_bytes_read,
    );
}
```

- [ ] **Step 2: Run to characterize — FAIL prints actuals**

Run: `cargo test -p musefs-core --features metrics --test perf_counters ingest_reads_bounded_prefix_not_whole_file -- --nocapture`
Expected: FAIL printing actual `scan_opens` (expect 3), `scan_preads`, `scan_bytes_read`. Record them.

- [ ] **Step 3: Replace the `(0, 0, 0)` tuple with the observed values**

```rust
    let (exp_opens, exp_preads, exp_bytes): (u64, u64, u64) = (3, /*…*/, /*…*/);
```

- [ ] **Step 4: Re-run, expect PASS; confirm `scan_bytes_read` ≪ slurp bound**

Run: `cargo test -p musefs-core --features metrics --test perf_counters ingest_reads_bounded_prefix_not_whole_file -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add musefs-core/tests/perf_counters.rs
git commit -m "test(core): ingest bounded-prefix scan_bytes golden (#211)"
```

### Task 1.4: refresh size-invariance unit test (in `tree.rs`)

`apply_changes` is `pub(crate)`, so this lives in the in-module test suite.
**Append at the very end of the `#[cfg(test)] mod` (before its closing `}`)** so
no line shifts above the `InodeAllocator` mutants anchors.

**Files:**
- Modify: `musefs-core/src/tree.rs` (end of the test module)

- [ ] **Step 1: Confirm the current `.cargo/mutants.toml` anchors pass (baseline)**

Run: `python3 scripts/check_mutant_anchors.py` (or the path the pre-commit hook uses — `grep -n mutant_anchors .pre-commit-config.yaml` to find it)
Expected: exits 0 (anchors valid before the edit).

- [ ] **Step 2: Append the test at the end of the test module**

Insert immediately before the final closing `}` of `mod tests` (after `dot_and_dotdot_plain_components_are_dropped`):

```rust
    /// Builds a many-album library, re-tags ONE track (renaming its leaf within
    /// its own album dir), and asserts `apply_changes` rebuilds exactly one
    /// subtree — *regardless of library size*. A reintroduced full reconstruction
    /// would rebuild every album dir, so the count would scale with size.
    fn library(albums: usize) -> Vec<(i64, String)> {
        let mut e = Vec::new();
        for a in 0..albums {
            for t in 0..3 {
                let id = (a * 3 + t) as i64;
                e.push((id, format!("Album{a:04}/t{t}.flac")));
            }
        }
        e
    }

    fn rebuilds_for_one_retag(albums: usize) -> usize {
        let entries = library(albums);
        let mut alloc = InodeAllocator::new(false);
        let mut t = VirtualTree::build_with(&entries, &mut alloc);
        // Re-tag track id 1 (Album0000/t1.flac) → renamed leaf in the SAME dir.
        let changed_id: i64 = 1;
        let mut new_entries = entries.clone();
        for (id, path) in &mut new_entries {
            if *id == changed_id {
                *path = "Album0000/renamed.flac".to_string();
            }
        }
        let new_paths: std::collections::HashMap<i64, TrackRenderState> =
            new_entries.iter().map(|&(id, ref p)| (id, trs(p))).collect();
        t.apply_changes(&new_paths, &[changed_id], &[], &[], &mut alloc)
            .unwrap()
    }

    #[test]
    fn apply_changes_rebuild_count_is_size_invariant() {
        let small = rebuilds_for_one_retag(43); // 129 tracks
        let large = rebuilds_for_one_retag(683); // 2049 tracks
        assert_eq!(small, 1, "one re-tag must rebuild exactly its own album dir");
        assert_eq!(
            small, large,
            "rebuild count must not grow with library size (O(changed), not O(N))",
        );
    }
```

- [ ] **Step 3: Run the new test, expect PASS**

Run: `cargo test -p musefs-core apply_changes_rebuild_count_is_size_invariant -- --nocapture`
Expected: PASS. (If `small != 1`, the re-tag is touching more than its dir — investigate; do not loosen the assertion to match.)

- [ ] **Step 4: Re-check mutants anchors are still valid**

Run: `python3 scripts/check_mutant_anchors.py`
Expected: exits 0. If it fails, re-anchor the shifted `.cargo/mutants.toml` entries (each via its `# guard:` tag) in this same change.

- [ ] **Step 5: Commit**

```bash
git add musefs-core/src/tree.rs
git commit -m "test(core): assert refresh rebuild count is size-invariant (#211)"
```

### Task 1.5: documentation

**Files:**
- Modify: `CONTRIBUTING.md` (test-tiers section)
- Modify: `BENCHMARKS.md` (new subsection)

- [ ] **Step 1: Find the CONTRIBUTING test-tiers anchor**

Run: `grep -nE "^#+ .*[Tt]est|mutation gate|test tiers" CONTRIBUTING.md | head`
Expected: a heading under which the test tiers (fuzzing, interop, mutation gate) are described.

- [ ] **Step 2: Add a "Performance regression gating" paragraph under that section**

```markdown
### Performance regression gating

`cargo test -p musefs-core --features metrics` includes
`tests/perf_counters.rs`: golden assertions on deterministic work counters
(`preads`, `pread_bytes`, `scan_bytes_read`, art/binary-tag chunks) for the
read/serve and ingest paths, plus a `tree.rs` unit test pinning the refresh
rebuild count as size-invariant. These are a hard gate — a legitimate change to
read/ingest/refresh work must update the golden numbers in the same PR. They run
on every non-doc PR via CI's `check` job. Constant-factor (wall-clock) changes
are surfaced separately by the warn-only `perf-ab` job (below).
```

- [ ] **Step 3: Add a "CI regression gating" subsection to BENCHMARKS.md**

Run: `grep -nE "^## |^### " BENCHMARKS.md | head` to find a home near the Methodology section, then add:

```markdown
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
```

- [ ] **Step 4: Verify the full metrics-feature leg is green**

Run: `cargo test -p musefs-core --features metrics`
Expected: PASS (all perf_counters tests + existing metrics tests).

- [ ] **Step 5: Commit**

```bash
git add CONTRIBUTING.md BENCHMARKS.md
git commit -m "docs: document the perf counter gate + CI gating lanes (#211)"
```

PR1 is complete: open it, confirm the `check` job is green.

---

# PR 2 — Lane 2: same-runner A/B wall-clock (warn-only)

### Task 2.1: `perf` path-filter output on the `changes` job

**Files:**
- Modify: `.github/workflows/ci.yml` (`changes` job, lines ~22-77)

- [ ] **Step 1: Add `perf` to the job `outputs` map**

In the `changes:` job's `outputs:` block (alongside `src`/`fuse`/`lidarr`), add:

```yaml
      perf: ${{ steps.filter.outputs.perf }}
```

- [ ] **Step 2: Emit `perf` in the filter step**

In the `no usable base ref` early-exit block, add `echo "perf=true" >> "$GITHUB_OUTPUT"` next to the other `true` writes. Then, after the `lidarr` block, add:

```bash
          # Read/synthesis surface only: the A/B wall-clock job is expensive
          # (builds criterion twice), so gate it to changes that can actually
          # move read latency. tests/ and benches/ are excluded by the anchors.
          if printf '%s\n' "$changed" | grep -qE '^(musefs-core/src/|musefs-format/src/)'; then
            echo "perf=true" >> "$GITHUB_OUTPUT"
          else
            echo "perf=false" >> "$GITHUB_OUTPUT"
          fi
```

- [ ] **Step 3: Lint the workflow YAML**

Run: `yamllint .github/workflows/ci.yml`
Expected: no errors (warnings consistent with the rest of the file are fine).

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: add perf path-filter output to the changes job (#211)"
```

### Task 2.2: the A/B harness script

**Files:**
- Create: `scripts/perf-ab.sh`

- [ ] **Step 1: Write the script**

```bash
#!/usr/bin/env bash
# Same-runner A/B wall-clock comparison of the read_throughput criterion bench.
# Benches the base ref and HEAD back-to-back on ONE machine (robust to
# runner-to-runner variance), then diffs with critcmp. Warn-only: always exits 0.
#
# Usage: scripts/perf-ab.sh <base-sha> <out-markdown-file>
# Requires: cargo, critcmp on PATH. Run from the repo root with a clean tree.
set -euo pipefail

BASE_SHA="${1:?base sha required}"
OUT="${2:?output markdown path required}"
BENCH=(cargo bench -p musefs-core --bench read_throughput --)

head_sha="$(git rev-parse HEAD)"

run_baseline() {
  local name="$1"
  "${BENCH[@]}" --save-baseline "$name" >/dev/null 2>&1
}

echo "Benching base ($BASE_SHA)…" >&2
git checkout --quiet --detach "$BASE_SHA"
run_baseline base

echo "Benching head ($head_sha)…" >&2
git checkout --quiet --detach "$head_sha"
run_baseline pr

{
  echo "### Read-path A/B (same-runner, warn-only)"
  echo
  echo "Base \`${BASE_SHA:0:12}\` vs PR \`${head_sha:0:12}\`. Wall-clock on a"
  echo "shared GH runner — treat <10% moves as noise."
  echo
  base_n="$(critcmp --list 2>/dev/null | grep -c '^base' || true)"
  pr_n="$(critcmp --list 2>/dev/null | grep -c '^pr' || true)"
  common="$(critcmp base pr 2>/dev/null | tail -n +2 | grep -c . || true)"
  if [ "$common" -eq 0 ]; then
    echo "> ⚠️ No comparable benchmarks (benchmark IDs differ between base and PR"
    echo "> — a harness/bench rename?). Nothing to compare."
  else
    echo '```'
    critcmp base pr
    echo '```'
    echo
    echo "_base benches: ${base_n}, pr benches: ${pr_n}, compared: ${common}._"
  fi
} > "$OUT"

echo "Wrote $OUT" >&2
```

- [ ] **Step 2: Make it executable and shellcheck it**

Run: `chmod +x scripts/perf-ab.sh && shellcheck scripts/perf-ab.sh`
Expected: no findings.

- [ ] **Step 3: Dry-run locally against the merge-base**

Run: `scripts/perf-ab.sh "$(git rev-parse HEAD~1)" /tmp/perf-ab.md && cat /tmp/perf-ab.md`
Expected: completes (two bench runs — minutes), `/tmp/perf-ab.md` holds a critcmp table. Afterward run `git checkout perf-regression` (the script leaves a detached HEAD).

- [ ] **Step 4: Commit**

```bash
git add scripts/perf-ab.sh
git commit -m "ci: add same-runner read A/B harness script (#211)"
```

### Task 2.3: the `perf-ab` workflow job

**Files:**
- Modify: `.github/workflows/ci.yml`

- [ ] **Step 1: Add the job (after an existing job; copy the libfuse3 install step verbatim from the `check` job)**

Resolve the pins first:
- `marocchino/sticky-pull-request-comment` — `gh api repos/marocchino/sticky-pull-request-comment/commits/v2 --jq .sha` (pin to the **commit** SHA, not the tag).
- `critcmp` version — pick the current `cargo search critcmp` version; pin it.

```yaml
  perf-ab:
    name: Read A/B (warn-only)
    needs: changes
    if: needs.changes.outputs.perf == 'true'
    runs-on: ubuntu-latest
    permissions:
      contents: read
      pull-requests: write
    steps:
      - uses: actions/checkout@df4cb1c069e1874edd31b4311f1884172cec0e10
        with:
          persist-credentials: false
          fetch-depth: 0 # base merge-base must be present for the A/B checkout
      - name: Install libfuse3
        run: sudo apt-get update && sudo apt-get install -y libfuse3-dev
      - name: Install critcmp
        run: cargo install critcmp --locked --version <PINNED>
      - name: Run A/B
        run: scripts/perf-ab.sh "${{ github.event.pull_request.base.sha }}" "$RUNNER_TEMP/perf-ab.md"
      - name: Comment (same-repo PRs)
        if: github.event.pull_request.head.repo.fork == false
        uses: marocchino/sticky-pull-request-comment@<PINNED_COMMIT_SHA>
        with:
          header: perf-ab
          path: ${{ runner.temp }}/perf-ab.md
      - name: Job summary (fallback / forks)
        if: always()
        run: cat "$RUNNER_TEMP/perf-ab.md" >> "$GITHUB_STEP_SUMMARY"
```

- [ ] **Step 2: Confirm `perf-ab` is NOT added to any required-checks aggregator**

Run: `grep -nE "ci-ok|needs:.*perf-ab|aggregat" .github/workflows/ci.yml`
Expected: `perf-ab` appears only as its own job — never in the `needs:` list of the `ci-ok`/aggregator job (it must never block merge). If an aggregator lists every job, leave `perf-ab` out of it.

- [ ] **Step 3: Lint**

Run: `yamllint .github/workflows/ci.yml`
Expected: clean.

- [ ] **Step 4: Commit and push the PR; verify behavior on the PR itself**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: warn-only same-runner read A/B job (#211)"
```

Because this PR touches `.github/workflows/` (not `musefs-*/src/`), `perf=false`,
so `perf-ab` will be **skipped** on its own PR — expected. Validate it by either
(a) a follow-up trivial `musefs-core/src/` no-op PR, or (b) temporarily widening
the `perf` filter on a scratch branch. Confirm the comment posts and the job is
green/non-required.

### Task 2.4: document the A/B job

**Files:**
- Modify: `CONTRIBUTING.md`

- [ ] **Step 1: Extend the gating paragraph from Task 1.5**

Append to the "Performance regression gating" section:

```markdown
The `perf-ab` job runs only when `musefs-core/src/**` or `musefs-format/src/**`
change. It benches the base and PR commits back-to-back on one runner and posts a
`critcmp` delta as a sticky PR comment. It is **warn-only** and not a required
check — GH runner noise makes wall-clock unfit for hard gating. Reproduce locally
with `scripts/perf-ab.sh <base-sha> out.md`.
```

- [ ] **Step 2: Commit**

```bash
git add CONTRIBUTING.md
git commit -m "docs: document the perf-ab warn-only job (#211)"
```

---

# PR 3 — Lane 3: release full-bench record

### Task 3.1: the release bench script

**Files:**
- Create: `scripts/perf-release-bench.sh`

- [ ] **Step 1: Write the script**

```bash
#!/usr/bin/env bash
# Run the full read bench plus the ci-tier ingest/refresh benches and capture
# all output to a single artifact file. Record-only; never gates a release.
#
# Usage: scripts/perf-release-bench.sh <out-file>
set -euo pipefail
OUT="${1:?output file required}"

{
  echo "# musefs release benchmark snapshot"
  echo "commit: $(git rev-parse HEAD)"
  echo

  echo "## read_throughput (criterion)"
  cargo bench -p musefs-core --bench read_throughput -- 2>&1 || true

  echo "## bench_ingest (ci tier)"
  MUSEFS_BENCH_TIER=ci cargo test --release -p musefs-core --features metrics \
    --test bench_ingest -- --ignored --nocapture 2>&1 || true

  echo "## bench_refresh (ci tier)"
  MUSEFS_BENCH_TIER=ci cargo test --release -p musefs-core \
    --test bench_refresh -- --ignored --nocapture 2>&1 || true
} | tee "$OUT"
```

- [ ] **Step 2: Make executable and shellcheck**

Run: `chmod +x scripts/perf-release-bench.sh && shellcheck scripts/perf-release-bench.sh`
Expected: no findings.

- [ ] **Step 3: Smoke-run only the ingest leg locally (full read bench is slow)**

Run: `MUSEFS_BENCH_TIER=ci cargo test --release -p musefs-core --features metrics --test bench_ingest -- --ignored --nocapture 2>&1 | tail -20`
Expected: the ci-tier ingest bench runs and prints timings (confirms the command is valid).

- [ ] **Step 4: Commit**

```bash
git add scripts/perf-release-bench.sh
git commit -m "ci: add release benchmark-record script (#211)"
```

### Task 3.2: the `benchmarks` release job

**Files:**
- Modify: `.github/workflows/release.yml`

- [ ] **Step 1: Add the job (copy the checkout + libfuse3 install steps from an existing release job; pin `actions/upload-artifact` to the SHA already used elsewhere in the repo)**

Run first: `grep -rn "upload-artifact@" .github/workflows/ | head -1` to reuse the existing pinned SHA.

```yaml
  benchmarks:
    name: Benchmark snapshot
    runs-on: ubuntu-latest
    continue-on-error: true # record-only; never blocks a release
    permissions:
      contents: read
    steps:
      - uses: actions/checkout@df4cb1c069e1874edd31b4311f1884172cec0e10
        with:
          persist-credentials: false
      - name: Install libfuse3
        run: sudo apt-get update && sudo apt-get install -y libfuse3-dev
      - name: Run benchmarks
        run: scripts/perf-release-bench.sh "$RUNNER_TEMP/bench-snapshot.txt"
      - name: Upload snapshot
        uses: actions/upload-artifact@<PINNED_SHA>
        with:
          name: benchmark-snapshot-${{ github.ref_name }}
          path: ${{ runner.temp }}/bench-snapshot.txt
```

- [ ] **Step 2: Confirm it is not wired into any release gate**

Run: `grep -nE "needs:.*benchmarks" .github/workflows/release.yml`
Expected: no match (no other job depends on `benchmarks`; it cannot block `publish`/`build`/`release-assets`).

- [ ] **Step 3: Lint**

Run: `yamllint .github/workflows/release.yml`
Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/release.yml
git commit -m "ci: record a benchmark snapshot artifact on release (#211)"
```

### Task 3.3: close the loop in docs

**Files:**
- Modify: `BENCHMARKS.md`

- [ ] **Step 1: Note the artifact source in the "CI regression gating" subsection**

Append: "The release artifact is named `benchmark-snapshot-<tag>`; download it from the tag's workflow run and fold the numbers into the per-pass tables here."

- [ ] **Step 2: Commit**

```bash
git add BENCHMARKS.md
git commit -m "docs: point BENCHMARKS.md at the release snapshot artifact (#211)"
```

---

## Self-review notes (resolved during authoring)

- **Spec coverage:** Lane 1 read goldens → 1.1/1.2; ingest slurp guard → 1.3; refresh O(changed) via existing `apply_changes` return (no new counter) → 1.4; fsync==0 explicitly release-only → 3.1 (ci-tier `bench_ingest`) + noted in docs. Lane 2 path filter/job/script/fork-fallback/pin → 2.1–2.4. Lane 3 record-only artifact + ci tier → 3.1–3.3.
- **No production code in PR1** — every asserted counter already exists; the only `tree.rs` change is an appended test (mutants anchors re-checked in 1.4).
- **Characterization steps** (1.2, 1.3) discover format-specific exact constants via a shown command + a shown edit, not a vague "TODO" — the load-bearing invariants (`pread_bytes == AUDIO_BYTES`, bounded-seek/slurp upper bounds) are asserted independently of the frozen numbers.
- **Pins** (`critcmp` version, sticky-comment commit SHA, `upload-artifact` SHA) are resolved by the explicit commands in 2.3/3.2, honoring the commit-not-tag pin rule.
