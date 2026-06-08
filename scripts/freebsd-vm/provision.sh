#!/bin/sh
# Provision a FreeBSD host/VM to build and run musefs FUSE e2e tests.
# Run as root, from the repo root. Used by BOTH the CI `freebsd` job
# (vmactions/freebsd-vm) and local runs (run-local.sh); see the FreeBSD e2e
# section in CONTRIBUTING.md. Keep CI and local identical by editing only this file.
set -eu

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

# VCS + ffmpeg + rustup. We install the Rust toolchain via rustup (NOT pkg's
# `rust`): FreeBSD's packaged rust lags (e.g. 1.94) and is too old for some deps
# — libsqlite3-sys 0.38's build.rs uses `cfg_select!`, stable only in Rust 1.95+.
# rustup pulls the current stable, matching the Linux CI toolchain.
# ffmpeg is REQUIRED for the full e2e suite: playback_pcm.rs decodes served
# files to PCM and compares SHAs, and ogg_read_through.rs encodes opus/vorbis/
# flac-in-ogg fixtures — both shell out to `ffmpeg` and SILENTLY SKIP if it is
# absent (a vacuous pass). The default FreeBSD `ffmpeg` package ships the
# needed decoders/encoders (flac, opus, vorbis, aac, mp3, pcm/wav).
pkg install -y git ffmpeg rustup-init

# Install the current stable toolchain (cargo + rustc) into ~/.cargo. run-e2e.sh
# puts ~/.cargo/bin on PATH. `-y` makes it non-interactive / idempotent.
rustup-init -y --no-modify-path --profile minimal --default-toolchain stable

# FUSE support: load the in-kernel fusefs module. fuser uses its pure-rust
# /dev/fuse backend on FreeBSD, so NO libfuse package is required — only the
# kernel module and the base-system mount_fusefs(8). `|| true`: already-loaded
# is fine.
kldload fusefs || true

# Allow unprivileged mounts, so the e2e suite can mount as a non-root user if the
# CI/VM runs tests unprivileged. Harmless when already running as root.
sysctl vfs.usermount=1 || true
