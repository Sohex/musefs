# Release Graph Hardening Implementation Plan (#164 + #222 + #163)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Restructure `release.yml` into one ordered, fail-closed graph that publishes crates and GitHub assets only after the tagged commit's CI is green and binaries pass smoke, with crates.io index-propagation waits and idempotent re-runs.

**Architecture:** Two pure, unit-tested Python helpers under `scripts/` carry the logic that is awkward to test in YAML — `release_gate.py` (select the latest-completed required check-runs for a commit and decide pass/wait/fail) and `crates_index.py` (is a `name@version` resolvable from the crates.io sparse index). The GitHub workflow is a thin poll-and-wait loop around them. `coverage.yml` gains a `tags:['v*']` trigger so `coverage-ok` actually exists on a tagged commit.

**Tech Stack:** GitHub Actions (YAML), POSIX/bash shell, Python 3 (stdlib only — `urllib`, `json`), pytest, the GitHub `gh` CLI, cargo.

This plan is Component 1 of the release-process hardening spec:
`docs/superpowers/specs/2026-06-10-release-process-hardening-design.md`.

---

## File Structure

- Create `scripts/release_gate.py` — pure check-run selection/decision logic + a CLI that reads `gh api` JSON on stdin and exits 0 (all green) / 2 (still pending) / 1 (a required check failed).
- Create `scripts/test_release_gate.py` — pytest unit tests for the above.
- Create `scripts/crates_index.py` — `is_published(name, version)` against the sparse index + a CLI exiting 0 (published) / 3 (not yet).
- Create `scripts/test_crates_index.py` — pytest unit tests (injected fetcher; no network).
- Modify `.github/workflows/coverage.yml:2-5` — add the `tags:['v*']` trigger.
- Modify `.github/workflows/ci.yml:146-148` — run the two new script test files.
- Modify `.github/workflows/release.yml` — add the `gate` job, rewire `needs`, rewrite the publish loop to use `crates_index.py` (skip-if-present + post-publish wait), make `release-assets` depend on `[smoke, publish]`.

---

## Task 1: Add the `v*` tag trigger to coverage.yml

**Files:**
- Modify: `.github/workflows/coverage.yml:2-5`

Without this, `coverage.yml` runs only on `push: branches:[main]` and `pull_request`, so a tag push never produces a `coverage-ok` check-run on the tagged commit and the gate (Task 5) can never see it. `ci.yml:6` already has this trigger; we mirror it. The existing `changes` job treats a tag push (no usable `before` SHA) as a source change, so coverage actually runs.

- [ ] **Step 1: Add the trigger**

Change the `on:` block at the top of `.github/workflows/coverage.yml` from:

```yaml
on:
  push:
    branches: [main]
  pull_request:
```

to:

```yaml
on:
  push:
    branches: [main]
    tags: ['v*']   # release tags regenerate coverage-ok for the release gate (see release.yml)
  pull_request:
```

- [ ] **Step 2: Validate YAML parses**

Run: `python -c "import yaml,sys; yaml.safe_load(open('.github/workflows/coverage.yml'))" && echo OK`
Expected: `OK` (if PyYAML is unavailable, run `pip install pyyaml` first).

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/coverage.yml
git commit -m "ci(coverage): run coverage on v* tags so coverage-ok exists for the release gate"
```

---

## Task 2: `release_gate.py` — required-check selection logic (TDD)

**Files:**
- Create: `scripts/release_gate.py`
- Test: `scripts/test_release_gate.py`

The GitHub Checks API (`GET /repos/{owner}/{repo}/commits/{sha}/check-runs`) returns **all** check-runs for a commit across every workflow run, including re-runs. We must, for each required name, take the **latest by `completed_at`**, treat an incomplete run as "wait" (not fail), and fail only on a completed non-success conclusion.

- [ ] **Step 1: Write the failing tests**

Create `scripts/test_release_gate.py`:

```python
import json

import pytest

from release_gate import Decision, decide, latest_completed_by_name


