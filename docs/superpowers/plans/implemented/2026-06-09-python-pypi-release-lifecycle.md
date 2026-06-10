# Python Packages PyPI Release Lifecycle Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the four `contrib/` Python packages a PyPI release lifecycle that mirrors the crates-on-crates.io lifecycle — a unified Python version (decoupled from Rust) bumped by one script, an OIDC trusted-publishing workflow on a `py-v*` tag with a real test gate, and the supporting metadata/changelog/docs.

**Architecture:** A single `scripts/bump_python_version.py` is the source of truth for the shared version across all four packages; it rewrites every `pyproject.toml` version, the `__version__` strings, the `python-musefs` dependency floors, and re-vendors python-musefs into the Picard plugin. A new `.github/workflows/release-python.yml` triggers on `py-v*` tags, gates on a tag/version consistency check and the four Python test suites, then publishes the three publishable packages to PyPI via Trusted Publishing (OIDC). `musefs-picard` tracks the version but is never uploaded (Picard folder plugin).

**Tech Stack:** Python 3.8+/setuptools ≥77 (PEP 639 license metadata), GitHub Actions, `pypa/gh-action-pypi-publish`, `python -m build`, `twine`, `pytest`, `ruff`.

**Design spec:** `docs/superpowers/specs/2026-06-09-python-pypi-release-lifecycle-design.md`

---

## File Structure

**Create:**
- `scripts/bump_python_version.py` — the version-bump source of truth (pure string-transform helpers + `bump()` + `main()`).
- `scripts/test_bump_python_version.py` — unit tests for the bump helpers and tree rewrite.
- `.github/workflows/release-python.yml` — `py-v*`-triggered version gate + test gate + publish.
- `contrib/CHANGELOG.md` — Keep-a-Changelog changelog for the Python packages.
- `contrib/python-musefs/LICENSE`, `contrib/beets/LICENSE`, `contrib/lidarr/LICENSE`, `contrib/picard/LICENSE` — per-package copies of the root MIT license (needed because `python -m build` runs inside the package dir and cannot reach the repo-root `LICENSE`).

**Modify:**
- `contrib/python-musefs/pyproject.toml`, `contrib/beets/pyproject.toml`, `contrib/lidarr/pyproject.toml`, `contrib/picard/pyproject.toml` — add PEP 639 license metadata, readme, authors, urls, classifiers; bump `setuptools` build requirement.
- `.github/workflows/ci.yml` — add `scripts/` lint + bump-script test step to the `python-musefs` job.
- `.githooks/pre-commit` — add `scripts/` to `RUFF_PATHS`.
- `CHANGELOG.md` — add a near-top reference to `contrib/CHANGELOG.md`.
- `CONTRIBUTING.md` — add a "Releasing the Python packages" section incl. the one-time PyPI trusted-publisher pre-flight checklist.

**Reuse (do not modify):**
- `contrib/python-musefs/vendor_to_picard.py` — re-run by the bump script via subprocess; keeps Picard's vendored `_common/__init__.py` (`__version__`) in lockstep.
- The four `contrib/*/README.md` files — already exist; only wired into `[project]` via `readme = "README.md"`.

---

## Task 1: gitignore build artifacts + per-package LICENSE files

Two pieces of build prep. First, `python -m build` (run in later tasks and locally) produces `dist/` and `build/` directories that are **not** currently gitignored (only `*.egg-info/` is) — they must be ignored before any build runs, or a later `git add`/`git commit -am` could sweep them in. Second, the three published sdists must each bundle a `LICENSE`, and PEP 639 `license-files = ["LICENSE"]` must resolve within the package dir — `python -m build` runs inside each package directory and an sdist can only include files within it, so the repo-root `LICENSE` is unreachable.

**Files:**
- Modify: `.gitignore`
- Create: `contrib/python-musefs/LICENSE`, `contrib/beets/LICENSE`, `contrib/lidarr/LICENSE`, `contrib/picard/LICENSE`

- [ ] **Step 1: Ignore Python build artifacts**

In `.gitignore`, under the existing `# Python packaging metadata...` / `*.egg-info/` block, add:
```gitignore

# Python sdist/wheel build output (contrib plugins)
dist/
build/
```

- [ ] **Step 2: Verify the patterns ignore the build dirs**

Run: `git check-ignore contrib/python-musefs/dist contrib/python-musefs/build`
Expected: both paths echoed back (meaning they are now ignored).

- [ ] **Step 3: Copy the root license into each package dir**

```bash
for d in python-musefs beets lidarr picard; do
  cp LICENSE "contrib/$d/LICENSE"
done
```

- [ ] **Step 4: Verify the four copies are byte-identical to the root**

