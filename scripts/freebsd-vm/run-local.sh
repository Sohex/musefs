#!/bin/sh
# Stand up a FreeBSD VM locally (qemu/KVM) and run the FUSE e2e suite in it.
#
# Self-contained and credential-free: downloads the official FreeBSD VM image
# into .scratch/, boots it headless, and drives it over the serial console as
# root (the plain image has an empty root password — no SSH, no keys). The repo
# is handed to the VM over a throwaway HTTP server on qemu's user-net gateway
# (10.0.2.2), then the SAME in-guest scripts CI runs (provision.sh + run-e2e.sh)
# execute over the console. Everything lives under the gitignored
# .scratch/freebsd/; rerunning resets the VM from the cached base image.
#
# Host prerequisites (see the FreeBSD e2e section in CONTRIBUTING.md):
#   qemu-system-x86_64, qemu-img, curl, xz, python3
#   /dev/kvm for acceleration (works without it, just far slower)
#
# Usage:  sh scripts/freebsd-vm/run-local.sh
# Env overrides: FREEBSD_REL (default 14.3-RELEASE), VM_MEM (4096), VM_SMP (4),
#   VM_DISK (30G), HTTP_PORT (18080), RUN_TIMEOUT (2700 seconds).
set -eu

REL="${FREEBSD_REL:-14.3-RELEASE}"
MEM="${VM_MEM:-4096}"
SMP="${VM_SMP:-4}"
DISK_SIZE="${VM_DISK:-30G}"
HTTP_PORT="${HTTP_PORT:-18080}"
RUN_TIMEOUT="${RUN_TIMEOUT:-2700}"

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
HERE="$ROOT/scripts/freebsd-vm"
WORK="$ROOT/.scratch/freebsd"
mkdir -p "$WORK"

# The BASIC-CLOUDINIT image directs its console to the serial line (the plain
# image does not), which is what lets us drive it over the serial socket. We do
# NOT use cloud-init itself — just the serial getty + empty root password.
IMG="FreeBSD-$REL-amd64-BASIC-CLOUDINIT-ufs.qcow2"
URL="https://download.freebsd.org/releases/VM-IMAGES/$REL/amd64/Latest/$IMG.xz"
BASE="$WORK/base-$REL-cloudinit.qcow2"
DISK="$WORK/disk.qcow2"
SOCK="$WORK/serial.sock"
CONSOLE="$WORK/console.log"
PIDFILE="$WORK/qemu.pid"
TARBALL="$WORK/repo.tgz"
HTTP_PIDFILE="$WORK/http.pid"

log() { printf '\n=== %s ===\n' "$*"; }

cleanup() {
    for pf in "$PIDFILE" "$HTTP_PIDFILE"; do
        if [ -f "$pf" ]; then
            pid="$(cat "$pf" 2>/dev/null || true)"
            [ -n "${pid:-}" ] && kill "$pid" 2>/dev/null || true
            rm -f "$pf"
        fi
    done
    rm -f "$SOCK"
}
trap cleanup EXIT INT TERM

# --- 1. Base image (download + decompress once; cached) --------------------
if [ ! -f "$BASE" ]; then
    log "Downloading FreeBSD $REL VM image"
    curl -fL --retry 3 -o "$BASE.xz" "$URL"
    log "Decompressing"
    unxz -f "$BASE.xz"   # -> $BASE
fi

# --- 2. Fresh overlay disk each run (cheap reset of the cached base) -------
# The image's growfs firstboot service expands root to fill this on first boot.
log "Creating overlay disk ($DISK_SIZE)"
rm -f "$DISK"
qemu-img create -f qcow2 -b "$BASE" -F qcow2 "$DISK" "$DISK_SIZE" >/dev/null

# --- 3. Package the repo + serve it on the user-net gateway (10.0.2.2) ------
log "Packaging repo + starting HTTP server on 127.0.0.1:$HTTP_PORT"
tar -C "$ROOT" \
    --exclude=./target --exclude=./.git --exclude=./.scratch \
    --exclude='./**/target' -czf "$TARBALL" .
python3 -m http.server "$HTTP_PORT" --bind 127.0.0.1 --directory "$WORK" \
    >/dev/null 2>&1 &
echo $! > "$HTTP_PIDFILE"

# --- 4. Boot headless, serial on a unix socket -----------------------------
ACCEL=""
[ -e /dev/kvm ] && ACCEL="-accel kvm -cpu host"
rm -f "$SOCK" "$CONSOLE"
log "Booting VM (headless; serial -> $SOCK, logged to $CONSOLE)"
# shellcheck disable=SC2086
qemu-system-x86_64 $ACCEL -m "$MEM" -smp "$SMP" -display none \
    -drive file="$DISK",if=virtio \
    -netdev user,id=n0 -device virtio-net,netdev=n0 \
    -serial unix:"$SOCK",server,nowait -daemonize -pidfile "$PIDFILE"

# --- 5. Drive the whole run over the serial console ------------------------
# In-guest: wait for DHCP, fetch the repo from the host gateway, unpack, then run
# the same provision + e2e scripts CI uses. serial-run.py captures the exit code.
if [ -n "${PROBE:-}" ]; then
    # Validation mode: just prove serial root login works on this image.
    GUEST_CMD="id; uname -a; echo PROBE_OK"
else
    # No `set -e` and no `exit` here: serial-run.py appends `; echo MUSEFS_RC=$?`
    # to capture the result, and `set -e`/`exit` would abort the shell before that
    # marker prints (leaving the driver to hang). The `&&` chain propagates the
    # first failure into $? instead; a give-up in the fetch loop just `break`s,
    # leaving the absent tarball to fail the chain.
    GUEST_CMD="n=0; until fetch -q -o /root/repo.tgz http://10.0.2.2:$HTTP_PORT/repo.tgz; do \
n=\$((n+1)); [ \$n -ge 30 ] && { echo REPO_FETCH_FAILED; break; }; sleep 2; done; \
rm -rf /root/musefs && mkdir -p /root/musefs && tar -xzf /root/repo.tgz -C /root/musefs && \
cd /root/musefs && sh scripts/freebsd-vm/provision.sh && sh scripts/freebsd-vm/run-e2e.sh"
fi

log "Running provision + e2e over the console (up to ${RUN_TIMEOUT}s)"
set +e
python3 "$HERE/serial-run.py" "$SOCK" "$CONSOLE" "$RUN_TIMEOUT" "$GUEST_CMD"
rc=$?
set -e

log "Done (exit code from e2e: $rc); powering VM off"
exit "$rc"
