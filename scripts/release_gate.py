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


def latest_completed_by_name(runs, name, since=None):
    """Return the newest *completed* check-run with ``name``, or ``None``.

    The Checks API returns every run of a name (including re-runs); the gate
    only trusts the most recently completed one, sorted by ``completed_at``.

    When ``since`` (an ISO-8601 timestamp) is given, runs that *started* before
    it are discarded as stale: ``ci-ok``/``coverage-ok`` are late aggregator
    jobs, so a release tag's fresh runs only appear minutes in. Filtering on
    ``started_at`` keeps the gate from trusting a still-green main-branch run
    from before the tag, which would let the release skip the tag-only legs.
    """
    completed = [
        r
        for r in runs
        if r.get("name") == name
        and r.get("status") == "completed"
        and r.get("completed_at")
        and (since is None or (r.get("started_at") or "") >= since)
    ]
    if not completed:
        return None
    return max(completed, key=lambda r: r["completed_at"])


def decide(runs, names, since=None):
    """Return a :class:`Decision` for the required check ``names``.

    ``since`` is forwarded to :func:`latest_completed_by_name` to discard
    pre-tag (stale) runs.
    """
    saw_missing = False
    for name in names:
        chosen = latest_completed_by_name(runs, name, since)
        if chosen is None:
            saw_missing = True
            continue
        if chosen.get("conclusion") != "success":
            return Decision.FAIL
    return Decision.WAIT if saw_missing else Decision.PASS


def main(argv=None, stdin_text=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--names", nargs="+", required=True, help="required check-run names")
    parser.add_argument(
        "--since",
        default=None,
        help="ISO-8601 cutoff; ignore check-runs that started before it (stale "
        "pre-tag runs). Pass the release run's run_started_at.",
    )
    args = parser.parse_args(argv)

    text = stdin_text if stdin_text is not None else sys.stdin.read()
    try:
        payload = json.loads(text)
    except (json.JSONDecodeError, ValueError):
        # Empty/garbled input — e.g. a transient `gh api` failure left checks.json
        # empty — must degrade to "wait" so the poll loop retries, not crash and
        # abort the release.
        print("Could not parse check-runs JSON; will retry.")
        return 2
    # `or []` (not `.get(..., [])`): a present-but-null check_runs key — which a
    # mis-slurped gh payload can produce — must degrade to "wait", not raise.
    runs = (payload.get("check_runs") if isinstance(payload, dict) else None) or []

    result = decide(runs, args.names, args.since)
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
