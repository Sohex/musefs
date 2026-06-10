# systemd user units + env-var configuration — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let every scalar `musefs mount`/`scan` flag be set via a `MUSEFS_*` environment variable, and ship drop-in systemd **user** units plus a commented config file so musefs runs on the host without long argument lists.

**Architecture:** Enable clap's `env` feature and annotate each scalar arg in `musefs-cli/src/lib.rs` with `env = "MUSEFS_<NAME>"`; clap then resolves flag > env > default and lets env satisfy required args, with no custom code. Behavior is locked down by integration tests in `musefs/tests/` that spawn the real binary with a controlled, per-process environment (parallel-safe, no `/dev/fuse`: the children fail fast at arg-parse or DB-open). The systemd units are static example files under `contrib/systemd/` that source a `MUSEFS_*` `EnvironmentFile`.

**Tech Stack:** Rust 2024, clap 4 (derive + env), systemd user units, `std::process::Command` for tests, `tempfile` (already a dev-dependency of `musefs`).

---

## Spec

Source spec: `docs/superpowers/specs/2026-06-10-systemd-and-env-config-design.md`. Read it before starting.

## Background the implementer needs

- The CLI is defined in `musefs-cli/src/lib.rs`: `MountArgs` (struct, the `mount` flags) and `Command::Scan { .. }` (the `scan` flags). The binary entry point is `musefs/src/main.rs`, which calls `Cli::parse()` then `run(...)`.
- `Cli::parse()` exits with **code 2** on a usage/parse error (clap's default). On a runtime error, `main` prints `musefs: <err>` and exits **code 1**.
- `run_mount` (`musefs-cli/src/lib.rs:270`) opens the DB first with `Db::open(&args.db).with_context(|| format!("opening database at {}", args.db.display()))?` — so a bad `--db` produces stderr containing `opening database at <path>`. Opening a path whose **parent directory does not exist** fails deterministically; tests use that to prove parsing succeeded.
- `scan` over a directory with no audio files exits 0 and creates the DB file if absent — tests use that to prove `scan` read `MUSEFS_DB`.
- Integration tests in `musefs/tests/` get `env!("CARGO_BIN_EXE_musefs")` (the built binary path). These tests are **not** `#[ignore]`d: they never mount, so they need no `/dev/fuse`.
- **Commit discipline:** the pre-commit hook runs the full workspace test suite and rejects any commit with red tests. So within each task you write the test, watch it fail, implement, watch it pass, and only then commit (test + implementation in one commit). Never run the commit step while a test is red.
- clap's `env` feature: an env var satisfies a `required` arg; precedence is explicit flag > env var > default. For boolean (`ArgAction::SetTrue`) and `--case-insensitive` (`ArgAction::Set`) flags, the env value is parsed by clap's `BoolishValueParser`: case-insensitive `true/false`, `t/f`, `yes/no`, `y/n`, `on/off`, `1/0`; anything else (including empty) is a hard parse error.

## File structure

| File | Responsibility | Change |
| --- | --- | --- |
| `musefs-cli/Cargo.toml` | enable clap `env` feature | modify |
| `musefs-cli/src/lib.rs` | `env = "MUSEFS_*"` on `MountArgs` + `Command::Scan` scalar args | modify |
| `musefs/tests/env_config.rs` | spawn-the-binary env-precedence integration tests | create |
| `contrib/systemd/musefs.service` | mount daemon unit | create |
| `contrib/systemd/musefs-scan.service` | oneshot re-scan unit | create |
| `contrib/systemd/musefs-scan.timer` | timer for the re-scan | create |
| `contrib/systemd/musefs.conf.example` | every `MUSEFS_*` var, commented with defaults | create |
| `contrib/systemd/README.md` | install + gotcha docs | create |
| `README.md` | "Running as a systemd user service" subsection + env-var note | modify |

---

## Task 1: env support for `mount` (clap feature + `MountArgs`)

**Files:**
- Create: `musefs/tests/env_config.rs`
- Modify: `musefs-cli/Cargo.toml:11`
- Modify: `musefs-cli/src/lib.rs` (`MountArgs`, currently lines 40–105)

- [ ] **Step 1: Write the failing test file**

Create `musefs/tests/env_config.rs` with the shared helpers and the first three tests:

```rust
//! The `musefs` binary reads MUSEFS_* environment variables for scalar mount
//! and scan flags (clap's `env` feature). Each test spawns the real binary with
//! an isolated environment, so they are parallel-safe and need no /dev/fuse:
//! the children fail fast at arg-parse (exit 2) or DB-open (exit 1), never
//! reaching a mount.
//!
//! `env_clear()` is deliberate: it guarantees no ambient MUSEFS_* leaks in from
//! the developer's shell. It is safe here — the binary is launched by absolute
//! path (`CARGO_BIN_EXE_musefs`), and the assertions key on the
//! `opening database at <path>` stderr, which `main` emits via anyhow/eprintln,
//! not through env_logger — so a cleared `RUST_LOG` does not suppress it.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn musefs() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_musefs"));
    cmd.env_clear();
    cmd
}

/// A DB path whose parent directory does not exist, so opening it fails
/// deterministically — proving we got *past* arg parsing to the DB-open step.
fn unopenable_db(dir: &Path, name: &str) -> PathBuf {
    dir.join("missing").join(name)
}

#[test]
fn env_satisfies_required_mount_args() {
    let dir = tempfile::tempdir().unwrap();
    let db = unopenable_db(dir.path(), "env.db");
    let out: Output = musefs()
        .arg("mount")
        .env("MUSEFS_MOUNTPOINT", dir.path())
        .env("MUSEFS_DB", &db)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    // Not a usage error: env satisfied the required mountpoint and --db.
    assert_ne!(out.status.code(), Some(2), "stderr: {stderr}");
    // We reached DB-open, which fails on the missing parent directory.
    assert!(stderr.contains("opening database"), "stderr: {stderr}");
    assert!(
        stderr.contains(&db.display().to_string()),
        "stderr: {stderr}"
    );
}

#[test]
fn missing_required_mount_args_is_usage_error() {
    let out = musefs().arg("mount").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr).to_lowercase();
    assert!(
        stderr.contains("--db") || stderr.contains("required"),
        "stderr: {stderr}"
    );
}

#[test]
fn explicit_db_flag_overrides_env_db() {
    let dir = tempfile::tempdir().unwrap();
    let env_db = unopenable_db(dir.path(), "env.db");
    let flag_db = unopenable_db(dir.path(), "flag.db");
    let out = musefs()
        .arg("mount")
        .arg(dir.path()) // positional mountpoint
        .arg("--db")
        .arg(&flag_db)
        .env("MUSEFS_DB", &env_db)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&flag_db.display().to_string()),
        "expected flag db to win, stderr: {stderr}"
    );
    assert!(
        !stderr.contains(&env_db.display().to_string()),
        "env db should have been overridden, stderr: {stderr}"
    );
}

// Precedence on a value-bearing, non-required flag (the spec's --mode example).
// A bogus MUSEFS_MODE alone is rejected at parse (proves env is read); the same
// bogus env with an explicit --mode is accepted (proves the flag wins and env is
// not consulted). Observable purely via exit codes — no mount needed.
#[test]
fn invalid_mode_env_is_rejected_but_flag_overrides_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = unopenable_db(dir.path(), "env.db");

    let env_only = musefs()
        .arg("mount")
        .arg(dir.path())
        .arg("--db")
        .arg(&db)
        .env("MUSEFS_MODE", "bogus")
        .output()
        .unwrap();
    assert_eq!(
        env_only.status.code(),
        Some(2),
        "bogus MUSEFS_MODE should be a usage error, stderr: {}",
        String::from_utf8_lossy(&env_only.stderr)
    );

    let flag_wins = musefs()
        .arg("mount")
        .arg(dir.path())
        .arg("--db")
        .arg(&db)
        .arg("--mode")
        .arg("synthesis")
        .env("MUSEFS_MODE", "bogus")
        .output()
        .unwrap();
    // --mode on the CLI wins; the bogus env is never parsed. We fall through to
    // DB-open (exit 1), not a usage error.
    let stderr = String::from_utf8_lossy(&flag_wins.stderr);
    assert_ne!(flag_wins.status.code(), Some(2), "stderr: {stderr}");
    assert!(stderr.contains("opening database"), "stderr: {stderr}");
}

// Locks the per-flag env wiring: clap's `env` feature renders `[env: NAME=]` in
// help for annotated args. Catches a dropped `env=` that the precedence tests
// might miss, and confirms the list-valued carve-out (--fallback) has no env.
#[test]
fn mount_help_lists_env_vars() {
    let out = musefs().args(["mount", "--help"]).output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("MUSEFS_DB"), "stdout: {stdout}");
    assert!(stdout.contains("MUSEFS_MODE"), "stdout: {stdout}");
    assert!(stdout.contains("MUSEFS_MOUNTPOINT"), "stdout: {stdout}");
    assert!(
        !stdout.contains("MUSEFS_FALLBACK"),
        "--fallback is flag-only and must not advertise an env var, stdout: {stdout}"
    );
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p musefs --test env_config`
Expected: `missing_required_mount_args_is_usage_error` PASSES (db is already required). The others FAIL without the `env` wiring: `env_satisfies_required_mount_args` and `explicit_db_flag_overrides_env_db` exit 2 with a "required" error instead of reaching DB-open; `invalid_mode_env_is_rejected_but_flag_overrides_it` sees its bogus `MUSEFS_MODE` ignored (so the env-only case is not a usage error); and `mount_help_lists_env_vars` finds no `[env: ...]` in help.

- [ ] **Step 3: Enable the clap `env` feature**

In `musefs-cli/Cargo.toml`, change line 11 from:

```toml
clap = { version = "4", features = ["derive"] }
```

to:

```toml
clap = { version = "4", features = ["derive", "env"] }
```

- [ ] **Step 4: Add `env` attributes to every scalar `MountArgs` field**

Replace the whole `MountArgs` struct in `musefs-cli/src/lib.rs` with the version below. Only `#[arg(...)]` lines change — an `env = "MUSEFS_*"` is added to each scalar field; the two list-valued fields (`fallbacks`, plus `mountpoint` keeps its env) are handled as noted. Doc comments are preserved verbatim.

```rust
/// Flags for `musefs mount`, grouped so the mount plumbing passes one value
/// instead of ten ordering-fragile positional parameters.
#[derive(clap::Args, Debug)]
pub struct MountArgs {
    /// Empty directory to mount at.
    #[arg(env = "MUSEFS_MOUNTPOINT")]
    pub mountpoint: PathBuf,
    /// Path to the SQLite database.
    #[arg(long, env = "MUSEFS_DB")]
    pub db: PathBuf,
    /// Path template, e.g. "$albumartist/$album/$title". Supports ${a|b}
    /// fallback chains, [...] conditional sections ($[/$] for literal
    /// brackets), and $!{field} path fields that keep '/' as separators.
    #[arg(long, env = "MUSEFS_TEMPLATE", default_value = "$albumartist/$album/$title")]
    pub template: String,
    /// Fallback value substituted for any missing template field.
    #[arg(long, env = "MUSEFS_DEFAULT_FALLBACK", default_value = "Unknown")]
    pub default_fallback: String,
    /// Per-field fallback `FIELD=VALUE`, overriding `--default-fallback` for
    /// just that field when it is missing. Repeatable, e.g. `--fallback
    /// albumartist="Unknown Artist" --fallback genre=Misc`.
    #[arg(long = "fallback", value_name = "FIELD=VALUE", value_parser = parse_fallback)]
    pub fallbacks: Vec<(String, String)>,
    /// How file contents are served.
    #[arg(long, value_enum, env = "MUSEFS_MODE", default_value_t = CliMode::Synthesis)]
    pub mode: CliMode,
    /// Debounce window (ms) for picking up external DB edits.
    #[arg(long, env = "MUSEFS_POLL_INTERVAL_MS", default_value_t = 1000)]
    pub poll_interval_ms: u64,
    /// Entry/attr cache TTL (ms) the kernel may trust before re-validating.
    /// Higher cuts lookup/getattr traffic but slows visibility of DB edits.
    #[arg(long, env = "MUSEFS_ATTR_TTL_MS", default_value_t = 1000)]
    pub attr_ttl_ms: u64,
    /// Kernel read-ahead window (KiB). Larger hides HDD/NFS latency while
    /// streaming; clamped to the kernel maximum at mount.
    #[arg(long, env = "MUSEFS_MAX_READAHEAD_KIB", default_value_t = 512)]
    pub max_readahead_kib: u32,
    /// Max outstanding background (readahead/async) requests the kernel queues.
    #[arg(long, env = "MUSEFS_MAX_BACKGROUND", default_value_t = 64)]
    pub max_background: u16,
    /// Keep the kernel page cache across opens. External re-tags auto-invalidate
    /// the affected inodes on refresh, so cached bytes are dropped when content
    /// changes.
    #[arg(long, env = "MUSEFS_KEEP_CACHE")]
    pub keep_cache: bool,
    /// Compare filenames case-insensitively: case-variant directories merge and
    /// case-variant files are disambiguated. Defaults to true on macOS (whose
    /// volumes are usually case-insensitive), false on Linux/FreeBSD. Override
    /// with `--case-insensitive false` (e.g. a case-sensitive APFS volume).
    #[arg(long, env = "MUSEFS_CASE_INSENSITIVE", default_value_t = cfg!(target_os = "macos"), action = clap::ArgAction::Set)]
    pub case_insensitive: bool,
    /// Owning user for every entry: a username or numeric uid. Defaults to the
    /// launching process's uid.
    #[arg(long, env = "MUSEFS_OWNER", value_name = "NAME|UID", value_parser = parse_owner)]
    pub owner: Option<u32>,
    /// Owning group for every entry: a group name or numeric gid. Defaults to
    /// the launching process's gid.
    #[arg(long, env = "MUSEFS_GROUP", value_name = "NAME|GID", value_parser = parse_group)]
    pub group: Option<u32>,
    /// Permission bits for regular files, octal (e.g. 444). Defaults to 444.
    /// The mount is read-only, so write bits are advertised but inert.
    #[arg(long, env = "MUSEFS_FILE_MODE", value_name = "OCTAL", value_parser = parse_octal_mode)]
    pub file_mode: Option<u16>,
    /// Permission bits for directories, octal (e.g. 555). Defaults to 555.
    #[arg(long, env = "MUSEFS_DIR_MODE", value_name = "OCTAL", value_parser = parse_octal_mode)]
    pub dir_mode: Option<u16>,
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p musefs --test env_config`
Expected: all three tests PASS.

- [ ] **Step 6: Lint**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: clean (no warnings).

- [ ] **Step 7: Commit**

`clap`'s `env` lives in `clap_builder`, which is already in the tree, so flipping
the feature normally leaves `Cargo.lock` unchanged — stage it only if `git status`
shows it changed.

```bash
git add musefs-cli/Cargo.toml musefs-cli/src/lib.rs musefs/tests/env_config.rs
git status --short Cargo.lock && git add Cargo.lock   # only if it changed
git commit -m "feat(cli): MUSEFS_* env vars for mount flags

Enable clap's env feature and annotate every scalar MountArgs field with
env = \"MUSEFS_*\". Integration tests spawn the binary with a controlled
environment to verify env satisfies required args and that explicit flags
win over env."
```

---

## Task 2: boolean-from-env semantics (regression guard)

**Files:**
- Modify: `musefs/tests/env_config.rs`

This task adds tests only — the behavior already works after Task 1. It pins the hard-error semantics the systemd conf relies on.

- [ ] **Step 1: Add the boolean env tests**

Append to `musefs/tests/env_config.rs`:

```rust
#[test]
fn invalid_boolean_env_is_usage_error() {
    let dir = tempfile::tempdir().unwrap();
    let db = unopenable_db(dir.path(), "env.db");
    let out = musefs()
        .arg("mount")
        .arg(dir.path())
        .arg("--db")
        .arg(&db)
        .env("MUSEFS_KEEP_CACHE", "enabled") // not a boolish value
        .output()
        .unwrap();
    // Hard error at parse time, not a silent false — and pinned to the boolean
    // parse failure, not any exit-2.
    assert_eq!(
        out.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr).to_lowercase();
    assert!(
        stderr.contains("keep-cache") || stderr.contains("invalid value"),
        "expected a keep-cache boolean parse error, stderr: {stderr}"
    );
}

#[test]
fn valid_boolean_env_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let db = unopenable_db(dir.path(), "env.db");
    let out = musefs()
        .arg("mount")
        .arg(dir.path())
        .arg("--db")
        .arg(&db)
        .env("MUSEFS_KEEP_CACHE", "true")
        .output()
        .unwrap();
    // Got past parse for the right reason (reached DB-open), not merely "not
    // exit 2". Proves a valid boolish env value is accepted, not silently
    // dropped.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_ne!(out.status.code(), Some(2), "stderr: {stderr}");
    assert!(stderr.contains("opening database"), "stderr: {stderr}");
}
```

- [ ] **Step 2: Run the tests to verify they pass**

Run: `cargo test -p musefs --test env_config`
Expected: all seven tests PASS (the five from Task 1 plus these two — they need no new wiring, since Task 1 already enabled env on `--keep-cache`).

- [ ] **Step 3: Commit**

```bash
git add musefs/tests/env_config.rs
git commit -m "test(cli): pin boolean-from-env hard-error semantics"
```

---

## Task 3: env support for `scan`

**Files:**
- Modify: `musefs-cli/src/lib.rs` (`Command` enum, currently lines 107–136)
- Modify: `musefs/tests/env_config.rs`

- [ ] **Step 1: Write the failing test**

Append to `musefs/tests/env_config.rs`:

```rust
#[test]
fn scan_reads_db_from_env() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("library");
    std::fs::create_dir(&target).unwrap();
    let db = dir.path().join("scan-env.db");
    let out = musefs()
        .arg("scan")
        .arg(&target) // targets stay command-line only
        .env("MUSEFS_DB", &db)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(db.exists(), "scan should create the DB at the MUSEFS_DB path");
}

