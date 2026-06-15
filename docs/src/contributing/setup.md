# Getting set up

The working manual for building, testing, and landing a change. For what the
pieces *are*, read [ARCHITECTURE.md](../architecture/overview.md) first; for per-format
behavior, the docs under [`docs/`](../formats/overview.md).

Map of this document:

- [Getting set up](#getting-set-up) — prerequisites and the pre-commit hook.
- [Build & test](#build--test) — everyday commands, the FUSE e2e suite, the
  [FreeBSD VM harness](#freebsd-e2e).
- [Test tiers beyond `cargo test`](testing.md#test-tiers-beyond-cargo-test) — property
  tests, fuzzing, interop, the contract round trip, fault injection, mutation
  testing, sanitizers, coverage.
- [Code conventions](conventions.md#code-conventions) — errors, integer casts, lints,
  `unsafe`, layering.
- [Adding a format](conventions.md#adding-a-format) — the four-step recipe.
- [Python plugins (contrib)](plugins.md#python-plugins-contrib) — per-suite commands and
  the gotchas.
- [Releasing](releasing.md#releasing-the-python-packages) — the Python (`py-v*`) and
  [Rust (`v*`)](releasing.md#releasing-the-rust-crates-and-binaries) release flows.
- [PRs & commits](releasing.md#prs--commits) — conventions and the
  [before-you-push checklist](releasing.md#before-you-push).

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
every tracked shell script, `yamllint` (relaxed [`.yamllint`](../../../.yamllint)) over
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

The `musefs` binary enables the default-on `jemalloc` feature (jemalloc global
allocator + background purge thread). Build the system-allocator variant with
`cargo build -p musefs --no-default-features` — used for the RSS comparison
(`scripts/rss-churn-bench.sh`) and by packagers that forbid vendored C libs.

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
`scripts/freebsd-vm/`. They are the single source of
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

**CI.** The `freebsd` job in [`.github/workflows/ci.yml`](../../../.github/workflows/ci.yml)
runs these in a `vmactions/freebsd-vm` VM. It is expensive (a full in-VM build),
so it does **not** run on every PR — only when the FUSE/mount surface or its
harness changed (`musefs/`, `musefs-fuse/`, `scripts/freebsd-vm/`, `Cargo.lock`,
`ci.yml`) or on a release tag (`v*`).

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