Run:
```bash
for d in python-musefs beets lidarr picard; do
  cmp LICENSE "contrib/$d/LICENSE" && echo "ok contrib/$d/LICENSE"
done
```
Expected: four `ok ...` lines, no `differ` output.

- [ ] **Step 5: Commit**

```bash
git add .gitignore contrib/python-musefs/LICENSE contrib/beets/LICENSE contrib/lidarr/LICENSE contrib/picard/LICENSE
git commit -m "build: gitignore dist/build and add per-package LICENSE for PyPI sdists"
```

---

## Task 2: PyPI metadata on all four pyproject.toml

Add PEP 639 license metadata (SPDX expression string + `license-files`), `readme`, `authors`, `[project.urls]`, and `classifiers`. **Do not add a `License :: OSI Approved :: MIT License` trove classifier** — setuptools ≥77 rejects a license classifier alongside an SPDX `license` expression. Bump the `setuptools` build requirement to `>=77` (PEP 639 support) in every package.

**Files:**
- Modify: `contrib/python-musefs/pyproject.toml`, `contrib/beets/pyproject.toml`, `contrib/lidarr/pyproject.toml`, `contrib/picard/pyproject.toml`

- [ ] **Step 1: Bump the setuptools build requirement in all four**

In each of the four `pyproject.toml`, change the `[build-system]` line:
```toml
requires = ["setuptools>=61"]
```
to:
```toml
requires = ["setuptools>=77"]
```

- [ ] **Step 2: Add metadata to `contrib/python-musefs/pyproject.toml`**

Replace the `[project]` table so it reads exactly:
```toml
[project]
name = "python-musefs"
version = "0.1.0"
description = "Shared musefs SQLite-store contract for the beets and Picard plugins"
readme = "README.md"
requires-python = ">=3.8"
license = "MIT"
license-files = ["LICENSE"]
authors = [{ name = "Conor Futro" }]
classifiers = [
    "Development Status :: 4 - Beta",
    "Intended Audience :: Developers",
    "Operating System :: POSIX",
    "Programming Language :: Python :: 3",
    "Topic :: Multimedia :: Sound/Audio",
]

[project.urls]
Homepage = "https://github.com/Sohex/musefs"
Repository = "https://github.com/Sohex/musefs"
Issues = "https://github.com/Sohex/musefs/issues"
```
(Leave `[project.optional-dependencies]`, `[tool.setuptools.packages.find]`, and `[tool.pytest.ini_options]` unchanged, after the block above.)

- [ ] **Step 3: Add metadata to `contrib/beets/pyproject.toml`**

Set the `[project]` table to:
```toml
[project]
name = "beets-musefs"
version = "0.1.0"
description = "Sync beets metadata into the musefs SQLite store"
readme = "README.md"
requires-python = ">=3.9"
license = "MIT"
license-files = ["LICENSE"]
authors = [{ name = "Conor Futro" }]
dependencies = ["python-musefs>=0.1.0", "beets>=1.6"]
classifiers = [
    "Development Status :: 4 - Beta",
    "Intended Audience :: Developers",
    "Operating System :: POSIX",
    "Programming Language :: Python :: 3",
    "Topic :: Multimedia :: Sound/Audio",
]

[project.urls]
Homepage = "https://github.com/Sohex/musefs"
Repository = "https://github.com/Sohex/musefs"
Issues = "https://github.com/Sohex/musefs/issues"
```
(Leave the remaining tables unchanged.)

- [ ] **Step 4: Add metadata to `contrib/lidarr/pyproject.toml`**

Set the `[project]` table to:
```toml
[project]
name = "lidarr-musefs"
version = "0.1.0"
description = "Sync Lidarr metadata into the musefs SQLite store"
readme = "README.md"
requires-python = ">=3.9"
license = "MIT"
license-files = ["LICENSE"]
authors = [{ name = "Conor Futro" }]
dependencies = ["python-musefs>=0.1.0"]
classifiers = [
    "Development Status :: 4 - Beta",
    "Intended Audience :: Developers",
    "Operating System :: POSIX",
    "Programming Language :: Python :: 3",
    "Topic :: Multimedia :: Sound/Audio",
]

[project.urls]
Homepage = "https://github.com/Sohex/musefs"
Repository = "https://github.com/Sohex/musefs"
Issues = "https://github.com/Sohex/musefs/issues"
```
(Leave `[project.optional-dependencies]`, `[project.scripts]`, and the `[tool.*]` tables unchanged, after the block above.)

- [ ] **Step 5: Add metadata to `contrib/picard/pyproject.toml`**

