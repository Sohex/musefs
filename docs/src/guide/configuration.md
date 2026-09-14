# Ownership, permissions & config

## Ownership and permissions

By default the mount presents the launching process's uid/gid and read-only
permission bits (`555` dirs, `444` files), and is reachable only by the user who
performed the mount (and root).

To present a different owner — e.g. a media-server service account — and let that
account actually reach the mount, pass `--owner`/`--group` (or `--allow-other`).
Either makes musefs mount with `allow_other` and `default_permissions`: other
users can traverse the mount, and the kernel enforces the presented owner/mode
bits instead of ignoring them.

| Flag | Default | What it does |
| ---- | ------- | ------------ |
| `--owner <NAME\|UID>` | process uid | User presented as the owner of every entry. Accepts a username or a numeric uid. Implies `--allow-other`. |
| `--group <NAME\|GID>` | process gid | Group presented for every entry. Accepts a group name or a numeric gid. Implies `--allow-other`. |
| `--allow-other` | off | Mount with `allow_other` + `default_permissions` so accounts other than the mounting user can reach the mount and the owner/mode bits are enforced. Implied by `--owner`/`--group`. |
| `--file-mode <OCTAL>` | `444` | Permission bits for regular files, in octal. The mount is read-only, so write bits are advertised but writes still fail with `EROFS`. |
| `--dir-mode <OCTAL>` | `555` | Permission bits for directories, in octal. |

The default `444`/`555` bits are world-readable, so any account can read once
`allow_other` is on. To restrict the mount to the presented owner/group, drop the
world bits (e.g. `--file-mode 440 --dir-mode 550`) — only then does
`--owner`/`--group` gate access rather than merely label it.

**Non-root mounts need `user_allow_other`.** When you are not root, libfuse
refuses an `allow_other` mount unless `/etc/fuse.conf` contains a line
`user_allow_other`. musefs checks this before mounting and fails with an
explanatory error if it is missing; add the line to `/etc/fuse.conf`, or run
musefs as root. (This is libfuse/system policy, not a musefs restriction.) The
published container images already include this line, so non-root `allow_other`
mounts work out of the box there.

**`--allow-other` grants other users — but not root.** A FUSE mount made with
`allow_other` (not `allow_root`) is reachable by other unprivileged users, yet
**root specifically cannot traverse or stat it** when it is owned by another
user. This surprises root-run tooling (Ansible, boot scripts):

- `mountpoint -q <mnt>` / `stat <mnt>` run as root report it as *not a
  mountpoint* — they try to stat *through* the mount and get EACCES. Detect the
  mount from root with `findmnt <mnt>` or `/proc/mounts` instead, which read the
  mount table rather than the filesystem.
- Don't have root manage the mountpoint **directory** while it is mounted: a
  root task that re-asserts the directory (e.g. Ansible `file: state=directory`)
  fails with EACCES/EEXIST on every run after the first. Create the directory
  before mounting, or run such tasks as the mounting user.

## Configuring with environment variables

Most scalar flags of `mount`, `scan`, `revalidate`, `vacuum` and `migrate` can
also be set with a `MUSEFS_*` environment variable — uppercase the long flag and turn dashes into
underscores (e.g. `--poll-interval-ms` → `MUSEFS_POLL_INTERVAL_MS`, the
`mount` mountpoint → `MUSEFS_MOUNTPOINT`). An explicit flag always overrides
its env var, which overrides the default. Boolean flags (e.g.
`MUSEFS_KEEP_CACHE`, `MUSEFS_FOLLOW_SYMLINKS`,
`MUSEFS_QUIET`, `MUSEFS_ALLOW_OTHER`, `MUSEFS_CASE_INSENSITIVE`,
`MUSEFS_EXPOSE_METRICS`) accept a
case-insensitive boolish value — `true`/`false`, `yes`/`no`, `on`/`off`,
`1`/`0` — and reject anything else. Set to the empty string, the way
`Environment=MUSEFS_QUIET=` in a systemd unit or `MUSEFS_QUIET=` in an env file
blanks a variable, a boolean flag's variable counts as unset and the flag keeps
its default. The repeatable `--fallback`,
`mount --dry-run`, the `scan` and `revalidate` targets, and `migrate`'s
`--snapshot`, `--no-snapshot`, `--repair`, `--vacuum` and `--revalidate` are
command-line only. See
[`contrib/systemd/musefs.conf.example`](../../../contrib/systemd/musefs.conf.example)
for a commented example covering the common settings.

**Retired in 2.0.0.** `MUSEFS_REVALIDATE`, `MUSEFS_FAST` and `MUSEFS_STRICT` no
longer have a flag to read them, so `scan` refuses to start (exit `1`) while
any of them is set to a non-empty value, `false` included. Delete the line: use
the `revalidate` subcommand in place of the first, and `MUSEFS_MATCH=fast` or
`MUSEFS_MATCH=strict` in place of the others (see the
[release notes](../release-notes.md#upgrading-from-v130)).

These variables are read the same way no matter how musefs is launched:
exported into the shell before running the binary directly
(`MUSEFS_DB=… musefs mount`), set via a systemd `EnvironmentFile=` or
`Environment=` directive, or passed into a container with `-e`/`--env-file`.
The configuration surface is identical across all three; the sections below
just show the per-deployment wiring.

## Running as a systemd user service

To run musefs on the host at login, drop-in units live in
[`contrib/systemd/`](../integrations/systemd.md): a `musefs.service` mount daemon, an
optional `musefs-scan.timer` that periodically runs `musefs revalidate --prune`
(refreshing changed files and removing tracks whose files are gone; it adds no
new files), and a commented
`musefs.conf.example` holding every `MUSEFS_*` setting. Copy the units to
`~/.config/systemd/user/`, copy the config to `~/.config/musefs/musefs.conf`,
edit `MUSEFS_MOUNTPOINT` and `MUSEFS_DB`, then
`systemctl --user enable --now musefs.service`. See
[the systemd integration guide](../integrations/systemd.md) for the full
walkthrough and the `PATH` / linger gotchas.

**Upgrading to 2.0.0.** Stop `musefs.service` and the timer, delete any
`MUSEFS_REVALIDATE`, `MUSEFS_FAST` or `MUSEFS_STRICT` line from `musefs.conf`,
and run `musefs migrate` before starting them again. A 2.0.0 `mount` refuses a
1.x store, so the service fails at every start until the store is upgraded.
