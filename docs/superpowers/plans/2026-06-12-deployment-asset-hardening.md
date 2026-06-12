# Deployment-Asset Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Harden musefs's shipped deployment assets — sandbox the two systemd user units and run the container images as a non-root user — without breaking the FUSE mount or imposing path-configuration friction.

**Architecture:** Three independent, ascending-risk edits, each gated by a *live* run on the dedicated server (there are no unit tests for a systemd directive — the verification IS the gate). The mount-less scanner takes a full path-agnostic sandbox; the mount unit takes only directives that keep the FUSE mount visible (the riskier ones ship commented-out/opt-in); the containers drop to a fixed uid/gid 1000 with `user_allow_other` baked in so the post-#339 `/etc/fuse.conf` pre-flight check still passes for non-root `allow_other` mounts.

**Tech Stack:** systemd user units (`systemctl --user`, `systemd-analyze security`), FUSE/`fusermount3`, Podman (rootless + `podman unshare`), Cargo (host + `x86_64-unknown-linux-musl` static target).

**Spec:** `docs/superpowers/specs/2026-06-12-deployment-asset-hardening-design.md`

**Issues:** #317 (scanner unit), #318 (mount unit), #319 (container user). Tracking: #280.

---

## Pre-commit note (read once)

Per `CLAUDE.md`, the pre-commit hook skips the cargo gate *only* when every
staged path is under `docs/` or is a `*.md` file. Commits that stage a
`.service` file or a `Dockerfile` are **not** docs-only, so they run the full
workspace test suite. We change no Rust, so those tests pass — but expect each
such commit to take a few minutes. The `.service`/`Dockerfile` content is not
linted by shellcheck/yamllint/ruff. Do **not** use `--no-verify`.

## Safety note (read once)

The live tests install systemd user units and build container images. To avoid
clobbering any real musefs deployment on this machine, **every** test unit uses
a `musefs-harden-*` name and a scratch directory under
`$HOME/.cache/musefs-harden-test`. Never touch `~/.config/systemd/user/musefs.service`
or `musefs-scan.service` directly. The Task 5 cleanup removes all test artifacts.

---

## File Structure

Files created or modified by this plan:

- `contrib/systemd/musefs-scan.service` — **modify**: append the full sandbox block (Task 1).
- `contrib/systemd/musefs.service` — **modify**: extend the do-NOT-add comment, append safe directives + commented opt-in block (Task 2).
- `contrib/systemd/README.md` — **modify**: add a `## Hardening` section (Task 3).
- `docker/Dockerfile.glibc` — **modify**: add non-root user, `user_allow_other`, `USER` (Task 4).
- `docker/Dockerfile.musl` — **modify**: same, Alpine flavour (Task 4).
- `README.md` — **modify**: non-root container note + `user_allow_other` cross-ref (Task 4).

