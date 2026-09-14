# Releasing

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
   strings, Picard's `PLUGIN_VERSION`, the pinned `__version__ == "..."`
   assertions in `contrib/lidarr/tests/test_smoke.py` and
   `contrib/python-musefs/tests/test_public_api.py`, the `python-musefs>=`
   dependency floors, and re-vendors python-musefs into the Picard plugin.
2. Review `git diff` — it should touch only those version/floor lines and the
   Picard vendored `_common/` copy.
3. Promote the `## [Unreleased]` section of `contrib/CHANGELOG.md` to
   `## [X.Y.Z] - <date>`.
4. Commit, then tag and push:
   ```bash
   git commit -am "release: python packages X.Y.Z"
   git tag py-vX.Y.Z
   git push origin HEAD --tags
   ```
5. `release-python.yml` runs the version gate, then the four Python test suites
   and the real-Lidarr e2e (`lidarr-e2e.yml`); the `publish` job needs all five.
   It then publishes `python-musefs`, `beets-musefs`, and `lidarr-musefs` to
   PyPI (in that order).

## Releasing the Rust crates and binaries

The Rust workspace publishes to crates.io and ships prebuilt cross-compiled
binaries and container images on a `v*` tag, decoupled from the Python `py-v*`
flow. `release.yml` runs one ordered graph — `gate → build → smoke → publish →
release-assets`, with `images` branching off `smoke` and a record-only
`benchmarks` job beside it — and is the source of truth; this checklist is the
human side.

**Pre-flight.**

1. Working tree clean, on the commit you intend to release.
2. Confirm `main` is green (CI + coverage). The tag push triggers a fresh
   `ci.yml` and `coverage.yml` run, and the release `gate` job **waits for
   `ci-ok` and `coverage-ok` to be green on the tagged commit** before anything
   builds or publishes — a red tree blocks the release automatically.