Set the `[project]` table to:
```toml
[project]
name = "musefs-picard"
version = "0.1.0"
description = "Sync MusicBrainz Picard metadata into the musefs SQLite store"
readme = "README.md"
requires-python = ">=3.8"
license = "MIT"
license-files = ["LICENSE"]
authors = [{ name = "Conor Futro" }]
classifiers = [
    "Development Status :: 4 - Beta",
    "Intended Audience :: Developers",
    "Operating System :: POSIX",
    "Programming Language :: Python :: 3",
    "Topic :: Multimedia :: Sound/Audio",
]

[project.urls]
Homepage = "https://github.com/Sohex/musefs"
Repository = "https://github.com/Sohex/musefs"
Issues = "https://github.com/Sohex/musefs/issues"
```
(Leave the remaining tables unchanged.)

- [ ] **Step 6: Verify all four pyproject.toml still parse**

The system Python here is ≥3.11 (`tomllib` available). Confirm no TOML was broken by the edits — this is the only validation picard's pyproject gets, since it is never built:
```bash
for d in python-musefs beets lidarr picard; do
  python3 -c "import tomllib,sys; tomllib.load(open(sys.argv[1],'rb'))" "contrib/$d/pyproject.toml" && echo "ok contrib/$d"
done
```
Expected: four `ok ...` lines.

- [ ] **Step 7: Build + twine-check each published package to validate the metadata**

The system Python is PEP 668 externally-managed, so build/twine go in an ephemeral venv (a bare `pip install` would error). Run:
```bash
python3 -m venv /tmp/musefs-build && /tmp/musefs-build/bin/pip install -q build twine
for d in python-musefs beets lidarr; do
  rm -rf "contrib/$d/dist" "contrib/$d/build"
  /tmp/musefs-build/bin/python -m build "contrib/$d"
  /tmp/musefs-build/bin/twine check "contrib/$d"/dist/*
done
```
Expected: each `twine check` prints `PASSED`. The `METADATA` carries `License: MIT` and bundles `LICENSE` + `README.md`. If `twine check` complains about the license metadata form, the SPDX/classifier conflict (Step 2–5) or setuptools version (Step 1) is wrong.

