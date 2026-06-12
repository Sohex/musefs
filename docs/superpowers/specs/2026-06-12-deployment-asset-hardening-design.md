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
  creates no FUSE mount and can therefore take the full systemd sandbox.
- `contrib/systemd/musefs.service` sets only `NoNewPrivileges=true`. Several
  hardening directives that do **not** sever the FUSE mount's propagation to the
  session are absent, so the unit does not enforce the README's "no root, no
  `CAP_SYS_ADMIN`" posture.
- `docker/Dockerfile.glibc` and `docker/Dockerfile.musl` set no `USER`, so
  `ENTRYPOINT ["musefs"]` runs as **root** inside a container that already needs
  `--cap-add SYS_ADMIN` + `--device /dev/fuse` — a larger blast radius than
  necessary, contradicting the systemd units' non-root guidance.

## Guiding principle

Apply the strictest sandbox each asset's mount-visibility constraint allows.
This yields three tiers:

1. **Scanner** — no FUSE mount → full sandbox, *including* namespace /
   mount-hiding directives.
2. **Mount unit** — only directives that do not remount the path the FUSE mount
   lives under, nor sever mount propagation to the session.
3. **Container** — drop from root to a fixed unprivileged uid/gid 1000.

`SystemCallFilter` uses systemd's curated `@system-service` group (maintained
across kernels) rather than a handcrafted allowlist (fragile across
distros/libc), plus the minimal extra group a given unit provably needs.

## Facts grounding the design

- musefs touches no network: local SQLite + positioned file reads. No sockets
  beyond journald's `AF_UNIX`.
- Documented DB convention (`contrib/systemd/musefs.conf.example:20`):
  `~/.local/share/musefs/library.db`. There is no hardcoded default in the CLI;
  the DB is supplied via `MUSEFS_DB`/flag.
- The library may live outside `$HOME` (e.g. `/data`); reads remain permitted
  under `ProtectSystem=strict` (read-only ≠ inaccessible).
- On the **host**, `/dev/fuse` is mode `0666`; an unprivileged uid can open it.
  Inside a container the device node is created by `--device /dev/fuse` as
  `root:root` with the host's mode — it is the *in-container* perms that govern
  uid 1000's access, so the live test must assert `/dev/fuse` is openable by the
  non-root user as a named check (see Verification).