#[test]
fn scan_help_lists_env_vars() {
    let out = musefs().args(["scan", "--help"]).output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("MUSEFS_DB"), "stdout: {stdout}");
    assert!(stdout.contains("MUSEFS_JOBS"), "stdout: {stdout}");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p musefs --test env_config scan`
Expected: both FAIL — without env on `scan`, `scan_reads_db_from_env` exits 2 with a "required" error (so `success()` is false), and `scan_help_lists_env_vars` finds no `MUSEFS_*` in the scan help.

- [ ] **Step 3: Add `env` attributes to the `Scan` variant**

Replace the whole `Command` enum in `musefs-cli/src/lib.rs` with the version below. Only `#[arg(...)]` lines change on the scalar `Scan` fields (`db`, `revalidate`, `jobs`, `follow_symlinks`, `quiet`); `targets` stays flag-only. Doc comments are preserved verbatim.

```rust
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Walk backing files or directories, ingesting supported audio
    /// (FLAC, MP3, M4A, Ogg, WAV) into the SQLite store.
    Scan {
        /// One or more files or directories to scan (directories recurse).
        #[arg(required = true, num_args = 1..)]
        targets: Vec<PathBuf>,
        /// Path to the SQLite database (created if absent).
        #[arg(long, env = "MUSEFS_DB")]
        db: PathBuf,
        /// Re-validate: skip unchanged files, prune tracks whose backing file is
        /// gone, and garbage-collect orphaned art.
        #[arg(long, env = "MUSEFS_REVALIDATE")]
        revalidate: bool,
        /// Probe worker threads (0 = available parallelism). 1 = sequential.
        #[arg(long, env = "MUSEFS_JOBS", default_value_t = 0)]
        jobs: usize,
        /// Follow symlinks while walking directories. Off by default: symlinked
        /// files and directories are logged and skipped.
        #[arg(long, env = "MUSEFS_FOLLOW_SYMLINKS")]
        follow_symlinks: bool,
        /// Suppress the per-target summary on stdout (failures still surface via
        /// the `log` facade on stderr; raise detail with `RUST_LOG=info`).
        #[arg(long, short, env = "MUSEFS_QUIET")]
        quiet: bool,
    },
    /// Mount a read-only FUSE view of the store.
    Mount(MountArgs),
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p musefs --test env_config`
Expected: all nine tests PASS (seven from Tasks 1-2 plus these two).

