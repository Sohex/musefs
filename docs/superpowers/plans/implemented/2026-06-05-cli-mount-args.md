# CLI MountArgs Grouping Implementation Plan (#132)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Group the ten `musefs mount` CLI knobs into a `#[derive(clap::Args)] MountArgs` struct so `parse_mount_config` and `run_mount` drop their `#[allow(clippy::too_many_arguments)]` and the same-typed integer knobs stop being ordering hazards.

**Architecture:** Single-file refactor in `musefs-cli/src/lib.rs`. The inline fields of the `Command::Mount` variant move verbatim (attributes and doc comments included) into a new `MountArgs` struct; the variant becomes `Mount(MountArgs)`; `parse_mount_config` takes `&MountArgs`, `run_mount` takes `MountArgs`. The `CliMode → Mode` conversion moves from `run` into `parse_mount_config`. CLI parsing behavior is byte-for-byte identical.

**Tech Stack:** Rust, clap 4 derive (`Args`, `Subcommand`, `ValueEnum`).

**Spec:** `docs/superpowers/specs/2026-06-05-cli-mount-args-fh-newtype-design.md` (Part 1).

**Branch:** create `cli-mount-args` off `main` before Task 1 (`git checkout -b cli-mount-args main`). This PR also carries the spec document.

---

### Task 1: Commit the design spec

The spec rides this PR's branch (main is protected; specs land with the first feature PR per repo convention).

**Files:**
- Add: `docs/superpowers/specs/2026-06-05-cli-mount-args-fh-newtype-design.md` (already written)

- [ ] **Step 1: Commit the spec**

