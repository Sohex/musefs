# Test tiers

## Test tiers beyond `cargo test`

### Property tests

`proptest` invariants — panic-freedom, the byte-identical-audio guarantee,
tag round-trip stability — live in `musefs-format/tests/proptest_*.rs` and
`musefs-core/tests/proptest_read_fidelity.rs`. The format-layer suites are
gated on the `fuzzing` feature, which `musefs-format`'s self-dev-dependency
enables for all of its own test builds — so a plain
`cargo test -p musefs-format` runs them.

### Coverage-guided fuzzing

The `fuzz/` crate is **excluded from the workspace**: workspace-wide build,
test, and clippy do not compile it, so a format-layer signature change can
break fuzz targets without anything failing locally — CI's fuzz `smoke` job
(`cargo +nightly fuzz build`) is what catches it. Check locally before
pushing a format-layer API change:

```bash
cargo install cargo-fuzz                          # one-time; needs nightly
cargo +nightly fuzz build                         # what the CI smoke job runs
cargo +nightly fuzz run <target>                  # flac|mp3|mp4|ogg|wav|ogg_page|b64|vorbiscomment|serve
cargo +nightly fuzz coverage <target>             # confirm coverage reaches the parser
cargo run --manifest-path fuzz/Cargo.toml --bin generate_seeds   # (re)build seeds
```

### Fuzz crash regressions

When you fix a fuzz-found crash:

1. Drop the reproducer bytes into `fuzz/regressions/<target>/` (one file per
   reproducer). The per-PR fuzz `smoke` job's replay step runs every committed
   reproducer with `cargo +nightly fuzz run <target> <files> -- -runs=0` — a
   deterministic single pass that fails the build if any known input panics
   again. This is separate from `fuzz/corpus/`, which `cargo fuzz cmin`
   minimizes (and would prune reproducers from).
2. Where the crash exposed a real logic/behavior defect, also add a focused
   behavioral test for that logic in the owning crate's suite (the pre-commit
   hook gates it). The byte replay proves the exact input no longer panics; the
   behavioral test documents and locks in the fix. They are not interchangeable.

Coverage notes: the per-format targets also drive the bounded/ceiling probers
(`*_bounded`, `locate_audio_at_ceiling`, `read_structure_from`) and assert a
differential oracle against the full-buffer parse. The `serve` target fuzzes the
read-time serve path (`read_at_with_file` over adversarial layouts, including
`serve_ogg_window`/`OggArtSlice`) and is scheduled-only (built per-PR, not
smoke-run) because it builds a DB + temp backing file per input. The `serve`
target also exercises hostile DB rows (negative/oversized geometry,
invalid formats, orphaned/oversized art, stale binary-tag handles, content-version
mismatch) via the `musefs-db` `fuzzing`-gated `with_raw_conn`, plus binary-tag
streaming and distinct Opus/Vorbis/OggFLAC fixtures.

### Independent-reader interop (mutagen)

Asserts that an independent ecosystem reader sees the tags musefs
synthesizes, across all five formats:

```bash
pip install -r tests/interop/requirements.txt
MUSEFS_INTEROP_DIR=/tmp/i cargo test -p musefs-core --test interop_emit -- --ignored emit_interop_fixtures
MUSEFS_INTEROP_DIR=/tmp/i python -m pytest tests/interop
```

### External-writer contract round trip

CI's `contract` job mandatorily proves the Python -> Rust DB contract: it builds
the binary, runs each binary-only plugin's `musefs_bin` tier with
`MUSEFS_REQUIRE_BIN=1` (a missing binary fails instead of skipping), and runs the
round-trip harness. The harness is the single source of truth, run locally with:

```bash
pip install -r tests/contract/requirements.txt pytest && pip install -e contrib/python-musefs
bash scripts/contract-roundtrip.sh
```

It scans real ffmpeg-generated audio (so `musefs scan` owns the track geometry),
writes tags/art through `musefs_common.store`, synthesizes the served bytes via
`cargo test --test contract_emit`, and asserts with mutagen that the Python tags
and art survived. Picard's `musefs_bin` tier runs in the `picard` job (it needs
the system-Picard environment).

### Failure-path fault injection

The reader and DB error paths are exercised under simulated runtime faults.
`musefs_core::metrics::set_backing_fault(BackingFault::{Eio,ShortRead})`
(behind the `metrics` feature) installs a process-global fault at the positioned
backing-read site, cleared by the returned RAII guard. Because it is global, the
tests run in their own `metrics`-gated binaries.

