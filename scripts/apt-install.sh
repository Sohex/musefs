#!/usr/bin/env bash
# Install apt packages without depending on `apt-get update` succeeding.
#
# Two distinct runner failures motivate this, both seen during the v1.3.0
# release:
#
#   * A hang. `apt-get update`/`install` stops making progress against a sick
#     mirror and sits there until the job's own timeout fires — six jobs across
#     three workflows stalled 15+ minutes, long enough to blow the release
#     gate's 45-minute deadline. Retrying only helps if each attempt is bounded,
#     so every apt invocation gets its own `timeout`.
#
#   * A slow mirror. `apt-get update` alone was measured past 300s while still
#     making steady progress, and large installs at 200-360s where they normally
#     take ~20s. Chasing that by raising ceilings is a losing game: the ceiling
#     that catches a hang quickly is the one that kills a slow-but-working step.
#
# The way out is to notice that the refresh is usually unnecessary. GitHub's
# runner images ship with populated apt lists, and every package these workflows
# install is a plain archive package already indexed there. So try the install
# first, against the lists already on disk, and only pay for a refresh when that
# genuinely fails (stale lists, rotated archive). On a healthy runner this skips
# the slowest and most failure-prone step outright; on a degraded one it is the
# difference between a 20s step and a 10-minute failure.
#
# Usage: scripts/apt-install.sh fuse3 libfuse3-dev pkg-config
#
# Knobs (env): APT_RETRY_ATTEMPTS (default 2) refresh-then-install rounds tried
# after the fast path fails, APT_UPDATE_TIMEOUT (default 300),
# APT_INSTALL_TIMEOUT (default 420). Worst case is ~24 minutes, inside the
# release gate's 45-minute deadline, so a mirror that never recovers fails
# loudly instead of eating the whole window.
set -euo pipefail

if [ "$#" -eq 0 ]; then
    echo "usage: $0 <package>..." >&2
    exit 2
fi

attempts="${APT_RETRY_ATTEMPTS:-2}"
update_try="${APT_UPDATE_TIMEOUT:-300}"
install_try="${APT_INSTALL_TIMEOUT:-420}"

# Root in a container has no sudo; the runner jobs are non-root and do.
sudo=""
if [ "$(id -u)" -ne 0 ]; then
    sudo="sudo"
fi

export DEBIAN_FRONTEND=noninteractive

# Fast path: install straight from the image's existing apt lists.
# shellcheck disable=SC2086  # $sudo is deliberately unquoted: it must
# disappear entirely, not expand to an empty argument, when running as root.
if timeout "$install_try" $sudo apt-get install -y "$@"; then
    exit 0
fi
echo "install from the image's apt lists failed; refreshing" >&2

delay=5
attempt=1
while [ "$attempt" -le "$attempts" ]; do
    # `update` and `install` are timed separately because their healthy
    # runtimes differ by an order of magnitude, and a single shared ceiling
    # cannot bound the hang without killing the slow case.
    # Captured via `|| status=$?` rather than `$?` after an `if`: a failed `if`
    # condition leaves `$?` at 0, which would report every failure as exit 0.
    status=0
    # shellcheck disable=SC2086  # see above
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
