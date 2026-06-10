import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from release_gate import Decision, decide, latest_completed_by_name, main  # noqa: E402


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
    rc = main(["--names", "ci-ok", "coverage-ok"], stdin_text=json.dumps(payload))
    assert rc == 0


def test_cli_wait_exit_two():
    payload = {"check_runs": [_run("ci-ok", "completed", "success", "2026-06-10T11:00:00Z")]}
    rc = main(["--names", "ci-ok", "coverage-ok"], stdin_text=json.dumps(payload))
    assert rc == 2


def test_cli_fail_exit_one():
    payload = {
        "check_runs": [
            _run("ci-ok", "completed", "success", "2026-06-10T11:00:00Z"),
            _run("coverage-ok", "completed", "failure", "2026-06-10T11:05:00Z"),
        ]
    }
    rc = main(["--names", "ci-ok", "coverage-ok"], stdin_text=json.dumps(payload))
    assert rc == 1


def test_cli_handles_null_check_runs():
    # A mis-slurped gh payload can yield {"check_runs": null}; must wait, not raise.
    rc = main(["--names", "ci-ok"], stdin_text='{"check_runs": null}')
    assert rc == 2
