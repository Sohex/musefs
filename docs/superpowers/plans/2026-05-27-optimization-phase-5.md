# Optimization Phase 5 — Kernel / Mount Tuning Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Tune the FUSE kernel connection (read-ahead, background depth, async/parallel capabilities), make the entry/attr TTL configurable, and add an opt-in `FOPEN_KEEP_CACHE` lever — all surfaced as CLI flags with sensible defaults.

**Architecture:** Introduce a fuse-layer `FuseConfig` (separate from core's `MountConfig`) carrying TTL + kernel knobs + cache policy. `MusefsFs` stores it; a new `Filesystem::init` applies the knobs to `KernelConfig`; `open` sets the keep-cache flag; `lookup`/`getattr` use the configurable TTL. The CLI builds a `FuseConfig` from new flags and mounts via `mount_with`.

**Tech Stack:** Rust, fuser 0.14 (bumped to the `abi-7-31` feature), clap derive, threadpool.

---

## Context the implementer needs

- **fuser 0.14 API (verified against the vendored source):**
  - `Filesystem::init(&mut self, _req: &Request<'_>, config: &mut KernelConfig) -> Result<(), libc::c_int>` — returning `Err` **aborts the mount**, so we always return `Ok(())`.
  - `KernelConfig::set_max_readahead(u32 /*bytes*/) -> Result<u32,u32>` (always available).
  - `KernelConfig::set_max_background(u16) -> Result<u16,u16>` — **gated on `abi-7-13`**.
  - `KernelConfig::add_capabilities(u32) -> Result<(),u32>` — **all-or-nothing**: if any requested bit is unsupported it adds *none* and returns the unsupported bits. So add capabilities **one at a time**.
  - `fuser::consts::FUSE_ASYNC_READ` (already on by default via `INIT_FLAGS`), `fuser::consts::FUSE_PARALLEL_DIROPS` (**const gated on `abi-7-25`**), `fuser::consts::FOPEN_KEEP_CACHE` (`1<<1`).
  - `ReplyOpen::opened(self, fh: u64, flags: u32)` — the second arg carries the `FOPEN_*` flags (currently passed `0`).
- **Why the ABI bump is mandatory:** fuser's default features are `["libfuse"]` only — *no* `abi-7-*`. Without it, `set_max_background` is `#[cfg]`-compiled out and `fuser::consts::FUSE_PARALLEL_DIROPS` does not exist (won't compile). We enable `abi-7-31` (the max). fuser negotiates the protocol version *down* to whatever the running kernel supports, so this is safe on Linux ≥ 4.x; it only widens the capabilities musefs may request.
- **Setters are best-effort:** `set_max_readahead`/`set_max_background` clamp and return the nearest legal value on `Err`; `add_capabilities` returns unsupported bits. We `let _ =` all of them — tuning preferences must never abort a mount.
- **Back-pressure note:** `MusefsFs::new` currently carries a TODO worrying the unbounded `ThreadPool` queue could grow under a read storm. Setting `max_background` *is* the back-pressure mechanism: the kernel keeps at most `max_background` async/readahead requests in flight, so the pool's queue depth is bounded in practice. We update that comment rather than build a custom bounded queue (a bounded `execute` would block the single dispatch thread).

## File structure

- `musefs-fuse/Cargo.toml` — enable fuser `abi-7-31`.
- `musefs-fuse/src/lib.rs` — add `FuseConfig` + `Default`; add `config` field to `MusefsFs`; `new(core, config)`; `init`; `open_flags` helper; configurable TTL in `lookup`/`getattr`; keep-cache in `open`; `mount_with`/`spawn_with` (3-arg `mount`/`spawn` keep delegating with `FuseConfig::default()`); update the back-pressure comment; unit tests.
- `musefs-cli/src/lib.rs` — add `--attr-ttl-ms`, `--max-readahead-kib`, `--max-background`, `--keep-cache` to `Command::Mount`; thread through `run`; `run_mount` builds `FuseConfig` and calls `mount_with`.
- `musefs-cli/tests/cli.rs` — extend the mount-parse test (defaults + explicit values).

## Out of scope / follow-ups (do NOT build here)

