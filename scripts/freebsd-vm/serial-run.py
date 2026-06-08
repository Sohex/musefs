#!/usr/bin/env python3
"""Drive a freshly-booted FreeBSD VM over its serial console and run a command.

run-local.sh boots the official plain FreeBSD VM image (empty root password,
serial getty enabled) with the serial line on a unix socket. This driver logs in
as root over that console — no SSH, no key — switches to /bin/sh for predictable
syntax, runs the supplied command, and waits for a sentinel that carries the
command's exit code (printed as MUSEFS_RC=<n>). All console output is tee'd to the
log file for debugging. Exits with the guest command's exit code.

Usage: serial-run.py <serial.sock> <console-log> <timeout-seconds> <command>
"""
import re
import socket
import sys
import time

SOCK, LOGPATH, TIMEOUT, COMMAND = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4]
LOG = open(LOGPATH, "ab", buffering=0)
# The tty echoes the typed command back over serial, so the marker must look
# different in the command we SEND than in the line the command PRINTS — else we
# match our own echoed input before the command runs. We send `"MUSEFS""_RC=$?"`
# (printed by the shell as `MUSEFS_RC=<n>`) and match the digit form only.
MARKER_RE = re.compile(r"MUSEFS_RC=(\d+)")

sock = None
deadline = time.time() + 60
while time.time() < deadline:
    try:
        sock = socket.socket(socket.AF_UNIX)
        sock.connect(SOCK)
        break
    except OSError:
        sock = None
        time.sleep(1)
if sock is None:
    sys.exit(f"could not connect to serial socket {SOCK}")
sock.settimeout(2)

buf = b""


def pump():
    global buf
    try:
        d = sock.recv(4096)
    except socket.timeout:
        return
    if d:
        LOG.write(d)
        buf += d
        if len(buf) > 131072:
            buf = buf[-131072:]


def wait_for(text, timeout):
    end = time.time() + timeout
    while time.time() < end:
        if text.encode() in buf:
            return True
        pump()
    return False


def wait_for_re(rx, timeout):
    end = time.time() + timeout
    while time.time() < end:
        m = rx.search(buf.decode("latin-1"))
        if m:
            return m
        pump()
    return None


def send(line):
    sock.sendall(line.encode() + b"\n")


# A newline makes getty (re)print the prompt even if we attached late. The login
# prompt can be delayed several minutes by the image's synchronous firstboot
# (freebsd-update + growfs run in rc before getty), so wait generously.
send("")
if not wait_for("login:", 900):
    sys.exit("never saw a login prompt on the serial console")
send("root")
time.sleep(2)
pump()
if "assword:" in buf[-300:].decode("latin-1"):
    send("")  # empty root password
    time.sleep(2)
    pump()

# Wait for the login shell's prompt. It appears only AFTER the motd and the
# `resizewin` terminal-size probe that login runs — sending input before that
# risks it being eaten by resizewin's escape-sequence query.
if not wait_for("# ", 90):
    send("")  # nudge a fresh prompt
    wait_for("# ", 30)

buf = b""
# Run everything in ONE non-interactive `sh -c` so no interactive profile (hence
# no resizewin) runs. COMMAND is guaranteed free of single quotes by the caller.
# The split-quoted marker (`"MUSEFS""_RC="`) prints as `MUSEFS_RC=<n>` but the
# echoed input line shows `MUSEFS""_RC=`, so MARKER_RE matches only real output.
send("/bin/sh -c '" + COMMAND + '; echo "MUSEFS""_RC=$?"' + "'")
m = wait_for_re(MARKER_RE, TIMEOUT)
if not m:
    sys.exit(f"timed out after {TIMEOUT}s waiting for the run to finish")
rc = int(m.group(1))
print(f"guest command exited {rc}")
sys.exit(rc)
