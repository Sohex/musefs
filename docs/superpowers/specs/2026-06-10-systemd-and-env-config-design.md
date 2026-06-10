# Host-run systemd units + env-var configuration

**Date:** 2026-06-10
**Status:** Approved (design)

## Problem

musefs ships binaries (`cargo install`) and glibc/musl containers, and the
README explicitly recommends running on the host as the simplest,
best-supported option. But it provides no scaffolding to actually do that: a
user who wants musefs to mount at boot and stay running has to hand-write a
service unit. There is also no way to configure musefs other than by spelling
out every flag on the command line, which makes a service file verbose and
awkward to maintain.

Two coupled gaps:

1. **No host-run scaffolding.** Ship an example/default systemd **user**
   service file a user can drop into the appropriate directory and tweak.
2. **No env-var configuration.** Every reasonably settable CLI flag should also
   be settable via an environment variable, so the service file (and anything
   else) can configure musefs without long argument lists.

These are coupled: env-var support is what lets the systemd unit stay generic
while the user configures everything in one place.

## Goals

- Every scalar `mount` and `scan` flag is settable via a `MUSEFS_*` environment
  variable, with explicit flags taking precedence over env vars.
- Required arguments (`--db`, the `mountpoint` positional) can be satisfied
  entirely from the environment.
- A drop-in-ready systemd **user** unit set under `contrib/systemd/`, plus a
  fully commented config file listing every variable with its default.
- Documentation in the README covering host-run-as-a-service and the env-var
  surface.

## Non-goals

- System-wide (`/etc/systemd/system`) units. We ship **user** units
  (`systemctl --user`); the README can note that adapting to a system unit is
  straightforward but we do not ship one.
- Env-var support for list-valued / repeatable arguments (`--fallback`, scan
  `targets`). See "List-valued arguments" below.
- Packaging for distro repositories (deb/rpm), AUR, Homebrew, etc.
- A bespoke config-file format. systemd's `EnvironmentFile` is the config file.
- `Type=notify` mount-readiness signalling (an sd_notify call so consumer units
  can order after the mount is live). The shipped unit is `Type=simple`; see the
  readiness caveat under `musefs.service`.

## Part A — env-var support

### Mechanism

Enable clap's `env` feature:

```toml
clap = { version = "4", features = ["derive", "env"] }
```

Add `env = "MUSEFS_<NAME>"` to each scalar arg in `MountArgs` and the `Scan`
variant in `musefs-cli/src/lib.rs`.

clap's resolution order is exactly what we want and needs no custom code:

1. Explicit command-line flag (highest precedence)
2. `MUSEFS_*` environment variable
3. The arg's `default_value` / `default_value_t`

An env var also satisfies a `required` argument, so `--db` and the `mountpoint`
positional can come entirely from the environment.

**Present-but-empty is a value, not "unset".** systemd's `EnvironmentFile`
distinguishes an unset key from `KEY=` (set to the empty string). clap sees a
present env var and uses `""` as the value: for `--db` that yields
`PathBuf::from("")`, which passes clap's required check and fails later at
store-open; for a boolean it is an invalid bool and hard-errors (see below).
The conf example therefore comments out optional keys rather than leaving them
empty, and the README warns against bare `KEY=` lines.

### Naming convention

`MUSEFS_` + the long flag name in `SCREAMING_SNAKE_CASE`. The `mountpoint`
positional has no flag, so it is named `MUSEFS_MOUNTPOINT`.

### Variable mapping

mount (`MountArgs`):

