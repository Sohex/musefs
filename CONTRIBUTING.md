# Contributing

The working manual for building, testing, and landing a change. For what the
pieces *are*, read [ARCHITECTURE.md](ARCHITECTURE.md) first; for per-format
behavior, the docs under [`docs/`](docs/).

## Getting set up

You need stable Rust (edition 2021) with `rustfmt` and `clippy`, and — to
mount or run the FUSE end-to-end tests — Linux with `/dev/fuse` and libfuse
(`libfuse3-dev` / `libfuse3` plus `pkg-config`). The Python plugin suites
additionally want Python 3 with `ruff` and `pytest`.

Enable the repo's pre-commit hook once per clone:

```bash
git config core.hooksPath .githooks
```

The hook (`.githooks/pre-commit`) runs, in order: `cargo fmt --all --check`,
`cargo clippy --all-targets -- -D warnings`, **the full workspace test
suite** (`cargo test --workspace`), and `ruff check` + `ruff format --check`
over `contrib/beets/`, `contrib/picard/`, `contrib/python-musefs/`, and
`tests/interop/`. Two consequences worth internalizing:

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

### FreeBSD e2e

The FUSE e2e suite also runs on FreeBSD. CI uses the `freebsd` job in
[`.github/workflows/ci.yml`](.github/workflows/ci.yml), which invokes the
committed scripts in [`scripts/freebsd-vm/`](scripts/freebsd-vm/README.md).
Those scripts provision the VM (`rust`, `git`, `ffmpeg`, and the `fusefs`
kernel module) and then run the same workspace + `--ignored` FUSE test
commands locally and in CI. Keep a FreeBSD VM image under the gitignored
`/.scratch/`.

macOS support is best-effort: CI builds there with `fuser`'s
`macos-no-mount` feature, and the platform-specific logic is unit-tested.
Mounted e2e on macOS/FUSE-T is not yet validated.

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

### Independent-reader interop (mutagen)

Asserts that an independent ecosystem reader sees the tags musefs
synthesizes, across all five formats:

```bash
pip install -r tests/interop/requirements.txt
MUSEFS_INTEROP_DIR=/tmp/i cargo test -p musefs-core --test interop_emit -- --ignored emit_interop_fixtures
MUSEFS_INTEROP_DIR=/tmp/i python -m pytest tests/interop
```

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

The three packages share one drift-guarded contract; see
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
```

Gotchas that have bitten before:

- On PEP 668 "externally managed" systems, bare `pip install` fails — use a
  venv for the beets suite.
- The real-Picard tests `importorskip` Picard and Qt: without an importable
  Picard (e.g. the system package on `PYTHONPATH`), they **silently skip**.
  When touching the Picard plugin, make sure they actually ran.
- `musefs_common/schema.py` is **generated** from `musefs-db/src/schema.rs`.
  After a schema change:
  `MUSEFS_REGEN_SCHEMA_PY=1 cargo test -p musefs-db schema_py`, then
  re-vendor Picard's copy with
  `python contrib/python-musefs/vendor_to_picard.py`. Drift is enforced by a
  `musefs-db` unit test and the Picard vendor-sync test.
- `MAX_ART_BYTES` in `contrib/python-musefs/src/musefs_common/constants.py`
  is **hand-mirrored** from `musefs-core/src/scan.rs` — update both sides
  together.

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