- `fusermount3` is setuid-root, so the **container** mount works without
  `NoNewPrivileges` blocking the setuid escalation (the container is the one
  context where the setuid path is actually exercised). The **systemd** units
  keep `NoNewPrivileges=true`, which *does* neutralize that setuid — they rely
  instead on **unprivileged FUSE** (kernel ≥ 4.18 + fusermount3 ≥ 3.x mounting
  in the caller's own user/mount context). This is the version floor: on an
  older kernel/fusermount3 the existing `NNP=true` mount unit would itself not
  work. Dedi baseline (verified): systemd 259, fusermount3 3.18.2, kernel 7.0.
- musefs is Rust with no JIT → `MemoryDenyWriteExecute=true` is safe. The
  scanner crates pull in no `mmap`-exec path; the only `memmap2` user is
  `musefs-fuse` (not the scanner). The SQLite WAL `-shm` mapping is
  `MAP_SHARED` read/write, not anonymous W+X, so it survives `MDWE`.

## Design

### #317 — `musefs-scan.service`: full sandbox

Add the complete sandbox. The scanner reads the library (read-only, anywhere)
and writes only the DB directory.

```ini
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=read-only
ReadWritePaths=%h/.local/share/musefs   # the DB *directory*; ADJUST if MUSEFS_DB lives elsewhere
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
RestrictAddressFamilies=AF_UNIX          # REQUIRED for journald (/run/systemd/journal/socket); do NOT drop to none — it silently kills logs
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

**Friction (documented inline):** `ReadWritePaths` must name the DB's
**directory, not the file** — SQLite in WAL mode (confirmed:
`musefs-db/src/lib.rs:86`) writes `library.db-wal` and `library.db-shm` as
*siblings* in that directory, so a file-scoped RW path would silently break WAL
creation. A DB outside `~/.local/share/musefs` requires editing this line; the
library path needs no entry (reads are always allowed under
`ProtectSystem=strict`, including a library on `/data`). `PrivateTmp=true` gives
SQLite a private `/tmp` for any temp-store spill.

**Empirical-fallback discipline (same as the mount unit):** the `@system-service`
filter + `SystemCallErrorNumber=EPERM` combination can return `EPERM` (not
`ENOSYS`) for a filtered syscall, which some libc paths handle poorly. The live
test runs a real `scan --revalidate`; any directive that breaks the scan is
dropped with an inline comment recording the failure, exactly as for #318.

### #318 — `musefs.service`: mount-visible hardening

Keep `NoNewPrivileges=true` and the existing "do NOT add" comment; **extend**
that comment to state which new directives are safe and why. Add only
directives that do not create a private mount namespace over the mountpoint path
or sever propagation:

```ini
CapabilityBoundingSet=
RestrictAddressFamilies=AF_UNIX
RestrictNamespaces=true
LockPersonality=true
MemoryDenyWriteExecute=true
RestrictRealtime=true
RestrictSUIDSGID=true
ProtectClock=true
ProtectHostname=true
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectKernelLogs=true
ProtectControlGroups=true
SystemCallArchitectures=native
SystemCallFilter=@system-service @mount   # @mount: fusermount3 needs mount/umount2
```

**Forbidden (still):** `Protect{System,Home}=`, `ReadOnlyPaths=`,
`ReadWritePaths=`, `PrivateTmp=`, `PrivateMounts=`, `MountFlags=private` — each
either remounts the home path the FUSE mount lives under or severs propagation,
hiding the mount from the session.

**Rationale for the boundary (the load-bearing invariant):**
`ProtectKernel*` / `ProtectControlGroups` *do* place the unit in a private mount
namespace (they bind-mount `/proc/sys`, `/sys`, `/sys/fs/cgroup` read-only). The
reason a FUSE mount the service later creates under `$HOME` is **still visible
in the session** is systemd's default `MountFlags=shared`: mounts the unit makes
propagate *back* to the host via shared propagation. The Forbidden list is
forbidden precisely because it severs that — `MountFlags=private` /
`PrivateMounts=` switch propagation to private/slave and trap the mount, and
`Protect{System,Home}=` remount the very path the mount lives under. **This
spec's entire mount-unit hardening depends on default shared propagation; if any
future edit sets `MountFlags=private`, every "safe" directive above instantly
becomes mount-hiding.**

The directives that could *still* break the mount on this kernel —
`SystemCallFilter=@system-service @mount` (over-restrictive `@mount`),
`RestrictNamespaces`, `CapabilityBoundingSet=`, `MemoryDenyWriteExecute` — are
resolved empirically by the live test. On the dedi, `@mount` was verified to
include the new mount-API syscalls (`fsopen`/`fsmount`/`move_mount`/
`open_tree_attr`) that unprivileged FUSE uses on recent kernels; an *older*
systemd's `@mount` predates them, so a `@mount`-caused failure is a distinct
branch from a `RestrictNamespaces`-caused one. Any directive that breaks the
mount is dropped with an inline comment recording why, turning the failure into
documentation.

### #319 — Dockerfiles: non-root uid 1000

Both images: create a `musefs` group+user at gid/uid **1000**, add
`user_allow_other` to `/etc/fuse.conf` (so the documented multi-container pod
pattern's `allow_other` still works without root), then `USER musefs` before
`ENTRYPOINT`.

- **glibc / Debian:**
  `groupadd -g 1000 musefs && useradd -u 1000 -g 1000 -M -s /usr/sbin/nologin musefs`
- **musl / Alpine:**
  `addgroup -g 1000 musefs && adduser -u 1000 -G musefs -H -D -s /sbin/nologin musefs`
- `RUN echo 'user_allow_other' >> /etc/fuse.conf` (file created if absent).
  **Musl caveat:** confirm Alpine's `fusermount3` reads `/etc/fuse.conf` from
  the same path and honours `user_allow_other` identically — this is verified in
  the live test, not assumed, since the `allow_other` pod pattern is the one
  thing the non-root switch could regress.
- `USER musefs`

The store volume must be writable by uid 1000 — documented, not enforced in the
image (the chosen UID strategy: fixed 1000 + doc note, not a build arg).

`/dev/fuse` access is governed by the **in-container** device-node perms (Docker
creates it `root:root` with the host mode under `--device`), not the host's
`0666` — so the live test asserts uid 1000 can open it explicitly. Confirm no
base image / compose default injects `NoNewPrivileges` into the container, which
would block the `fusermount3` setuid escalation the container path relies on.

## Documentation updates

- `contrib/systemd/README.md` — add a short "Hardening" note: units ship
  sandboxed; the scanner's `ReadWritePaths` is the one line to adjust for a
  non-default DB path.
- `README.md` Docker section — the image now runs as uid 1000; the bind-mounted
  store dir must be owned by / writable to 1000 (or pass
  `--user $(id -u):$(id -g)`). Update the `docker run` example block
  (`README.md:116-120`) and the multi-container pod example (`README.md:142-149`)
  so the uid-1000 guidance does not contradict their existing
  `--security-opt apparmor=unconfined` lines.
- Inline comments in each unit / Dockerfile justifying the non-obvious
  directives.

## Implementation ordering & rollback

The three edits are independent *as files* but #317 and #318 are not atomic —
each is an **edit → live-test → prune-failed-directive → re-test** loop (the
spec expects some mount-unit directives to be dropped mid-test). Order:

1. **#319 (Dockerfiles)** first — no propagation subtlety, fastest to validate,
   unblocks the container doc update.
2. **#317 (scanner)** next — full sandbox, single live `scan` validates it; its
   prune loop is independent of the mount unit.
3. **#318 (mount unit)** last — the riskiest, with the empirical prune loop.

**Rollback / partial failure:** each unit/Dockerfile's pre-hardening version is
recoverable from git; a failed mount-unit live test must leave the **committed**
unit in its last-known-good directive set, never the in-flight experimental one.
The pre-commit cargo gate is **skipped** for these (docs/config-only paths), and
the shell/YAML legs do not lint `.service`/`Dockerfile` content — so the real
gate is the manual live run, and each commit must represent a directive set that
actually passed it. Do not commit an untested directive set.

## Verification (live, on the dedicated server)

1. `systemd-analyze --user security musefs.service` and
   `musefs-scan.service` — record before/after exposure scores **as evidence,
   not a pass/fail gate**. The mount unit deliberately omits the mount-hiding
   directives, so its score is *expected* to stay higher than the scanner's;
   that is correct, not a regression. The scanner is the unit chasing a low
   score.
2. Install both user units (mount under `$HOME` per the AppArmor-fusermount3
   constraint — `/data` mounts are denied by the host profile, not a musefs
   bug). Run a real `musefs-scan.service`; assert the DB **and its `-wal`/`-shm`
   siblings** are written. Start `musefs.service`; assert the mount is visible
   in the session and a served file reads back correctly. The `musefs-scan.timer`
   is unchanged (hardening lives on the service); confirm it still triggers.
3. Build **both** `Dockerfile.glibc` and `Dockerfile.musl`. For each: assert
   uid 1000 can open `/dev/fuse`; run `scan` (DB write) and `mount` as the
   non-root uid with `--cap-add SYS_ADMIN --device /dev/fuse`; assert both
   succeed, the store write lands owned by uid 1000, and `allow_other` works
   non-root (the `user_allow_other` path — the musl-specific regression risk).

## Out of scope (YAGNI)

- CI smoke harness — verification is the live dedi run, not a committed CI test.
- Build-arg-configurable container UID — fixed 1000 + doc note was chosen.
- Any change to the binary, the FUSE/read path, or the store schema. This is
  deployment-asset config only.

## Risks

- A `SystemCallFilter` / `RestrictNamespaces` / `CapabilityBoundingSet` choice
  silently breaks the FUSE mount. **Mitigation:** these surface only at runtime,
  so the live test on the mount unit is mandatory, not optional; failures are
  recorded inline as documentation.
- The non-root container regresses the multi-container `allow_other` pod
  pattern. **Mitigation:** `user_allow_other` in the image; the pod pattern is
  exercised in the container live test if feasible.
- Distro-specific user-creation flag drift (Debian `useradd` vs Alpine
  `adduser`). **Mitigation:** both image builds are exercised in the live test.

**Known sharp edges (carry into inline comments, not blockers):**

- `ProcSubset=pid` + `ProtectProc=invisible` on the scanner hide non-pid `/proc`
  entries. Safe today, but a future media parser that reads `/proc/cpuinfo` for
  SIMD detection would break — note it in the unit comment.
- `ProtectControlGroups=true` on a `--user` unit remounts `/sys/fs/cgroup`
  read-only in the namespace; confirm it does not interfere with the user
  manager's own cgroup bookkeeping for the unit (expected harmless).
- `RestrictNamespaces=true` on the scanner is safe only because the scanner
  spawns no namespace-cloning helper (probe workers are plain threads) — verify
  during the live scan.