No source files change. No new committed files (the live-test harness is inline, per the spec's "no CI smoke harness" scope decision).

---

## Task 0: Live-test harness setup

Sets up the scratch fixtures and the two binaries every later task reuses. This
task produces **no commit** — it builds throwaway artifacts under `$HOME/.cache`
and `/tmp`.

**Files:** none (scratch only).

- [ ] **Step 1: Confirm no name collision with a real deployment**

Run:
```bash
ls ~/.config/systemd/user/ 2>/dev/null | grep -i musefs || echo "no existing musefs user units"
systemctl --user is-system-running
```
Expected: either "no existing musefs user units", or a list you will leave
untouched. The manager state should print `running` (or `degraded` — fine).

- [ ] **Step 2: Point at the real library and create scratch dirs**

The dedicated server has a real music library at `/data/media/music`. The
scanner reads it directly — AppArmor only gates FUSE *mounts* under `/data`, not
reads, and `ProtectSystem=true` leaves `/data` readable. A full scan of the
whole library can be slow (HDD); for a sandbox smoke test, narrow `LIB` to a
single artist/album.

Run:
```bash
export HARDEN="$HOME/.cache/musefs-harden-test"
export LIB="/data/media/music"          # for speed: set to one album, e.g. "$LIB/<artist>/<album>"
rm -rf "$HARDEN"
mkdir -p "$HARDEN"/{db,custom-db,mnt}
test -r "$LIB" && find "$LIB" -type f \( -iname '*.flac' -o -iname '*.mp3' -o -iname '*.m4a' -o -iname '*.ogg' -o -iname '*.wav' \) | head -3
```
Expected: `$LIB` is readable and at least one audio file is listed. (If
`/data/media/music` is unavailable, fall back to a scratch lib:
`mkdir -p "$HARDEN/lib" && cp musefs-format/tests/fixtures/sample.m4a "$HARDEN/lib/" && export LIB="$HARDEN/lib"`.)

- [ ] **Step 3: Build the host (glibc) binary for the systemd tests**

Run:
```bash
cargo build --release
ls -l target/release/musefs
```
Expected: `target/release/musefs` exists. This native binary is what the
systemd user units exec in Tasks 1–2.

- [ ] **Step 4: Build the static musl binary for the container tests**

The host glibc is newer than `bookworm`/`alpine`, so a host-built glibc binary
will not start inside either image. A static musl binary runs in both.

Run:
```bash
rustup target add x86_64-unknown-linux-musl
# libsqlite3-sys (bundled) compiles C; musl needs a musl C toolchain:
command -v musl-gcc || sudo apt-get install -y musl-tools
cargo build --release --target x86_64-unknown-linux-musl
file target/x86_64-unknown-linux-musl/release/musefs
```
Expected: `file` reports `statically linked` (or `static-pie`). If the build
fails on the C toolchain, that is the `musl-tools` step — install it and retry.

- [ ] **Step 5: Stage a clean Podman build context**

The Dockerfiles `COPY ${TARGETARCH}/musefs`; stage the musl binary as `amd64/musefs`
in a temp context so the repo tree stays clean.

Run:
```bash
export CTX=/tmp/musefs-harden-ctx
rm -rf "$CTX"; mkdir -p "$CTX/amd64"
cp target/x86_64-unknown-linux-musl/release/musefs "$CTX/amd64/musefs"
chmod +x "$CTX/amd64/musefs"
ls -l "$CTX/amd64/"
```
Expected: `$CTX/amd64/musefs` present and executable. (Dockerfiles are copied
into `$CTX` in Task 4, after they are edited.)

---

## Task 1: #317 — Full path-agnostic sandbox on `musefs-scan.service`

**Files:**
- Modify: `contrib/systemd/musefs-scan.service`

- [ ] **Step 1: Append the sandbox block to the unit**

Append the following block to the end of the `[Service]` section of
`contrib/systemd/musefs-scan.service` (after the existing `ExecStart=` line, so
the file's `[Service]` section ends with this):

```ini

# --- Sandbox -------------------------------------------------------------
# The scanner creates no FUSE mount, so it can take the full systemd sandbox
# (unlike musefs.service). None of these directives needs to know where your
# library or DB live: ProtectSystem=true keeps system dirs read-only while
# leaving $HOME and data volumes writable, so a custom MUSEFS_DB path works
# with no ReadWritePaths= edit.
NoNewPrivileges=true
ProtectSystem=true
PrivateTmp=true
PrivateDevices=true
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectKernelLogs=true
ProtectControlGroups=true
ProtectClock=true
ProtectHostname=true
# ProcSubset=pid hides non-pid /proc; a future parser reading /proc/cpuinfo for
# SIMD detection would need this relaxed.
ProtectProc=invisible
ProcSubset=pid
RestrictNamespaces=true
# AF_UNIX is REQUIRED for journald logging; do NOT drop to none.
RestrictAddressFamilies=AF_UNIX
RestrictRealtime=true
RestrictSUIDSGID=true
LockPersonality=true
MemoryDenyWriteExecute=true
CapabilityBoundingSet=
SystemCallFilter=@system-service
SystemCallArchitectures=native
SystemCallErrorNumber=EPERM
UMask=077
```

- [ ] **Step 2: Install the unit under a test name with a scratch drop-in**

The drop-in overrides only `ExecStart`/`PATH`/`EnvironmentFile` so the *sandbox
directives under test come verbatim from the edited file*.

Run:
```bash
export HARDEN="$HOME/.cache/musefs-harden-test"
export LIB="${LIB:-/data/media/music}"
export BIN="$PWD/target/release/musefs"
install -Dm644 contrib/systemd/musefs-scan.service \
  ~/.config/systemd/user/musefs-harden-scan.service
mkdir -p ~/.config/systemd/user/musefs-harden-scan.service.d
cat > ~/.config/systemd/user/musefs-harden-scan.service.d/override.conf <<EOF
[Service]
EnvironmentFile=
Environment=PATH=$(dirname "$BIN"):/usr/bin
ExecStart=
ExecStart=$BIN scan $LIB --db $HARDEN/db/library.db --revalidate
EOF
systemctl --user daemon-reload
```
Expected: no error from `daemon-reload`.

- [ ] **Step 3: Run the sandboxed scan and verify the DB was written**

Run:
```bash
systemctl --user start musefs-harden-scan.service
systemctl --user status musefs-harden-scan.service --no-pager | sed -n '1,6p'
ls -l "$HARDEN/db/"
journalctl --user -u musefs-harden-scan.service --no-pager | tail -15
```
Expected: status shows `Deactivated successfully` / `code=exited, status=0`;
`$HARDEN/db/library.db` exists (with `-wal`/`-shm` siblings during the run);
the journal shows scan log lines (proves `RestrictAddressFamilies=AF_UNIX`
still reaches journald). If the unit failed with a syscall/permission error,
that is a directive the sandbox got wrong — drop the offending directive, add an
inline `# dropped: <reason>` comment, `daemon-reload`, and retry (the
empirical-fallback discipline from the spec).

- [ ] **Step 4: Verify a custom (non-default) DB path also works**

This is the case the dropped `ProtectSystem=strict`+`ReadWritePaths` would have
broken — `$HARDEN/custom-db` is nowhere near `~/.local/share/musefs`.

Run:
```bash
sed -i "s#--db $HARDEN/db/library.db#--db $HARDEN/custom-db/lib.db#" \
  ~/.config/systemd/user/musefs-harden-scan.service.d/override.conf
systemctl --user daemon-reload
systemctl --user start musefs-harden-scan.service
ls -l "$HARDEN/custom-db/"
```
Expected: `$HARDEN/custom-db/lib.db` written, exit 0 — confirming
`ProtectSystem=true` needs no per-path config.

- [ ] **Step 5: Record the exposure score as evidence**

Run:
```bash
systemd-analyze --user security musefs-harden-scan.service --no-pager | tail -3
```
Expected: an `Overall exposure level` line with a low score (the scanner is the
unit chasing a low score). Note it in the commit message or PR.

- [ ] **Step 6: Commit**

```bash
git add contrib/systemd/musefs-scan.service
git commit -m "$(cat <<'EOF'
feat(contrib): sandbox the musefs-scan.service systemd unit (#317)

The scanner creates no FUSE mount, so it takes the full systemd
sandbox. Uses ProtectSystem=true (not strict) so a custom MUSEFS_DB
path needs no ReadWritePaths edit. Verified live: sandboxed scan
writes the DB at both default and custom paths, journald logging
intact.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```
(The cargo gate runs here — no Rust changed, so it passes.)

---

## Task 2: #318 — Mount-visible hardening on `musefs.service`

**Files:**
- Modify: `contrib/systemd/musefs.service`

- [ ] **Step 1: Replace the trailing comment + `NoNewPrivileges` block**

In `contrib/systemd/musefs.service`, replace this existing block:

```ini
# NoNewPrivileges is safe for a FUSE mount. Do NOT add ProtectHome=,
# PrivateMounts=, or MountFlags=private: they place the mount in a private
# namespace and hide it from the rest of your session.
NoNewPrivileges=true
```

with:

```ini
# NoNewPrivileges is safe for a FUSE mount. Do NOT add ProtectHome=,
# ProtectSystem=, ReadOnlyPaths=, PrivateTmp=, PrivateMounts=, or
# MountFlags=private: they remount the path the FUSE mount lives under, or
# sever mount propagation, hiding the mount from the rest of your session.
#
# The directives below are safe: they do not touch the mountpoint's path and
# rely on systemd's default MountFlags=shared, which propagates the FUSE mount
# back to your session. (ProtectKernel*/ProtectControlGroups DO create a mount
# namespace, but only over /proc and /sys, so the $HOME mount still propagates.)
NoNewPrivileges=true
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectKernelLogs=true
ProtectControlGroups=true
ProtectClock=true
ProtectHostname=true
LockPersonality=true
RestrictRealtime=true
RestrictSUIDSGID=true
RestrictAddressFamilies=AF_UNIX
SystemCallArchitectures=native
#
# Opt-in (commented): these CAN trap the mount on a kernel/systemd older than
# the maintainer's test box (kernel 7.0 / systemd 259). Uncomment one at a time
# and confirm `mountpoint <your mountpoint>` still succeeds after a restart
# before keeping it.
#SystemCallFilter=@system-service @mount
#RestrictNamespaces=true
#CapabilityBoundingSet=
#MemoryDenyWriteExecute=true
```

- [ ] **Step 2: Install the mount unit under a test name with a scratch drop-in**

This reuses the DB built in Task 1. The mountpoint is under `$HOME`, which the
AppArmor `fusermount3` profile permits (mounting under `/data` would be denied —
that is an AppArmor policy, not a musefs bug).

Run:
```bash
export HARDEN="$HOME/.cache/musefs-harden-test"
export BIN="$PWD/target/release/musefs"
install -Dm644 contrib/systemd/musefs.service \
  ~/.config/systemd/user/musefs-harden-mount.service
mkdir -p ~/.config/systemd/user/musefs-harden-mount.service.d
cat > ~/.config/systemd/user/musefs-harden-mount.service.d/override.conf <<EOF
[Service]
EnvironmentFile=
Environment=PATH=$(dirname "$BIN"):/usr/bin
ExecStart=
ExecStart=$BIN mount $HARDEN/mnt --db $HARDEN/db/library.db
EOF
systemctl --user daemon-reload
```
Expected: `daemon-reload` succeeds.

- [ ] **Step 3: Start the mount and verify it is visible in the session**

Run:
```bash
systemctl --user start musefs-harden-mount.service
sleep 1
mountpoint "$HARDEN/mnt" && echo "MOUNT VISIBLE"
ls -R "$HARDEN/mnt" | head
# read a served file end-to-end:
find "$HARDEN/mnt" -type f | head -1 | xargs -r -I{} sh -c 'head -c 16 "{}" | xxd | head -1'
```
Expected: `mountpoint` prints `is a mountpoint` and `MOUNT VISIBLE`; the tree
lists synthesized entries; the file read returns bytes. **This is the gate** —
if the mount is not visible, a directive severed propagation; bisect by
commenting directives, record the cause inline, and retry.

- [ ] **Step 4: Spot-check one opt-in directive (document the opt-in path)**

Confirm the strictest opt-in directive still mounts on *this* (recent) box, so
the comment's "uncomment one at a time" guidance is real.

Run:
```bash
systemctl --user stop musefs-harden-mount.service
fusermount3 -u "$HARDEN/mnt" 2>/dev/null || true
cat > ~/.config/systemd/user/musefs-harden-mount.service.d/optin.conf <<'EOF'
[Service]
SystemCallFilter=@system-service @mount
EOF
systemctl --user daemon-reload
systemctl --user start musefs-harden-mount.service
sleep 1
mountpoint "$HARDEN/mnt" && echo "OPT-IN MOUNT OK"
```
Expected: `OPT-IN MOUNT OK` (validates `@mount` works on kernel 7.0 / systemd
259). Then tear down: `systemctl --user stop musefs-harden-mount.service; fusermount3 -u "$HARDEN/mnt" 2>/dev/null || true; rm ~/.config/systemd/user/musefs-harden-mount.service.d/optin.conf; systemctl --user daemon-reload`.

- [ ] **Step 5: Record the exposure score as evidence**

Run:
```bash
systemd-analyze --user security musefs-harden-mount.service --no-pager | tail -3
```
Expected: an `Overall exposure level` line. It will be **higher** (worse) than
the scanner's — that is correct, the mount unit intentionally omits the
mount-hiding directives. It is evidence, not a pass/fail gate.

- [ ] **Step 6: Commit**

```bash
git add contrib/systemd/musefs.service
git commit -m "$(cat <<'EOF'
feat(contrib): add mount-visible hardening to musefs.service (#318)

Adds the systemd directives that do not sever the FUSE mount's
propagation to the session (ProtectKernel*, ProtectControlGroups,
LockPersonality, Restrict*, RestrictAddressFamilies=AF_UNIX). The four
directives that can trap the mount on older kernels ship commented-out
as opt-in. Verified live: mount visible in session, served file reads;
opt-in @mount confirmed on kernel 7.0 / systemd 259.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Document the systemd hardening

**Files:**
- Modify: `contrib/systemd/README.md`

- [ ] **Step 1: Insert a `## Hardening` section**

In `contrib/systemd/README.md`, insert the following section immediately before
the existing `## Notes` heading:

```markdown
## Hardening

Both units ship sandboxed; no per-user path edits are required. The scanner uses
`ProtectSystem=true` (not `strict`), so a custom `MUSEFS_DB` location works
without a `ReadWritePaths=` change.

- `musefs-scan.service` takes the **full** systemd sandbox — it creates no FUSE
  mount, so namespace and mount-hiding directives are safe there.
- `musefs.service` takes only the directives that keep the FUSE mount visible in
  your session. A handful of stricter directives (`SystemCallFilter`,
  `RestrictNamespaces`, `CapabilityBoundingSet=`, `MemoryDenyWriteExecute`) ship
  **commented out** at the bottom of the unit — they can trap the mount on
  kernels older than the maintainer's test box. Uncomment them one at a time and
  confirm `mountpoint <your mountpoint>` still succeeds after a restart.

Inspect the result with `systemd-analyze --user security musefs-scan.service`.

```

- [ ] **Step 2: Verify the file still reads coherently**

Run:
```bash
sed -n '/## Hardening/,/## Notes/p' contrib/systemd/README.md
```
Expected: the new section prints, directly followed by the `## Notes` heading.

- [ ] **Step 3: Commit**

```bash
git add contrib/systemd/README.md
git commit -m "$(cat <<'EOF'
docs(contrib): document systemd unit hardening (#317 #318)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```
(Docs-only `*.md` commit — cargo gate skipped.)

---

## Task 4: #319 — Non-root container images (uid 1000)

**Files:**
- Modify: `docker/Dockerfile.glibc`
- Modify: `docker/Dockerfile.musl`
- Modify: `README.md`

- [ ] **Step 1: Edit `docker/Dockerfile.glibc`**

Replace the whole file with:

```dockerfile
# syntax=docker/dockerfile:1
FROM debian:bookworm-slim

# fuse3 provides the setuid `fusermount3` helper musefs execs at mount/unmount.
RUN apt-get update \
 && apt-get install -y --no-install-recommends fuse3 \
 && rm -rf /var/lib/apt/lists/*

# Run as a dedicated unprivileged user (uid/gid 1000). musefs mounts via the
# setuid fusermount3 helper and needs no root, so a bind-mounted store volume
# must be writable by uid 1000 (or pass `--user $(id -u):$(id -g)`).
# user_allow_other lets a non-root --allow-other / --owner mount pass musefs's
# /etc/fuse.conf pre-flight check (needed for the multi-container pod pattern).
RUN groupadd -g 1000 musefs \
 && useradd -u 1000 -g 1000 -M -s /usr/sbin/nologin musefs \
 && echo 'user_allow_other' >> /etc/fuse.conf

# TARGETARCH is auto-populated by buildx per platform (amd64 / arm64); the build
# context root holds <arch>/musefs for each.
ARG TARGETARCH
COPY ${TARGETARCH}/musefs /usr/local/bin/musefs

USER musefs
ENTRYPOINT ["musefs"]
```

- [ ] **Step 2: Edit `docker/Dockerfile.musl`**

Replace the whole file with:

```dockerfile
# syntax=docker/dockerfile:1
FROM alpine:3.20

# fuse3 provides the setuid `fusermount3` helper musefs execs at mount/unmount.
RUN apk add --no-cache fuse3

# Run as a dedicated unprivileged user (uid/gid 1000). musefs mounts via the
# setuid fusermount3 helper and needs no root, so a bind-mounted store volume
# must be writable by uid 1000 (or pass `--user $(id -u):$(id -g)`).
# user_allow_other lets a non-root --allow-other / --owner mount pass musefs's
# /etc/fuse.conf pre-flight check (needed for the multi-container pod pattern).
RUN addgroup -g 1000 musefs \
 && adduser -u 1000 -G musefs -H -D -s /sbin/nologin musefs \
 && echo 'user_allow_other' >> /etc/fuse.conf

# TARGETARCH is auto-populated by buildx per platform (amd64 / arm64); the build
# context root holds <arch>/musefs for each.
ARG TARGETARCH
COPY ${TARGETARCH}/musefs /usr/local/bin/musefs

USER musefs
ENTRYPOINT ["musefs"]
```

- [ ] **Step 3: Build both images from the staged context**

Run:
```bash
export CTX=/tmp/musefs-harden-ctx
cp docker/Dockerfile.glibc docker/Dockerfile.musl "$CTX/"
podman build -f "$CTX/Dockerfile.glibc" --build-arg TARGETARCH=amd64 -t musefs-harden:glibc "$CTX"
podman build -f "$CTX/Dockerfile.musl"  --build-arg TARGETARCH=amd64 -t musefs-harden:musl  "$CTX"
```
Expected: both builds succeed. (If the glibc image errors on `useradd`/`nologin`,
those are in `bookworm-slim` by default — re-check the RUN line.)

- [ ] **Step 4: Verify the entrypoint runs as uid 1000 (both images)**

Run:
```bash
for tag in glibc musl; do
  echo "== $tag =="
  podman run --rm --entrypoint id "musefs-harden:$tag" -u
done
```
Expected: each prints `1000`.

- [ ] **Step 5: Verify a non-root `scan` writes the store (ownership friction)**

Rootless Podman remaps container uid 1000 to a host subuid, so the bind-mounted
store must be made writable for that mapping — the real-world ownership step.

Run:
```bash
export STORE=/tmp/musefs-harden-store; rm -rf "$STORE"; mkdir -p "$STORE"
export LIB="${LIB:-/data/media/music}"         # narrow to one album for a fast container scan
podman unshare chown 1000:1000 "$STORE"        # "make the store writable by uid 1000"
podman run --rm \
  -v "$LIB":/library:ro \
  -v "$STORE":/store \
  musefs-harden:glibc scan /library --db /store/library.db --revalidate
podman unshare ls -ln "$STORE"
```
Expected: scan exits 0; `library.db` listed with owner `1000`. (This proves the
store-ownership requirement and that a non-root scan works.)

- [ ] **Step 6: Verify the `/etc/fuse.conf` pre-flight check both ways**

The baked `user_allow_other` must satisfy musefs's pre-flight; masking
`/etc/fuse.conf` with an empty file must trigger the explicit error. The
pre-flight runs *before* the privileged `mount()` syscall, so this validates the
fuse.conf plumbing without needing a successful in-container mount.

Run:
```bash
echo "== WITH baked user_allow_other (expect: past pre-flight) =="
podman run --rm --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined \
  -v /tmp/musefs-harden-store:/store --entrypoint sh musefs-harden:glibc \
  -c 'mkdir -p /tmp/m && musefs mount /tmp/m --db /store/library.db --allow-other 2>&1 | head -5' || true

echo "== WITHOUT user_allow_other (mask fuse.conf; expect: explicit error) =="
podman run --rm --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined \
  -v /dev/null:/etc/fuse.conf:ro \
  -v /tmp/musefs-harden-store:/store --entrypoint sh musefs-harden:glibc \
  -c 'mkdir -p /tmp/m && musefs mount /tmp/m --db /store/library.db --allow-other 2>&1 | head -5' || true
```
Expected: the WITH run does **not** print the "add `user_allow_other` to
`/etc/fuse.conf`" error (it gets past the check; any later failure is the
unprivileged-mount step, acceptable here). The WITHOUT run prints exactly that
pre-flight error. Repeat the WITH run with `musefs-harden:musl` to confirm
Alpine's `fusermount3` honours `/etc/fuse.conf` identically (the musl-specific
regression risk).

