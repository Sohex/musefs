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
- `/dev/fuse` is mode `0666`; an unprivileged uid can open it. `fusermount3` is
  setuid-root, so the **container** mount works without `NoNewPrivileges`
  blocking the setuid escalation (the container is the one context where the
  setuid path is actually exercised).
- musefs is Rust with no JIT → `MemoryDenyWriteExecute=true` is safe.

## Design

### #317 — `musefs-scan.service`: full sandbox

Add the complete sandbox. The scanner reads the library (read-only, anywhere)
and writes only the DB directory.

```ini
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=read-only
ReadWritePaths=%h/.local/share/musefs   # DB dir; ADJUST if MUSEFS_DB lives elsewhere
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
RestrictAddressFamilies=AF_UNIX          # journald; tighten to none if logging survives
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
directory. A DB outside `~/.local/share/musefs` requires editing this line; the
library path needs no entry (reads are always allowed under
`ProtectSystem=strict`).

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

**Rationale for the boundary:** `ProtectKernel*` / `ProtectControlGroups` touch
`/proc` and `/sys`, not the home path under which the mount sits, so they do not
break propagation. The four directives that *could* break the mount —
`@mount` (could be over-restrictive), `RestrictNamespaces`,
`CapabilityBoundingSet=`, `MemoryDenyWriteExecute` — are resolved empirically by
the live test. Any that breaks the mount is dropped with an inline comment
recording why, turning the failure into documentation.

### #319 — Dockerfiles: non-root uid 1000

Both images: create a `musefs` group+user at gid/uid **1000**, add
`user_allow_other` to `/etc/fuse.conf` (so the documented multi-container pod
pattern's `allow_other` still works without root), then `USER musefs` before
`ENTRYPOINT`.

- **glibc / Debian:**
  `groupadd -g 1000 musefs && useradd -u 1000 -g 1000 -M -s /usr/sbin/nologin musefs`
- **musl / Alpine:**
  `addgroup -g 1000 musefs && adduser -u 1000 -G musefs -H -D -s /sbin/nologin musefs`
- `RUN echo 'user_allow_other' >> /etc/fuse.conf` (file created if absent)
- `USER musefs`

The store volume must be writable by uid 1000 — documented, not enforced in the
image (the chosen UID strategy: fixed 1000 + doc note, not a build arg).

## Documentation updates

- `contrib/systemd/README.md` — add a short "Hardening" note: units ship
  sandboxed; the scanner's `ReadWritePaths` is the one line to adjust for a
  non-default DB path.
- `README.md` Docker section — the image now runs as uid 1000; the bind-mounted
  store dir must be owned by / writable to 1000 (or pass
  `--user $(id -u):$(id -g)`).
- Inline comments in each unit / Dockerfile justifying the non-obvious
  directives.

## Verification (live, on the dedicated server)

1. `systemd-analyze --user security musefs.service` and
   `musefs-scan.service` — record before/after exposure scores as evidence.
2. Install both user units (mount under `$HOME` per the AppArmor-fusermount3
   constraint — `/data` mounts are denied by the host profile, not a musefs
   bug). Run a real `musefs-scan.service`; assert the DB is updated. Start
   `musefs.service`; assert the mount is visible in the session and a served
   file reads back correctly.
3. Build `Dockerfile.glibc`; run `scan` (DB write) and `mount` as the non-root
   uid with `--cap-add SYS_ADMIN --device /dev/fuse`; assert both succeed and
   the store write lands owned by uid 1000.

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