```bash
cargo test -p musefs-core --features metrics --test reader_faults
cargo test -p musefs-core --test backing_changed_fault   # real file mutation
cargo test -p musefs-core --test db_corruption_fault      # byte-corrupt DB
cargo test -p musefs-fuse --features metrics -- --ignored # EIO through the mount (needs /dev/fuse)
```

`BackingChanged` (re-validated in `HeaderCache::resolve`) and DB corruption are
driven by real conditions, not the seam. `ENOSPC`/read-only faults are write-path
concerns and are out of scope for the read-time suite.

### Mutation testing

`scripts/mutants.sh` wraps `cargo-mutants` for the logic-bearing crates;
`.cargo/mutants.toml` permanently excludes the thin glue crates
(`musefs-fuse`, `musefs-cli`, `musefs`) and feature-gated instrumentation.
`musefs-latencyfs` carries real logic and has its own leg (it needs
`/dev/fuse` to kill its mutants).

The CI parity check for a branch is the **in-diff gate** — mutate only the
lines your branch changed:

```bash
git diff "$(git merge-base main HEAD)...HEAD" -- '*.rs' > mutants.diff
grep -q '^@@ ' mutants.diff   # IMPORTANT: an empty diff mutates nothing and exits 0 — a silent false pass
cargo mutants --in-diff mutants.diff -j2 --exclude 'musefs-latencyfs/**' --output /tmp/mutants-out/in-diff
```

Sharp edges:

- **Check the exit status directly.** Don't pipe the run through
  `tail`/`grep` — that masks the exit code.
- **Scratch space and memory.** cargo-mutants copies the source tree into a
  scratch dir under `TMPDIR`/`MUTANTS_TMP` (which must be *outside* the
  repo). For a small in-diff mutant set, the default tmpfs `/tmp` is fine —
  and faster. For *large* sets (a full-crate campaign), some mutants are
  allocation bombs (e.g. a constant-return on a parser position helper spins
  a collect-loop) that can OOM the host before the test timeout fires: put
  `TMPDIR` on real disk and run inside a memory-capped cgroup, e.g.

  ```bash
  mkdir -p ~/.cache/musefs-mutants-tmp
  TMPDIR="$HOME/.cache/musefs-mutants-tmp" systemd-run --user --scope --collect \
      -p MemoryMax=10G -p MemorySwapMax=0 \
      cargo mutants --in-diff mutants.diff -j2 --exclude 'musefs-latencyfs/**' --output /tmp/mutants-out/in-diff
  ```

  `scripts/mutants.sh` also supports sharding (`MUTANTS_SHARD=i/n`, used by
  CI to split the long `musefs-format` leg), though a sharded local
  workflow hasn't been built out.
- Known-unkillable mutant classes get a *documented* `exclude_re` in
  `.cargo/mutants.toml`, not test contortions. Note that cargo-mutants
  mutates `const` initializer expressions too — a constant is not a hiding
  place for arithmetic the gate flags.
- **`exclude_re` entries are guarded against drift.** A few exclusions must
  pin a specific `file:line:col:` (the operator+function alone isn't unique in
  the function); those coordinates rot silently when `cargo fmt` shifts the
  code, and a stale anchor can re-point onto a *killable* mutant — a silent
  false pass. `scripts/check_mutant_anchors.py` prevents that: it lists the
  full unfiltered mutant set (`cargo mutants --no-config --list --json`) and
  re-validates every `exclude_re` entry. It runs in the per-PR `in-diff` job
  (`.github/workflows/mutants.yml`) and its unit tests run in CI's
  `python-musefs` job. Run it locally with:

  ```bash
  cargo mutants --no-config --list --json > /tmp/mutants-list.json
  python3 scripts/check_mutant_anchors.py --mutants-json /tmp/mutants-list.json
  ```

  Each entry carries a machine-checked `# guard:` comment on the line directly
  above it:
  - **`file:line:col` anchors** — `# guard: op="<" fn="probe_file" rows=3`. The
    guard asserts the matched mutants all share that operator and function,
    occupy one site, and number exactly `rows` (use `fn=""` for a const-level
    site with no enclosing function). A *narrowing* entry (one that embeds a
    replacement to leave same-site siblings killable) sets `rows` to that
    subset's size.
  - **description anchors** — `# guard: count=N` (default 1) asserts the entry
    matches mutants spanning exactly `N` distinct sites; this is what catches a
    newly-added killable sibling silently joining the match set. A bare
    single-site description entry needs no tag.

  When the guard fails: a `found none` message means a line:col anchor drifted
  — re-anchor it to the current coordinates from the listing **and re-confirm
  the mutant there is still genuinely equivalent** (a reformat can change
  surrounding logic, not just line numbers). A `count`/`rows` mismatch means a
  sibling appeared or disappeared — investigate before bumping the number.
  Pure `cargo fmt`/line-shift drift can often be repaired automatically with
  `python3 scripts/check_mutant_anchors.py --fix`, which re-points an anchor to
  its current coordinates by operator+function. It only does so when the mapping
  is unambiguous — every same-operator site in the function is anchored, so the
  positional match is exact. An anchor that pins one of several same-operator
  sites (the usual reason it is a `file:line:col` anchor rather than a
  description) cannot be derived from the tag alone, so `--fix` leaves it for
  manual re-anchoring and reports `can't auto-derive the coordinate`; it also
  declines when a site was added or removed. Always eyeball the resulting diff
  before committing.
  Every new `file:line:col` exclusion needs a `# guard:` tag (the guard rejects
  an untagged one), and `exclude_re` patterns must stay within the
  Rust-regex/Python-`re` shared subset the guard allows (`\. \d + | ^ ( ) *`,
  no inline `(?...)` groups).

