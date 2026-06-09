# Python packages: PyPI release lifecycle

**Status:** design
**Date:** 2026-06-09

## Context

The Rust crates have a clean, automated release lifecycle: one workspace
version in the root `Cargo.toml`, a manual version bump + `CHANGELOG.md`
update, a `v*` git tag that triggers `.github/workflows/release.yml`, a
gate verifying the tag matches the workspace version, then `cargo publish`
of the six public crates in dependency order. Full CI runs on the tag.

The four Python packages under `contrib/` have **no equivalent**. They sit
at an independent `0.1.0`, are pure-Python setuptools projects, and have no
publish automation. Downstream users (and our own beets/Lidarr CI jobs)
cannot `pip install` them — the CI jobs install `python-musefs` from a local
checkout as a workaround.

This design gives the Python packages a PyPI release lifecycle that mirrors
the crates' lifecycle in spirit, while respecting the structural differences
between Cargo workspaces and independent Python packages.

### The packages

| Package | Dir | Publishes? | Depends on |
| --- | --- | --- | --- |
| `python-musefs` | `contrib/python-musefs` | yes (first) | — |
| `beets-musefs` | `contrib/beets` | yes | `python-musefs` |
| `lidarr-musefs` | `contrib/lidarr` | yes | `python-musefs` |
| `musefs-picard` | `contrib/picard` | **no** | vendors `python-musefs` |

`musefs-picard` is a Picard *folder* plugin: Picard cannot pip-install plugin
dependencies, so the shared library is vendored into `contrib/picard/musefs/_common/`
by `contrib/python-musefs/vendor_to_picard.py` (byte-identical, drift-guarded
by `contrib/picard/tests/test_vendor_sync.py`). It is never uploaded to PyPI,
but it **does** track the shared Python version so the vendored copy stays in
lockstep. This mirrors the Rust split where `fuzz` and `musefs-latencyfs` are
`publish = false` workspace members.

## Decisions

These were settled during brainstorming:

1. **Unified Python version, own tag.** All four `contrib/` packages share a
   single version, **decoupled from the Rust workspace version**, released on
   its own tag namespace **`py-v*`** (Rust keeps `v*`). A Python-only fix can
   ship without a Rust release.
2. **Source of truth: a bump script.** `scripts/bump-python-version.py` is the
   single entry point that rewrites the version everywhere. No setuptools
   dynamic-version machinery.
3. **Trusted Publishing (OIDC).** No long-lived PyPI tokens in GitHub secrets.
4. **Real test gate before publish.** The `py-v*` workflow runs the four
   Python CI checks as a `needs:` dependency before any upload. (This is
   stricter than the Rust `release.yml`, which runs concurrently with CI
   rather than gating on it — chosen because the Python suites are fast.)
5. **Separate changelog.** A dedicated `contrib/CHANGELOG.md`, referenced from
   the top of the root `CHANGELOG.md`.
6. **Bump only.** The bump script rewrites files; tagging and committing are
   manual.

## The bump script

`scripts/bump-python-version.py <new-version>` is the single source of truth
for the shared Python version. Invoked as e.g. `bump-python-version.py 0.2.0`,
it performs **only file edits** (no git operations):

1. Rewrites `version = "X"` in all four `pyproject.toml`:
   `contrib/python-musefs`, `contrib/beets`, `contrib/lidarr`, `contrib/picard`.
2. Rewrites `__version__ = "X"` in the packages that carry one:
   `contrib/python-musefs/src/musefs_common/__init__.py` and
   `contrib/lidarr/src/musefs_lidarr/__init__.py`.
3. Bumps the dependency floor `python-musefs>=X` in the `beets` and `lidarr`
   `pyproject.toml`. The pin is a **floor** (`>=X`), not `==X` — the dependents
   require *at least* the contract they were built against, matching the
   repo's existing `>=0.1.0` style.
4. Re-runs the vendor step (`contrib/python-musefs/vendor_to_picard.py`) so the
   Picard vendored `_common/` copy — including its `__version__` — stays
   byte-identical to the canonical library. `test_vendor_sync.py` then passes.

The script validates that the argument is a valid PEP 440 version and that all
target files were actually updated (fails loudly if a `version = ` line it
expected to find is missing — catching format drift in a `pyproject.toml`).

It does **not** touch `contrib/CHANGELOG.md`, create a commit, or create a tag.
Those steps are manual, performed by the releaser after reviewing the diff.

## Release workflow

New file `.github/workflows/release-python.yml`, separate from the Rust
`release.yml`:

```yaml
on:
  push:
    tags:
      - 'py-v*'
```

### Job 1 — version gate

