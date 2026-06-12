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

Two of the three issues also intersect an **in-flight PR** touching FUSE
`allow_other` / `default_permissions`:

- **#318 (mount unit)** and **#319 (`/etc/fuse.conf` + container)** are **parked**
  until that PR lands, to avoid conflicting on the same surface.
- **#317 (scanner)** is independent of `allow_other`/`default_permissions` and
  can proceed on its own.

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
3. **Container** — drop from root to a fixed unprivileged uid/gid 1000. Mostly a
   docs change (volume ownership). Parked behind the `allow_other` PR.

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

### #319 — Dockerfiles: non-root uid 1000 (parked behind the `allow_other` PR)

Both images: create a `musefs` group+user at gid/uid **1000**, add
`user_allow_other` to `/etc/fuse.conf` (so the multi-container pod pattern's
`allow_other` still works without root), then `USER musefs`.

- **glibc / Debian:**
  `groupadd -g 1000 musefs && useradd -u 1000 -g 1000 -M -s /usr/sbin/nologin musefs`
- **musl / Alpine:**
  `addgroup -g 1000 musefs && adduser -u 1000 -G musefs -H -D -s /sbin/nologin musefs`
- `RUN echo 'user_allow_other' >> /etc/fuse.conf` (created if absent). **Musl
  caveat:** confirm Alpine's `fusermount3` reads `/etc/fuse.conf` and honours
  `user_allow_other` — verified in the live test, not assumed.
- `USER musefs`

The store volume must be writable by uid 1000 — **this is the real friction**:
the existing copy-paste `docker run -v /store …` breaks unless the host store
dir is owned by 1000 or `--user $(id -u):$(id -g)` is passed. This is a docs
change as much as an image change. Confirm no base image / compose default
injects `NoNewPrivileges` (would block the setuid escalation the container
relies on).

**This issue is parked** until the in-flight `allow_other`/`default_permissions`
PR lands, since both edit `/etc/fuse.conf` and the FUSE permission surface.

## Documentation updates

- `contrib/systemd/README.md` — short "Hardening" note: the units ship
  sandboxed; no per-user path edits are required (the scanner uses
  `ProtectSystem=true`, not `strict`). Mention the opt-in mount-unit directives.
- `README.md` Docker section — the image runs as uid 1000; the bind-mounted
  store dir must be owned by / writable to 1000 (or pass `--user`). Update the
  `docker run` example (`README.md:116-120`) and the pod example
  (`README.md:142-149`) so uid-1000 guidance does not contradict their existing
  `--security-opt apparmor=unconfined` lines. (Deferred with #319.)
- Inline comments in each unit / Dockerfile justifying the non-obvious
  directives.

## Implementation ordering

1. **#317 (scanner)** — independent of the `allow_other` PR; do this now. Single
   live `scan --revalidate` validates it.
2. **#318 (mount unit)** and **#319 (container)** — after the `allow_other` PR
   lands. #318 is a single edit (no prune loop now that the risky directives are
   opt-in/commented). #319 is image + docs.

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
2. **#318 (post-PR):** install the unit; assert the mount is visible in the
   session and a served file reads back correctly. `systemd-analyze` score is
   *evidence, not a gate* — the mount unit intentionally scores higher than the
   scanner. Spot-check that uncommenting one opt-in directive and restarting
   still mounts, to document the opt-in path.
3. **#319 (post-PR):** build **both** images; for each assert uid 1000 can open
   `/dev/fuse`, run `scan` + `mount` non-root with
   `--cap-add SYS_ADMIN --device /dev/fuse`, and confirm the store write is
   uid-1000-owned and `allow_other` works non-root (musl-specific regression
   risk).

## Out of scope (YAGNI)

- CI smoke harness — verification is the live dedi run.
- Build-arg-configurable container UID — fixed 1000 + doc note.
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