- [ ] **Step 7: (Optional) Full in-container mount under rootful Podman**

A real mount needs unnamespaced `CAP_SYS_ADMIN`; rootless Podman cannot grant
it (see the project's FUSE/CAP_SYS_ADMIN note). Skip unless you want end-to-end
confirmation:
```bash
sudo podman run --rm --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined \
  -v /tmp/musefs-harden-store:/store --entrypoint sh musefs-harden:glibc \
  -c 'mkdir -p /tmp/m && musefs mount /tmp/m --db /store/library.db & sleep 1; mountpoint /tmp/m'
```
Expected (if run): `/tmp/m is a mountpoint`. Note: running under `sudo` writes
the store as root, so do this *after* Step 5's ownership assertion, not before.

- [ ] **Step 8: Update `README.md` — non-root container note**

In `README.md`, immediately after the `CAP_SYS_ADMIN` paragraph that ends with
"...prefer running musefs on the host, which needs no such capability." (just
before the `#### The mount-visibility gotcha` heading), insert:

```markdown

#### Runs as a non-root user

The images run as a dedicated unprivileged user (uid/gid 1000), not root —
musefs mounts via the setuid `fusermount3` helper and needs no root of its own.
Two consequences for the commands above:

- The bind-mounted **store** volume must be writable by uid 1000. Either
  `chown 1000:1000 /path/to/store` on the host, or add `--user $(id -u):$(id -g)`
  to run as your own uid. The **library** volume is mounted `:ro`, so its
  ownership does not matter.
- The images include `user_allow_other` in `/etc/fuse.conf`, so a non-root
  `--allow-other` / `--owner` / `--group` mount (used by the pod pattern below)
  passes musefs's pre-flight check. See
  [Ownership and permissions](#ownership-and-permissions).
```

- [ ] **Step 9: Update `README.md` — cross-reference from the ownership note**

In `README.md`, in the "**Non-root mounts need `user_allow_other`**" paragraph
(in the *Ownership and permissions* section), append this sentence to the end of
that paragraph (after "...not a musefs restriction.)"):

```markdown
 The published container images already include this line, so non-root
`allow_other` mounts work out of the box there.
```

- [ ] **Step 10: Commit**

```bash
git add docker/Dockerfile.glibc docker/Dockerfile.musl README.md
git commit -m "$(cat <<'EOF'
feat(docker): run the musefs container as non-root uid 1000 (#319)

Both images create a dedicated uid/gid 1000 user and USER it, and bake
user_allow_other into /etc/fuse.conf so a non-root --allow-other mount
passes musefs's post-#339 pre-flight check (the multi-container pod
pattern). Documents the store-volume ownership requirement. Verified
live: entrypoint runs as 1000, non-root scan writes a 1000-owned DB,
fuse.conf pre-flight passes with the baked line and fails cleanly when
masked, on both glibc and musl images.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Tear down the live-test harness

Leaves no test units, mounts, images, or scratch dirs behind. **No commit.**

- [ ] **Step 1: Stop and remove test units + scratch**

Run:
```bash
systemctl --user stop musefs-harden-mount.service musefs-harden-scan.service 2>/dev/null || true
fusermount3 -u "$HOME/.cache/musefs-harden-test/mnt" 2>/dev/null || true
rm -rf ~/.config/systemd/user/musefs-harden-scan.service ~/.config/systemd/user/musefs-harden-scan.service.d
rm -rf ~/.config/systemd/user/musefs-harden-mount.service ~/.config/systemd/user/musefs-harden-mount.service.d
systemctl --user daemon-reload
rm -rf "$HOME/.cache/musefs-harden-test" /tmp/musefs-harden-ctx /tmp/musefs-harden-store
```
Expected: no errors; `ls ~/.config/systemd/user/ | grep harden` returns nothing.

- [ ] **Step 2: (Optional) Remove the test images**

Run:
```bash
podman rmi musefs-harden:glibc musefs-harden:musl 2>/dev/null || true
```

- [ ] **Step 3: Confirm the tree is clean and on-branch**

Run:
```bash
git status -sb
git log --oneline -4
```
Expected: clean working tree; the four feature commits (#317, #318, docs, #319)
on top of the rebased branch.

---

## Self-Review (completed during planning)

**Spec coverage:**
- #317 full path-agnostic scanner sandbox → Task 1 (directive block matches the spec's #317 block exactly, `ProtectSystem=true`, no `ReadWritePaths`).
- #318 mount-visible subset + commented opt-in → Task 2 (matches the spec's safe set + the four opt-in directives).
- #319 non-root uid 1000 + `user_allow_other` → Task 4.
- Doc updates: `contrib/systemd/README.md` → Task 3; `README.md` Docker + ownership → Task 4 Steps 8–9; inline unit/Dockerfile comments → Tasks 1, 2, 4.
- Verification (live, dedi): scanner default+custom DB path + journald (Task 1), mount visibility + opt-in spot-check + exposure scores (Task 2), uid 1000 + store ownership + fuse.conf pre-flight both ways on both images (Task 4).
- "Known sharp edges" (`ProcSubset=pid`, `ProtectControlGroups` on `--user`) → captured as inline comments in Task 1.

**Placeholder scan:** none — every edit shows full file/block content; every verification step has an exact command and expected output.

**Consistency:** the scratch env vars (`HARDEN`, `BIN`, `CTX`, `STORE`) and test
unit names (`musefs-harden-scan`, `musefs-harden-mount`) and image tags
(`musefs-harden:glibc|musl`) are used identically across Tasks 0–5. Task 5 cleans
up exactly what Tasks 0–4 create.

**Scope:** deployment-asset config only; no source/schema/FUSE-path changes, no
CI harness (per the spec's explicit live-test-not-CI decision).
