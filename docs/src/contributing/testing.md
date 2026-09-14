# Test tiers

## Test tiers beyond `cargo test`

### Property tests

`proptest` invariants — panic-freedom, the byte-identical-audio guarantee,
tag round-trip stability — live in `musefs-format/tests/proptest_*.rs` and
`musefs-core/tests/proptest_read_fidelity.rs`. The format-layer suites are
gated on the `fuzzing` feature, which `musefs-format`'s self-dev-dependency
enables for all of its own test builds — so a plain
`cargo test -p musefs-format` runs them.

The same pattern keeps the rest of the test scaffolding out of the published
API. `musefs-core` and `musefs-db` each have a `test-support` feature gating the
hooks their integration tests reach across the crate boundary — the whole-file
oracle scan, the `*_for_test` methods — and switched on by a self-dev-dependency
(plus `musefs-core`'s dev-dependency on `musefs-db`), so no flag is needed. A new
test hook goes behind that feature when an integration test needs it, and is
`#[cfg(test)] pub(crate)` when only the crate's own unit tests do. A plain
`cargo build` does not compile any of it; check such a change with
`cargo clippy --workspace` as well as `--all-targets`.

### Coverage-guided fuzzing

The `fuzz/` crate is **excluded from the workspace**: workspace-wide build,
test, and clippy do not compile it, so a signature change can break fuzz
targets without anything failing locally. It depends on `musefs-format`, and
the `serve` target also on `musefs-core` and `musefs-db`. CI's fuzz `smoke` job
(`cargo +nightly fuzz build`) catches a break, but on PRs it triggers only on
`musefs-format/**` and `fuzz/**`, so a `musefs-core` or `musefs-db` API change
can break the fuzz build with nothing in the PR noticing. Check locally before
pushing an API change to any of the three:

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
`serve_ogg_window`/`OggArtSlice`); it builds a DB + temp backing file per
input, so it is slower per execution than the format targets, but it is
smoke-run per PR alongside them and runs in the scheduled campaign. The `serve`
target also exercises hostile DB rows (negative/oversized geometry,
invalid formats, orphaned/oversized art, stale binary-tag handles, content-version
mismatch) via the `musefs-db` `fuzzing`-gated `with_raw_conn`, plus binary-tag
streaming and distinct Opus/Vorbis/OggFLAC fixtures.

### Independent-reader interop (mutagen)

Asserts that an independent ecosystem reader sees the tags musefs
synthesizes, across all five formats:

```bash
pip install -r tests/interop/requirements.txt pytest
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
and art survived. Picard's `musefs_bin` tier runs in the same job: its path-gate
tests import the bundled `musefs._common`, not the system-Picard environment.

The job also runs the beets `e2e` tier (`python -m pytest contrib/beets/tests -m
e2e`), the only test that drives the whole chain: generated audio, `beet
import`, a retag, `beet musefs`, a real FUSE mount, and tags plus byte-identical
audio read back from it. It is deselected by default because it needs `ffmpeg`,
the built binary, `/dev/fuse` and a `fusermount3`/`fusermount` helper. Under
`MUSEFS_REQUIRE_BIN=1` a missing one fails the tier rather than skipping it: for
a while it ran in no job at all, and a guard that skipped hid it
([#728](https://github.com/Sohex/musefs/issues/728)).

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
the scheduled campaign has a leg each for `musefs-db`, `musefs-core`,
`musefs-format`, `musefs-fuse` and `musefs-latencyfs`. `.cargo/mutants.toml`'s
`exclude_globs` permanently exclude `musefs-cli/**`, `musefs/**`, the
feature-gated `musefs-core/src/metrics.rs`, and, in `musefs-fuse`,
`src/lib.rs` and `src/platform/**`. The rest of `musefs-fuse` —
`src/convert.rs`'s pure helpers — is mutated. `musefs-latencyfs` carries real
logic and needs `/dev/fuse` to kill its mutants, so its leg installs libfuse and
runs the mounted `#[ignore]`d tests.

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

  `scripts/mutants.sh` also supports sharding (`MUTANTS_SHARD=i/n`). CI shards
  every crate's scheduled campaign into ~50-mutant shards, and the per-PR
  in-diff gate the same way (`cargo mutants --shard i/n`), though a sharded
  local workflow hasn't been built out.
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
  re-validates every `exclude_re` entry. It runs in the per-PR `in-diff-plan` job
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

The A/B benchmark runs only on PRs that change `musefs-core/src/**` or
`musefs-format/src/**`. The `perf-ab` job runs `scripts/perf-ab.sh`, which
benches the base and PR commits back-to-back on one runner and posts a
`critcmp` delta as a sticky PR comment (for a fork PR, which cannot be commented
on, the delta lands in the job summary only). It is **warn-only** and not a
required check — GH runner noise makes wall-clock unfit for hard gating.
Reproduce locally with the same `scripts/perf-ab.sh <base-sha> out.md`.

### Concurrency + sanitizers

Concurrent-reader coverage exists at two levels:

```bash
cargo test -p musefs-core --test concurrent_reads          # core: HeaderCache + WAL reads (default suite)
cargo test -p musefs-fuse --test concurrent_reads -- --ignored  # mount: DbPool::PerThread (needs /dev/fuse)
```

The core binary also carries the read-ahead pool's budget-accounting stress
(`readahead_*`, #628): 16 threads register/read/deregister streams against a
4 MiB budget so eviction fires constantly, then assert
`charged == Σ(registered buffers' bytes.len())` at quiescence — the
invariant #536 restored. It lives in this binary precisely so the sanitizer
legs below reach it; the serve-path tests around it run with read-ahead
disabled. Rounds
default to a sanitizer-friendly 24 per thread;
`MUSEFS_READAHEAD_STRESS_ROUNDS=200` soaks it locally.

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

The uninstrumented-C caveat is not theoretical, and it reaches the core test —
not just the mounted one. The `tsan` leg intermittently reports a `data race` on
a `memcpy` inside `walIndexWriteHdr` / `walIndexTryHdr` (`sqlite3.c`), reached
through `Db::open_readonly` from the serve-path tests. That is SQLite's
WAL-index header, guarded by barriers of its own that TSan cannot see — a false
positive, not a musefs defect. Every test still passes when it fires; the leg
reddens only because TSan exits 66 whenever it emits a warning at all. It is
probabilistic, so it shows up on some runs and not others.

So before treating a red `tsan` leg as a finding, check whether any frame in the
reported stacks names musefs code. Pure `sqlite3.c` stacks are this known noise;
a frame in `musefs_core` is worth investigating.

### Dependency advisories & licenses

Two supply-chain gates run in CI and are worth reproducing locally before a
dependency bump:

```bash
cargo install cargo-audit
# Same 0.19 pin both CI jobs use — the config schema shifts across minor releases.
cargo install cargo-deny --locked --version '^0.19'
cargo audit                                                    # root Cargo.lock, RUSTSEC advisories
# advisories + licenses + bans + sources, as ci.yml's deny job runs it
cargo deny check --deny advisory-not-detected --deny license-not-encountered
# the fuzz lockfile, as audit.yml runs it
cargo deny --manifest-path fuzz/Cargo.toml check --config deny.toml \
  --allow advisory-not-detected advisories
```

`audit.yml` runs `rustsec/audit-check` (root lockfile only) plus the third
command above, because `fuzz/` is a separate workspace with its own lockfile
that neither the action nor the `ci.yml` `cargo-deny` job would otherwise see.
`cargo-deny` adds the license allow-list, which `cargo-audit` does not check.

Advisories with no upgrade path are allow-listed **with a written rationale at
the ignore site** — `.cargo/audit.toml` for `cargo-audit`/the action,
`deny.toml` for `cargo-deny` (both lists must agree). An `unmaintained`
advisory on a compile-time-only or unreachable dependency is a candidate; a
`vulnerability` is not — those get fixed. `cargo-deny` reports
`advisory-not-detected` for an ignore whose crate is absent from the lockfile
being scanned, and `license-not-encountered` for an allowed license nothing
uses. The `ci.yml` `deny` job promotes both to errors, so a stale entry in
`deny.toml` fails the gate: remove it in the same change that drops the crate
or license. `deny.toml` is authored against the root graph, so the fuzz scan
in `audit.yml` allows `advisory-not-detected` instead; an entry that applies
only to the fuzz lockfile belongs in a separate config, not the root list.

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
rot (they require `/dev/fuse` + `fusermount3`). CI (`coverage.yml`) runs this on
every PR, on pushes to `main`, and on `v*` tags (which regenerate `coverage-ok`
for the release gate), and uploads to Codecov (`CODECOV_TOKEN` repo secret).
There is no trigger-level path filter: a `changes` job skips the coverage run
when every changed path is under `docs/` or ends in `.md`, and the
`coverage-ok` aggregator still reports.