def _run(name, status, conclusion, completed_at):
    return {
        "name": name,
        "status": status,
        "conclusion": conclusion,
        "completed_at": completed_at,
    }


def test_latest_completed_picks_newest_by_completed_at():
    runs = [
        _run("ci-ok", "completed", "failure", "2026-06-10T10:00:00Z"),
        _run("ci-ok", "completed", "success", "2026-06-10T11:00:00Z"),
    ]
    chosen = latest_completed_by_name(runs, "ci-ok")
    assert chosen["conclusion"] == "success"


def test_latest_completed_ignores_incomplete_runs():
    runs = [
        _run("ci-ok", "completed", "success", "2026-06-10T10:00:00Z"),
        _run("ci-ok", "in_progress", None, None),
    ]
    chosen = latest_completed_by_name(runs, "ci-ok")
    assert chosen["conclusion"] == "success"


def test_decide_all_success():
    runs = [
        _run("ci-ok", "completed", "success", "2026-06-10T11:00:00Z"),
        _run("coverage-ok", "completed", "success", "2026-06-10T11:05:00Z"),
    ]
    assert decide(runs, ["ci-ok", "coverage-ok"]) is Decision.PASS


def test_decide_failure_when_a_check_failed():
    runs = [
        _run("ci-ok", "completed", "success", "2026-06-10T11:00:00Z"),
        _run("coverage-ok", "completed", "failure", "2026-06-10T11:05:00Z"),
    ]
    assert decide(runs, ["ci-ok", "coverage-ok"]) is Decision.FAIL


def test_decide_wait_when_a_check_absent():
    runs = [_run("ci-ok", "completed", "success", "2026-06-10T11:00:00Z")]
    assert decide(runs, ["ci-ok", "coverage-ok"]) is Decision.WAIT


def test_decide_wait_when_a_check_still_running():
    runs = [
        _run("ci-ok", "completed", "success", "2026-06-10T11:00:00Z"),
        _run("coverage-ok", "in_progress", None, None),
    ]
    assert decide(runs, ["ci-ok", "coverage-ok"]) is Decision.WAIT


def test_cli_pass_exit_zero(capsys, tmp_path):
    payload = {
        "check_runs": [
            _run("ci-ok", "completed", "success", "2026-06-10T11:00:00Z"),
            _run("coverage-ok", "completed", "success", "2026-06-10T11:05:00Z"),
        ]
    }
    from release_gate import main

    rc = main(["--names", "ci-ok", "coverage-ok"], stdin_text=json.dumps(payload))
    assert rc == 0


def test_cli_wait_exit_two():
    payload = {"check_runs": [_run("ci-ok", "completed", "success", "2026-06-10T11:00:00Z")]}
    from release_gate import main

    rc = main(["--names", "ci-ok", "coverage-ok"], stdin_text=json.dumps(payload))
    assert rc == 2


def test_cli_fail_exit_one():
    payload = {
        "check_runs": [
            _run("ci-ok", "completed", "success", "2026-06-10T11:00:00Z"),
            _run("coverage-ok", "completed", "failure", "2026-06-10T11:05:00Z"),
        ]
    }
    from release_gate import main

    rc = main(["--names", "ci-ok", "coverage-ok"], stdin_text=json.dumps(payload))
    assert rc == 1
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `python -m pytest scripts/test_release_gate.py -v`
Expected: FAIL — `ModuleNotFoundError: No module named 'release_gate'`.

- [ ] **Step 3: Write the implementation**

Create `scripts/release_gate.py`:

```python
"""Decide whether a commit's required CI check-runs permit a release.

Pure logic + a thin CLI. The release workflow polls `gh api
.../commits/<sha>/check-runs` and pipes the JSON here; the exit code drives a
wait loop: 0 = all required checks succeeded, 2 = keep waiting (a required check
is absent or still running), 1 = a required check failed.
"""

from __future__ import annotations

import argparse
import enum
import json
import sys


class Decision(enum.Enum):
    PASS = "pass"
    WAIT = "wait"
    FAIL = "fail"


def latest_completed_by_name(runs, name):
    """Return the newest *completed* check-run with ``name``, or ``None``.

    The Checks API returns every run of a name (including re-runs); the gate
    only trusts the most recently completed one, sorted by ``completed_at``.
    """
    completed = [
        r
        for r in runs
        if r.get("name") == name
        and r.get("status") == "completed"
        and r.get("completed_at")
    ]
    if not completed:
        return None
    return max(completed, key=lambda r: r["completed_at"])


def decide(runs, names):
    """Return a :class:`Decision` for the required check ``names``."""
    saw_missing = False
    for name in names:
        chosen = latest_completed_by_name(runs, name)
        if chosen is None:
            saw_missing = True
            continue
        if chosen.get("conclusion") != "success":
            return Decision.FAIL
    return Decision.WAIT if saw_missing else Decision.PASS


def main(argv=None, stdin_text=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--names", nargs="+", required=True, help="required check-run names")
    args = parser.parse_args(argv)

    text = stdin_text if stdin_text is not None else sys.stdin.read()
    payload = json.loads(text)
    runs = payload.get("check_runs", [])

    result = decide(runs, args.names)
    if result is Decision.FAIL:
        print(f"::error::A required check did not succeed for this commit ({args.names}).")
        return 1
    if result is Decision.WAIT:
        print("A required check is missing or still running; will retry.")
        return 2
    print(f"All required checks passed: {args.names}.")
    return 0


if __name__ == "__main__":  # pragma: no cover
    raise SystemExit(main())
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `python -m pytest scripts/test_release_gate.py -v`
Expected: PASS (9 passed).

- [ ] **Step 5: Lint**

Run: `ruff check scripts/release_gate.py scripts/test_release_gate.py && ruff format --check scripts/release_gate.py scripts/test_release_gate.py`
Expected: no errors (run `ruff format scripts/release_gate.py scripts/test_release_gate.py` first if format check fails).

- [ ] **Step 6: Commit**

```bash
git add scripts/release_gate.py scripts/test_release_gate.py
git commit -m "feat(release): add release_gate.py to select required CI check-runs (#164)"
```

---

## Task 3: `crates_index.py` — sparse-index publish probe (TDD)

**Files:**
- Create: `scripts/crates_index.py`
- Test: `scripts/test_crates_index.py`

crates.io's sparse index serves one newline-delimited-JSON file per crate at
`https://index.crates.io/<dir>/<name>`, where `<dir>` is derived from the name
length (1→`1`, 2→`2`, 3→`3/<first-char>`, else `<chars 1-2>/<chars 3-4>`). Each
line is a version object with a `"vers"` field. We use this both to skip
already-published crates (idempotent re-run) and to wait for propagation after
publishing.

- [ ] **Step 1: Write the failing tests**

Create `scripts/test_crates_index.py`:

```python
import pytest

from crates_index import index_path, is_published


def test_index_path_short_names():
    assert index_path("a") == "1/a"
    assert index_path("ab") == "2/ab"
    assert index_path("abc") == "3/a/abc"


def test_index_path_long_name():
    assert index_path("musefs-db") == "mu/se/musefs-db"
    assert index_path("musefs") == "mu/se/musefs"


def test_is_published_true_when_version_present():
    body = '{"name":"musefs-db","vers":"0.2.0"}\n{"name":"musefs-db","vers":"1.0.0"}\n'
    assert is_published("musefs-db", "1.0.0", fetch=lambda url: body) is True


def test_is_published_false_when_version_absent():
    body = '{"name":"musefs-db","vers":"0.2.0"}\n'
    assert is_published("musefs-db", "1.0.0", fetch=lambda url: body) is False


def test_is_published_false_when_crate_missing():
    def fetch_404(url):
        raise FileNotFoundError(url)

    assert is_published("musefs-db", "1.0.0", fetch=fetch_404) is False


def test_is_published_uses_correct_url():
    seen = {}

    def fetch(url):
        seen["url"] = url
        return '{"vers":"1.0.0"}\n'

    is_published("musefs-db", "1.0.0", fetch=fetch)
    assert seen["url"] == "https://index.crates.io/mu/se/musefs-db"
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `python -m pytest scripts/test_crates_index.py -v`
Expected: FAIL — `ModuleNotFoundError: No module named 'crates_index'`.

- [ ] **Step 3: Write the implementation**

Create `scripts/crates_index.py`:

```python
"""Check whether a crate version is resolvable from the crates.io sparse index.

Used by the release workflow to (a) skip crates already published — so a
whole-workflow re-run after a partial failure is idempotent — and (b) wait for
index propagation between dependency-ordered publishes (#163).
"""