3. `CARGO_REGISTRY_TOKEN` is present in repo secrets.
4. Smoke-build every cross target so `jemalloc-sys` is known to compile under
   zig before tagging (the release matrix builds with the `jemalloc` feature on).
   These are the six `build` targets in `release.yml`. Add the Rust triples
   first; `rustup` takes them without a glibc suffix:

   ```bash
   rustup target add x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu \
     x86_64-unknown-linux-musl aarch64-unknown-linux-musl \
     riscv64gc-unknown-linux-gnu riscv64gc-unknown-linux-musl
   ```

   Then build each one. `cargo zigbuild --target` takes the suffixed form, which
   pins the glibc floor (2.17, or 2.27 for riscv64):

   ```bash
   for t in x86_64-unknown-linux-gnu.2.17 aarch64-unknown-linux-gnu.2.17 \
            x86_64-unknown-linux-musl aarch64-unknown-linux-musl \
            riscv64gc-unknown-linux-gnu.2.27 riscv64gc-unknown-linux-musl; do
     cargo zigbuild --release -p musefs --target "$t"
   done
   ```

   `scripts/smoke-binary.sh <binary>` is the smoke `release.yml` runs on each
   build (scan, mount, read back, byte-identical audio, clean unmount). It needs
   `ffmpeg` and `fusermount3` (the `fuse3` package) on `PATH` and `/dev/fuse`.
   A native binary runs it directly, as the `smoke` job's host legs do:

   ```bash
   ./scripts/apt-install.sh fuse3 ffmpeg
   ./scripts/smoke-binary.sh ./bin/musefs
   ```

   A musl one runs inside `alpine`, with the same container flags the job uses:

   ```bash
   docker run --rm \
     --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined \
     -v "$PWD":/w -w /w alpine:3.24 \
     sh -c 'apk add --no-cache fuse3 ffmpeg >/dev/null && sh scripts/smoke-binary.sh ./bin/musefs'
   ```

   Another architecture needs user-mode QEMU (`qemu-user`, or
   `qemu-user-static` with binfmt registered) and adds `--platform
   linux/riscv64` to that `docker run`, installing `fuse3 ffmpeg` with the
   image's package manager (`debian:trixie-slim` for gnu, `alpine:3.24` for
   musl). The riscv64 smoke legs are emulated in CI and do not block the
   release.

   If a target cannot build `jemalloc-sys`, don't block the release: add a key
   to that target's `build` matrix entry in `release.yml` (e.g.
   `cargo_flags: --no-default-features`) and append it in the shared `Build`
   step (`cargo zigbuild --release -p musefs --target ${{ matrix.zig_target }}
   ${{ matrix.cargo_flags }}`), so only that entry gets the flag. The Docker
   images `COPY` the binary this step produces (they don't run cargo), so the
   matching container inherits the opt-out automatically.

**Version bump (do this in one commit before tagging).**

1. Pick the new version `X.Y.Z`.
2. Bump the workspace `version` in `Cargo.toml`.
3. Bump every internal `musefs-*` path-dependency constraint that pins the old
   version (e.g. `musefs-db = { version = "X.Y.Z", path = "..." }`) — a stale
   internal floor fails the publish.
4. Refresh the lockfiles. `cargo update --workspace` moves the workspace
   crates' versions in `Cargo.lock` and leaves every existing non-workspace
   entry locked (Cargo still adds an entry a new dependency needs); the
   dry run below and every `release.yml` publish use `--locked`, so a stale
   lockfile fails them. `fuzz/` is outside the workspace and keeps its own
   `fuzz/Cargo.lock`, which still pins the old musefs versions until
   `cargo +nightly fuzz build` rebuilds it; commit that too.
5. Promote the changelogs. In both `CHANGELOG.md` and `docs/src/changelog.md`,
   rename `## [Unreleased]` to `## [X.Y.Z] - <date>` and open a fresh, empty
   `## [Unreleased]` above it. Then update each file's footer link definitions:
   point `[Unreleased]` at `compare/vX.Y.Z...HEAD` and add
   `[X.Y.Z]: https://github.com/Sohex/musefs/releases/tag/vX.Y.Z`. Without the
   definition, GitHub renders the new heading with literal brackets. Finally,
   confirm `docs/src/release-notes.md` has a `## vX.Y.Z` section, with upgrade
   steps where the release needs any, and that its issue link definitions cover
   every `[#N]` it cites.
6. Dry-run package every published crate in **one** invocation, which packages
   and verifies them in dependency order:

   ```bash
   cargo package --locked -p musefs-db -p musefs-format -p musefs-core \
     -p musefs-fuse -p musefs-cli -p musefs
   ```

   Packaged one at a time, every crate after `musefs-db` fails while the new
   version is not yet on crates.io: a lone `cargo package` resolves its
   siblings from the registry, not the workspace. The dry run catches packaging
   errors but **not** the cross-crate index-propagation problem at publish
   time; that is handled in-workflow (next section).
7. Commit, e.g. `git commit -am "release: vX.Y.Z"`.

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
2. `build` — cross-compiles the six target binaries.
3. `smoke` — runs the binary smoke on each target: natively (host or Alpine)
   for x86_64 and aarch64, and emulated for riscv64, whose legs do not block
   the release.
4. `publish` — publishes crates in dependency order. For each crate it **skips**
   the publish if `name@version` already resolves from the crates.io index, then
   **waits** for that version to appear before publishing the next dependent
   crate (index-propagation; #163). The skip makes a whole-workflow re-run after
   a partial failure safe.
5. `release-assets` — creates/updates the GitHub Release and uploads the binary
   tarballs + checksums (only after crates publishing succeeds).
6. `images` — needs `smoke` only, so it runs alongside `publish` rather than
   after it. For each of the glibc and musl variants it builds the amd64 image
   and runs the binary smoke inside it, then pushes a multi-arch
   (amd64/arm64/riscv64) manifest to GHCR under the tags
   `scripts/container_tags.py` computes: `:X.Y.Z` and `:latest` for glibc,
   `:X.Y.Z-musl` and `:musl` for musl (a prerelease version gets only the
   version-pinned tag).
7. `benchmarks` — runs `scripts/perf-release-bench.sh` independently of the
   graph above and uploads a `benchmark-snapshot-vX.Y.Z` artifact. It is
   record-only (`continue-on-error`) and never blocks the release; see
   [Benchmarks](../benchmarks.md#ci-regression-gating).

**Retry / rollback.**

- crates.io is **yank-only** — a published version cannot be un-published.
- A partial failure (e.g. crate 3 of 6 published, then a transient error) is
  recovered by **re-running the workflow**: the publish loop skips the crates
  already in the index and resumes, then runs `release-assets`. No manual
  cleanup of the published crates is needed.
- GitHub asset upload is idempotent (`gh release upload --clobber`), so re-runs
  re-upload safely.
- A CI job that fails on the tag for a reason outside the tree (a runner or
  mirror outage, the FreeBSD VM image download returning a 5xx) is recovered
  without re-tagging:
  1. Re-run the tag's CI run: `gh run rerun <ci-run-id> --failed`. That re-runs
     the failed jobs and everything that needs them, `ci-ok` included. Do the
     same for `coverage.yml` if `coverage-ok` failed.
  2. Wait for the re-run's `ci-ok` (and `coverage-ok`) to finish green. Order
     matters here: while a re-run is still going, the newest *completed*
     `ci-ok` is the failed one, and the gate fails as soon as it sees it.
  3. Re-run the release: `gh run rerun <release-run-id> --failed`. The `gate`
     job ignores check-runs that started before the release run was
     *created*, which a re-run does not change, so the fresh `ci-ok` counts.
     Its 45-minute wait starts over with each attempt.

  Don't delete and re-push the tag to force a clean run: it starts the whole
  matrix again, FreeBSD leg included, and gains nothing a re-run doesn't.

**Post-release verification.**

1. `cargo install musefs` (or `cargo install musefs --version X.Y.Z`) from a
   clean machine/container.
2. Download a release tarball and verify its checksum:
   `sha256sum -c musefs-X.Y.Z-<triple>.tar.gz.sha256`.
3. Confirm all six target tarballs + `.sha256` files are attached to the
   GitHub Release.
4. Confirm the container tags exist on GHCR, e.g.
   `docker manifest inspect ghcr.io/sohex/musefs:X.Y.Z` and
   `ghcr.io/sohex/musefs:X.Y.Z-musl` (plus `:latest` and `:musl` for a stable
   release).

**The Lidarr gate.** The Lidarr real-instance e2e (`lidarr-e2e.yml`) gates the
Python `py-v*` release, not this Rust flow. A release that bumps both the Rust
workspace and the `contrib/` packages cuts both tags, `vX.Y.Z` and
`py-vX.Y.Z`, so the Lidarr e2e runs as part of it.

## PRs & commits

- Conventional-style subjects (`fix(format): …`, `docs: …`, `ci: …`), scoped
  and imperative.
- `main` is protected by required status checks: the `ci-ok` and
  `coverage-ok` aggregator jobs must pass. Docs-only changes skip the
  expensive jobs at the *job* level — the aggregators still report.
- Three more workflows run on PRs, filtered by path and not required checks:
  the fuzz smoke (`fuzz.yml`: `musefs-format/**`, `fuzz/**`, the workflow),
  the in-diff mutation gate (`mutants.yml`: `musefs-db/**`, `musefs-core/**`,
  `musefs-format/**`, `scripts/mutants.sh`, `.cargo/mutants.toml`,
  `scripts/check_mutant_anchors.py`, the workflow), and the security audit
  (`audit.yml`: `**/Cargo.toml`, `Cargo.lock`, `fuzz/Cargo.lock`, the
  workflow).
- Benchmark results, when a change warrants them, are recorded in
  [Benchmarks](../benchmarks.md).

### Before you push

The pre-commit hook already gates every commit on `ruff`, and on fmt, clippy
and the workspace tests unless every staged path is a doc. `shellcheck` and
`yamllint` run when a shell or YAML file is staged, and the mutant-anchor drift
guard when the mutants config, its check script, or a
`musefs-core`/`musefs-format` source file is staged. What it does **not** run — check the ones your change
triggers:

- **Logic changes** → the [in-diff mutation gate](testing.md#mutation-testing). It is CI
  parity, not optional polish.
- **`musefs-format`, `musefs-core` or `musefs-db` API changes** →
  `cargo +nightly fuzz build`; the `fuzz/` crate is outside the workspace, so
  nothing else compiles it locally, and CI's fuzz smoke only triggers on
  `musefs-format/**` and `fuzz/**`
  ([coverage-guided fuzzing](testing.md#coverage-guided-fuzzing)).
- **`musefs-db` schema changes** → regenerate and re-vendor the Python schema
  mirror ([Python plugins](plugins.md#python-plugins-contrib)).
- **Picard plugin changes** → make sure the real-Picard tests actually ran
  rather than silently skipped ([gotchas](plugins.md#python-plugins-contrib)).
- **FUSE/mount-surface changes** → run the `--ignored` e2e suite locally
  ([Build & test](setup.md#build--test)); the FreeBSD CI leg only runs on PRs that
  touch that surface.
