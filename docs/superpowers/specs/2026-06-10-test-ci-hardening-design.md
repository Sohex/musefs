# Test/CI hardening for robustness & contract invariants

Date: 2026-06-10
Issues: #204, #208, #209
Status: design approved, pending spec review

## Problem

Three robustness properties are *designed but unverified in CI*. Each is a place
where the suite can stay green while the property it claims to guarantee is
broken:

- **#204 — external-writer contract.** Python writers (beets, Picard, Lidarr,
  and the shared `python-musefs`) are tested against Python fixtures. CI never
  proves that rows a Python writer produces are actually accepted *and correctly
  interpreted* by the current Rust binary/core path. The plugin `musefs_bin`
  tiers that would prove this are opt-in and default-skipped
  (`addopts = "-m 'not musefs_bin and not e2e'"`), and CI runs the default
  invocation.
- **#208 — concurrency.** Correctness under parallel reads rests on
  `DbPool::PerThread` and the `quick_cache` header cache, but no test reads the
  same file from multiple threads, opens many files in parallel, or sustains
  load; and no sanitizer runs anywhere in `.github/workflows/`. `unsafe_code =
  "deny"` gives no protection at the libfuse / libsqlite3 FFI boundary where a
  race would actually occur.
- **#209 — failure paths.** The injection harness (`musefs-latencyfs`,
  `metrics::set_fault_pread`) injects *latency only*. The reader and DB error
  variants (`CoreError::BackingChanged`, format and DB errors) are reached by
  malformed-input fuzzing and happy-path tests, never by a simulated runtime
  fault: a backing file truncated or vanishing mid-read, a short / `EIO` pread,
  a locked or corrupt SQLite database, `ENOSPC`.

## Goals

1. A mandatory CI path where Python writes are consumed by Rust — proving the
   DB schema contract end to end, catching both "Rust rejects the row" and
   "Rust misinterprets the row."
2. Deterministic concurrent-reader coverage plus a sanitizer pass, so race and
   lock-ordering defects in the serve path have a chance to fail CI.
3. A fault-injection facility that drives the reader and DB error paths under
   real runtime failures, asserting errno mapping, partial-response behaviour,
   and post-failure cache state.

## Non-goals

- Loom: it only models code using loom's own atomics and cannot reach the
  libfuse / libsqlite3 FFI boundary where the races of concern live.
- Making the full FUSE-mount e2e a required check. It stays optional; the
  Python→Rust DB contract is what becomes mandatory.
- Fuzzing changes (separate hardening track).
- Any production-code behaviour change beyond adding the `metrics`-gated fault
  seam. The seam is compiled out of default and release builds.

## Shared infrastructure

Two pieces are common to the workstreams:

- **A built `musefs` binary in CI.** Workstream A needs it; it is produced once
  in the new job and its path exported to the Python tiers.
- **Branch-protection wiring.** `main` requires the `ci-ok` aggregator
  (`main-branch-protection`). Every new *required* job is added to `ci-ok`'s
  `needs:` list and gated on `needs.changes.outputs.src == 'true'` so docs-only
  PRs still report the check (a job-level skip, never a workflow-level
  `paths-ignore`, which would deadlock the required check). New *non-required*
  jobs (the TSan pass) are deliberately left out of `ci-ok`.

`/dev/fuse` is reliably available on the CI runners, so tests live wherever they
read most naturally — mount-level in `musefs-fuse` is acceptable and not avoided
on principle.

---

## Workstream A — #204: non-skipping Python→Rust contract tier

### Mechanism

A new CI job, `contract`, that:

1. Builds the `musefs` binary once (release or debug; debug is fine and faster)
   and exports its path via an environment variable (e.g. `MUSEFS_BIN`).
2. Runs each plugin's `musefs_bin` tier (`-m musefs_bin`) for `python-musefs`,
   beets, Picard, and Lidarr — **with skip-on-missing-binary turned into a hard
   failure.** Today a missing binary silently skips; under this job a missing or
   unset `MUSEFS_BIN` must fail. The binary-discovery logic is **not currently
   uniform** — each plugin has its own `conftest.py` / discovery fixture — so the
   plan must wire a single shared signal (e.g. `MUSEFS_REQUIRE_BIN=1`, set by the
   `contract` job) into each plugin's discovery fixture, converting `pytest.skip`
   → hard failure consistently across all four. The default opt-out
   (`-m 'not musefs_bin and not e2e'`) is untouched, so local default runs still
   skip.
