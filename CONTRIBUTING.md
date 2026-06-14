# Contributing

The working manual for building, testing, and landing a change. For what the
pieces *are*, read [ARCHITECTURE.md](ARCHITECTURE.md) first; for per-format
behavior, the docs under [`docs/`](docs/).

Map of this document:

- [Getting set up](#getting-set-up) — prerequisites and the pre-commit hook.
- [Build & test](#build--test) — everyday commands, the FUSE e2e suite, the
  [FreeBSD VM harness](#freebsd-e2e).
- [Test tiers beyond `cargo test`](#test-tiers-beyond-cargo-test) — property
  tests, fuzzing, interop, the contract round trip, fault injection, mutation
  testing, sanitizers, coverage.
- [Code conventions](#code-conventions) — errors, integer casts, lints,
  `unsafe`, layering.
- [Adding a format](#adding-a-format) — the four-step recipe.
- [Python plugins (contrib)](#python-plugins-contrib) — per-suite commands and
  the gotchas.
- [Releasing](#releasing-the-python-packages) — the Python (`py-v*`) and
  [Rust (`v*`)](#releasing-the-rust-crates-and-binaries) release flows.
- [PRs & commits](#prs--commits) — conventions and the
  [before-you-push checklist](#before-you-push).

## Getting set up

Prerequisites:

- **Rust** — stable (edition 2024) with `rustfmt` and `clippy`.
- **FUSE** (to mount, or to run the FUSE end-to-end tests) — Linux with
  `/dev/fuse` and libfuse (`libfuse3-dev` / `libfuse3` plus `pkg-config`), or
  FreeBSD with `/dev/fuse` and the `fusefs` kernel module (no libfuse — see
  [FreeBSD e2e](#freebsd-e2e) for the in-tree VM harness).
- **Python 3** with `ruff` and `pytest` — only for the Python plugin suites.
- **`shellcheck` and `yamllint`** — optional; the pre-commit hook's shell and
  YAML lint legs each skip with a notice if not installed.

Enable the repo's pre-commit hook once per clone:

```bash
git config core.hooksPath .githooks
```

The hook (`.githooks/pre-commit`) runs, in order: `cargo fmt --all --check`,
`cargo clippy --all-targets -- -D warnings`, **the full workspace test
suite** (`cargo test --workspace`), a conditional cargo-mutants anchor-drift
guard (only when `.cargo/mutants.toml`, `scripts/check_mutant_anchors.py`, or
a `musefs-core`/`musefs-format` source file is staged), `shellcheck` over
every tracked shell script, `yamllint` (relaxed [`.yamllint`](.yamllint)) over
every tracked YAML file, and `ruff check` + `ruff format --check` over
`contrib/beets/`, `contrib/picard/`, `contrib/lidarr/`,
`contrib/python-musefs/`, `scripts/`, and `tests/interop/`. A few consequences
worth internalizing:

- A commit with red tests is always rejected — there is no
  "commit-now-fix-later" workflow here.
- Python-only changes hit the hook too: the ruff gate lints exactly the
  union of paths the CI jobs lint, so a commit can't pass the hook yet fail
  CI lint.
- The cargo gate (fmt/clippy/test) is skipped when every staged path is under
  `docs/` or is a Markdown file, so a docs-only commit stays fast.
- The `shellcheck`/`yamllint` legs fire only when a shell or YAML file is
  staged, and skip with a notice when the tool is absent; when they do run they
  lint *all* tracked files of that type, so a sibling file can't drift
  unnoticed.
- The mutant-anchor guard fires only when the mutants config, its check
  script, or a `musefs-core`/`musefs-format` source file is staged, and skips
  with a notice when `cargo-mutants` is absent (CI re-checks it regardless). It
  re-validates that the `.cargo/mutants.toml` `exclude_re` anchors still point
  at their intended `file:line:col` after a line-shifting edit.

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
- The Lidarr integration is gated by two automated tiers, both deterministic
  and network-free (Lidarr's metadata server is mocked too):
  - **PR check — `.github/workflows/lidarr-smoke.yml`** (`scripts/lidarr-smoke.sh`):
    a fast smoke that proves the Custom Script exec path on a real Lidarr (its
    Test event) and runs the content leg (`musefs-lidarr-sync` tag-writes,
    `musefs-lidarr-import` symlink, served-mount tags, unchanged bytes) against a
    local mock Lidarr API. Runs on PRs touching the Lidarr surface.
  - **Release gate — `.github/workflows/lidarr-e2e.yml`** (`scripts/lidarr-e2e/run-e2e.sh`):
    the full real-instance e2e. A real Lidarr, driven by local
    metadata/indexer/qBittorrent mocks, performs a genuine **download-client
    import** of a real CC0 album as a `NewDownload`, firing `OnReleaseImport`,
    which execs the real musefs scripts; the served mount is then asserted to
    carry Lidarr-supplied metadata the backing file lacked, bytes unchanged. This
    **gates the Python `py-v*` publish** and closes what used to be the manual
    download-client gap. The vendored CC0 fixture is `scripts/lidarr-e2e/fixtures/`.
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

## Releasing the Rust crates and binaries

The Rust workspace publishes to crates.io and ships prebuilt cross-compiled
binaries on a `v*` tag, decoupled from the Python `py-v*` flow. `release.yml`
runs one ordered graph — `gate → build → smoke → publish → release-assets` —
and is the source of truth; this checklist is the human side.

**Pre-flight.**

1. Working tree clean, on the commit you intend to release.
2. Confirm `main` is green (CI + coverage). The tag push triggers a fresh
   `ci.yml` and `coverage.yml` run, and the release `gate` job **waits for
   `ci-ok` and `coverage-ok` to be green on the tagged commit** before anything
   builds or publishes — a red tree blocks the release automatically.
3. `CARGO_REGISTRY_TOKEN` is present in repo secrets.

**Version bump (do this in one commit before tagging).**

1. Pick the new version `X.Y.Z`.
2. Bump the workspace `version` in `Cargo.toml`.
3. Bump every internal `musefs-*` path-dependency constraint that pins the old
   version (e.g. `musefs-db = { version = "X.Y.Z", path = "..." }`) — a stale
   internal floor fails the publish.
4. Promote the `## [Unreleased]` section of `CHANGELOG.md` to
   `## [X.Y.Z] - <date>`.
5. Dry-run package each crate: `cargo package -p <crate> --locked` for each of
   `musefs-db musefs-format musefs-core musefs-fuse musefs-cli musefs`. This
   catches packaging errors but **not** the cross-crate index-propagation
   problem (it resolves siblings via path deps); that is handled in-workflow
   (next section).
6. Commit, e.g. `git commit -am "release: vX.Y.Z"`.

**Tag and push.**

```bash
git tag vX.Y.Z
git push origin HEAD --tags
```

The tag push starts both CI and `release.yml`. The `gate` job blocks publishing
until `ci-ok` + `coverage-ok` are green on the tagged tree (45-minute timeout,
covering the full matrix including the FreeBSD VM e2e).

**What `release.yml` does.**

1. `gate` — verifies the tag matches the workspace version and waits for the
   required CI checks to pass on the tagged commit (fails closed on a failed
   check or timeout).
2. `build` — cross-compiles the four target binaries.
3. `smoke` — runs the binary smoke on each target (host + Alpine).
4. `publish` — publishes crates in dependency order. For each crate it **skips**
   the publish if `name@version` already resolves from the crates.io index, then
   **waits** for that version to appear before publishing the next dependent
   crate (index-propagation; #163). The skip makes a whole-workflow re-run after
   a partial failure safe.
5. `release-assets` — creates/updates the GitHub Release and uploads the binary
   tarballs + checksums (only after crates publishing succeeds).

**Retry / rollback.**

- crates.io is **yank-only** — a published version cannot be un-published.
- A partial failure (e.g. crate 3 of 6 published, then a transient error) is
  recovered by **re-running the workflow**: the publish loop skips the crates
  already in the index and resumes, then runs `release-assets`. No manual
  cleanup of the published crates is needed.
- GitHub asset upload is idempotent (`gh release upload --clobber`), so re-runs
  re-upload safely.

**Post-release verification.**

1. `cargo install musefs` (or `cargo install musefs --version X.Y.Z`) from a
   clean machine/container.
2. Download a release tarball and verify its checksum:
   `sha256sum -c musefs-X.Y.Z-<triple>.tar.gz.sha256`.
3. Confirm all four target tarballs + `.sha256` files are attached to the
   GitHub Release.

**Lidarr gate at a v1.0.0 milestone.** The Lidarr real-instance e2e
(`lidarr-e2e.yml`) gates the Python `py-v*` release, not this Rust flow. When a
v1.0.0 milestone bundles both, ensure the Python release (and therefore its
Lidarr e2e gate) is also run.

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

### Before you push

The pre-commit hook already gates fmt, clippy, the workspace tests, and the
Python/shell/YAML lints on every commit. What it does **not** run — check the
ones your change triggers:

- **Logic changes** → the [in-diff mutation gate](#mutation-testing). It is CI
  parity, not optional polish.
- **Format-layer API changes** → `cargo +nightly fuzz build`; the `fuzz/`
  crate is outside the workspace, so nothing else compiles it
  ([coverage-guided fuzzing](#coverage-guided-fuzzing)).
- **`musefs-db` schema changes** → regenerate and re-vendor the Python schema
  mirror ([Python plugins](#python-plugins-contrib)).
- **Picard plugin changes** → make sure the real-Picard tests actually ran
  rather than silently skipped ([gotchas](#python-plugins-contrib)).
- **FUSE/mount-surface changes** → run the `--ignored` e2e suite locally
  ([Build & test](#build--test)); the FreeBSD CI leg only runs on PRs that
  touch that surface.