- [ ] **Step 8: Clean the build artifacts (gitignored as of Task 1, but don't leave them lying around)**

Run:
```bash
rm -rf contrib/python-musefs/dist contrib/python-musefs/build \
       contrib/beets/dist contrib/beets/build \
       contrib/lidarr/dist contrib/lidarr/build
rm -rf /tmp/musefs-build
```

- [ ] **Step 9: Commit**

```bash
git add contrib/python-musefs/pyproject.toml contrib/beets/pyproject.toml contrib/lidarr/pyproject.toml contrib/picard/pyproject.toml
git commit -m "build: add PyPI metadata (PEP 639 license, urls, classifiers) to contrib packages"
```

---

## Task 3: Version-bump script (TDD)

`scripts/bump_python_version.py` is the single source of truth for the shared Python version. It edits files only — no git, no tagging. The module name uses underscores (importable); invoke it as `python scripts/bump_python_version.py <version>`.

**Files:**
- Create: `scripts/bump_python_version.py`
- Test: `scripts/test_bump_python_version.py`

- [ ] **Step 1: Write the failing tests**

Create `scripts/test_bump_python_version.py`:
```python
import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parent))
import bump_python_version as bp  # noqa: E402


def test_set_project_version_replaces_first_only():
    text = '[project]\nname = "x"\nversion = "0.1.0"\n'
    assert bp.set_project_version(text, "0.2.0") == '[project]\nname = "x"\nversion = "0.2.0"\n'


def test_set_project_version_missing_raises():
    with pytest.raises(ValueError):
        bp.set_project_version('[project]\nname = "x"\n', "0.2.0")


def test_set_init_version():
    assert bp.set_init_version('__version__ = "0.1.0"\n', "0.2.0") == '__version__ = "0.2.0"\n'


def test_set_dep_floor_keeps_siblings():
    text = 'dependencies = ["python-musefs>=0.1.0", "beets>=1.6"]'
    assert (
        bp.set_dep_floor(text, "0.2.0")
        == 'dependencies = ["python-musefs>=0.2.0", "beets>=1.6"]'
    )


def test_bump_rewrites_tree(tmp_path):
    files = {
        "contrib/python-musefs/pyproject.toml": '[project]\nname = "python-musefs"\nversion = "0.1.0"\n',
        "contrib/beets/pyproject.toml": '[project]\nversion = "0.1.0"\ndependencies = ["python-musefs>=0.1.0", "beets>=1.6"]\n',
        "contrib/lidarr/pyproject.toml": '[project]\nversion = "0.1.0"\ndependencies = ["python-musefs>=0.1.0"]\n',
        "contrib/picard/pyproject.toml": '[project]\nname = "musefs-picard"\nversion = "0.1.0"\n',
        "contrib/python-musefs/src/musefs_common/__init__.py": '__version__ = "0.1.0"\n',
        "contrib/lidarr/src/musefs_lidarr/__init__.py": '__version__ = "0.1.0"\n',
    }
    for rel, content in files.items():
        p = tmp_path / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(content)

    bp.bump("0.2.0", root=tmp_path, run_vendor=False)

    for rel in bp.PYPROJECTS:
        assert 'version = "0.2.0"' in (tmp_path / rel).read_text()
    for rel in bp.INIT_FILES:
        assert '__version__ = "0.2.0"' in (tmp_path / rel).read_text()
    for rel in bp.DEPENDENTS:
        assert "python-musefs>=0.2.0" in (tmp_path / rel).read_text()


def test_main_rejects_bad_version(capsys):
    assert bp.main(["not a version"]) == 2
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `python -m pytest scripts/test_bump_python_version.py -v`
Expected: collection error / FAIL — `ModuleNotFoundError: No module named 'bump_python_version'`.

- [ ] **Step 3: Write the bump script**

Create `scripts/bump_python_version.py`:
```python
#!/usr/bin/env python3
"""Bump the shared version of all contrib/ Python packages.

Single source of truth for the unified Python package version (decoupled from
the Rust workspace version). Rewrites every pyproject.toml version, the
__version__ in the packages that carry one, the python-musefs dependency floor
in the dependents, and re-vendors python-musefs into the Picard plugin so the
vendored copy's __version__ stays in lockstep. Does not commit or tag.

Usage: python scripts/bump_python_version.py <version>
"""

from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent

# Every package's pyproject [project] version is bumped (incl. Picard, which is
# not published but vendors python-musefs and must track the same number).
PYPROJECTS = [
    "contrib/python-musefs/pyproject.toml",
    "contrib/beets/pyproject.toml",
    "contrib/lidarr/pyproject.toml",
    "contrib/picard/pyproject.toml",
]
# Packages that carry a canonical __version__ string in code.
INIT_FILES = [
    "contrib/python-musefs/src/musefs_common/__init__.py",
    "contrib/lidarr/src/musefs_lidarr/__init__.py",
]
# Packages that pin python-musefs and need their dependency floor bumped.
DEPENDENTS = [
    "contrib/beets/pyproject.toml",
    "contrib/lidarr/pyproject.toml",
]
VENDOR_SCRIPT = "contrib/python-musefs/vendor_to_picard.py"

_VERSION_RE = re.compile(r'(?m)^version = "[^"]*"')
_INIT_VERSION_RE = re.compile(r'(?m)^__version__ = "[^"]*"')
_DEP_FLOOR_RE = re.compile(r"python-musefs>=[^\"]*")
_PEP440_RE = re.compile(r"^[0-9]+(\.[0-9]+)*([abrc][0-9]+|\.[a-z0-9.]+)?$")


def set_project_version(text: str, version: str) -> str:
    new, n = _VERSION_RE.subn(f'version = "{version}"', text, count=1)
    if n != 1:
        raise ValueError("no [project] version line found")
    return new


def set_init_version(text: str, version: str) -> str:
    new, n = _INIT_VERSION_RE.subn(f'__version__ = "{version}"', text, count=1)
    if n != 1:
        raise ValueError("no __version__ line found")
    return new


def set_dep_floor(text: str, version: str) -> str:
    new, n = _DEP_FLOOR_RE.subn(f"python-musefs>={version}", text)
    if n < 1:
        raise ValueError("no python-musefs>= dependency found")
    return new


def bump(version: str, root: Path = REPO_ROOT, run_vendor: bool = True) -> None:
    for rel in PYPROJECTS:
        p = root / rel
        p.write_text(set_project_version(p.read_text(), version))
    for rel in INIT_FILES:
        p = root / rel
        p.write_text(set_init_version(p.read_text(), version))
    for rel in DEPENDENTS:
        p = root / rel
        p.write_text(set_dep_floor(p.read_text(), version))
    if run_vendor:
        subprocess.run([sys.executable, str(root / VENDOR_SCRIPT)], check=True)


def main(argv: list[str]) -> int:
    if len(argv) != 1 or not _PEP440_RE.match(argv[0]):
        print("usage: bump_python_version.py <version>", file=sys.stderr)
        return 2
    bump(argv[0])
    print(f"bumped contrib/ Python packages to {argv[0]}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `python -m pytest scripts/test_bump_python_version.py -v`
Expected: 6 passed.

- [ ] **Step 5: Verify ruff is clean on the new files**

Run: `ruff check scripts/ && ruff format --check scripts/`
Expected: no diagnostics, no reformat. (Run `ruff format scripts/` first if needed, then re-stage.)

- [ ] **Step 6: End-to-end dry run against the real tree, then revert**

Run:
```bash
python scripts/bump_python_version.py 0.2.0
git diff --stat
python -m pytest contrib/picard/tests/test_vendor_sync.py -v
git checkout -- contrib/
```
Expected: the diff touches the four pyprojects, the two `__init__.py`, and the Picard vendored `_common/__init__.py`; `test_vendor_sync.py` passes (proving the re-vendor kept the copy in sync); `git checkout` restores everything.

- [ ] **Step 7: Add `scripts/` (and the pre-existing `contrib/lidarr/` gap) to the pre-commit ruff paths**

The installed hook (`.git/hooks/pre-commit` → `.githooks/pre-commit`) reads `RUFF_PATHS` from disk at commit time, so editing it now makes the *next* commit lint `scripts/`. In `.githooks/pre-commit`, change:
```bash
RUFF_PATHS=(contrib/beets/ contrib/picard/ contrib/python-musefs/ tests/interop/)
```
to:
```bash
RUFF_PATHS=(contrib/beets/ contrib/lidarr/ contrib/picard/ contrib/python-musefs/ scripts/ tests/interop/)
```
(`contrib/lidarr/` was already linted in CI but missing from this list — closing it here since we're editing this exact line. `scripts/` is new.)

- [ ] **Step 8: Commit (the hook edit lands with the script it lints)**

```bash
git add .githooks/pre-commit scripts/bump_python_version.py scripts/test_bump_python_version.py
git commit -m "build: add bump_python_version.py to bump all contrib package versions"
```
Expected: the pre-commit hook runs `ruff check`/`ruff format --check` over `scripts/` (and lidarr) and passes — confirming the script is lint-clean on the commit that introduces it.

---

## Task 4: Run the bump script lint + test in CI

Lint `scripts/` and run the bump-script unit test in the `python-musefs` CI job. (The pre-commit hook was already pointed at `scripts/` in Task 3.) The `changes` job sets `src=true` for any non-docs path (including `scripts/`), so the `python-musefs` job already runs when the bump script changes.

**Files:**
- Modify: `.github/workflows/ci.yml` (`python-musefs` job)

- [ ] **Step 1: Lint `scripts/` and run the bump-script test in the `python-musefs` CI job**

In `.github/workflows/ci.yml`, in the `python-musefs` job, change the `Lint` step:
```yaml
      - name: Lint
        run: |
          ruff check contrib/python-musefs/
          ruff format --check contrib/python-musefs/
```
to:
```yaml
      - name: Lint
        run: |
          ruff check contrib/python-musefs/ scripts/
          ruff format --check contrib/python-musefs/ scripts/
```
and add a new step immediately after the existing `Test` step in that job:
```yaml
      - name: Test bump script
        run: python -m pytest scripts/test_bump_python_version.py -v
```

- [ ] **Step 2: Verify the lint passes locally**

Run:
```bash
ruff check contrib/python-musefs/ scripts/ && ruff format --check contrib/python-musefs/ scripts/
```
Expected: clean.

- [ ] **Step 3: Verify the workflow YAML parses**

Run: `python -c "import yaml; yaml.safe_load(open('.github/workflows/ci.yml'))"`
Expected: no output (valid YAML). If `actionlint` is installed, also run `actionlint .github/workflows/ci.yml`.

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: lint scripts/ and run the bump-script test in the python-musefs job"
```

---

## Task 5: Python packages changelog

A dedicated `contrib/CHANGELOG.md` (Keep-a-Changelog), referenced from the top of the root `CHANGELOG.md`, since the Python cadence is decoupled from the Rust release cadence.

**Files:**
- Create: `contrib/CHANGELOG.md`
- Modify: `CHANGELOG.md` (near the top)

- [ ] **Step 1: Create `contrib/CHANGELOG.md`**

```markdown
# Python packages changelog

Changelog for the musefs `contrib/` Python packages — `python-musefs`,
`beets-musefs`, `lidarr-musefs`, and the (unpublished) Picard plugin. These
share a single version, released on `py-v*` tags and decoupled from the Rust
crate version tracked in the [root CHANGELOG](../CHANGELOG.md).

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and these packages adhere to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- PyPI distribution: `python-musefs`, `beets-musefs`, and `lidarr-musefs` are
  published to PyPI on `py-v*` tags via a trusted-publishing release workflow.
```

- [ ] **Step 2: Reference the Python changelog from the root `CHANGELOG.md`**

In `CHANGELOG.md`, immediately after the introductory paragraph (the line ending `…Semantic Versioning](https://semver.org/spec/v2.0.0.html).`) and before `## [Unreleased]`, insert:
```markdown

> The `contrib/` Python packages have their own decoupled version and changelog:
> see [contrib/CHANGELOG.md](contrib/CHANGELOG.md).
```

- [ ] **Step 3: Commit**

```bash
git add contrib/CHANGELOG.md CHANGELOG.md
git commit -m "docs: add contrib/CHANGELOG.md for the Python packages"
```

---

## Task 6: Release workflow (`release-python.yml`)

A new workflow triggered by `py-v*` tags: a version gate (tag == every contrib package version), a test gate mirroring the four `ci.yml` Python jobs (minus the `changes` filter — a tag always runs them), then publish of the three publishable packages via OIDC trusted publishing. The four test jobs are duplicated from `ci.yml` for a self-contained release workflow, consistent with how `release.yml` duplicates its own setup; a future refactor could extract a `workflow_call` reusable workflow.

> **Drift note:** these four test jobs are hand-copied from `ci.yml` with no shared source. If `ci.yml`'s Python jobs change (e.g. their lint paths or setup steps), the corresponding jobs here must be updated in lockstep. The `test-python-musefs` job below already includes the `scripts/` lint + bump-script test added to `ci.yml` in Task 4.

**Files:**
- Create: `.github/workflows/release-python.yml`

- [ ] **Step 1: Look up the pinned SHA for the publish action**

The repo pins actions by **commit** SHA. Use the `commits/{ref}` endpoint, which dereferences a tag to the commit it points at — do **not** use `git/refs/tags/...` `.object.sha`, which for an *annotated* tag returns the tag-object SHA (not a commit), and GitHub Actions cannot resolve `uses: …@<tag-object-sha>` so the publish step would fail at release time:
```bash
LATEST="$(gh api repos/pypa/gh-action-pypi-publish/releases/latest --jq '.tag_name')"
gh api "repos/pypa/gh-action-pypi-publish/commits/$LATEST" --jq '.sha'   # commit SHA to pin
```
Use that commit SHA in Step 2 in place of `PINNED_PYPI_PUBLISH_SHA`, optionally with a trailing `# <tag>` comment. Sanity-check that the value is a commit, not a tag object: `gh api repos/pypa/gh-action-pypi-publish/commits/<sha> --jq '.sha'` must echo the same SHA back (it 422s for a tag-object SHA).

- [ ] **Step 2: Create `.github/workflows/release-python.yml`**

```yaml
name: Release (Python)

on:
  push:
    tags:
      - 'py-v*'

concurrency:
  group: release-python-${{ github.ref }}
  cancel-in-progress: false

permissions:
  contents: read

jobs:
  version-gate:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@df4cb1c069e1874edd31b4311f1884172cec0e10
        with:
          persist-credentials: false
      - name: Verify tag matches every contrib package version
        run: |
          set -euo pipefail
          TAG_VERSION="${GITHUB_REF_NAME#py-v}"
          echo "tag=$TAG_VERSION"
          fail=0
          for f in \
            contrib/python-musefs/pyproject.toml \
            contrib/beets/pyproject.toml \
            contrib/lidarr/pyproject.toml \
            contrib/picard/pyproject.toml; do
            V="$(grep -m1 '^version = ' "$f" | sed -E 's/version = "([^"]+)"/\1/')"
            echo "$f => $V"
            if [ "$V" != "$TAG_VERSION" ]; then
              echo "::error file=$f::version $V does not match tag $TAG_VERSION"
              fail=1
            fi
          done
          [ "$fail" -eq 0 ]

  test-python-musefs:
    needs: version-gate
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@df4cb1c069e1874edd31b4311f1884172cec0e10
        with:
          persist-credentials: false
      - uses: actions/setup-python@a309ff8b426b58ec0e2a45f0f869d46889d02405
        with:
          python-version: '3.x'
      - name: Install Ruff
        run: pip install ruff
      - name: Lint
        run: |
          ruff check contrib/python-musefs/ scripts/
          ruff format --check contrib/python-musefs/ scripts/
      - name: Install library
        run: pip install -e "contrib/python-musefs[test]"
      - name: Test
        run: python -m pytest contrib/python-musefs/tests -v
      - name: Test bump script
        run: python -m pytest scripts/test_bump_python_version.py -v

  test-beets:
    needs: version-gate
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@df4cb1c069e1874edd31b4311f1884172cec0e10
        with:
          persist-credentials: false
      - uses: actions/setup-python@a309ff8b426b58ec0e2a45f0f869d46889d02405
        with:
          python-version: '3.x'
      - name: Install Ruff
        run: pip install ruff
      - name: Lint
        run: ruff check contrib/beets/ tests/interop/
      - name: Format check
        run: ruff format --check contrib/beets/ tests/interop/
      - name: Install python-musefs (local, unpublished dependency)
        run: pip install -e contrib/python-musefs
      - name: Install beets
        run: pip install -e "contrib/beets[test]"
      - name: Test
        run: python -m pytest contrib/beets/tests -v

  test-lidarr:
    needs: version-gate
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@df4cb1c069e1874edd31b4311f1884172cec0e10
        with:
          persist-credentials: false
      - uses: actions/setup-python@a309ff8b426b58ec0e2a45f0f869d46889d02405
        with:
          python-version: '3.x'
      - name: Install Ruff
        run: pip install ruff
      - name: Lint
        run: |
          ruff check contrib/lidarr/
          ruff format --check contrib/lidarr/
      - name: Install python-musefs (local, unpublished dependency)
        run: pip install -e contrib/python-musefs
      - name: Install Lidarr integration
        run: pip install -e "contrib/lidarr[test]"
      - name: Test
        run: python -m pytest contrib/lidarr/tests -v

  test-picard:
    needs: version-gate
    runs-on: ubuntu-24.04
    steps:
      - uses: actions/checkout@df4cb1c069e1874edd31b4311f1884172cec0e10
        with:
          persist-credentials: false
      - name: Install Picard (system Python + PyQt5)
        run: sudo apt-get update && sudo apt-get install -y picard
      - name: Verify Picard module path
        run: test -d /usr/lib/picard/picard || { echo "::error::picard package not at /usr/lib/picard; PYTHONPATH is stale"; exit 1; }
      - uses: astral-sh/setup-uv@fac544c07dec837d0ccb6301d7b5580bf5edae39
      - name: Create venv on the system Python
        run: uv venv --system-site-packages --python "$(which python3)"
      - name: Install plugin test deps + ruff
        run: uv pip install -e 'contrib/picard[test]' ruff
      - name: Lint
        run: |
          .venv/bin/ruff check contrib/picard/
          .venv/bin/ruff format --check contrib/picard/
      - name: Test (real Picard, headless)
        env:
          PYTHONPATH: /usr/lib/picard
          QT_QPA_PLATFORM: offscreen
        run: .venv/bin/python -m pytest contrib/picard/tests -v

  publish:
    needs: [test-python-musefs, test-beets, test-lidarr, test-picard]
    runs-on: ubuntu-latest
    environment: pypi
    permissions:
      id-token: write
    steps:
      - uses: actions/checkout@df4cb1c069e1874edd31b4311f1884172cec0e10
        with:
          persist-credentials: false
      - uses: actions/setup-python@a309ff8b426b58ec0e2a45f0f869d46889d02405
        with:
          python-version: '3.x'
      - name: Install build tooling
        run: pip install build twine
      # python-musefs first (beets/lidarr depend on it).
      - name: Build python-musefs
        run: python -m build contrib/python-musefs
      - name: Check python-musefs
        run: twine check contrib/python-musefs/dist/*
      - name: Publish python-musefs
        uses: pypa/gh-action-pypi-publish@PINNED_PYPI_PUBLISH_SHA
        with:
          packages-dir: contrib/python-musefs/dist
      - name: Build beets-musefs
        run: python -m build contrib/beets
      - name: Check beets-musefs
        run: twine check contrib/beets/dist/*
      - name: Publish beets-musefs
        uses: pypa/gh-action-pypi-publish@PINNED_PYPI_PUBLISH_SHA
        with:
          packages-dir: contrib/beets/dist
      - name: Build lidarr-musefs
        run: python -m build contrib/lidarr
      - name: Check lidarr-musefs
        run: twine check contrib/lidarr/dist/*
      - name: Publish lidarr-musefs
        uses: pypa/gh-action-pypi-publish@PINNED_PYPI_PUBLISH_SHA
        with:
          packages-dir: contrib/lidarr/dist
```

- [ ] **Step 3: Replace the pinned-SHA placeholder and confirm none survive**

Replace all three `PINNED_PYPI_PUBLISH_SHA` occurrences with the SHA from Step 1, then guard against a missed one (the YAML parses fine either way, so this is the only thing that catches a leftover placeholder before tag time):
```bash
! grep -n PINNED_PYPI_PUBLISH_SHA .github/workflows/release-python.yml
```
Expected: no matches (the command exits 0 because `grep` found nothing).

- [ ] **Step 4: Validate the workflow**

Run:
```bash
python -c "import yaml; yaml.safe_load(open('.github/workflows/release-python.yml'))"
```
Expected: no output. If `actionlint` is available: `actionlint .github/workflows/release-python.yml` (expect no findings).

- [ ] **Step 5: Sanity-check the version gate locally**

Run the gate's core logic against the working tree with a deliberately wrong tag and confirm it fails, then with the real version and confirm it passes:
```bash
for TAG in 9.9.9 0.1.0; do
  echo "== tag $TAG =="
  for f in contrib/python-musefs/pyproject.toml contrib/beets/pyproject.toml contrib/lidarr/pyproject.toml contrib/picard/pyproject.toml; do
    V="$(grep -m1 '^version = ' "$f" | sed -E 's/version = "([^"]+)"/\1/')"
    [ "$V" = "$TAG" ] && echo "ok $f" || echo "MISMATCH $f ($V)"
  done
done
```
Expected: tag `9.9.9` → four `MISMATCH` lines; tag `0.1.0` → four `ok` lines.

- [ ] **Step 6: Commit**

```bash
git add .github/workflows/release-python.yml
git commit -m "ci: add release-python.yml to publish contrib packages to PyPI on py-v* tags"
```

---

## Task 7: Release docs + PyPI pre-flight checklist

Document the `py-v*` release flow and the one-time trusted-publisher setup in `CONTRIBUTING.md`, mirroring how the Rust release is documented.

**Files:**
- Modify: `CONTRIBUTING.md`

- [ ] **Step 1: Add a "Releasing the Python packages" section**

Append to `CONTRIBUTING.md` (or place near any existing release/contrib section) this section:
```markdown
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
```

- [ ] **Step 2: Verify the doc renders / no broken intra-repo references**

Run: `grep -n "release-python.yml\|bump_python_version.py\|contrib/CHANGELOG.md" CONTRIBUTING.md`
Expected: the three references appear; the referenced files exist (created in earlier tasks).

- [ ] **Step 3: Commit**

```bash
git add CONTRIBUTING.md
git commit -m "docs: document the py-v* Python package release flow"
```

---

## Self-Review

**Spec coverage** (against `2026-06-09-python-pypi-release-lifecycle-design.md`):
- §Scope / what publishes (3 published, Picard tracked-not-published) → Task 6 version gate covers all four, publish covers three; Task 2/Task 3 include Picard.
- §Version model (unified, `py-v*`, decoupled) → Task 6 trigger + gate; Task 3 bump script.
- §Bump script (rewrites versions, `__version__`, dep floor, re-vendors, no git) → Task 3.
- §Release workflow (version gate, test gate, build+check, OIDC publish, order) → Task 6.
- §Metadata prep (PEP 639 license, license-files, per-package LICENSE, readme, authors, urls, classifiers, no license classifier) → Task 1 + Task 2.
- §Changelog & docs (separate `contrib/CHANGELOG.md` + root reference; CONTRIBUTING section) → Task 5 + Task 7.
- §One-time PyPI setup → Task 7 pre-flight checklist.

**Placeholder scan:** `PINNED_PYPI_PUBLISH_SHA` is an intentional lookup-and-replace handled explicitly in Task 6 Steps 1 & 3, not a leftover TODO. No other placeholders.

**Type/name consistency:** bump-script symbols (`set_project_version`, `set_init_version`, `set_dep_floor`, `bump`, `PYPROJECTS`, `INIT_FILES`, `DEPENDENTS`) are identical between the test (Task 3 Step 1), the implementation (Step 3), and the CI step (Task 4). Workflow job names (`version-gate`, `test-python-musefs`, `test-beets`, `test-lidarr`, `test-picard`, `publish`) and `needs:` references are consistent within Task 6.

---

## End-to-end verification

After all tasks:
1. `ruff check contrib/ scripts/ tests/interop/ && ruff format --check contrib/ scripts/ tests/interop/` — clean.
2. `python -m pytest scripts/test_bump_python_version.py contrib/python-musefs/tests contrib/beets/tests contrib/lidarr/tests -v` — all pass (Picard suite needs system Picard + Qt, per its CI job).
3. Metadata build check (in a venv — system Python is PEP 668 managed): `python3 -m venv /tmp/musefs-build && /tmp/musefs-build/bin/pip install -q build twine && for d in python-musefs beets lidarr; do /tmp/musefs-build/bin/python -m build contrib/$d && /tmp/musefs-build/bin/twine check contrib/$d/dist/*; done` — all `PASSED`; then `rm -rf contrib/*/dist contrib/*/build /tmp/musefs-build`.
4. Dry-run bump: `python scripts/bump_python_version.py 0.2.0 && python -m pytest contrib/picard/tests/test_vendor_sync.py -v && git checkout -- contrib/` — vendor stays in sync, tree restored.
5. Workflow YAML parses: `python -c "import yaml; yaml.safe_load(open('.github/workflows/release-python.yml'))"`.
6. After merge + PyPI publisher setup, push a real `py-vX.Y.Z` tag and confirm the three packages appear on PyPI at that version (python-musefs first), then `pip install beets-musefs` resolves `python-musefs` from PyPI.
