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
4. Smoke-build every cross target so `jemalloc-sys` is known to compile under
   zig before tagging (the release matrix builds with the `jemalloc` feature on):

   ```bash
   for t in x86_64-unknown-linux-gnu.2.17 aarch64-unknown-linux-gnu.2.17 \
            x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do
     cargo zigbuild --release -p musefs --target "$t"
   done
   ```

   If a target cannot build `jemalloc-sys`, add `--no-default-features` to that
   matrix entry's `cargo zigbuild` in `release.yml`, rather than blocking the
   release. The Docker images `COPY` the binary this step produces (they don't
   run cargo), so the matching container inherits the opt-out automatically.

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
  [BENCHMARKS.md](../benchmarks.md).

### Before you push

The pre-commit hook already gates fmt, clippy, the workspace tests, and the
Python/shell/YAML lints on every commit. What it does **not** run — check the
ones your change triggers:

- **Logic changes** → the [in-diff mutation gate](testing.md#mutation-testing). It is CI
  parity, not optional polish.
- **Format-layer API changes** → `cargo +nightly fuzz build`; the `fuzz/`
  crate is outside the workspace, so nothing else compiles it
  ([coverage-guided fuzzing](testing.md#coverage-guided-fuzzing)).
- **`musefs-db` schema changes** → regenerate and re-vendor the Python schema
  mirror ([Python plugins](plugins.md#python-plugins-contrib)).
- **Picard plugin changes** → make sure the real-Picard tests actually ran
  rather than silently skipped ([gotchas](plugins.md#python-plugins-contrib)).
- **FUSE/mount-surface changes** → run the `--ignored` e2e suite locally
  ([Build & test](setup.md#build--test)); the FreeBSD CI leg only runs on PRs that
  touch that surface.