### Performance regression gating

`cargo test -p musefs-core --features metrics` includes
`tests/perf_counters.rs`: golden assertions on deterministic work counters
(`preads`, `pread_bytes`, `scan_bytes_read`, art/binary-tag chunks) for the
read/serve and ingest paths, plus a `tree.rs` unit test pinning the refresh
rebuild count as size-invariant. These are a hard gate — a legitimate change to
read/ingest/refresh work must update the golden numbers in the same PR. They run
on every non-doc PR via CI's `check` job. Constant-factor (wall-clock) changes
are surfaced separately by the warn-only `perf-ab` job (below).

The A/B benchmark runs only when `musefs-core/src/**` or `musefs-format/src/**`
change. The `perf-bench` matrix job benches the base and PR commits in parallel
on separate runners (one ref each), then the `perf-ab` job downloads both
exported baselines and posts a `critcmp` delta as a sticky PR comment. It is
**warn-only** and not a required check — GH runner noise (now including
cross-runner variance) makes wall-clock unfit for hard gating. Reproduce locally
on one machine with `scripts/perf-ab.sh <base-sha> out.md`.

### Concurrency + sanitizers

Concurrent-reader coverage exists at two levels:

```bash
cargo test -p musefs-core --test concurrent_reads          # core: HeaderCache + WAL reads (default suite)
cargo test -p musefs-fuse --test concurrent_reads -- --ignored  # mount: DbPool::PerThread (needs /dev/fuse)
```

CI runs the core test under **AddressSanitizer** as a required gate (`asan` job)
and both tests under **ThreadSanitizer** as a non-required best-effort signal
(`tsan` job, `continue-on-error`). TSan cannot instrument the system C libraries
(libfuse, libsqlite3), so it is a signal, not a gate. ASan is ABI-compatible with
an uninstrumented std, but TSan is not — so the TSan command needs `-Zbuild-std`
(and the `rust-src` component) to rebuild std with the sanitizer. Reproduce
locally with:

```bash
rustup toolchain install nightly
rustup component add rust-src --toolchain nightly   # for TSan's -Zbuild-std
RUSTFLAGS="-Zsanitizer=address" ASAN_OPTIONS="detect_leaks=0" \
  cargo +nightly test -p musefs-core --test concurrent_reads --target x86_64-unknown-linux-gnu
RUSTFLAGS="-Zsanitizer=thread" TSAN_OPTIONS="halt_on_error=0" \
  cargo +nightly test -p musefs-core -Zbuild-std --test concurrent_reads --target x86_64-unknown-linux-gnu
```

### Coverage

```bash
cargo install cargo-llvm-cov
cargo llvm-cov --workspace --exclude musefs-fuse --exclude musefs-latencyfs --open
cargo llvm-cov --workspace --exclude musefs-fuse --exclude musefs-latencyfs --lcov --output-path lcov.info
```

`musefs-fuse` and `musefs-latencyfs` are excluded because these FUSE crates'
tests need a real mount; their behavior is covered by the separate `e2e` CI
job rather than `llvm-cov`. The CI `e2e` job also runs the binary-level
`cargo test -p musefs -- --ignored` and
`cargo test -p musefs-latencyfs -- --ignored` suites so they cannot silently
rot (they require `/dev/fuse` + `fusermount3`). CI (`coverage.yml`) runs this on every push/PR and
uploads to Codecov (`CODECOV_TOKEN` repo secret).
