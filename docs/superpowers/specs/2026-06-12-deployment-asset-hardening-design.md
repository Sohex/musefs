# Deployment-asset hardening: systemd units + container images

**Date:** 2026-06-12
**Issues:** #317 (scanner unit), #318 (mount unit), #319 (container user)
**Tracking:** #280 · Audit: Task 29 (`docker-systemd-assets`)

## Problem

The shipped deployment assets carry defense-in-depth hardening gaps. None is a
live exploit; all are hardening of assets that handle untrusted media or run in
already-privileged contexts.

- `contrib/systemd/musefs-scan.service` runs the oneshot scanner — which parses
  adversarial media bytes — with **zero** sandboxing directives, even though it
  creates no FUSE mount and can therefore take a strong sandbox.
- `contrib/systemd/musefs.service` sets only `NoNewPrivileges=true`. A few
  hardening directives that do **not** sever the FUSE mount's propagation to the
  session are absent.
- `docker/Dockerfile.glibc` and `docker/Dockerfile.musl` set no `USER`, so
  `ENTRYPOINT ["musefs"]` runs as **root** inside a container that already needs
  `--cap-add SYS_ADMIN` + `--device /dev/fuse` — a larger blast radius than
  necessary.

## Scope decision: right-sized, not a full checklist sweep

An earlier draft applied every systemd directive each asset could technically
take. That was over-built: most of the marginal directives defend an
already-unprivileged, socket-less, network-less daemon against threats it does
not face, while the riskiest ones can **break the FUSE mount** or impose
user-visible friction. This spec keeps only directives that are *both* worth the
risk and friction-free, and moves the rest to an explicit opt-in bucket.