- **Notifier-based auto cache-invalidation.** fuser 0.14 *does* expose `Notifier::inval_inode`/`inval_entry` (via `Session::notifier()`), which would let an external re-tag auto-drop the stale kernel cache and make `keep_cache` safe-by-default. It needs precise changed-inode tracking across a rebuild plus an `OnceLock<Notifier>` cell (the notifier only exists after the session is created, and `MusefsFs` is moved into it). That is its own phase. For now `--keep-cache` ships with documented caveats; manual flush is `echo 1 | sudo tee /proc/sys/vm/drop_caches` or unmount/remount.
- **Kernel-level negative-lookup caching.** Dropped: not cleanly expressible via fuser 0.14's safe `ReplyEntry` API, and the in-process lookup is already O(1) with the poll debounced (Phase 4), so the value is low.
- **Custom bounded worker queue.** Resolved by `max_background` (see note above).

---

## Task 1: `FuseConfig` + wiring (ABI bump, configurable TTL, keep-cache flag plumbing)

**Files:**
- Modify: `musefs-fuse/Cargo.toml` (fuser feature)
- Modify: `musefs-fuse/src/lib.rs` (imports, `FuseConfig`, `MusefsFs` field + `new`, TTL in `lookup`/`getattr`, `open_flags` + `open`, `mount_with`/`spawn_with`, tests)

- [ ] **Step 1: Enable the fuser ABI feature**

In `musefs-fuse/Cargo.toml`, change the dependency line:
```toml
fuser = { version = "0.14", features = ["abi-7-31"] }
```
Run: `cargo build -p musefs-fuse`
Expected: PASS (compiles; this only widens available API).

- [ ] **Step 2: Write the failing unit tests**

In `musefs-fuse/src/lib.rs`, inside the existing `#[cfg(test)] mod tests` block (after `converts_dir_and_file_attrs`), add:
```rust
    #[test]
    fn fuse_config_default_is_conservative() {
        let c = FuseConfig::default();
        assert_eq!(c.ttl, Duration::from_secs(1));
        assert_eq!(c.max_readahead, 512 * 1024);
        assert_eq!(c.max_background, 64);
        assert!(!c.keep_cache);
    }

    #[test]
    fn open_flags_sets_keep_cache_bit_only_when_enabled() {
        assert_eq!(open_flags(false), 0);
        assert_eq!(open_flags(true), fuser::consts::FOPEN_KEEP_CACHE);
    }
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p musefs-fuse fuse_config_default_is_conservative open_flags 2>&1 | head -20`
Expected: FAIL — `cannot find type FuseConfig` / `cannot find function open_flags`.

- [ ] **Step 4: Add the `KernelConfig` import**

In `musefs-fuse/src/lib.rs`, add `KernelConfig` to the `use fuser::{...}` list:
```rust
use fuser::{
    BackgroundSession, FileAttr, FileType, Filesystem, KernelConfig, MountOption, ReplyAttr,
    ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, Request,
};
```

- [ ] **Step 5: Add `FuseConfig` + `open_flags`, remove the `const TTL`**

Replace the line `const TTL: Duration = Duration::from_secs(1);` with:
```rust
/// Fuse-layer mount knobs: kernel tuning + page-cache policy. Distinct from
/// `musefs_core::MountConfig`, which governs how the virtual tree is rendered.
#[derive(Debug, Clone)]
pub struct FuseConfig {
    /// Entry/attr cache lifetime the kernel may trust before re-validating.
    /// Longer cuts `lookup`/`getattr` traffic but bounds how fast external DB
    /// edits become visible (the existing freshness trade-off).
    pub ttl: Duration,
    /// Kernel read-ahead window in bytes (clamped to the kernel's max).
    pub max_readahead: u32,
    /// Max outstanding background (readahead/async) requests the kernel queues;
    /// also bounds the work in flight to the worker pool.
    pub max_background: u16,
    /// Keep the kernel page cache across opens (`FOPEN_KEEP_CACHE`). Safe only
    /// for static libraries: after an external re-tag the kernel may serve stale
    /// cached bytes until the cache is dropped (`drop_caches`) or remount.
    pub keep_cache: bool,
}

impl Default for FuseConfig {
    fn default() -> FuseConfig {
        FuseConfig {
            ttl: Duration::from_secs(1),
            max_readahead: 512 * 1024,
            max_background: 64,
            keep_cache: false,
        }
    }
}

/// `FOPEN_*` flags for an `open` reply, derived from the cache policy.
fn open_flags(keep_cache: bool) -> u32 {
    if keep_cache {
        fuser::consts::FOPEN_KEEP_CACHE
    } else {
        0
    }
}
```

