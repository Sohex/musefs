# Running musefs as a systemd user service

These units run musefs on the host (the recommended deployment) under your own
user account — no root, no `CAP_SYS_ADMIN`.

## Files

- `musefs.service` — the mount daemon (`musefs mount`); blocks until stopped.
- `musefs-scan.service` + `musefs-scan.timer` — optional daily
  `musefs revalidate --prune` over the library.
- `musefs.conf.example` — the common `MUSEFS_*` settings, commented with
  defaults.

## Install

```bash
mkdir -p ~/.config/systemd/user ~/.config/musefs
cp musefs.service musefs-scan.service musefs-scan.timer ~/.config/systemd/user/
cp musefs.conf.example ~/.config/musefs/musefs.conf
$EDITOR ~/.config/musefs/musefs.conf   # set MUSEFS_MOUNTPOINT and MUSEFS_DB
systemctl --user daemon-reload
systemctl --user enable --now musefs.service
```

Enable the periodic re-scan too (edit the library path in
`musefs-scan.service` first). It runs `musefs revalidate <library> --prune`,
which refreshes the rows of files that changed on disk, and deletes the rows of
files that are gone or that this version refuses as unsupported (a chained Ogg
an older musefs stored), taking their curated tags with them. It adds no new
files; run `musefs scan` over the library for those.

```bash
systemctl --user enable --now musefs-scan.timer
```

## Hardening

These units run under the `--user` manager, which constrains what systemd
sandboxing is possible. The two units differ sharply:

- **`musefs-scan.service` is fully sandboxed.** The scanner creates no FUSE
  mount, so it takes a strong sandbox (`ProtectSystem=true`, `SystemCallFilter=`,
  plus namespace and seccomp restrictions). `ProtectSystem=true` (not `strict`)
  keeps system directories read-only while leaving your library and `MUSEFS_DB`
  writable, so a custom DB location needs no `ReadWritePaths=` edit. A few
  directives that require capability-bounding-set drops
  (`CapabilityBoundingSet=`, `PrivateDevices`, `ProtectKernelModules`,
  `ProtectKernelLogs`, `ProtectClock`) are omitted: the unprivileged user
  manager cannot apply them, and the process is already capability-less, so
  nothing is lost. Inspect with
  `systemd-analyze --user security musefs-scan.service`.

- **`musefs.service` is intentionally *not* sandboxed, and cannot be.** musefs
  mounts via the **setuid** `fusermount3` helper. `NoNewPrivileges=true` — and
  nearly every other systemd hardening directive, since installing a seccomp
  filter for an unprivileged process forces the kernel `no_new_privs` flag —
  disables the setuid escalation, and the mount then fails with `fusermount3:
  mount failed: Operation not permitted`. The unit comment explains this in
  full.

## Notes

- **The store must exist before the mount starts.** `musefs mount` never
  creates the DB — it requires a populated store and exits non-zero otherwise,
  so a mount unit that starts before anything has scanned hard-fails and (with
  `Restart=on-failure`) crash-loops. Seed the store with an initial
  `musefs scan` before `enable --now musefs.service`. If you generate the store
  from another unit, order this one after it with a drop-in
  (`systemctl --user edit musefs`):

  ```ini
  [Unit]
  After=musefs-initial-scan.service
  Requires=musefs-initial-scan.service
  ```

  (The `musefs-scan.timer` is a periodic *re-scan*, not the initial seed.)
- **Upgrading to 2.0.0.** A 2.0.0 `musefs mount` refuses a 1.x store until
  `musefs migrate` upgrades it, and with `Restart=on-failure` the mount unit
  restart-loops on that refusal. `migrate` itself refuses while a mount or scan
  has the store open, so stop the units first, upgrade, then start them again:

  ```bash
  systemctl --user stop musefs.service musefs-scan.timer musefs-scan.service
  musefs migrate --db ~/.local/share/musefs/library.db   # your MUSEFS_DB
  systemctl --user start musefs.service musefs-scan.timer
  ```

  Run `migrate` from a terminal, or pass `--yes`: with no terminal to ask on it
  will not upgrade without it. Delete any `MUSEFS_REVALIDATE`, `MUSEFS_FAST` or
  `MUSEFS_STRICT` line from `musefs.conf` as well, since `scan` refuses to start
  while one is set. Until a revalidate has re-probed every track, the journal
  shows a `warning: N track(s) have not been re-probed since the store was
  upgraded …` line on each mount start and scan run. See
  [Upgrading from v1.3.0](../release-notes.md#upgrading-from-v130).
- **Binary location.** The `--user` manager does not inherit your shell's
  `PATH`. The units set `PATH` for a `cargo install` binary in `~/.cargo/bin`;
  if musefs is elsewhere, edit the `Environment=PATH=` line (or make
  `ExecStart` an absolute path).
- **`%h` vs `~`.** Unit files expand `%h` to your home directory; the
  `musefs.conf` EnvironmentFile does **not** expand `%h` or `~` — use absolute
  paths there, and never paste `~/...` into a unit directive (it is taken
  literally).
- **Settings.** `musefs.conf.example` is a commented example of the common
  `MUSEFS_*` mount/scan variables; it leaves some out, such as
  `MUSEFS_WORKERS` and `MUSEFS_CHECKSUM`. Every scalar `mount`, `scan` and
  `revalidate` flag except `--dry-run` has a `MUSEFS_*` form — uppercase the
  long flag, dashes to underscores. Explicit flags override env vars;
  `--fallback` and scan targets are command-line only (set them in
  `ExecStart`).
- **Inline overrides.** Prefer `systemctl --user edit musefs` to add
  `Environment=` lines in a drop-in; it survives reinstalls.
- **Headless servers.** A `--user` timer only fires while your user manager
  runs. For a daily scan when you are not logged in:
  `loginctl enable-linger <user>`.
- **Logs.** `journalctl --user -u musefs -f`. The units log at `warn` by
  default; raise it with `Environment=RUST_LOG=info` in a drop-in or in
  `musefs.conf` — see
  [Logging & troubleshooting](../guide/troubleshooting.md#reading-the-logs-under-systemd).
