#!/usr/bin/env python3
"""Drive a freshly-booted FreeBSD VM over its serial console and run a command.

run-local.sh boots the official FreeBSD VM image (empty root password, serial
getty enabled) with the serial line on a unix socket. This driver logs in as
root over that console — no SSH, no key — runs the supplied command in a single
non-interactive /bin/sh, and waits for a sentinel carrying the exit code
(printed as MUSEFS_RC=<n>). All console output is tee'd to the log for debugging.
Exits with the guest command's exit code.

Usage: serial-run.py <serial.sock> <console-log> <timeout-seconds> <command>
"""

from __future__ import annotations

import re
import socket
import sys
import time

# The tty echoes the typed command back over serial, so the marker must look
# different in the command we SEND than in the line the command PRINTS — else we
# match our own echoed input before the command runs. We send `"MUSEFS""_RC=$?"`
# (printed by the shell as `MUSEFS_RC=<n>`) and match the digit form only.
MARKER_RE = re.compile(r"MUSEFS_RC=(\d+)")
MAX_BUF = 128 * 1024


class SerialConsole:
    """Minimal expect-style driver over a qemu unix-socket serial line."""

    def __init__(self, sock_path: str, log_path: str) -> None:
        self._log = open(log_path, "ab", buffering=0)
        self._buf = b""
        self._sock = self._connect(sock_path)
        self._sock.settimeout(2)

    @staticmethod
    def _connect(sock_path: str) -> socket.socket:
        # qemu may take a moment to create the socket after -daemonize.
        deadline = time.time() + 60
        while time.time() < deadline:
            try:
                sock = socket.socket(socket.AF_UNIX)
                sock.connect(sock_path)
                return sock
            except OSError:
                time.sleep(1)
        sys.exit(f"could not connect to serial socket {sock_path}")

    def _pump(self) -> None:
        try:
            data = self._sock.recv(4096)
        except socket.timeout:
            return
        if data:
            self._log.write(data)
            self._buf += data
            if len(self._buf) > MAX_BUF:
                self._buf = self._buf[-MAX_BUF:]

    def send(self, line: str) -> None:
        self._sock.sendall(line.encode() + b"\n")

    def wait_for(self, text: str, timeout: float) -> bool:
        end = time.time() + timeout
        while time.time() < end:
            if text.encode() in self._buf:
                return True
            self._pump()
        return False

    def wait_for_re(self, rx: re.Pattern[str], timeout: float) -> re.Match[str] | None:
        end = time.time() + timeout
        while time.time() < end:
            match = rx.search(self._buf.decode("latin-1"))
            if match:
                return match
            self._pump()
        return None

    def clear(self) -> None:
        self._buf = b""


def main(argv: list[str]) -> int:
    if len(argv) != 5:
        sys.exit(f"usage: {argv[0]} <serial.sock> <console-log> <timeout-seconds> <command>")
    sock_path, log_path, timeout_arg, command = argv[1], argv[2], argv[3], argv[4]
    try:
        timeout = int(timeout_arg)
    except ValueError:
        sys.exit(f"timeout must be an integer number of seconds, got {timeout_arg!r}")

    con = SerialConsole(sock_path, log_path)

    # A newline makes getty (re)print the prompt even if we attached late. The
    # login prompt can be delayed several minutes by the image's synchronous
    # firstboot (freebsd-update + growfs run in rc before getty), so wait long.
    con.send("")
    if not con.wait_for("login:", 900):
        sys.exit("never saw a login prompt on the serial console")

    con.send("root")
    # The image has an empty root password, but handle a prompt just in case.
    if con.wait_for("assword:", 5):
        con.send("")

    # Wait for the login shell's prompt. It appears only AFTER the motd and the
    # `resizewin` terminal-size probe that login runs — sending input before that
    # risks it being eaten by resizewin's escape-sequence query.
    if not con.wait_for("# ", 90):
        con.send("")  # nudge a fresh prompt
        con.wait_for("# ", 30)

    con.clear()
    # Run everything in ONE non-interactive `sh -c` so no interactive profile
    # (hence no resizewin) runs. `command` is guaranteed free of single quotes by
    # the caller. The split-quoted marker (`"MUSEFS""_RC="`) prints as
    # `MUSEFS_RC=<n>` but the echoed input shows `MUSEFS""_RC=`, so MARKER_RE
    # matches only real output.
    con.send("/bin/sh -c '" + command + '; echo "MUSEFS""_RC=$?"' + "'")
    match = con.wait_for_re(MARKER_RE, timeout)
    if not match:
        sys.exit(f"timed out after {timeout}s waiting for the run to finish")
    rc = int(match.group(1))
    print(f"guest command exited {rc}")
    return rc


if __name__ == "__main__":
    sys.exit(main(sys.argv))