- [ ] **Step 6: Add the `config` field and update `MusefsFs::new`**

In the `MusefsFs` struct definition, add the field:
```rust
pub struct MusefsFs {
    core: Arc<Musefs>,
    pool: ThreadPool,
    uid: u32,
    gid: u32,
    mount_time: SystemTime,
    config: FuseConfig,
}
```
Replace the whole `impl MusefsFs` block with:
```rust
impl MusefsFs {
    pub fn new(core: Musefs, config: FuseConfig) -> MusefsFs {
        // Work is I/O-bound (especially on NFS), so oversize the pool vs CPUs.
        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            * 2;
        MusefsFs {
            core: Arc::new(core),
            // The kernel keeps at most `max_background` async/readahead requests
            // in flight (set in `init`), so this pool's queue depth is bounded in
            // practice even though `ThreadPool`'s queue is nominally unbounded.
            pool: ThreadPool::new(workers),
            // SAFETY: getuid/getgid are always-successful libc calls.
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            mount_time: SystemTime::now(),
            config,
        }
    }
}
```

- [ ] **Step 7: Use the configurable TTL in `lookup` and `getattr`**

In `lookup`, change the capture tuple and the `reply.entry` TTL. Replace:
```rust
        let core = Arc::clone(&self.core);
        let (uid, gid, mt) = (self.uid, self.gid, self.mount_time);
        self.pool.execute(move || match core.getattr(child) {
            Ok(attr) => reply.entry(&TTL, &to_file_attr(&attr, uid, gid, mt), 0),
            Err(e) => reply.error(errno(&e)),
        });
```
with:
```rust
        let core = Arc::clone(&self.core);
        let (uid, gid, mt, ttl) = (self.uid, self.gid, self.mount_time, self.config.ttl);
        self.pool.execute(move || match core.getattr(child) {
            Ok(attr) => reply.entry(&ttl, &to_file_attr(&attr, uid, gid, mt), 0),
            Err(e) => reply.error(errno(&e)),
        });
```
In `getattr`, replace:
```rust
        let core = Arc::clone(&self.core);
        let (uid, gid, mt) = (self.uid, self.gid, self.mount_time);
        self.pool.execute(move || match core.getattr(ino) {
            Ok(attr) => reply.attr(&TTL, &to_file_attr(&attr, uid, gid, mt)),
            Err(e) => reply.error(errno(&e)),
        });
```
with:
```rust
        let core = Arc::clone(&self.core);
        let (uid, gid, mt, ttl) = (self.uid, self.gid, self.mount_time, self.config.ttl);
        self.pool.execute(move || match core.getattr(ino) {
            Ok(attr) => reply.attr(&ttl, &to_file_attr(&attr, uid, gid, mt)),
            Err(e) => reply.error(errno(&e)),
        });
```

- [ ] **Step 8: Set the keep-cache flag in `open`**

Replace the `open` method body:
```rust
    fn open(&mut self, _req: &Request<'_>, ino: u64, _flags: i32, reply: ReplyOpen) {
        let core = Arc::clone(&self.core);
        let flags = open_flags(self.config.keep_cache);
        self.pool.execute(move || match core.open_handle(ino) {
            Ok(fh) => reply.opened(fh, flags),
            Err(e) => reply.error(errno(&e)),
        });
    }
```

- [ ] **Step 9: Add `mount_with`/`spawn_with`; make `mount`/`spawn` delegate**