3. Runs a **synthesis round-trip test** (the part that catches "misinterprets").
   The external-writer contract (`ARCHITECTURE.md` "The external-writer
   contract") splits ownership: the **scanner** owns the `tracks` geometry
   columns (`backing_path`, `format`, `audio_offset`, `audio_length`,
   `backing_size`, `backing_mtime`, …) and `structural_blocks`; **external
   writers** (python-musefs) own only `tags`, `art`, `track_art`. The round trip
   must respect that split:
   - a small real backing audio file is placed on disk;
   - **`musefs scan`** (the binary this job already builds) is run over it,
     creating the `tracks` row + geometry + `structural_blocks` — python-musefs
     cannot and must not write these (its store API is tags/art only:
     `replace_tags` / `replace_track_art`, no geometry write);
   - `python-musefs` then writes tags + art for that scanned track (the store
     API exercised by `test_store_db` / `test_store_art`);
   - a **new** Rust harness opens that externally-written DB read-only via
     `Db::open_readonly` (`musefs-db/src/lib.rs`) and synthesizes the served
     bytes via `reader::read_at` (no FUSE mount). It reuses `interop_emit`'s
     read-back / mutagen assertion helpers but **not** its DB construction
     (`interop_emit` builds its own DB in-process and cannot be pointed at an
     external one). Form: a `--ignored` cargo test in `musefs-core` pointed at a
     `MUSEFS_DB` env path, run by the `contract` job after the scan + Python
     write;
   - the synthesized bytes are asserted to parse and the tags/art are read back
     and compared to what Python wrote, using the same independent-reader
     (mutagen) assertions the existing `interop` job already relies on.

The round trip is written once at the `python-musefs` layer (the common write
path all plugins funnel through); the per-plugin `musefs_bin` tiers cover the
plugin-specific glue.

### Why this shape

Schema/constraint validation alone would catch only "Rust rejects the row." The
issue's stated risk is rows Rust "rejects **or misinterprets**." Synthesizing
the file and reading the tags back is what proves the bytes are actually
serveable and the tags survive the Python→DB→Rust→file→reader trip intact.

### CI

`contract` is a required job: added to `ci-ok`'s `needs:`, gated on
`needs.changes.outputs.src == 'true'`. Full FUSE-mount e2e (`e2e` job) is
unchanged and stays optional.

---

## Workstream B — #208: concurrency coverage + sanitizers

### Stress tests (required)

Deterministic, ordinary `cargo test` cases that exercise the serve path under
parallelism (placed wherever they read most naturally — mount-level in
`musefs-fuse` or reader-level in `musefs-core`):

- the **same file** read concurrently from N threads;
- **many files** opened and read in parallel;
- a **sustained-load** loop (bounded iterations/time) hammering reads.

These exercise `DbPool::PerThread` and the `quick_cache` header cache under real
contention. They must be deterministic (bounded, no sleeps-as-synchronization)
so they can be a required gate.

### ASan (required)

A CI job that runs the concurrent tests under AddressSanitizer
(`-Zsanitizer=address`, nightly toolchain). Required: added to `ci-ok`.
AddressSanitizer is tractable here — it catches memory errors at the FFI
boundary and within workspace code.

### TSan (non-required, best-effort)

A separate CI job that runs the same tests under ThreadSanitizer
(`-Zsanitizer=thread`). **Not** added to `ci-ok`; allowed to be noisy. It is
documented as best-effort because it cannot instrument the system C libraries
(libfuse, libsqlite3), so it sees races in *our* code around the FFI but may
miss or false-positive inside the C deps. Kept as a signal, not a gate.

### Notes

- Sanitizer jobs need a nightly toolchain and may need `-Zbuild-std` for std
  instrumentation; the plan will pin the approach. They are gated on
  `needs.changes.outputs.src == 'true'` like the rest.

---

## Workstream C — #209: fault-injection for failure paths

### Backing-read fault seam

A test-only fault-injection seam at the backing-read boundary, **gated behind
the existing `metrics` feature** (not `cfg(test)`), so:

- it is compiled out of default and release builds;
- it is visible across crate boundaries (a mount-level test in `musefs-fuse` can
  drive it, which a `cfg(test)` seam local to `musefs-core` could not);
- it reuses the feature that already has a dedicated CI step (`Core
  metrics-feature tests`) and the single-test-binary pattern used for the
  global `set_fault_pread` `OnceLock`.

The seam sits at the **positioned backing-read call in the per-handle read
path** — the same call site that `metrics::on_pread` already instruments
(`musefs-core/src/reader.rs`, around the positioned read feeding `on_pread`).
That is the choke point every backing read passes through, so it can intercept
the read result before the reader's size/mtime re-validation runs.

The seam is **distinct from** `set_fault_pread`: it is a properly-scoped,
per-test configurable injector (not the latency-only global hook, which stays
latency-only). It can be configured to produce, on a backing read:

- a **short read** (fewer bytes than requested);
- an **`EIO`** (or other errno) error;
- a **mid-read truncation** that trips the size/mtime re-validation →
  `CoreError::BackingChanged`.

Tests assert: the resulting error variant, its mapping to errno at the FUSE
layer, the partial-response behaviour, and the header-cache state after the
failure (no poisoned/corrupt cache entry).

### DB faults via real conditions

The read-path DB failure that is reachable in production is driven with a real
condition rather than a mock:

- a **byte-corrupted** DB file → the read path surfaces a mapped `DbError`
  (`CoreError::Db`), not a panic or a wrong-but-`Ok` result.

`SQLITE_BUSY` / exclusive-lock and `ENOSPC` / read-only are **not** test targets
on the serve path, and this is by design, not a gap:

- The serve path opens **read-only WAL** connections, and WAL readers are
  **contention-free** — that is the entire point of `DbPool::PerThread`. A
  concurrent writer never makes a read return `SQLITE_BUSY`; only a pathological
  `PRAGMA locking_mode=EXCLUSIVE` (which musefs never sets) could, so a lock test
  would assert the behaviour of a configuration the product doesn't use. Lock
  *contention correctness* (concurrent readers don't corrupt/deadlock) is proven
  by the Workstream B concurrency tests instead.
- `ENOSPC` / read-only are **write-path** conditions (scan, WAL checkpoint); the
  read-only serve path cannot hit them. They remain the documented best-effort
  case (see Open feasibility risks) and are out of scope for the read-time suite.

---

## Testing & verification

- Workstream A: the `contract` job is the test. Locally runnable by building the
  binary, setting `MUSEFS_BIN` + `MUSEFS_REQUIRE_BIN=1`, and running the plugin
  `-m musefs_bin` tiers plus the round-trip test.
- Workstream B: stress tests run in the normal suite; ASan/TSan jobs run them
  under instrumentation. Stress tests must be deterministic to be a gate.
- Workstream C: fault tests run under the `metrics` feature
  (`cargo test -p musefs-core --features metrics` and the relevant
  `musefs-fuse` tests with the feature enabled).

## Documentation

- `CONTRIBUTING.md`: document the new contract tier, the sanitizer jobs (and the
  TSan best-effort caveat), and the fault-injection seam under the test-tier /
  conventions sections.
- The pre-commit hook runs the full workspace test suite; the new
  `metrics`-gated fault tests and stress tests must be green in a single commit
  (no red-test commits — `musefs-prepush-checks`).
- Any in-tree harness (e.g. the round-trip script) is committed as a runnable
  script that CI invokes, per the repo convention for test harnesses.

## Open feasibility risks the plan must resolve

These are deferred to the plan, but flagged here so they are decided
deliberately rather than discovered mid-implementation:

- **Sanitizers over C deps (B).** ASan/TSan need a nightly toolchain. Whether to
  use `-Zbuild-std` for instrumented std, and whether `libsqlite3-sys` /
  `fuser`'s C is built with usable symbolization, must be pinned. The fallback if
  `-Zbuild-std` proves too slow/flaky is ASan without it (still catches workspace
  + FFI-boundary memory errors). ASan is the required gate, so its config must be
  reliable; TSan can stay noisy.
- **DB read-path faults (C) — resolved.** Only **byte-corruption** is a
  reachable, mandatory read-path DB fault and is covered. `SQLITE_BUSY`/lock is
  unreachable on the read-only WAL serve path by design (see "DB faults via real
  conditions"), and `ENOSPC`/read-only are write-path conditions — both are out
  of scope for the read-time suite, not best-effort TODOs.
- **Stress-test determinism (B).** The concurrent tests are a *required* gate, so
  they must be deterministic: bounded iteration counts, barrier/latch
  synchronization rather than sleeps, and assertions on *outcomes* (correct bytes,
  no error, no deadlock within a timeout) rather than on timing. A test that is
  merely "usually" green cannot gate `ci-ok`.

## Sequencing

The three workstreams are independent and can land as separate commits/slices:

1. **C** (fault seam) — self-contained, no CI-runner dependencies.
2. **B** (stress tests, then ASan job, then TSan job).
3. **A** (contract tier) — touches the most CI plumbing (binary build, conftest
   require-binary signal, round-trip harness).

Each new required job must be added to the `ci-ok` aggregator in the same change
that introduces it, or branch protection will not see it.
