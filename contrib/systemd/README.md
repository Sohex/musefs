# Running musefs as a systemd user service

These units run musefs on the host (the recommended deployment) under your own
user account — no root, no `CAP_SYS_ADMIN`.

## Files

- `musefs.service` — the mount daemon (`musefs mount`); blocks until stopped.
- `musefs-scan.service` + `musefs-scan.timer` — optional periodic
  `musefs scan --revalidate`.
- `musefs.conf.example` — every `MUSEFS_*` setting, commented with defaults.

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
`musefs-scan.service` first):

```bash
systemctl --user enable --now musefs-scan.timer
```

## Notes

- **Binary location.** The `--user` manager does not inherit your shell's
  `PATH`. The units set `PATH` for a `cargo install` binary in `~/.cargo/bin`;
  if musefs is elsewhere, edit the `Environment=PATH=` line (or make
  `ExecStart` an absolute path).
- **`%h` vs `~`.** Unit files expand `%h` to your home directory; the
  `musefs.conf` EnvironmentFile does **not** expand `%h` or `~` — use absolute
  paths there, and never paste `~/...` into a unit directive (it is taken
  literally).
- **Settings.** `musefs.conf.example` is the full, canonical list of
  `MUSEFS_*` variables. Explicit flags override env vars; `--fallback` and scan
  targets are command-line only (set them in `ExecStart`).
- **Inline overrides.** Prefer `systemctl --user edit musefs` to add
  `Environment=` lines in a drop-in; it survives reinstalls.
- **Headless servers.** A `--user` timer only fires while your user manager
  runs. For a daily scan when you are not logged in:
  `loginctl enable-linger <user>`.
- **Logs.** `journalctl --user -u musefs -f`.