Replace the existing `mount` and `spawn` functions with:
```rust
/// Mount `core` at `mountpoint` with default fuse tuning, blocking until unmounted.
pub fn mount(core: Musefs, mountpoint: &Path, fs_name: &str) -> std::io::Result<()> {
    mount_with(core, mountpoint, fs_name, FuseConfig::default())
}

/// Mount `core` at `mountpoint` with explicit fuse tuning, blocking until unmounted.
pub fn mount_with(
    core: Musefs,
    mountpoint: &Path,
    fs_name: &str,
    config: FuseConfig,
) -> std::io::Result<()> {
    fuser::mount2(MusefsFs::new(core, config), mountpoint, &mount_options(fs_name))
}

/// Background-session mount with default tuning; the handle's `Drop` unmounts.
pub fn spawn(core: Musefs, mountpoint: &Path, fs_name: &str) -> std::io::Result<BackgroundSession> {
    spawn_with(core, mountpoint, fs_name, FuseConfig::default())
}

/// Background-session mount with explicit tuning; the handle's `Drop` unmounts.
pub fn spawn_with(
    core: Musefs,
    mountpoint: &Path,
    fs_name: &str,
    config: FuseConfig,
) -> std::io::Result<BackgroundSession> {
    fuser::spawn_mount2(MusefsFs::new(core, config), mountpoint, &mount_options(fs_name))
}
```
(The existing 3-arg `spawn` callers in `musefs-fuse/tests/{mount,concurrency,ogg_read_through}.rs` keep working unchanged — they get `FuseConfig::default()`.)

- [ ] **Step 10: Run the tests + gates**

Run: `cargo test -p musefs-fuse fuse_config_default_is_conservative open_flags 2>&1 | tail -10`
Expected: PASS (2 tests).
Run: `cargo test -p musefs-fuse 2>&1 | tail -10` → all pass (existing tests unaffected).
Run: `cargo clippy -p musefs-fuse --all-targets 2>&1 | tail -5` → no warnings.
Run: `cargo fmt -p musefs-fuse -- --check` → clean.

- [ ] **Step 11: Commit**

```bash
git add musefs-fuse/Cargo.toml musefs-fuse/src/lib.rs Cargo.lock
git commit -m "feat(fuse): FuseConfig (configurable TTL + kernel knobs + keep-cache); abi-7-31"
```

---

## Task 2: Implement `Filesystem::init` (kernel tuning)

**Files:**
- Modify: `musefs-fuse/src/lib.rs` (add `init` as the first method of `impl Filesystem for MusefsFs`)

> **Testing note:** `KernelConfig` has a private constructor, so `init` cannot be unit-tested by constructing one. Its runtime effect is covered by the `#[ignore]` end-to-end mount test (`end_to_end_read_through_mount`), which performs a real mount through `init` and asserts byte-identical reads. This task's gate is build + clippy + (where `/dev/fuse` is available) the ignored e2e suite.

- [ ] **Step 1: Add the `init` method**

In `impl Filesystem for MusefsFs`, add `init` as the first method (before `lookup`):
```rust
    fn init(&mut self, _req: &Request<'_>, config: &mut KernelConfig) -> Result<(), libc::c_int> {
        // All tuning is best-effort and must never abort the mount: the setters
        // clamp to the kernel-supported range (returning the nearest legal value
        // on Err), so we discard their results.
        let _ = config.set_max_readahead(self.config.max_readahead);
        let _ = config.set_max_background(self.config.max_background);
        // `add_capabilities` is all-or-nothing — a single unsupported bit drops
        // the rest — so request them individually. ASYNC_READ is already on by
        // default; PARALLEL_DIROPS may be unsupported on older kernels (ignored).
        let _ = config.add_capabilities(fuser::consts::FUSE_ASYNC_READ);
        let _ = config.add_capabilities(fuser::consts::FUSE_PARALLEL_DIROPS);
        Ok(())
    }
```

- [ ] **Step 2: Build + lint gates**

Run: `cargo build -p musefs-fuse 2>&1 | tail -5` → PASS.
Run: `cargo clippy -p musefs-fuse --all-targets 2>&1 | tail -5` → no warnings.
Run: `cargo fmt -p musefs-fuse -- --check` → clean.

- [ ] **Step 3: End-to-end verification (if `/dev/fuse` is available)**

Run: `cargo test -p musefs-fuse -- --ignored 2>&1 | tail -20`
Expected: the e2e mount tests pass — the mount now goes through `init`, and reads remain byte-identical (the hard gate). If `/dev/fuse` is unavailable in this environment, record that the e2e suite must be run on a FUSE-capable host before merge.

- [ ] **Step 4: Commit**

```bash
git add musefs-fuse/src/lib.rs
git commit -m "perf(fuse): tune kernel connection in init (readahead, max_background, async/parallel caps)"
```

---