| Env var | Arg |
| --- | --- |
| `MUSEFS_MOUNTPOINT` | `mountpoint` (positional) |
| `MUSEFS_DB` | `--db` |
| `MUSEFS_TEMPLATE` | `--template` |
| `MUSEFS_DEFAULT_FALLBACK` | `--default-fallback` |
| `MUSEFS_MODE` | `--mode` |
| `MUSEFS_POLL_INTERVAL_MS` | `--poll-interval-ms` |
| `MUSEFS_ATTR_TTL_MS` | `--attr-ttl-ms` |
| `MUSEFS_MAX_READAHEAD_KIB` | `--max-readahead-kib` |
| `MUSEFS_MAX_BACKGROUND` | `--max-background` |
| `MUSEFS_KEEP_CACHE` | `--keep-cache` |
| `MUSEFS_CASE_INSENSITIVE` | `--case-insensitive` |
| `MUSEFS_OWNER` | `--owner` |
| `MUSEFS_GROUP` | `--group` |
| `MUSEFS_FILE_MODE` | `--file-mode` |
| `MUSEFS_DIR_MODE` | `--dir-mode` |

scan (`Command::Scan`):

| Env var | Arg |
| --- | --- |
| `MUSEFS_DB` | `--db` (intentionally the same name as mount's `--db`) |
| `MUSEFS_JOBS` | `--jobs` |
| `MUSEFS_REVALIDATE` | `--revalidate` |
| `MUSEFS_FOLLOW_SYMLINKS` | `--follow-symlinks` |
| `MUSEFS_QUIET` | `--quiet` |

`MUSEFS_DB` is deliberately shared between `mount` and `scan`: a single
`EnvironmentFile` can point both subcommands at the same store. A shared conf
also means mount-only vars (e.g. `MUSEFS_MODE`) are in the environment when
`scan` runs; this is harmless — clap only consults the env vars declared on the
subcommand being parsed and ignores the rest.

### Boolean flags

`--keep-cache`, `--revalidate`, `--follow-symlinks`, `--quiet` are
`ArgAction::SetTrue` flags. With clap's `env`, the env var is parsed by clap's
`BoolishValueParser`, which accepts a **fixed, case-insensitive set** —
`true`/`false`, `t`/`f`, `yes`/`no`, `y`/`n`, `on`/`off`, `1`/`0` — and
**hard-errors on anything else** (including an empty string). It does not
silently fall back to false: `MUSEFS_QUIET=enabled` or a typo is a clap parse
error. `--case-insensitive` already uses `ArgAction::Set` with an explicit
`true`/`false` value and a `cfg!(target_os = "macos")` default; its env var
takes the same boolish set.

Consequence under systemd: with `Type=simple` a bad boolean spelling in the
conf makes the unit fail to start (fast and loud — arguably desirable, but it is
not a silent no-op). The conf example therefore enumerates the **exact** accepted
spellings rather than saying "the usual values," and the implementation must
verify the hard-error behavior in tests (see Testing).

### List-valued arguments (flag-only, by design)

`--fallback FIELD=VALUE` (repeatable `Vec<(String, String)>` with a custom
`value_parser`) and scan `targets` (positional `Vec<PathBuf>`) do **not** get
env vars. Mapping a repeatable argument onto a single env var forces a
delimiter that collides with legitimate values — commas appear in fallback
strings, `:` appears in paths. Rather than introduce a surprising
delimiter-splitting behavior, these stay command-line only.

Under systemd this is not a limitation in practice:

- scan `targets` is the library path, which is inherently per-instance, so the
  scan unit names it directly in `ExecStart`.
- `--fallback` is an advanced knob; a user who needs it appends it to
  `ExecStart` (in the shipped unit or a `systemctl --user edit` drop-in).

## Part B — systemd units

Shipped under `contrib/systemd/`, matching the existing `contrib/` convention
(beets, picard, lidarr). All units are **user** units, intended for
`~/.config/systemd/user/`.

### `musefs.service` (the mount daemon)

```ini
[Unit]
Description=musefs read-only re-tagging FUSE mount
Documentation=https://github.com/Sohex/musefs

[Service]
Type=simple
EnvironmentFile=-%h/.config/musefs/musefs.conf
# The --user manager does not inherit a login shell's PATH, so a cargo-installed
# binary in ~/.cargo/bin is not found by a bare `musefs`. Adjust this PATH (or
# use an absolute ExecStart) to match where musefs is installed.
Environment=PATH=%h/.cargo/bin:/usr/local/bin:/usr/bin
ExecStart=musefs mount
# musefs unmounts cleanly on SIGTERM (systemd's default stop signal), so no
# ExecStop is required. Uncomment as a fallback if a mount is ever left behind:
#ExecStop=-fusermount3 -u ${MUSEFS_MOUNTPOINT}
Restart=on-failure
RestartSec=5
# A misconfigured unit (missing/invalid MUSEFS_* value) exits non-zero on every
# start; the default start-limiter eventually stops retrying.

# NoNewPrivileges is safe for a FUSE mount. Do NOT add ProtectHome=,
# PrivateMounts=, or MountFlags=private: they put the mount in a private
# namespace, hiding it from the rest of your session.
NoNewPrivileges=true

[Install]
WantedBy=default.target
```

Notes:

- `EnvironmentFile=-...` — the leading `-` makes the file optional, so the unit
  still starts if the user has not created the conf yet (and is relying on a
  drop-in or inline `Environment=`).
- No `After=` ordering. For a `--user` unit `WantedBy=default.target` already
  starts it once the user session is up; ordering `After=default.target` (the
  top of the user boot) is both unnecessary and self-referential, so it is
  omitted.
- `ExecStart=musefs mount` carries no arguments; everything comes from the
  environment. Required values (`MUSEFS_MOUNTPOINT`, `MUSEFS_DB`) must therefore
  be present in the conf or a drop-in, or the unit fails fast with clap's
  "required argument not provided" error.
- **`Type=simple` readiness caveat.** `run_mount` blocks (it calls
  `musefs_fuse::mount_with`), so `Type=simple` is the correct type. But systemd
  marks the unit "active" the instant it forks — *before* the FUSE mount is
  actually established. Nothing should assume the mount is live merely because
  the service is active; mount-ready ordering for a consumer unit is out of
  scope (would require `Type=notify` + an sd_notify call, see Non-goals).
- We do **not** embed commented `Environment=` example lines for `MUSEFS_*` in
  the unit. Inline overrides are supported the systemd-native way via
  `systemctl --user edit musefs`, which writes an upgrade-safe drop-in. The conf
  file is the documented place for the full variable list.
- `%h` is the unit-file home expansion; `~` only expands in a shell. The README
  uses `~/...` for human readability — warn against pasting those `~` paths
  verbatim into unit directives, where they would be taken literally.

### `musefs-scan.service` + `musefs-scan.timer` (optional periodic re-scan)

A oneshot service plus timer for periodic `scan --revalidate`, useful now that
env covers scan. The scan target is explicit per the list-valued carve-out.

`musefs-scan.service`:

```ini
[Unit]
Description=musefs library re-scan
Documentation=https://github.com/Sohex/musefs

[Service]
Type=oneshot
EnvironmentFile=-%h/.config/musefs/musefs.conf
Environment=PATH=%h/.cargo/bin:/usr/local/bin:/usr/bin
# Set the library path(s) here; targets are not env-configurable.
ExecStart=musefs scan %h/Music --revalidate
```

`musefs-scan.timer`:

```ini
[Unit]
Description=Periodic musefs library re-scan

[Timer]
OnCalendar=daily
Persistent=true

[Install]
WantedBy=timers.target
```

`MUSEFS_DB` and other scan knobs come from the shared conf; only the target
path is inlined. A `--user` timer only fires while the user manager is running;
on a headless server where the user is not logged in, the contrib README must
tell users to run `loginctl enable-linger <user>` so the timer fires when
logged out.

### `musefs.conf.example`

A fully commented file listing every scalar `MUSEFS_*` variable with its
default value, shipped for the user to copy to `~/.config/musefs/musefs.conf`.
Structure: required vars uncommented with placeholder values at the top
(`MUSEFS_MOUNTPOINT`, `MUSEFS_DB`), every optional var **commented** with its
default below. Specific requirements:

- Optional vars are left commented (`#MUSEFS_MODE=synthesis`), never set to an
  empty value — `KEY=` is "present and empty", which clap treats as a value and
  which breaks required-path and boolean args (see "Present-but-empty" above).
- Boolean vars list the exact accepted spellings inline
  (`true`/`false`/`yes`/`no`/`on`/`off`/`1`/`0`, case-insensitive), since an
  unrecognized value is a hard error, not a silent false.
- Use **absolute paths**. Two reasons: under a `--user` unit the working
  directory defaults to home, so a relative `MUSEFS_DB` resolves there; and
  `%h` (and other systemd specifiers) are **not** expanded inside an
  `EnvironmentFile` — only in unit directives — so `MUSEFS_DB=%h/...` would
  pass the literal string `%h/...` to musefs. The example uses a
  `/home/youruser/...` placeholder and tells the user to edit it.

systemd's `EnvironmentFile` parses `KEY=value` lines (no shell quoting, values
are literal to end of line — no `$VAR` expansion, no quote stripping); the
example is written within those constraints and notes them.

