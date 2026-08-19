#!/usr/bin/env bash
# Install apt packages, tolerating the transient runner failures that have
# repeatedly wedged CI and the release gate.
#
# The failure mode is a *hang*, not an error exit: `apt-get update` (or the
# install that follows) stops making progress against a sick mirror and sits
# there until the job's own timeout fires — six jobs across three workflows
# stalled 15+ minutes on this during the v1.3.0 release, which is long enough to
# blow the release gate's 45-minute deadline. A plain `until ... do` retry loop
# would never re-enter, so each attempt gets its own `timeout`; only then is a
# retry with backoff useful.
#
# Usage: scripts/apt-install.sh fuse3 libfuse3-dev pkg-config
#
# `update` and `install` get separate budgets because their healthy runtimes
# differ by an order of magnitude: a refresh is ~15s and is the step that hangs,
# while installing e.g. ffmpeg's dependency tree legitimately takes 80s+. A
# ceiling tight enough to catch a hung refresh quickly would kill a slow-but-fine
# install, turning a flake into a self-inflicted failure.
#
# The install budget is sized off measurement, not guesswork: on a degraded
# afternoon these steps were observed taking 200-234s where they normally take
# ~20s, so the ceiling has to clear that comfortably or a slow-but-progressing
# install gets killed. Attempts are kept low to compensate, since a genuinely
# hung mirror does not recover within a job's lifetime anyway — the retry is for
# transient failures, and the bound is what protects the release gate.
#
# Knobs (env): APT_RETRY_ATTEMPTS (default 2), APT_UPDATE_TIMEOUT (default 120),
# APT_INSTALL_TIMEOUT (default 420). Worst case is ~18 minutes, inside the
# release gate's 45-minute deadline, so a mirror that never recovers fails
# loudly instead of eating the whole window.
set -euo pipefail

if [ "$#" -eq 0 ]; then
    echo "usage: $0 <package>..." >&2
    exit 2
fi

attempts="${APT_RETRY_ATTEMPTS:-2}"
update_try="${APT_UPDATE_TIMEOUT:-120}"
install_try="${APT_INSTALL_TIMEOUT:-420}"

# Root in a container has no sudo; the runner jobs are non-root and do.
sudo=""
if [ "$(id -u)" -ne 0 ]; then
    sudo="sudo"
fi

export DEBIAN_FRONTEND=noninteractive

delay=5
attempt=1
while [ "$attempt" -le "$attempts" ]; do
    # `update` and `install` are timed separately: a mirror that hangs the
    # refresh is the common case, and re-running it is the cheap fix.
    # Captured via `|| status=$?` rather than `$?` after an `if`: a failed `if`
    # condition leaves `$?` at 0, which would report every failure as exit 0.
    status=0
    # shellcheck disable=SC2086  # $sudo is deliberately unquoted: it must
    # disappear entirely, not expand to an empty argument, when running as root.
    timeout "$update_try" $sudo apt-get update \
        && timeout "$install_try" $sudo apt-get install -y "$@" \
        || status=$?
    if [ "$status" -eq 0 ]; then
        exit 0
    fi
    if [ "$attempt" -eq "$attempts" ]; then
        break
    fi
    # 124 is `timeout`'s own "killed it" code, worth calling out from a plain
    # apt error because it is the flake this script exists for.
    if [ "$status" -eq 124 ]; then
        echo "apt attempt ${attempt}/${attempts} timed out" >&2
    else
        echo "apt attempt ${attempt}/${attempts} failed (exit ${status})" >&2
    fi
    echo "retrying in ${delay}s: $*" >&2
    sleep "$delay"
    delay=$((delay * 2))
    attempt=$((attempt + 1))
done

echo "apt-get failed after ${attempts} attempts: $*" >&2
exit 1