## Task 3: CLI flags (`--attr-ttl-ms`, `--max-readahead-kib`, `--max-background`, `--keep-cache`)

**Files:**
- Modify: `musefs-cli/src/lib.rs` (`Command::Mount` fields, `run` destructure, `run_mount` signature + `FuseConfig` build + `mount_with`)
- Test: `musefs-cli/tests/cli.rs`

- [ ] **Step 1: Write the failing test additions**

In `musefs-cli/tests/cli.rs`, replace the `// Mode defaults to synthesis; poll interval defaults to 1000ms.` block (and its following `--poll-interval-ms` block) so that the default-mount match also asserts the new defaults, and add an explicit-values case. Concretely, replace the existing default-mount match:
```rust
    // Mode defaults to synthesis; poll interval defaults to 1000ms.
    let cli = Cli::parse_from(["musefs", "mount", "/mnt/x", "--db", "/tmp/m.db"]);
    match cli.command {
        Command::Mount {
            mode,
            poll_interval_ms,
            ..
        } => {
            assert_eq!(mode, CliMode::Synthesis);
            assert_eq!(poll_interval_ms, 1000); // default
        }
        _ => panic!("expected mount"),
    }
```
with:
```rust
    // Mode defaults to synthesis; tuning knobs have conservative defaults.
    let cli = Cli::parse_from(["musefs", "mount", "/mnt/x", "--db", "/tmp/m.db"]);
    match cli.command {
        Command::Mount {
            mode,
            poll_interval_ms,
            attr_ttl_ms,
            max_readahead_kib,
            max_background,
            keep_cache,
            ..
        } => {
            assert_eq!(mode, CliMode::Synthesis);
            assert_eq!(poll_interval_ms, 1000); // default
            assert_eq!(attr_ttl_ms, 1000); // default
            assert_eq!(max_readahead_kib, 512); // default
            assert_eq!(max_background, 64); // default
            assert!(!keep_cache); // default off
        }
        _ => panic!("expected mount"),
    }

    // Tuning flags parse to their given values.
    let cli = Cli::parse_from([
        "musefs",
        "mount",
        "/mnt/x",
        "--db",
        "/tmp/m.db",
        "--attr-ttl-ms",
        "2000",
        "--max-readahead-kib",
        "1024",
        "--max-background",
        "128",
        "--keep-cache",
    ]);
    match cli.command {
        Command::Mount {
            attr_ttl_ms,
            max_readahead_kib,
            max_background,
            keep_cache,
            ..
        } => {
            assert_eq!(attr_ttl_ms, 2000);
            assert_eq!(max_readahead_kib, 1024);
            assert_eq!(max_background, 128);
            assert!(keep_cache);
        }
        _ => panic!("expected mount"),
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-cli --test cli 2>&1 | head -20`
Expected: FAIL — the `Command::Mount` pattern binds fields (`attr_ttl_ms`, …) that don't exist yet (compile error).

- [ ] **Step 3: Add the flags to `Command::Mount`**

In `musefs-cli/src/lib.rs`, in the `Command::Mount { … }` variant, after the `poll_interval_ms` field add:
```rust
        /// Entry/attr cache TTL (ms) the kernel may trust before re-validating.
        /// Higher cuts lookup/getattr traffic but slows visibility of DB edits.
        #[arg(long, default_value_t = 1000)]
        attr_ttl_ms: u64,
        /// Kernel read-ahead window (KiB). Larger hides HDD/NFS latency while
        /// streaming; clamped to the kernel maximum at mount.
        #[arg(long, default_value_t = 512)]
        max_readahead_kib: u32,
        /// Max outstanding background (readahead/async) requests the kernel queues.
        #[arg(long, default_value_t = 64)]
        max_background: u16,
        /// Keep the kernel page cache across opens. Best for static libraries;
        /// after an external re-tag the kernel may serve stale bytes until the
        /// cache is dropped (`drop_caches`) or the mount is replaced.
        #[arg(long)]
        keep_cache: bool,
```

- [ ] **Step 4: Thread the flags through `run`**

In `run`, extend the `Command::Mount { … }` destructure and the `run_mount(...)` call:
```rust
        Command::Mount {
            mountpoint,
            db,
            template,
            default_fallback,
            mode,
            poll_interval_ms,
            attr_ttl_ms,
            max_readahead_kib,
            max_background,
            keep_cache,
        } => run_mount(
            &db,
            &mountpoint,
            template,
            default_fallback,
            mode.into(),
            poll_interval_ms,
            attr_ttl_ms,
            max_readahead_kib,
            max_background,
            keep_cache,
        ),
```

