# jemalloc global allocator for the `musefs` binary

**Issue:** [#360](https://github.com/Sohex/musefs/issues/360) (part A only)
**Date:** 2026-06-14
**Status:** Approved design — ready for implementation plan

## Problem

The release binary sets no `#[global_allocator]`, so it uses the system
allocator (glibc malloc on the main release targets). The hot path does heavy
concurrent `Arc` cloning, `DashMap` sharding, and `sharded_slab` allocate/free
churn, with periodic tree-refresh bursts — many small, uniformly-sized,
short-to-medium-lived allocations, frequently allocated on one worker thread and
freed on another. This is the canonical pattern under which glibc malloc
accumulates dirty pages in per-thread arenas and never returns them to the OS,
so a long-lived `musefs` daemon's RSS can creep upward over days or weeks
without a true leak.

Issue #360 also raises a runtime telemetry surface; that is a separate,
independent subsystem and is **explicitly out of scope** for this spec. It will
get its own spec/plan cycle.

## Goal

Replace the system allocator with jemalloc in the `musefs` binary, tuned to
return idle memory to the OS, so steady-state RSS under sustained churn stays
bounded. Provide a build-time opt-out for downstream packagers and
memory-debugging, and a committed harness that measures the RSS effect.

## Non-goals

- Any runtime telemetry / observability surface (issue #360 part B).
- Changing the allocator used by the library crates, benches, tests, or the
  sanitizer CI jobs. The override lives in the binary crate only.
- Per-call or per-arena allocator tuning beyond enabling background purging.

## Why jemalloc

The workload (small uniform objects, cross-thread alloc/free, periodic bursts)
is exactly what jemalloc's per-size-class arenas absorb without fragmenting.
Decay-based purging plus a background purge thread directly target the named
RSS-creep failure mode by returning dirty pages to the OS during quiet periods.
jemalloc is native to the project's most fragile non-Linux CI legs (FreeBSD test
job; macOS clippy/test job) and to musl release builds, where it also sidesteps
musl's weak built-in allocator. FUSE rules out Windows, so jemalloc's lack of
MSVC support is irrelevant here. `tikv-jemalloc-ctl`'s stats API is additionally
a head start on the parked telemetry work, though that is not built here.

mimalloc (simpler, but coarser RSS-return control and no stats binding) and
snmalloc (strong cross-thread-free story, but the least battle-tested binding
and finickier on our FreeBSD/musl cross-targets) were considered and rejected.

## Design

### 1. Crate wiring (`musefs/Cargo.toml`)

Add a **default-on** `jemalloc` feature gating two optional dependencies:

```toml
[features]
default = ["jemalloc"]
jemalloc = ["dep:tikv-jemallocator", "dep:tikv-jemalloc-ctl"]

[dependencies]
tikv-jemallocator = { version = "0.7", optional = true }
tikv-jemalloc-ctl = { version = "0.7", optional = true }
```

(`background_threads_runtime_support` is on by default via `tikv-jemalloc-sys`,
so enabling the background thread at runtime needs no extra cargo feature; the
plan pins exact versions and updates `Cargo.lock`.) The opt-out for packagers,
memory-debuggers, and
A/B benchmarking is `cargo build -p musefs --no-default-features`. A cargo
feature is a build-time toggle, so end users running a prebuilt release binary
are unaffected — only source builds can flip it, which is precisely who the
opt-out serves.

### 2. Allocator + background thread (`musefs/src/main.rs`)

Set the global allocator under the feature. No `unsafe` is required, so this
does not trip the workspace `unsafe_code = "deny"` lint:

```rust
#[cfg(feature = "jemalloc")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;
```

After `env_logger` is initialised in `main`, call a small
`#[cfg(feature = "jemalloc")]` helper that enables jemalloc's background purge
thread via the safe ctl API:

```rust
#[cfg(feature = "jemalloc")]
fn enable_jemalloc_background_thread() {
    if let Err(e) = tikv_jemalloc_ctl::background_thread::write(true) {
        log::debug!("jemalloc background_thread unavailable: {e}");
    }
}
```

- **Best-effort:** on platforms where jemalloc does not support background
  threads (notably macOS), `write(true)` returns `Err`; we log and continue.
  jemalloc stays active and still purges on allocation activity; only the
  timed-idle purge is absent there. The long-lived daemon target is Linux, where
  it is supported.
- **Unconditional at startup:** enabled before `run()` so it covers `mount`; it
  is harmless for the short-lived `scan` path (one cheap jemalloc-managed
  thread).
- **Default decay otherwise:** jemalloc's default dirty/muzzy decay is sane; the
  background thread is the missing piece that makes an *idle* daemon actually
  return pages. No further `MALLOC_CONF` tuning. (Setting `malloc_conf` via an
  exported symbol would require `#[unsafe(no_mangle)]` and is deliberately
  avoided in favour of the runtime ctl call.)

The allocator setup stays in the binary crate: setting a global allocator is
binary-specific, so this does not violate the "keep the binary thin,
cross-cutting logic in `musefs-core`" layering rule.

### 3. Platform / CI behavior

- **Sanitizers — unaffected, no gating needed.** The ASan job builds
  `cargo +nightly test -p musefs-core --test concurrent_reads`; the TSan job
  builds `-p musefs-core` and `-p musefs-fuse`. Neither builds the `musefs`
  binary, so the `#[global_allocator]` is never compiled into a sanitizer test
  binary. No `cfg(sanitize)` guard is required.
- **Native builds everywhere clippy/tests run.** jemalloc compiles natively on
  the Linux `check` job, the macOS job (`cargo clippy --all-targets` +
  `cargo test --workspace`), and the FreeBSD in-VM job. No cross-compilation of
  jemalloc C is performed for any clippy gate.
- **Release pipeline — the real build risk.** `release.yml` builds four targets
  with `cargo-zigbuild` (`x86_64`/`aarch64` × `gnu`/`musl`) plus Docker
  musl/glibc images; all must build `jemalloc-sys` under zig cross-compilation.
  Because the project's release workflow has a history of first-run breakage,
  this is a checklist item, not an assumption:
  - Smoke-build all four targets locally with `cargo zigbuild --release -p
    musefs --target <zig_target>` before relying on the release.
  - If any target cannot build `jemalloc-sys`, set `--no-default-features` for
    that matrix entry (and document it) rather than blocking the release. The
    Docker image variants consume the same triples, so a dropped target must
    propagate to its matching image — otherwise the tarball and container for one
    arch would ship different allocators.
- **Mutants — no re-anchoring.** `mutants.toml` anchors live in `musefs-core` /
  `musefs-format` source, which this change does not touch.

### 4. Verification (committed harness + `BENCHMARKS.md`)

Days-scale RSS creep is not CI-testable, so verification is a manual/local gate
backed by a committed, runnable harness:

- **Script:** `scripts/rss-churn-bench.sh` (runnable, committed in-tree; any
  large corpus stays gitignored). It:
  1. Builds both variants: `--no-default-features` (system malloc) and the
     default build (jemalloc).
  2. Mounts each variant under `$HOME` (AppArmor permits FUSE there) against the
     test music corpus, with `/data`-backed files as needed.
  3. Drives a **concurrent** churn loop — cross-thread alloc-here/free-there is
     the exact mechanism jemalloc must win on, so a single-threaded loop would
     show a false null. The script pins, as configurable parameters with stated
     defaults:
     - **Concurrency:** `WORKERS` reader threads (default = number of cores),
       each independently opening/reading/releasing across the file set, so
       allocations and frees land on different threads.
     - **File set:** `FILES` distinct tracks (default ≥ 500) read end-to-end so
       the read path, header cache, and handle table all churn.
     - **Refresh cadence:** trigger a tree refresh every `REFRESH_SECS` (default
       30 s) to exercise the `im`-tree alloc/free bursts.
     - **Duration:** `CYCLES` churn cycles (default ≥ 200) after a discarded
       `WARMUP` cycles (default 20).
  4. Samples `VmRSS` from `/proc/<pid>/status` once per cycle and reports
     **steady-state** RSS per variant: the median `VmRSS` over the last 25 % of
     cycles, after warmup, once the curve has flattened (max-vs-median over that
     window within a few percent). This is distinct from the *peak* RSS already
     reported elsewhere in `BENCHMARKS.md`.
- **Decision rule (the actual gate):** ship jemalloc only if its steady-state
  RSS is **≤** the system-malloc baseline. If the two are within run-to-run
  noise, record both and make an explicit ship/no-ship call in the spec/PR
  rather than assuming a win. A jemalloc result meaningfully *worse* than
  baseline blocks the change pending investigation.
- **Record:** add a new allocator/RSS section to `BENCHMARKS.md` with the
  methodology, the parameters used, and the measured numbers (system malloc vs
  jemalloc) plus the resulting decision.
- **CI's role** is only to confirm the binary builds/links and all tests pass
  under jemalloc — already covered by `cargo test --workspace` and
  `cargo test -p musefs -- --ignored` (the latter spawns the real
  default-feature binary, so it exercises jemalloc end-to-end). CI does not run
  the RSS bench. The cross-built release targets get their first jemalloc
  *runtime* exercise from the existing release smoke (`scripts/smoke-binary.sh`
  on host + the Alpine/musl container smoke in `release.yml`), so a jemalloc
  init failure under musl/aarch64 surfaces there, not in `cargo test`.

### 5. Documentation

- `BENCHMARKS.md`: the RSS/allocator section above.
- `README.md`: one line noting jemalloc is the default allocator and the
  `--no-default-features` opt-out for source builds / packaging.
- `CONTRIBUTING.md`: note the `jemalloc` feature in the build/feature matrix and
  the release smoke-build step.

## Risks

| Risk | Mitigation |
| ---- | ---------- |
| `jemalloc-sys` fails to build for a release target under zig cross | Smoke-build all four targets pre-release; fall back to `--no-default-features` per target |
| `background_thread` unsupported on a platform (macOS) | Best-effort enable; log and continue, jemalloc still active |
| Bigger binary / slightly slower tiny allocations | Accepted; the RSS-bounding win is the goal |
| RSS bench shows no improvement on this workload | The bench is the gate: if jemalloc does not help, revisit before shipping |

## Acceptance criteria

1. The default `musefs` build sets jemalloc as the global allocator and enables
   the background purge thread on Linux (best-effort elsewhere).
2. `cargo build -p musefs --no-default-features` produces a working
   system-malloc binary.
3. `cargo test --workspace` and `cargo test -p musefs -- --ignored` pass under
   the default (jemalloc) build; the ASan/TSan jobs are unchanged.
4. All four release targets build with `jemalloc-sys` under `cargo-zigbuild`
   (or are documented as `--no-default-features`).
5. `scripts/rss-churn-bench.sh` runs the concurrent churn workload and
   `BENCHMARKS.md` records the system malloc vs jemalloc **steady-state** RSS
   comparison (median over the flattened tail, not peak), together with the
   explicit ship decision per the §4 rule — jemalloc steady-state RSS must be
   ≤ baseline (or, if within noise, an explicit recorded call).
6. `README.md` and `CONTRIBUTING.md` document the allocator and the opt-out.
