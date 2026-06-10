# Contributing

The working manual for building, testing, and landing a change. For what the
pieces *are*, read [ARCHITECTURE.md](ARCHITECTURE.md) first; for per-format
behavior, the docs under [`docs/`](docs/).

## Getting set up

You need stable Rust (edition 2024) with `rustfmt` and `clippy`. To mount or run
the FUSE end-to-end tests you need a FUSE-capable OS: Linux with `/dev/fuse` and
libfuse (`libfuse3-dev` / `libfuse3` plus `pkg-config`), or FreeBSD with
`/dev/fuse` and the `fusefs` kernel module (no libfuse — see [FreeBSD
e2e](#freebsd-e2e) for the in-tree VM harness). The Python plugin suites
additionally want Python 3 with `ruff` and `pytest`.

Enable the repo's pre-commit hook once per clone:

```bash
git config core.hooksPath .githooks
```

The hook (`.githooks/pre-commit`) runs, in order: `cargo fmt --all --check`,
`cargo clippy --all-targets -- -D warnings`, **the full workspace test
suite** (`cargo test --workspace`), and `ruff check` + `ruff format --check`
over `contrib/beets/`, `contrib/picard/`, `contrib/lidarr/`,
`contrib/python-musefs/`, and `tests/interop/`. Two consequences worth
internalizing:

- A commit with red tests is always rejected — there is no
  "commit-now-fix-later" workflow here.
- Python-only changes hit the hook too: the ruff gate lints exactly the
  union of paths the CI jobs lint, so a commit can't pass the hook yet fail
  CI lint.

## Build & test

```bash
cargo build                              # build the workspace
cargo test                               # all crates (excludes FUSE e2e)
cargo test -p musefs-core                # one crate
cargo test -p musefs-core read_at        # tests matching a substring
cargo clippy --all-targets               # lint (policy: see below)
cargo fmt                                # format
```

The FUSE end-to-end tests perform real mounts and are `#[ignore]`d:

```bash
cargo test -p musefs-fuse -- --ignored   # needs /dev/fuse + libfuse
```

The kernel-passthrough e2e additionally needs `CAP_SYS_ADMIN`. Don't run
cargo under sudo — build first, then run the prebuilt test binary with sudo
(find it in `target/debug/deps/`):

```bash
cargo test -p musefs-fuse --no-run
sudo target/debug/deps/<e2e_test_binary> --ignored <passthrough_test_name>
```

- **Read-consistency harness** (`musefs-fuse/tests/read_consistency.rs`): a seeded,
  reproducible randomized `pread`/`mmap` sweep compares live-mount reads against an
  in-memory oracle (the seed is printed on failure to reproduce). The hermetic FLAC
  tests — whole-file mmap fidelity and the read-only write-refusal matrix — always
  run; the multi-format breadth sweep generates fixtures with ffmpeg and skips any
  format whose codec is unavailable.

### FreeBSD e2e

The FUSE e2e suite also runs on FreeBSD, via the scripts in
[`scripts/freebsd-vm/`](scripts/freebsd-vm/). They are the single source of
truth — CI and local runs invoke the same scripts, so they can't drift:

- `run-local.sh` — host-side orchestrator: creates and boots a FreeBSD VM under
  qemu/KVM and runs the suite in it. All artifacts go under the gitignored
  `.scratch/freebsd/`.
- `provision.sh` — in-guest: installs `git`, `ffmpeg`, and the current stable
  Rust toolchain via `rustup` (FreeBSD's packaged `rust` lags and is too old for
  some deps), and loads the `fusefs` kernel module. Run by `run-local.sh` and CI.
- `run-e2e.sh` — in-guest: `cargo test --workspace` then the `--ignored` FUSE
  e2e suite (guards that `ffmpeg` is present so the decode/encode tests don't
  silently skip).
- `serial-run.py` — drives the VM over its serial console (the console driver
  used by `run-local.sh`).

**CI.** The `freebsd` job in [`.github/workflows/ci.yml`](.github/workflows/ci.yml)
runs these in a `vmactions/freebsd-vm` VM. It is expensive (a full in-VM build),
so it does **not** run on every PR — only when the FUSE/mount surface or its
harness changed (`musefs-fuse/`, `scripts/freebsd-vm/`, `ci.yml`) or on a release
tag (`v*`).

**Local run (one command):**

```sh
sh scripts/freebsd-vm/run-local.sh
```

Host prerequisites (Debian/Ubuntu packages in parens): `qemu-system-x86_64` +
`qemu-img` (`qemu-system-x86`, `qemu-utils`), `xorriso` (`xorriso`), `curl` +
`xz` (`curl`, `xz-utils`), `python3`. `/dev/kvm` for acceleration (it runs
without it, just far slower); ~6 GB free under `.scratch/`.

What it does, end to end:

1. Downloads the official `FreeBSD-<rel>-amd64-BASIC-CLOUDINIT-ufs.qcow2` image
   into `.scratch/freebsd/` (cached; downloaded once). That image directs its
   console to the serial line, which is what lets the harness drive it.
2. Creates a fresh overlay disk from the cached base each run (cheap reset).
3. Boots the VM headless and logs in as `root` over the serial console (the
   image has an empty root password — no SSH, no keys, no cloud-init).
4. Serves this repo over a throwaway HTTP server on qemu's user-net gateway
   (`10.0.2.2`); the guest `fetch`es and unpacks it.
5. Runs `provision.sh` + `run-e2e.sh` over the console and propagates the exit
   code, then powers the VM off.

Tunable via env: `FREEBSD_REL` (default `14.3-RELEASE`), `VM_MEM`, `VM_SMP`,
`VM_DISK`, `HTTP_PORT`, `RUN_TIMEOUT`.

To drive your own VM instead, boot any FreeBSD image and, from the repo root
inside it as root, run `sh scripts/freebsd-vm/provision.sh` then
`sh scripts/freebsd-vm/run-e2e.sh`.

Notes:

- FreeBSD uses fuser's pure-rust `/dev/fuse` backend — **no libfuse package**;
  only the `fusefs` kernel module and base-system `mount_fusefs(8)` are needed.
- Kernel FUSE passthrough (StructureOnly) is **Linux-only**; on FreeBSD it falls
  back to daemon serving.

macOS support is best-effort: CI builds there with `fuser`'s `macos-no-mount`
feature, and the platform-specific logic is unit-tested. Mounted e2e on
macOS/FUSE-T is not yet validated.

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
cargo +nightly fuzz run <target>                  # flac|mp3|mp4|ogg|wav|ogg_page|b64|vorbiscomment
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
smoke-run) because it builds a DB + temp backing file per input.

### Independent-reader interop (mutagen)

Asserts that an independent ecosystem reader sees the tags musefs
synthesizes, across all five formats:

```bash
pip install -r tests/interop/requirements.txt
MUSEFS_INTEROP_DIR=/tmp/i cargo test -p musefs-core --test interop_emit -- --ignored emit_interop_fixtures
MUSEFS_INTEROP_DIR=/tmp/i python -m pytest tests/interop
```

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
  Every new `file:line:col` exclusion needs a `# guard:` tag (the guard rejects
  an untagged one), and `exclude_re` patterns must stay within the
  Rust-regex/Python-`re` shared subset the guard allows (`\. \d + | ^ ( ) *`,
  no inline `(?...)` groups).

### Coverage

```bash
cargo install cargo-llvm-cov
cargo llvm-cov --workspace --exclude musefs-fuse --open
cargo llvm-cov --workspace --exclude musefs-fuse --lcov --output-path lcov.info
```

`musefs-fuse` is excluded because its tests need a real mount; the FUSE
behavior is covered by the separate `e2e` CI job. CI (`coverage.yml`) runs
this on every push/PR and uploads to Codecov (`CODECOV_TOKEN` repo secret).

## Code conventions

- **Errors.** Each crate has its own `error.rs` with a `thiserror` enum;
  `musefs-core` wraps lower layers in `CoreError`; the CLI is the only
  `anyhow` consumer. Internal error paths never discard diagnostics: no
  `Result<_, ()>`, no `.map_err(|_| …)` that drops a source — each variant
  carries its source (`#[from]`) or a static reason naming the broken
  invariant.
- **Integer conversions.** The four clippy cast lints are deny-via-CI.
  Widenings use `From`; `u64 -> usize` only via the sanctioned `usize_from`
  helpers (`musefs_db::convert`, re-exported by core; `musefs-format` and
  `musefs-latencyfs` carry crate-local siblings — the workspace is declared
  64-bit-only); genuine narrowings use `try_from` (`?` for input-dependent
  values, `.expect` for structurally bounded ones, `.unwrap` in tests);
  deliberate bit-truncation keeps `as` under a reasoned `#[expect]`.
  Non-negative DB row fields are unsigned; rusqlite's checked conversions
  (feature `fallible_uint`) validate at the row boundary.
- **Lint policy.** `clippy::pedantic` minus a few intentional/noisy groups,
  defined in the root `Cargo.toml` under `[workspace.lints]`. The hook and
  CI deny all warnings.
- **Unsafe code.** `unsafe_code = "deny"` is set for the workspace members in
  the root `Cargo.toml` (`[workspace.lints.rust]`); the standalone `fuzz/`
  crate is outside the workspace and is not covered. A genuinely-necessary
  `unsafe` is opted in per-site with `#[expect(unsafe_code, reason = "...")]`
  — never a bare `unsafe` block and never by relaxing the workspace lint, so
  every `unsafe` is greppable and review-visible. Prefer a safe crate (e.g.
  `rustix` for syscalls) over hand-rolled FFI.
- **Layering.** Keep `musefs-fuse`, `musefs-cli`, and the `musefs` binary
  thin; cross-cutting logic belongs in `musefs-core`
  (see [ARCHITECTURE.md](ARCHITECTURE.md#crate-layout)).
- **Hidden API consumers.** `benches/` directories and each crate's
  `tests/` are compiled only by `--all-targets`: after an API change,
  compile-check with `cargo clippy --all-targets`, not `cargo build`.

## Adding a format

1. Implement probe + `synthesize_layout` in `musefs-format` (mirror an
   existing module — `flac.rs`, `mp3.rs`, `mp4.rs`, `ogg/`, `wav.rs`),
   returning a `RegionLayout`.
2. Add the variant to `musefs-db`'s `Format` enum, then wire it into the
   `match track.format` arms in `reader::HeaderCache::resolve`
   (`musefs-core/src/reader.rs`) and into `scan.rs` (extension list, probe
   dispatch).
3. Extend the test surface: a `fuzz_check::fixtures::<fmt>()` minimal file,
   a `fuzz/fuzz_targets/<fmt>.rs` target with a seed in `generate_seeds`, a
   `musefs-format/tests/proptest_<fmt>.rs`, and a manifest row in
   `musefs-core/tests/interop_emit.rs`.
4. Write `docs/<FMT>.md` (follow the shape of the existing five).

## Python plugins (contrib)

The four packages share one drift-guarded contract; see
[ARCHITECTURE.md](ARCHITECTURE.md#the-contrib-ecosystem) for the layout and
each README for plugin-specific setup.

```bash
# python-musefs: self-contained
cd contrib/python-musefs && python -m pytest && ruff check . && ruff format --check .

# beets: python-musefs is UNPUBLISHED — install the local lib first or
# dependency resolution fails (see contrib/beets/README.md for the venv flow)
cd contrib/beets && pip install -e ../python-musefs && pip install -e ".[test]" && python -m pytest tests

# picard: no install needed (vendored + pythonpath=".")
cd contrib/picard && python -m pytest tests

# lidarr: python-musefs is UNPUBLISHED — install the local lib first or
# dependency resolution fails (see contrib/lidarr/README.md for the env flow)
cd contrib/lidarr && pip install -e ../python-musefs && pip install -e ".[test]" && python -m pytest tests
```

Gotchas that have bitten before:

- On PEP 668 "externally managed" systems, bare `pip install` fails — use a
  venv for the beets suite.
- The real-Picard tests `importorskip` Picard and Qt: without an importable
  Picard (e.g. the system package on `PYTHONPATH`), they **silently skip**.
  When touching the Picard plugin, make sure they actually ran.
- The Lidarr real-instance smoke test is a release gate, not a default CI job.
  It verifies Lidarr accepts script-created symlink destinations and emits the
  expected Custom Script event.
- `musefs_common/schema.py` is **generated** from `musefs-db/src/schema.rs`.
  After a schema change:
  `MUSEFS_REGEN_SCHEMA_PY=1 cargo test -p musefs-db schema_py`, then
  re-vendor Picard's copy with
  `python contrib/python-musefs/vendor_to_picard.py`. Drift is enforced by a
  `musefs-db` unit test and the Picard vendor-sync test.
- `MAX_ART_BYTES` in `contrib/python-musefs/src/musefs_common/constants.py`
  is **hand-mirrored** from `musefs-core/src/scan.rs` — update both sides
  together.

## Releasing the Python packages

The `contrib/` Python packages (`python-musefs`, `beets-musefs`,
`lidarr-musefs`, and the unpublished `musefs-picard`) share a single version,
decoupled from the Rust crates and released on a `py-v*` tag. `musefs-picard`
tracks the version but is not uploaded to PyPI (Picard has its own plugin
registry; the shared library is vendored into it).

**One-time setup (before the first release).** Trusted Publishing fails until
the publisher exists on PyPI. For each of `python-musefs`, `beets-musefs`, and
`lidarr-musefs`:

1. Create/reserve the project on PyPI.
2. Add a GitHub Actions trusted publisher pointing at: owner/repo `Sohex/musefs`,
   workflow `release-python.yml`, environment `pypi`.

Also create a GitHub environment named `pypi` in the repo settings (it gates the
`publish` job).

**Cutting a release:**

1. Choose the new version `X.Y.Z` and run `python scripts/bump_python_version.py X.Y.Z`.
   This rewrites every `contrib/*/pyproject.toml` version, the `__version__`
   strings, the `python-musefs>=` dependency floors, and re-vendors python-musefs
   into the Picard plugin.
2. Review `git diff` — it should touch only the version/floor lines and the
   Picard vendored `_common/` copy.
3. Promote the `## [Unreleased]` section of `contrib/CHANGELOG.md` to
   `## [X.Y.Z] - <date>`.
4. Commit, then tag and push:
   ```bash
   git commit -am "release: python packages X.Y.Z"
   git tag py-vX.Y.Z
   git push origin HEAD --tags
   ```
5. `release-python.yml` runs the version gate, the four Python test suites, then
   publishes `python-musefs`, `beets-musefs`, and `lidarr-musefs` to PyPI (in
   that order).

## PRs & commits

- Conventional-style subjects (`fix(format): …`, `docs: …`, `ci: …`), scoped
  and imperative.
- `main` is protected by required status checks: the `ci-ok` and
  `coverage-ok` aggregator jobs must pass. CI also runs the fuzz smoke
  build, the in-diff mutation gate, and a security audit on PRs. Docs-only
  changes skip the expensive jobs at the *job* level — the aggregators still
  report.
- Benchmark results, when a change warrants them, are recorded in
  [BENCHMARKS.md](BENCHMARKS.md).
- Run the in-diff mutation gate (above) for logic changes — it is CI parity,
  not optional polish.