- [ ] **Step 5: Build the `FuseConfig` in `run_mount`**

Replace `run_mount` with:
```rust
/// Build a `Musefs` from the DB at `db_path` and mount it (blocking) at
/// `mountpoint`.
#[allow(clippy::too_many_arguments)]
pub fn run_mount(
    db_path: &Path,
    mountpoint: &Path,
    template: String,
    default_fallback: String,
    mode: musefs_core::Mode,
    poll_interval_ms: u64,
    attr_ttl_ms: u64,
    max_readahead_kib: u32,
    max_background: u16,
    keep_cache: bool,
) -> Result<()> {
    let db =
        Db::open(db_path).with_context(|| format!("opening database at {}", db_path.display()))?;
    let config = MountConfig {
        template,
        fallbacks: BTreeMap::new(),
        default_fallback,
        mode,
        poll_interval: std::time::Duration::from_millis(poll_interval_ms),
    };
    let fuse_config = musefs_fuse::FuseConfig {
        ttl: std::time::Duration::from_millis(attr_ttl_ms),
        max_readahead: max_readahead_kib.saturating_mul(1024),
        max_background,
        keep_cache,
    };
    let core = Musefs::open(db, config).context("building the virtual filesystem")?;
    musefs_fuse::mount_with(core, mountpoint, "musefs", fuse_config)
        .with_context(|| format!("mounting at {}", mountpoint.display()))?;
    Ok(())
}
```

- [ ] **Step 6: Run the test + gates**

Run: `cargo test -p musefs-cli --test cli 2>&1 | tail -10`
Expected: PASS (`parses_scan_and_mount_invocations`, `parses_mode_and_revalidate_flags`).
Run: `cargo build -p musefs-cli 2>&1 | tail -5` → PASS.
Run: `cargo clippy -p musefs-cli --all-targets 2>&1 | tail -5` → no warnings.
Run: `cargo fmt -p musefs-cli -- --check` → clean.

- [ ] **Step 7: Commit**

```bash
git add musefs-cli/src/lib.rs musefs-cli/tests/cli.rs
git commit -m "feat(cli): mount tuning flags (--attr-ttl-ms, --max-readahead-kib, --max-background, --keep-cache)"
```

---

## Final verification (whole phase)

- [ ] `cargo build --workspace 2>&1 | tail -15` → PASS, no warnings.
- [ ] `cargo test --workspace 2>&1 | tail -25` → all non-ignored tests pass.
- [ ] `cargo clippy --all-targets 2>&1 | tail -15` → no warnings.
- [ ] `cargo fmt --all -- --check` → clean.
- [ ] metrics feature still builds: `cargo build -p musefs-core --features metrics` and `cargo build -p musefs-fuse --features metrics`.
- [ ] e2e compiles/links: `cargo test -p musefs-fuse -- --ignored --list 2>&1 | tail -15`; run the ignored e2e on a FUSE-capable host and confirm byte-identical reads through the tuned mount.

## Self-review (completed during planning)

- **Spec coverage:** `Filesystem::init` + `max_readahead` (Task 2/1), `max_background` (Task 2/1) — note: spec said "FUSE_CAP_ASYNC_READ", which fuser already enables by default; we still add it explicitly and add `FUSE_PARALLEL_DIROPS` (Task 2). Configurable TTL (Task 1/3). The spec's "verify the exact `KernelConfig` API in fuser 0.14" gate is discharged in the Context section (incl. the mandatory `abi-7-31` bump and the all-or-nothing `add_capabilities` behavior). Negative-lookup caching and notifier auto-invalidation are explicitly deferred per the spec's "conditional" framing and the user's scope decision.
- **Type consistency:** `FuseConfig { ttl, max_readahead, max_background, keep_cache }` is used identically in `lib.rs`, the CLI builder, and tests; `mount_with`/`spawn_with`/`MusefsFs::new(core, config)` signatures match every call site; `open_flags(bool) -> u32` matches its test and its use in `open`.
- **No placeholders:** every code step is complete and concrete.