The FUSE `allow_other` / `default_permissions` PR (**#339**, merged) has now
landed and this branch is rebased on it. What shipped changes #319's rationale:

- An explicit `--allow-other` flag (implied by `--owner`/`--group`) and a
  `MUSEFS_ALLOW_OTHER` env var.
- musefs now **pre-flight-checks `/etc/fuse.conf` for `user_allow_other`** and
  fails with an explanatory error when a non-root `allow_other` mount lacks it
  (`musefs-fuse/src/platform/mount.rs`).

Consequence: switching the container to non-root (**#319**) makes the
`user_allow_other` line in the image **required** to preserve the documented
multi-container pod pattern (a non-root `--allow-other` mount now hard-fails the
pre-flight without it), not the optional polish the earlier draft treated it as.
All three issues are now unblocked.

## Guiding principle

Apply the strongest sandbox each asset can take **without path-configuration
friction and without risking the mount**. This yields three proportionate tiers:

1. **Scanner** — no FUSE mount → full *path-agnostic* sandbox. The justified
   one: it is the sole component ingesting adversarial bytes, so containing a
   parser RCE has real value. Deliberately omits write-path confinement
   (`ProtectSystem=strict` + `ReadWritePaths=`), whose only marginal gain is
   confining writes the scanner already restricts to the DB by design — at the
   cost of every custom-`MUSEFS_DB` user silently failing their scan.
2. **Mount unit** — only the safe-anywhere, friction-free directives. The
   mount-capable / namespace / syscall-filter directives are **opt-in**, because
   they can trap the mount on kernels older than the dev box.
3. **Container** — drop from root to an unprivileged user (default uid/gid 1000,
   build-arg configurable). Mostly a docs change (volume ownership), plus the
   now-required `user_allow_other` line.

## Facts grounding the design

- musefs touches no network: local SQLite + positioned file reads. The only
  socket need is journald's `AF_UNIX`.
- The library may live outside `$HOME` (e.g. `/data`); `ProtectSystem=true`
  leaves `/home` and arbitrary data paths writable/readable, so no path config
  is required (unlike `strict`).
- On the **host**, `/dev/fuse` is mode `0666`; an unprivileged uid can open it.
  Inside a container the device node is created by `--device /dev/fuse` as
  `root:root` with the host's mode — it is the *in-container* perms that govern
  uid 1000's access, so the live test must assert `/dev/fuse` is openable by the
  non-root user.
- `fusermount3` is setuid-root, so the **container** mount works without
  `NoNewPrivileges` blocking the setuid escalation. The **systemd** units keep
  `NoNewPrivileges=true`, which *does* neutralize that setuid — they rely
  instead on **unprivileged FUSE** (kernel ≥ 4.18 + fusermount3 ≥ 3.x). Dedi
  baseline (verified): systemd 259, fusermount3 3.18.2, kernel 7.0 — newer than
  most users, which is exactly why the mount unit's risky directives are opt-in
  rather than shipped on.
- musefs is Rust with no JIT → `MemoryDenyWriteExecute=true` is safe where used
  (the scanner pulls in no `mmap`-exec path; `memmap2` lives only in
  `musefs-fuse`).

## Design

### #317 — `musefs-scan.service`: full path-agnostic sandbox

The scanner reads the library (anywhere) and writes the DB. None of these
directives needs to know where either lives:

```ini
NoNewPrivileges=true
ProtectSystem=true                       # /usr,/boot,/etc read-only; /home & data stay writable — no path config
PrivateTmp=true
PrivateDevices=true
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectKernelLogs=true
ProtectControlGroups=true
ProtectClock=true
ProtectHostname=true
ProtectProc=invisible
ProcSubset=pid
RestrictNamespaces=true
RestrictAddressFamilies=AF_UNIX          # REQUIRED for journald; do NOT drop to none (silently kills logs)
RestrictRealtime=true
RestrictSUIDSGID=true
LockPersonality=true
MemoryDenyWriteExecute=true
CapabilityBoundingSet=                    # empty
SystemCallFilter=@system-service
SystemCallArchitectures=native
SystemCallErrorNumber=EPERM
UMask=077
```

**Dropped from the earlier draft (friction):** `ProtectSystem=strict`,
`ProtectHome=read-only`, `ReadWritePaths=%h/.local/share/musefs`. Those confine
writes to a single declared directory — which forces every user with a custom
`MUSEFS_DB` to edit a *second*, non-obvious line or hit a confusing read-only
error. The scanner already only writes the DB by design, so the lost
confinement is low-value; `ProtectSystem=true` keeps system dirs read-only with
zero path coupling.

**Empirical-fallback discipline:** `@system-service` + `SystemCallErrorNumber=
EPERM` can return `EPERM` (not `ENOSYS`) for a filtered syscall, which some libc
paths handle poorly. The live test runs a real `scan --revalidate`; any
directive that breaks the scan is dropped with an inline comment recording why.

### #318 — `musefs.service`: safe-anywhere subset (the rest opt-in)

Keep `NoNewPrivileges=true` and the existing "do NOT add" comment; extend it to
state which new directives are safe and why. Add only directives that cannot
trap the mount on any supported kernel:

```ini
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
```

**Why these are safe:** `ProtectKernel*` / `ProtectControlGroups` *do* place the
unit in a private mount namespace (they bind-mount `/proc/sys`, `/sys`,
`/sys/fs/cgroup` read-only), but a FUSE mount the service later makes under
`$HOME` is **still visible in the session** because of systemd's default
`MountFlags=shared`: the mount propagates back to the host. The existing
Forbidden list (`Protect{System,Home}=`, `PrivateMounts=`, `MountFlags=private`)
is forbidden precisely because it severs that propagation or remounts the
mountpoint's parent.

**Opt-in / deferred (documented in the unit comment, commented out):**

```ini
#SystemCallFilter=@system-service @mount   # @mount can be stale on older systemd → mount fails
#RestrictNamespaces=true                    # unprivileged-FUSE path may need a namespace on some kernels
#CapabilityBoundingSet=                     # NNP already neuters fusermount3 setuid; net gain marginal
#MemoryDenyWriteExecute=true                # read-only mmap is safe in principle, unverified across platforms
```

These four are the ones that can break the mount on a kernel/systemd older than
the dev box. They are shipped **commented out**, with a note that a user on a
known-recent platform can uncomment them after confirming the mount still
appears. We do not ship them on, because the dedi (kernel 7.0 / systemd 259) is
not representative — "works here" does not generalize to Debian 12.

### #319 — Dockerfiles: non-root user (default uid/gid 1000, build-arg configurable)

Both images: create a `musefs` group+user at a **build-arg-configurable** uid/gid
(`ARG MUSEFS_UID=1000` / `MUSEFS_GID=1000`), add `user_allow_other` to
`/etc/fuse.conf` (now **required** for the non-root pod pattern — post-#339
musefs pre-flight-checks for it and hard-fails an `allow_other`/`--owner`/
`--group` mount without it), then `USER musefs`.

- **glibc / Debian:**
  `groupadd -g "$MUSEFS_GID" musefs && useradd -u "$MUSEFS_UID" -g "$MUSEFS_GID" -M -s /usr/sbin/nologin musefs`
- **musl / Alpine:**
  `addgroup -g "$MUSEFS_GID" musefs && adduser -u "$MUSEFS_UID" -G musefs -H -D -s /sbin/nologin musefs`
- `RUN echo 'user_allow_other' >> /etc/fuse.conf` (created if absent). **Musl
  caveat:** confirm Alpine's `fusermount3` reads `/etc/fuse.conf` and honours
  `user_allow_other` — verified in the live test, not assumed.
- `USER musefs` (by name — works regardless of the chosen numeric uid).

The store volume must be writable by the chosen uid — **this is the real
friction**: the existing copy-paste `docker run -v /store …` breaks unless the
host store dir is owned by that uid, or `--user $(id -u):$(id -g)` is passed.
The build arg is the friction-reducer for self-builders: `--build-arg
MUSEFS_UID=$(id -u) --build-arg MUSEFS_GID=$(id -g)` bakes an image whose user
matches the host owner, so no chown or `--user` is needed at run time (the
published image defaults to 1000). Confirm no base image / compose default
injects `NoNewPrivileges` (would block the setuid escalation the container
relies on).

A plain single-container mount read only by uid 1000 itself needs **no**
`allow_other`; the `user_allow_other` line matters only for the
cross-container / owner-presenting case, and is harmless when unused.

## Documentation updates

- `contrib/systemd/README.md` — short "Hardening" note: the units ship
  sandboxed; no per-user path edits are required (the scanner uses
  `ProtectSystem=true`, not `strict`). Mention the opt-in mount-unit directives.
- `README.md` Docker section — the image runs as a non-root user (default uid
  1000); the bind-mounted store dir must be owned by / writable to that uid (or
  pass `--user`, or build with `--build-arg MUSEFS_UID=$(id -u)`). Add this as a
  single central "Runs as a non-root user" note inserted between the `docker run`
  example and the pod example (rather than editing each example inline — keeps
  the examples copy-pasteable and the guidance in one place). Cross-reference the
  "Non-root mounts need `user_allow_other`" note (`README.md:297-302`), since the
  non-root image now
  relies on the image-baked `user_allow_other` for that pattern.
- Inline comments in each unit / Dockerfile justifying the non-obvious
  directives.

## Implementation ordering

All three are unblocked (#339 is merged). Suggested order by ascending risk:

1. **#317 (scanner)** — simplest; a single live `scan --revalidate` validates it.
2. **#318 (mount unit)** — a single edit (no prune loop now that the risky
   directives are opt-in/commented).
3. **#319 (container)** — image + docs; exercise the non-root `allow_other` pod
   path against the now-merged pre-flight check.

**Rollback:** each asset's pre-hardening version is recoverable from git. The
pre-commit cargo gate is **skipped** for these (docs/config-only paths) and the
shell/YAML legs do not lint `.service`/`Dockerfile` content — so the real gate
is the manual live run; do not commit an untested directive set.

## Verification (live, on the dedicated server)

1. **#317:** `systemd-analyze --user security musefs-scan.service` (record the
   exposure score as evidence). Install the unit, run a real
   `musefs-scan.service`; assert the DB (and `-wal`/`-shm` siblings) are written
   for a DB both at the default location and at a **custom `MUSEFS_DB` path** —
   the case the dropped `ReadWritePaths` would have broken. Confirm the scanner
   spawns no namespace-cloning helper (probe workers are plain threads, so
   `RestrictNamespaces` is safe).
2. **#318:** install the unit; assert the mount is visible in the session and a
   served file reads back correctly. `systemd-analyze` score is *evidence, not a
   gate* — the mount unit intentionally scores higher than the scanner.
   Spot-check that uncommenting one opt-in directive and restarting still mounts,
   to document the opt-in path.
3. **#319:** build **both** images at the default uid and confirm a build-arg
   override (`--build-arg MUSEFS_UID=…`) takes effect; for each assert the uid can open
   `/dev/fuse`, run `scan` + `mount` non-root with
   `--cap-add SYS_ADMIN --device /dev/fuse`. Confirm the store write is
   uid-1000-owned, and that a non-root `--allow-other` (or `--owner`) mount
   passes the merged `/etc/fuse.conf` pre-flight check thanks to the image-baked
   `user_allow_other` — and fails cleanly if that line is removed (musl-specific
   regression risk).

## Out of scope (YAGNI)

- CI smoke harness — verification is the live dedi run.
- Write-path confinement on the scanner (`ProtectSystem=strict` +
  `ReadWritePaths`) — dropped for friction; revisit only if a concrete
  parser-write threat appears.
- Aggressive mount-unit syscall/namespace hardening — shipped opt-in, not on.
- Any change to the binary, the FUSE/read path, or the store schema.

## Known sharp edges (inline-comment level)

- `ProcSubset=pid` + `ProtectProc=invisible` on the scanner hide non-pid
  `/proc`. Safe today; a future parser reading `/proc/cpuinfo` for SIMD
  detection would break — note it in the comment.
- `ProtectControlGroups=true` on a `--user` unit remounts `/sys/fs/cgroup`
  read-only in the namespace; confirm it does not interfere with the user
  manager's cgroup bookkeeping (expected harmless).