```bash
git add docs/superpowers/specs/2026-06-05-cli-mount-args-fh-newtype-design.md
git commit -m "$(cat <<'EOF'
docs: spec for CLI MountArgs grouping (#132) and Fh newtype (#134)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task 2: MountArgs struct, tuple variant, and the new conversion unit test

**Files:**
- Modify: `musefs-cli/src/lib.rs` (Command enum at :39, `parse_mount_config` at :131, `run_mount` at :160, `run` at :193, tests module at :228)

The whole change is one compilation unit — the signature changes ripple through four functions and the tests module, so they land together. TDD here means writing the new test first; it fails to compile until the refactor exists, then passes.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module at the bottom of `musefs-cli/src/lib.rs` (after `scan_command_parses_multiple_paths`):

```rust
    #[test]
    fn mount_args_parse_into_configs() {
        use clap::Parser;
        let cli = Cli::try_parse_from([
            "musefs",
            "mount",
            "/mnt/muse",
            "--db",
            "/tmp/x.db",
            "--poll-interval-ms",
            "250",
            "--attr-ttl-ms",
            "750",
            "--max-readahead-kib",
            "64",
            "--max-background",
            "32",
        ])
        .unwrap();
        let Command::Mount(args) = cli.command else {
            panic!("expected Mount");
        };
        let (config, fuse_config) = parse_mount_config(&args);
        // Defaults survive the move into the struct.
        assert_eq!(config.template, "$artist/$title");
        assert_eq!(config.default_fallback, "Unknown");
        assert_eq!(config.mode, musefs_core::Mode::Synthesis);
        assert!(!fuse_config.keep_cache);
        // ms → Duration.
        assert_eq!(config.poll_interval, std::time::Duration::from_millis(250));
        assert_eq!(fuse_config.ttl, std::time::Duration::from_millis(750));
        // KiB → bytes.
        assert_eq!(fuse_config.max_readahead, 64 * 1024);
        assert_eq!(fuse_config.max_background, 32);
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-cli mount_args_parse_into_configs`
Expected: compile error — `Command::Mount` is not a tuple variant and `parse_mount_config` does not accept `&MountArgs`.

- [ ] **Step 3: Implement the refactor**

In `musefs-cli/src/lib.rs`:

**(a)** Insert the `MountArgs` struct between the `Cli` struct and the `Command` enum. Every field, `#[arg(...)]` attribute, and doc comment is moved verbatim from the current `Mount` variant:

```rust
/// Flags for `musefs mount`, grouped so the mount plumbing passes one value
/// instead of ten ordering-fragile positional parameters.
#[derive(clap::Args, Debug)]
pub struct MountArgs {
    /// Empty directory to mount at.
    pub mountpoint: PathBuf,
    /// Path to the SQLite database.
    #[arg(long)]
    pub db: PathBuf,
    /// Path template, e.g. "$albumartist/$album/$title".
    #[arg(long, default_value = "$artist/$title")]
    pub template: String,
    /// Fallback value substituted for any missing template field.
    #[arg(long, default_value = "Unknown")]
    pub default_fallback: String,
    /// How file contents are served.
    #[arg(long, value_enum, default_value_t = CliMode::Synthesis)]
    pub mode: CliMode,
    /// Debounce window (ms) for picking up external DB edits.
    #[arg(long, default_value_t = 1000)]
    pub poll_interval_ms: u64,
    /// Entry/attr cache TTL (ms) the kernel may trust before re-validating.
    /// Higher cuts lookup/getattr traffic but slows visibility of DB edits.
    #[arg(long, default_value_t = 1000)]
    pub attr_ttl_ms: u64,
    /// Kernel read-ahead window (KiB). Larger hides HDD/NFS latency while
    /// streaming; clamped to the kernel maximum at mount.
    #[arg(long, default_value_t = 512)]
    pub max_readahead_kib: u32,
    /// Max outstanding background (readahead/async) requests the kernel queues.
    #[arg(long, default_value_t = 64)]
    pub max_background: u16,
    /// Keep the kernel page cache across opens. External re-tags auto-invalidate
    /// the affected inodes on refresh, so cached bytes are dropped when content
    /// changes.
    #[arg(long)]
    pub keep_cache: bool,
}
```

**(b)** Replace the `Mount { ... }` variant in `Command` with the tuple variant (keep the variant's own doc comment):

```rust
    /// Mount a read-only FUSE view of the store.
    Mount(MountArgs),
```

**(c)** Replace `parse_mount_config` — the `#[allow(clippy::too_many_arguments)]` is dropped, and the `CliMode → Mode` conversion now happens here:

```rust
/// Parse mount CLI flags into `MountConfig` and `FuseConfig`. Pure function —
/// no DB access, no mounting. Exported for unit testing.
pub fn parse_mount_config(args: &MountArgs) -> (MountConfig, musefs_fuse::FuseConfig) {
    let config = MountConfig {
        template: args.template.clone(),
        fallbacks: BTreeMap::new(),
        default_fallback: args.default_fallback.clone(),
        mode: args.mode.into(),
        poll_interval: std::time::Duration::from_millis(args.poll_interval_ms),
    };
    let fuse_config = musefs_fuse::FuseConfig {
        ttl: std::time::Duration::from_millis(args.attr_ttl_ms),
        max_readahead: args.max_readahead_kib.saturating_mul(1024),
        max_background: args.max_background,
        keep_cache: args.keep_cache,
    };
    (config, fuse_config)
}
```

**(d)** Replace `run_mount` — the `#[allow(clippy::too_many_arguments)]` is dropped:

```rust
/// Build a `Musefs` from the DB at `args.db` and mount it (blocking) at
/// `args.mountpoint`.
pub fn run_mount(args: MountArgs) -> Result<()> {
    let db = Db::open(&args.db)
        .with_context(|| format!("opening database at {}", args.db.display()))?;
    let (config, fuse_config) = parse_mount_config(&args);
    let core = Musefs::open(db, config).context("building the virtual filesystem")?;
    musefs_fuse::mount_with(core, &args.mountpoint, "musefs", fuse_config)
        .with_context(|| format!("mounting at {}", args.mountpoint.display()))?;
    Ok(())
}
```

**(e)** In `run`, replace the entire `Command::Mount { ... } => run_mount(...)` arm (the ten-field destructure and the call, including the `mode.into()` — the conversion moved into `parse_mount_config`) with:

```rust
        Command::Mount(args) => run_mount(args),
```

**(f)** In the two existing scan tests, change both panic arms (lib.rs:243 and :263) from:

```rust
            Command::Mount { .. } => panic!("expected Scan"),
```

to:

```rust
            Command::Mount(..) => panic!("expected Scan"),
```

- [ ] **Step 4: Run the crate tests to verify they pass**

Run: `cargo test -p musefs-cli`
Expected: PASS — all three tests (`scan_command_parses_jobs_flag`, `scan_command_parses_multiple_paths`, `mount_args_parse_into_configs`).

- [ ] **Step 5: Verify the CLI surface is unchanged**

Run: `cargo run -p musefs -- mount --help`
Expected: identical flags, defaults, and help text as before (`--template` default `$artist/$title`, `--poll-interval-ms` default `1000`, etc.). The clap derive on a tuple-variant `Args` struct produces the same parser.

- [ ] **Step 6: Lint and format**

Run: `cargo clippy --all-targets -p musefs-cli && cargo fmt --all && cargo fmt --all --check`
Expected: clippy clean with **no** `too_many_arguments` allow left in the file (`grep -c too_many_arguments musefs-cli/src/lib.rs` → 0); fmt check exits 0.

- [ ] **Step 7: Commit**

```bash
git add musefs-cli/src/lib.rs
git commit -m "$(cat <<'EOF'
Group the mount CLI knobs into a clap MountArgs struct (#132)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task 3: Full validation gate

**Files:** none (verification only).

- [ ] **Step 1: Workspace tests**

Run: `cargo test`
Expected: PASS (the binary crate's `run` dispatch and any cross-crate users compile against the new signatures).

- [ ] **Step 2: Workspace clippy + fmt**

Run: `cargo clippy --all-targets && cargo fmt --all --check`
Expected: clean, exit 0 (check the exit status directly — CI gates on it).

- [ ] **Step 3: In-diff mutation gate (CI parity)**

Always `-j2`, output on /tmp, do NOT set TMPDIR. Sanity-check the diff is non-empty first — an empty diff mutates nothing and exits 0, a silent false pass:

```bash
git diff "$(git merge-base main HEAD)...HEAD" -- '*.rs' > mutants.diff
grep -q '^@@ ' mutants.diff
cargo mutants --in-diff mutants.diff -j2 --exclude 'musefs-latencyfs/**' --output /tmp/mutants-out/in-diff
```

Expected: zero missed mutants. The `mount_args_parse_into_configs` test is what catches mutations of `from_millis` and `saturating_mul(1024)`; if a mutant survives, strengthen that test rather than excluding the mutant.