## Documentation

- **README** — add a "Running as a systemd user service" subsection in the
  host-run discussion: how to install the binary, where to put the unit and
  conf, and the `systemctl --user enable --now musefs` flow, including the
  `PATH` note (the shipped unit sets `PATH` for the `~/.cargo/bin` case; users
  with a different install location adjust it) and the `%h`-vs-`~` warning. Add
  an env-var note (and/or an "Env var" column) to the existing mount/scan flag
  tables so the mapping is discoverable. To bound drift across the three places
  the mapping lives (code attributes, conf example, README), treat
  `musefs.conf.example` as the canonical list and have the README point to it.
- **contrib/systemd/** — a short `README.md` is acceptable here (the other
  `contrib/` integrations each have one) describing the three files, the install
  steps, and `loginctl enable-linger` for the timer on headless servers. This is
  the one place a new doc file is warranted.

## Testing

Existing CLI tests in `musefs-cli/tests/cli.rs` use in-process
`Cli::parse_from(...)`. Reading process env in-process is global state and races
under parallel test execution, so env-precedence tests instead **spawn the real
binary** with a controlled environment:

- Location: `musefs/tests/` (the binary crate), where Cargo sets
  `CARGO_BIN_EXE_musefs`. No new dependency — `std::process::Command` with
  `.env(...)` / `.env_clear()` suffices, and each spawned process gets an
  isolated environment so the tests stay parallel-safe.
- Cases:
  - Required args supplied via `MUSEFS_MOUNTPOINT` + `MUSEFS_DB` get **past**
    clap parsing (the process fails later at runtime — e.g. nonexistent db /
    mountpoint — rather than with clap's "required arguments were not
    provided"). This proves env satisfies required args.
  - With no flags and no env, `musefs mount` fails with clap's required-arg
    error (baseline).
  - An explicit flag overrides the corresponding env var (e.g.
    `MUSEFS_MODE=structure-only` on the env but `--mode synthesis` on the
    command line resolves to synthesis) — asserted via an observable difference.
  - Boolean-from-env behavior on a `SetTrue` flag (e.g. `MUSEFS_KEEP_CACHE`): a
    valid truthy value is accepted, and an invalid spelling (e.g.
    `MUSEFS_KEEP_CACHE=enabled`) is a clap parse error — guarding the hard-error
    semantics the conf relies on.
- Scalar-arg parse coverage that does not need real env (e.g. confirming the
  `env` attribute is present and named correctly) can also be asserted by
  reading the generated clap help/`--help` output where convenient.

systemd unit files and the `.conf` are neither shell nor YAML, so the
pre-commit shellcheck/yamllint legs do not apply to them.

## Affected files

- `musefs-cli/Cargo.toml` — add clap `env` feature.
- `musefs-cli/src/lib.rs` — `env = "MUSEFS_*"` on `MountArgs` fields and `Scan`
  variant fields.
- `contrib/systemd/musefs.service` — new.
- `contrib/systemd/musefs-scan.service` — new.
- `contrib/systemd/musefs-scan.timer` — new.
- `contrib/systemd/musefs.conf.example` — new.
- `contrib/systemd/README.md` — new.
- `README.md` — host-run-as-a-service section + env-var documentation.
- `musefs/tests/` — env-precedence integration tests.