from __future__ import annotations

import argparse
import json
import sys
import urllib.error
import urllib.request


def index_path(name: str) -> str:
    """Return the sparse-index path for ``name`` (crates.io's layout)."""
    n = len(name)
    if n == 1:
        return f"1/{name}"
    if n == 2:
        return f"2/{name}"
    if n == 3:
        return f"3/{name[0]}/{name}"
    return f"{name[0:2]}/{name[2:4]}/{name}"


def _http_fetch(url: str) -> str:
    req = urllib.request.Request(url, headers={"User-Agent": "musefs-release"})
    try:
        with urllib.request.urlopen(req, timeout=15) as resp:
            return resp.read().decode("utf-8")
    except urllib.error.HTTPError as exc:
        if exc.code == 404:
            raise FileNotFoundError(url) from exc
        raise


def is_published(name: str, version: str, *, fetch=_http_fetch) -> bool:
    """True if ``name@version`` appears in the sparse index."""
    url = f"https://index.crates.io/{index_path(name)}"
    try:
        body = fetch(url)
    except FileNotFoundError:
        return False
    for line in body.splitlines():
        line = line.strip()
        if not line:
            continue
        if json.loads(line).get("vers") == version:
            return True
    return False


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("name")
    parser.add_argument("version")
    args = parser.parse_args(argv)
    if is_published(args.name, args.version):
        print(f"{args.name}@{args.version} is in the index.")
        return 0
    print(f"{args.name}@{args.version} not in the index yet.")
    return 3


if __name__ == "__main__":  # pragma: no cover
    raise SystemExit(main())
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `python -m pytest scripts/test_crates_index.py -v`
Expected: PASS (6 passed).

- [ ] **Step 5: Lint**

Run: `ruff check scripts/crates_index.py scripts/test_crates_index.py && ruff format --check scripts/crates_index.py scripts/test_crates_index.py`
Expected: no errors.

- [ ] **Step 6: Commit**

```bash
git add scripts/crates_index.py scripts/test_crates_index.py
git commit -m "feat(release): add crates_index.py sparse-index publish probe (#163)"
```

---

## Task 4: Wire the new script tests into CI

**Files:**
- Modify: `.github/workflows/ci.yml:146-148`

The `python-musefs` job already runs the other `scripts/test_*.py` files. Add ours so they gate every PR.

- [ ] **Step 1: Add the test steps**

In `.github/workflows/ci.yml`, after the existing block:

```yaml
      - name: Test bump script
        run: python -m pytest scripts/test_bump_python_version.py -v
      - name: Test mutant-anchor guard
        run: python -m pytest scripts/test_check_mutant_anchors.py -v
```

add:

```yaml
      - name: Test release gate
        run: python -m pytest scripts/test_release_gate.py -v
      - name: Test crates-index probe
        run: python -m pytest scripts/test_crates_index.py -v
```

- [ ] **Step 2: Validate YAML parses**