- [ ] **Step 5: Lint**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add musefs-cli/src/lib.rs musefs/tests/env_config.rs
git commit -m "feat(cli): MUSEFS_* env vars for scan flags"
```

---

## Task 4: systemd unit files

**Files:**
- Create: `contrib/systemd/musefs.service`
- Create: `contrib/systemd/musefs-scan.service`
- Create: `contrib/systemd/musefs-scan.timer`
- Create: `contrib/systemd/musefs.conf.example`

No *linter* gate applies to these files (they are not Rust, shell, or YAML, so the pre-commit hook does not lint them). Note this does **not** mean the Task 4 commit skips the cargo gate: it adds `contrib/systemd/*.service`/`.timer`/`.conf.example` (none under `docs/` or a `*.md`), so the hook still runs the full fmt/clippy/test suite on this commit — it passes because the tree is already green from Task 3. Unit-syntax verification is `systemd-analyze` if available.

- [ ] **Step 1: Create `contrib/systemd/musefs.service`**

```ini
[Unit]
Description=musefs read-only re-tagging FUSE mount
Documentation=https://github.com/Sohex/musefs

[Service]
Type=simple
EnvironmentFile=-%h/.config/musefs/musefs.conf
# The --user manager does not inherit a login shell's PATH, so a cargo-installed
# binary in ~/.cargo/bin is not found by a bare `musefs`. Adjust this PATH (or
# replace ExecStart with an absolute path) to match where musefs is installed.
Environment=PATH=%h/.cargo/bin:/usr/local/bin:/usr/bin
ExecStart=musefs mount
# musefs unmounts cleanly on SIGTERM (systemd's default stop signal), so no
# ExecStop is required. Uncomment as a fallback if a mount is ever left behind:
#ExecStop=-fusermount3 -u ${MUSEFS_MOUNTPOINT}
Restart=on-failure
RestartSec=5
# NoNewPrivileges is safe for a FUSE mount. Do NOT add ProtectHome=,
# PrivateMounts=, or MountFlags=private: they place the mount in a private
# namespace and hide it from the rest of your session.
NoNewPrivileges=true

[Install]
WantedBy=default.target
```

- [ ] **Step 2: Create `contrib/systemd/musefs-scan.service`**

```ini
[Unit]
Description=musefs library re-scan
Documentation=https://github.com/Sohex/musefs

[Service]
Type=oneshot
EnvironmentFile=-%h/.config/musefs/musefs.conf
Environment=PATH=%h/.cargo/bin:/usr/local/bin:/usr/bin
# Scan target(s) are not env-configurable; set your library path here.
ExecStart=musefs scan %h/Music --revalidate
```

- [ ] **Step 3: Create `contrib/systemd/musefs-scan.timer`**

```ini
[Unit]
Description=Periodic musefs library re-scan

[Timer]
OnCalendar=daily
Persistent=true

[Install]
WantedBy=timers.target
```

- [ ] **Step 4: Create `contrib/systemd/musefs.conf.example`**

```ini
# musefs configuration — sourced by the systemd user units as an EnvironmentFile.
#
# Copy to ~/.config/musefs/musefs.conf and edit. This is systemd
# EnvironmentFile syntax, NOT shell:
#   - one KEY=value per line; the value is literal to end of line
#   - no quote stripping, no $VAR expansion, no command substitution
#   - systemd specifiers like %h are NOT expanded here (only in unit files);
#     write real absolute paths
#   - leave optional settings COMMENTED. A bare `KEY=` means "set to empty",
#     which is treated as a value and breaks path/boolean settings.
#
# Booleans accept (case-insensitive): true/false, yes/no, on/off, 1/0.
# Any other value is a hard error that stops the unit from starting.

# --- Required ---------------------------------------------------------------

# Directory to mount at (must exist and be empty). Replace /home/youruser.
MUSEFS_MOUNTPOINT=/home/youruser/Music
# Path to the SQLite store (shared by `mount` and `scan`).
MUSEFS_DB=/home/youruser/.local/share/musefs/library.db

# --- Mount: layout ----------------------------------------------------------

# Path template. Supports ${a|b} fallback chains, [ ... ] conditionals
# ($[ / $] for literal brackets), and $!{field} path fields.
#MUSEFS_TEMPLATE=$albumartist/$album/$title
# Value substituted for any missing template field.
#MUSEFS_DEFAULT_FALLBACK=Unknown
# How file contents are served: synthesis or structure-only.
#MUSEFS_MODE=synthesis
# Compare filenames case-insensitively. Default: false on Linux/FreeBSD,
# true on macOS.
#MUSEFS_CASE_INSENSITIVE=false

# --- Mount: tuning ----------------------------------------------------------

# Debounce window (ms) for picking up external DB edits.
#MUSEFS_POLL_INTERVAL_MS=1000
# Entry/attr cache TTL (ms) the kernel may trust before re-validating.
#MUSEFS_ATTR_TTL_MS=1000
# Kernel read-ahead window (KiB); clamped to the kernel maximum at mount.
#MUSEFS_MAX_READAHEAD_KIB=512
# Max outstanding background (readahead/async) requests the kernel queues.
#MUSEFS_MAX_BACKGROUND=64
# Keep the kernel page cache across opens.
#MUSEFS_KEEP_CACHE=false

# --- Mount: ownership & permissions ----------------------------------------

# Owner presented for every entry: a username or numeric uid.
# Default: the launching process's uid.
#MUSEFS_OWNER=media
# Group presented for every entry: a group name or numeric gid.
# Default: the launching process's gid.
#MUSEFS_GROUP=media
# Permission bits for regular files, octal. Default: 444.
#MUSEFS_FILE_MODE=444
# Permission bits for directories, octal. Default: 555.
#MUSEFS_DIR_MODE=555

# --- Scan -------------------------------------------------------------------
# (scan targets are set in musefs-scan.service, not here.)

# Probe worker threads (0 = available parallelism).
#MUSEFS_JOBS=0
# Re-validate on scan: skip unchanged, prune missing, GC orphaned art.
#MUSEFS_REVALIDATE=false
# Follow symlinks while walking.
#MUSEFS_FOLLOW_SYMLINKS=false
# Suppress the per-target scan summary.
#MUSEFS_QUIET=false
```

- [ ] **Step 5: Verify the units parse (best-effort)**

Run: `command -v systemd-analyze >/dev/null && systemd-analyze --user verify contrib/systemd/musefs.service contrib/systemd/musefs-scan.service contrib/systemd/musefs-scan.timer; echo done`
Expected: prints `done`. Treat only genuine syntax errors as failures — lines containing `Failed to parse`, `Invalid`, or `Unknown lvalue`. The following are **expected and harmless**: `Failed to resolve executable musefs` / `... is not executable` (musefs is not installed here), and any advisory `Notice:`/hint lines (e.g. about `NoNewPrivileges` or an inactive bound unit). If `systemd-analyze` is absent, skip this step.

- [ ] **Step 6: Commit**

```bash
git add contrib/systemd/musefs.service contrib/systemd/musefs-scan.service contrib/systemd/musefs-scan.timer contrib/systemd/musefs.conf.example
git commit -m "feat(contrib): ship systemd user units + example config"
```

---

## Task 5: documentation

**Files:**
- Create: `contrib/systemd/README.md`
- Modify: `README.md` (after the "Ownership and permissions" subsection, which ends at the `--dir-mode` table row, currently line 135)

- [ ] **Step 1: Create `contrib/systemd/README.md`**

````markdown
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
````

- [ ] **Step 2: Add the README subsection**

The spec floated an optional per-flag "Env var" column on the flag tables; we
deliberately use a short prose note plus a pointer to the canonical
`musefs.conf.example` instead, to avoid maintaining the mapping in a third place
(the README has a mount table and an ownership table but no scan table). In
`README.md`, immediately after the "Ownership and permissions" table (the line `| `--dir-mode <OCTAL>` | `555` | Permission bits for directories, in octal. |`) and before `## Supported formats`, insert:

```markdown
### Configuring with environment variables

Every scalar `mount` and `scan` flag can also be set with a `MUSEFS_*`
environment variable — uppercase the long flag and turn dashes into
underscores (e.g. `--poll-interval-ms` → `MUSEFS_POLL_INTERVAL_MS`, the
`mount` mountpoint → `MUSEFS_MOUNTPOINT`). An explicit flag always overrides
its env var, which overrides the default. The repeatable `--fallback` and the
`scan` targets are command-line only. See
[`contrib/systemd/musefs.conf.example`](contrib/systemd/musefs.conf.example)
for the full, canonical list.

### Running as a systemd user service

To run musefs on the host at login, drop-in units live in
[`contrib/systemd/`](contrib/systemd/): a `musefs.service` mount daemon, an
optional `musefs-scan.timer` for periodic re-scans, and a commented
`musefs.conf.example` holding every `MUSEFS_*` setting. Copy the units to
`~/.config/systemd/user/`, copy the config to `~/.config/musefs/musefs.conf`,
edit `MUSEFS_MOUNTPOINT` and `MUSEFS_DB`, then
`systemctl --user enable --now musefs.service`. See
[`contrib/systemd/README.md`](contrib/systemd/README.md) for the full walkthrough
and the `PATH` / linger gotchas.
```

- [ ] **Step 3: Verify the doc links resolve**

Run: `ls contrib/systemd/musefs.conf.example contrib/systemd/README.md`
Expected: both paths listed (no "No such file").

- [ ] **Step 4: Commit (docs-only — cargo gate is skipped)**

```bash
git add README.md contrib/systemd/README.md
git commit -m "docs: document MUSEFS_* env vars and systemd user units"
```

---

## Final verification

- [ ] **Full workspace test suite** — Run: `cargo test`. Expected: passes (this is what the pre-commit hook runs).
- [ ] **Lint** — Run: `cargo clippy --all-targets -- -D warnings`. Expected: clean.
- [ ] **Format** — Run: `cargo fmt --all --check`. Expected: no diff.
- [ ] **Help output sanity** — Run: `cargo run -p musefs -- mount --help`. Expected: each annotated flag shows its `[env: MUSEFS_*=]` (clap renders env names in help automatically), and `--fallback` does not. This is now also enforced by the `mount_help_lists_env_vars` / `scan_help_lists_env_vars` tests; the manual run is just a final eyeball.