The analog of the Rust grep gate, generalized across the published packages.
Strip the `py-v` prefix from `GITHUB_REF_NAME`, then for each of the three
publishable `pyproject.toml` files extract its `version` and assert it equals
the tag version. Any mismatch is a hard `::error::` + `exit 1`. (Picard's
version is checked too for consistency even though it isn't published.)

### Job 2 — test gate (`needs: version-gate`)

Run the existing four Python CI check sequences from `ci.yml` — ruff lint +
format check and `pytest` for `python-musefs`, `beets`, `lidarr`, and `picard`.
These run with the same environment setup the CI jobs already use (Picard needs
the system Picard package + PyQt5 on `ubuntu-24.04`; beets/lidarr install
`python-musefs` from the local checkout first). Default pytest markers apply, so
`musefs_bin`/`e2e` integration tests stay opt-in, exactly as in CI.

The publish job `needs:` this, so a red Python suite blocks the release.

### Job 3 — build, check, publish (`needs: test-gate`)

`permissions: { id-token: write }` for OIDC. For each publishable package, in
order `python-musefs` → `beets-musefs` → `lidarr-musefs`:

1. `python -m build` (sdist + wheel) in the package directory.
2. `twine check dist/*`.
3. Upload via `pypa/gh-action-pypi-publish` (Trusted Publishing).

PyPI does not resolve dependencies at upload time, so strict ordering is a
courtesy to users installing immediately after a release, not a hard
requirement — but we keep `python-musefs` first regardless. Each package
uploads only its own `dist/` to avoid cross-contamination.

## Package metadata prep

The current `pyproject.toml` files are thin (name, version, description,
python requirement, deps). Before the first publish, add to all four — sourced
to match the Rust workspace metadata (MIT license,
`repository = https://github.com/Sohex/musefs`):

- **License (PEP 639 / setuptools ≥77 form).** Set `license = "MIT"` as an SPDX
  *expression string* and `license-files = ["LICENSE"]`. Do **not** also add a
  `License :: OSI Approved :: MIT License` trove classifier — recent setuptools
  treats an SPDX license expression and a license classifier as mutually
  exclusive and will error. The `classifiers` list below therefore carries
  Python-version / topic / audience entries only, no license classifier.
- **A per-package `LICENSE` file.** `python -m build` runs inside each package
  directory and an sdist can only include files within that directory, so it
  cannot reach the repo-root `LICENSE`. Each of the four package dirs needs its
  own `LICENSE` (a copy of the root MIT text) for `license-files` to resolve and
  for the sdist to carry it. The implementation plan must create these four
  files (and note them so they don't drift from the root license).
- `authors`
- `[project.urls]` — Homepage / Repository / Issues
- PyPI `classifiers` — Python versions, topic, intended audience, development
  status (no license classifier, per the license note above)
- `readme = "README.md"` — all four packages **already have** a `README.md`;
  this is just wiring it into `[project]`, not authoring new files.

This is the PyPI equivalent of the per-crate `description` / `license` /
`repository` metadata the crates already carry. `musefs-picard` gets the same
metadata for consistency even though it is not uploaded.

## Changelog & docs

- **New `contrib/CHANGELOG.md`** in Keep-a-Changelog format (same structure as
  the root `CHANGELOG.md`), tracking the unified Python version. Entries are
  added under `## [Unreleased]` during development and promoted to a dated
  version on release — same manual flow as the Rust changelog.
- **Root `CHANGELOG.md`** gains a one-line reference near the top pointing at
  `contrib/CHANGELOG.md` for the Python packages, so a reader of the main
  changelog discovers the separate cadence.
- **`CONTRIBUTING.md`** gains a short "Releasing the Python packages" section
  documenting the flow: run `bump-python-version.py`, update
  `contrib/CHANGELOG.md`, review the diff, commit, push a `py-v<version>` tag,
  and let `release-python.yml` publish. This mirrors how the Rust release is
  documented.

## One-time PyPI setup (out of band, before first tag)

Trusted Publishing fails until the publisher exists on PyPI, so before the
first `py-v*` tag the releaser must, for **each** of the three published
packages:

1. Register/reserve the project name on PyPI (`python-musefs`, `beets-musefs`,
   `lidarr-musefs`).
2. Configure a Trusted Publisher tied to: this GitHub repo, workflow filename
   `release-python.yml`, and the chosen environment name (if an environment is
   used).

This pre-flight is documented as an explicit checklist in the CONTRIBUTING
release section.

## Out of scope / non-goals

- **No native/wheel build complexity.** All packages are pure Python; no
  maturin/PyO3, so no per-platform wheel matrix.
- **No TestPyPI dry-run stage.** The Rust release has no dry-run; `twine check`
  + the test gate are the verification. (Can be added later if desired.)
- **No automated changelog generation** — manual, matching the Rust side.
- **No coupling of the Python version to the Rust version.** Deliberately
  decoupled per decision 1.
- **No change to how the Rust crates release.** `release.yml` and the `v*` tag
  are untouched.

## Verification

- `scripts/bump-python-version.py 0.2.0` updates all four pyprojects, both
  `__init__.py` versions, the beets/lidarr dependency floors, and re-vendors
  Picard; `git diff` shows exactly those edits and nothing else.
- After a bump, the full Python test suite (incl. `test_vendor_sync.py`) passes
  locally, proving the vendored copy stayed in sync.
- The version-gate job fails when a `pyproject.toml` version disagrees with the
  tag (test by tagging with a deliberately mismatched version against a dry
  branch, or unit-test the gate logic).
- On a real `py-v<version>` tag (after PyPI publisher setup), the three packages
  appear on PyPI at the tagged version, `python-musefs` first; `pip install
  beets-musefs` then resolves `python-musefs` from PyPI.
- A red Python test suite on the tag blocks the publish job (test-gate `needs:`).
- `python -m build` in each package dir produces an sdist + wheel whose
  `METADATA` carries the SPDX `License: MIT` expression and bundles the
  package-local `LICENSE` and `README.md`; `twine check dist/*` passes (this is
  the cheap check that the PEP 639 license form is well-formed and not in
  conflict with a license classifier).