Run: `python -c "import yaml; yaml.safe_load(open('.github/workflows/ci.yml'))" && echo OK`
Expected: `OK`.

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: run release_gate and crates_index unit tests on PRs"
```

---

## Task 5: Restructure release.yml into one ordered graph

**Files:**
- Modify: `.github/workflows/release.yml`

Current jobs: `publish` (lines 16-46, depends on nothing), `build` (48), `smoke` (101, needs build), `release-assets` (150, needs smoke). Target DAG: `gate → build → smoke → publish → release-assets`.

- [ ] **Step 1: Add the `gate` job and update top-level permissions**

The top of `release.yml` currently has:

```yaml
permissions:
  contents: read

jobs:
  publish:
    runs-on: ubuntu-latest
    steps:
```

Replace the `jobs:` opening so a new `gate` job comes first. Insert this `gate` job as the first job under `jobs:` (before `publish`):

```yaml
jobs:
  gate:
    runs-on: ubuntu-latest
    permissions:
      contents: read
      checks: read
    steps:
      - uses: actions/checkout@df4cb1c069e1874edd31b4311f1884172cec0e10
        with:
          persist-credentials: false
      - uses: actions/setup-python@a309ff8b426b58ec0e2a45f0f869d46889d02405
        with:
          python-version: '3.x'
      - name: Verify tag matches workspace version
        run: |
          set -euo pipefail
          TAG_VERSION="${GITHUB_REF_NAME#v}"
          WS_VERSION="$(grep -m1 '^version = ' Cargo.toml | sed -E 's/version = "([^"]+)"/\1/')"
          echo "tag=$TAG_VERSION workspace=$WS_VERSION"
          if [ "$TAG_VERSION" != "$WS_VERSION" ]; then
            echo "::error::Tag $GITHUB_REF_NAME does not match workspace version $WS_VERSION"
            exit 1
          fi
      - name: Wait for required CI checks to be green on the tagged commit
        env:
          GH_TOKEN: ${{ github.token }}
          SHA: ${{ github.sha }}
        run: |
          set -euo pipefail
          # Poll the Checks API until ci-ok and coverage-ok complete on this
          # commit. release_gate.py decides: exit 0 = all green, 2 = keep
          # waiting, 1 = a required check failed. The tag also triggers ci.yml
          # and coverage.yml, so these checks are (re)generated for this tree.
          deadline=$(( $(date +%s) + 45 * 60 ))   # 45 min covers the full matrix
          while :; do
            # --slurp collects all pages into one JSON array; the jq filter then
            # flattens every page's check_runs into a single {check_runs: [...]}
            # object (plain --paginate would emit one object per page, which is
            # not valid single-document JSON).
            gh api "repos/${GITHUB_REPOSITORY}/commits/${SHA}/check-runs" \
              --paginate --slurp -q '{check_runs: [.[].check_runs[]]}' > checks.json || true
            set +e
            python scripts/release_gate.py --names ci-ok coverage-ok < checks.json
            rc=$?
            set -e
            if [ "$rc" -eq 0 ]; then break; fi
            if [ "$rc" -eq 1 ]; then exit 1; fi
            if [ "$(date +%s)" -ge "$deadline" ]; then
              echo "::error::Timed out waiting for ci-ok/coverage-ok on ${SHA}"
              exit 1
            fi
            sleep 20
          done
```

Note: `--paginate` with `-q '{check_runs: [.check_runs[]]}'` flattens paginated pages into a single `{check_runs: [...]}` object that `release_gate.py` parses.

- [ ] **Step 2: Strip the now-duplicated version check from `publish` and gate it on smoke**

In the `publish` job, change its header to depend on `smoke` and remove the `Verify tag matches workspace version` step (it now lives in `gate`). The `publish` job header becomes:

```yaml
  publish:
    needs: smoke
    runs-on: ubuntu-latest
    steps:
```

Delete the entire `- name: Verify tag matches workspace version` step from `publish` (it moved to `gate`). Keep the checkout, libfuse3, toolchain, and rust-cache steps.

- [ ] **Step 3: Rewrite the publish loop to skip-if-present and wait for propagation**

Replace the existing `Publish crates in dependency order` step in `publish` with:

```yaml
      - uses: actions/setup-python@a309ff8b426b58ec0e2a45f0f869d46889d02405
        with:
          python-version: '3.x'
      - name: Publish crates in dependency order
        env:
          CARGO_REGISTRY_TOKEN: ${{ secrets.CARGO_REGISTRY_TOKEN }}
        run: |
          set -euo pipefail
          VERSION="${GITHUB_REF_NAME#v}"
          # Keep this list in sync with the workspace members in Cargo.toml,
          # in dependency order (a crate must come after everything it depends on).
          for c in musefs-db musefs-format musefs-core musefs-fuse musefs-cli musefs; do
            if python scripts/crates_index.py "$c" "$VERSION"; then
              echo "=== $c@$VERSION already published; skipping ==="
            else
              echo "=== publishing $c ==="
              cargo publish -p "$c" --locked
            fi
            # Wait for the just-published (or pre-existing) version to be
            # resolvable before the next dependent crate publishes (#163).
            echo "=== waiting for $c@$VERSION to appear in the index ==="
            deadline=$(( $(date +%s) + 10 * 60 ))
            until python scripts/crates_index.py "$c" "$VERSION"; do
              if [ "$(date +%s)" -ge "$deadline" ]; then
                echo "::error::$c@$VERSION did not appear in the crates.io index within 10m"
                exit 1
              fi
              sleep 10
            done
          done
```

- [ ] **Step 4: Gate `build` on `gate`**

Change the `build` job header from:

```yaml
  build:
    runs-on: ubuntu-latest
```

to:

```yaml
  build:
    needs: gate
    runs-on: ubuntu-latest
```

- [ ] **Step 5: Make `release-assets` depend on both `smoke` and `publish`**

Change the `release-assets` job header from:

```yaml
  release-assets:
    needs: smoke
    runs-on: ubuntu-latest
```

to:

```yaml
  release-assets:
    needs: [smoke, publish]
    runs-on: ubuntu-latest
```

- [ ] **Step 6: Validate YAML and job graph**

Run: `python -c "import yaml; d=yaml.safe_load(open('.github/workflows/release.yml')); print(sorted(d['jobs'])); print({k:v.get('needs') for k,v in d['jobs'].items()})"`
Expected output (order may vary):
```
['build', 'gate', 'publish', 'release-assets', 'smoke']
{'gate': None, 'publish': 'smoke', 'build': 'gate', 'smoke': 'build', 'release-assets': ['smoke', 'publish']}
```

- [ ] **Step 7: Lint the workflows with actionlint (if available)**

Run: `command -v actionlint >/dev/null && actionlint .github/workflows/release.yml || echo "actionlint not installed; skipping (CI will catch syntax)"`
Expected: no errors, or the skip message.

- [ ] **Step 8: Commit**

```bash
git add .github/workflows/release.yml
git commit -m "ci(release): one ordered gate->build->smoke->publish->assets graph (#222, #164, #163)"
```

---

## Task 6: Final review against the spec

- [ ] **Step 1: Confirm the partial-ship gap is closed**

Re-read the DAG: `release-assets` needs `[smoke, publish]`, so assets never upload unless crates published; `publish` needs `smoke`, so crates never publish unless binaries passed smoke; `build` needs `gate`, so nothing runs unless the tagged commit's `ci-ok`+`coverage-ok` are green. Confirm there is no job left with `needs:` pointing at the deleted independent `publish` chain.

Run: `grep -n "needs:" .github/workflows/release.yml`
Expected: `gate` absent from the list; `publish: needs: smoke`; `build: needs: gate`; `smoke: needs: build`; `release-assets: needs: [smoke, publish]`.

- [ ] **Step 2: Confirm idempotent re-run path**

Confirm the publish loop calls `crates_index.py` to skip already-published crates before `cargo publish`. Re-running the workflow after a mid-loop failure must skip the already-published crates and proceed.

- [ ] **Step 3: Run the full script test suite once more**

Run: `python -m pytest scripts/test_release_gate.py scripts/test_crates_index.py -v`
Expected: PASS (15 passed total).
