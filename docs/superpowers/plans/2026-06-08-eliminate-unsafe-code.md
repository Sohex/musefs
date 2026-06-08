# Eliminate `unsafe` Code Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove every `unsafe` block from the workspace by replacing libc FFI with safe `rustix` calls and replacing one test's `std::env::set_var` with an in-process metrics setter, then enforce `unsafe_code = "deny"` workspace-wide.

**Architecture:** Five `unsafe` sites (all thin libc FFI) are swapped to `rustix` (already in the dep tree via `tempfile`). The one `set_var` test is reworked to call a new `musefs_core::metrics::set_fault_pread` hook that pre-seeds the existing per-pread fault `OnceLock`, leaving the documented `MUSEFS_FAULT_*_US` env-var benchmark interface untouched. A workspace `[workspace.lints.rust] unsafe_code = "deny"` lint is added **last** (after all sites are clean) so every intermediate commit stays green; future `unsafe` requires a visible per-site `#[expect(unsafe_code, reason = "...")]`.

**Tech Stack:** Rust (edition 2021), `rustix` 1.x (`process` + `fs` features), `fuser` 0.17, `libc` (kept for safe errno/`statvfs`-free constants only).

**Reference spec:** `docs/superpowers/specs/2026-06-08-eliminate-unsafe-code-design.md`

---

## File Structure

| File | Change | Responsibility |
| ---- | ------ | -------------- |
| `musefs-fuse/Cargo.toml` | Modify | Add `rustix` dep (`process` feature) |
| `musefs-fuse/src/lib.rs:188-190` | Modify | getuid/getgid → rustix |
| `musefs-latencyfs/Cargo.toml` | Modify | Add `rustix` dep (`process` + `fs` features) |
| `musefs-latencyfs/src/lib.rs:232-234` | Modify | getuid/getgid → rustix |
| `musefs-latencyfs/src/lib.rs:416-449` | Modify | `statfs` MaybeUninit/`libc::statvfs` → `rustix::fs::statvfs` |
| `musefs-latencyfs/tests/passthrough.rs:107-117` | Modify | test `libc::statvfs` → `rustix::fs::statvfs` |
| `musefs-core/src/metrics.rs:26-78` | Modify | Lift per-pread fault cell to module scope; add `set_fault_pread` |
| `musefs-core/tests/fault_injection.rs` | Create | TDD test for `set_fault_pread` (its own metrics-gated binary) |
| `musefs-fuse/tests/concurrency.rs:118-127` | Modify | `set_var` → `set_fault_pread` |
| `Cargo.toml` (root) | Modify | Add `[workspace.lints.rust] unsafe_code = "deny"` |
| `CONTRIBUTING.md` (Code conventions) | Modify | Document the unsafe policy + opt-in convention |

**Note on commit ordering:** the `unsafe_code = "deny"` lint is the **final** task. Every prior commit must already pass the pre-commit hook (`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace`), which it does because removing `unsafe` and adding `rustix` introduces no warnings on its own.

---

## Task 1: `musefs-fuse` getuid/getgid → rustix

**Files:**
- Modify: `musefs-fuse/Cargo.toml`
- Modify: `musefs-fuse/src/lib.rs:188-190`

This is a behavior-preserving FFI swap. There is no clean unit test for it (the
uid/gid are read from the live process at mount-construction time), so the safety
net is: it compiles, and the existing `musefs-fuse` test suite stays green.

- [ ] **Step 1: Add the `rustix` dependency**

In `musefs-fuse/Cargo.toml`, under `[dependencies]`, add the `rustix` line (keep
the existing `libc = "0.2"` line — it is still used for safe errno constants like
`libc::EIO`):

```toml
[dependencies]
fuser = "0.17"
libc = "0.2"
log = "0.4"
musefs-core = { path = "../musefs-core", version = "0.2.0" }
rustix = { version = "1", features = ["process"] }
threadpool = "1"
```

Leave `default-features` on (do not pass `default-features = false`) — rustix's
default `std` is what provides the safe API and the non-Linux backends.

- [ ] **Step 2: Replace the getuid/getgid unsafe block**

In `musefs-fuse/src/lib.rs`, the current `MusefsFs` construction (lines 188-190)
reads:

```rust
            // SAFETY: getuid/getgid are always-successful libc calls.
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
```

Replace those three lines with:

```rust
            uid: rustix::process::getuid().as_raw(),
            gid: rustix::process::getgid().as_raw(),
```

`as_raw()` returns `RawUid`/`RawGid` (= `u32`), matching the `uid: u32` / `gid: u32`
fields.

- [ ] **Step 3: Verify it compiles and existing tests pass**

Run: `cargo test -p musefs-fuse`
Expected: PASS (all existing tests; e.g. `to_file_attr` unit tests are unaffected).

- [ ] **Step 4: Verify clippy is clean**

Run: `cargo clippy -p musefs-fuse --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 5: Commit**

```bash
git add musefs-fuse/Cargo.toml musefs-fuse/src/lib.rs
git commit -m "musefs-fuse: getuid/getgid via rustix (drop unsafe)"
```

---

## Task 2: `musefs-latencyfs` getuid/getgid + statvfs → rustix

**Files:**
- Modify: `musefs-latencyfs/Cargo.toml`
- Modify: `musefs-latencyfs/src/lib.rs:232-234` (getuid/getgid)
- Modify: `musefs-latencyfs/src/lib.rs:416-449` (`statfs`)
- Modify: `musefs-latencyfs/tests/passthrough.rs:107-117` (test statvfs)

Behavior-preserving FFI swaps. The `statfs` path is exercised by the ignored
`mkdir_rmdir_and_statfs_through_the_mount` e2e test (real mount; needs `/dev/fuse`).

- [ ] **Step 1: Add the `rustix` dependency**

In `musefs-latencyfs/Cargo.toml`, under `[dependencies]`, add the `rustix` line
(keep `libc = "0.2"` — still used for errno constants):

```toml
[dependencies]
fuser = "0.17"
libc = "0.2"
rustix = { version = "1", features = ["process", "fs"] }
tempfile = "3"
```

- [ ] **Step 2: Replace the getuid/getgid unsafe block**

In `musefs-latencyfs/src/lib.rs`, the `PassthroughFs::new` body (lines 232-234)
reads:

```rust
            // SAFETY: getuid/getgid never fail.
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
```

Replace those three lines with:

```rust
            uid: rustix::process::getuid().as_raw(),
            gid: rustix::process::getgid().as_raw(),
```

- [ ] **Step 3: Replace the `statfs` MaybeUninit/`libc::statvfs` block**

In `musefs-latencyfs/src/lib.rs`, the `statfs` method (lines 416-449) currently
reads:

```rust
    fn statfs(&self, _req: &Request, ino: INodeNo, reply: ReplyStatfs) {
        nap(self.lat.stat);
        // Pass through real statvfs of the inode's path; fall back to benign values.
        if let Some(p) = self.ipath(ino.0) {
            // Use the raw OS path bytes (not `to_string_lossy`, which would
            // mangle non-UTF-8 names into U+FFFD and statvfs a different path).
            if let Ok(cstr) =
                std::ffi::CString::new(std::os::unix::ffi::OsStrExt::as_bytes(p.as_os_str()))
            {
                let mut s = std::mem::MaybeUninit::<libc::statvfs>::uninit();
                // SAFETY: cstr is a valid NUL-terminated path; s is a valid out-param.
                if unsafe { libc::statvfs(cstr.as_ptr(), s.as_mut_ptr()) } == 0 {
                    // SAFETY: statvfs returned 0, so it fully initialized `s`.
                    let s = unsafe { s.assume_init() };
                    // statvfs field types vary by platform: the count fields are
                    // u64 on Linux/FreeBSD (so `as u64` is unnecessary) but u32 on
                    // macOS (so it's a lossless widening). Allow both lints rather
                    // than branch per-OS (rust-lang/rust-clippy#17166).
                    #[allow(clippy::unnecessary_cast, clippy::cast_lossless)]
                    return reply.statfs(
                        s.f_blocks as u64,
                        s.f_bfree as u64,
                        s.f_bavail as u64,
                        s.f_files as u64,
                        s.f_ffree as u64,
                        u32::try_from(s.f_bsize as u64).unwrap_or(u32::MAX),
                        u32::try_from(s.f_namemax as u64).unwrap_or(u32::MAX),
                        u32::try_from(s.f_frsize as u64).unwrap_or(u32::MAX),
                    );
                }
            }
        }
        reply.statfs(0, 0, 0, 0, 0, 512, 255, 0);
    }
```

Replace the entire method body with:

```rust
    fn statfs(&self, _req: &Request, ino: INodeNo, reply: ReplyStatfs) {
        nap(self.lat.stat);
        // Pass through real statvfs of the inode's path; fall back to benign values.
        if let Some(p) = self.ipath(ino.0) {
            // rustix accepts the path directly (no CString needed) and returns a
            // safe StatVfs with all-u64 fields, so no per-platform casts.
            if let Ok(s) = rustix::fs::statvfs(&p) {
                return reply.statfs(
                    s.f_blocks,
                    s.f_bfree,
                    s.f_bavail,
                    s.f_files,
                    s.f_ffree,
                    u32::try_from(s.f_bsize).unwrap_or(u32::MAX),
                    u32::try_from(s.f_namemax).unwrap_or(u32::MAX),
                    u32::try_from(s.f_frsize).unwrap_or(u32::MAX),
                );
            }
        }
        reply.statfs(0, 0, 0, 0, 0, 512, 255, 0);
    }
```

`rustix::fs::statvfs` returns all fields as `u64`: the first five `reply.statfs`
arguments take `u64` directly (no cast), and the three `u32` arguments use
`u32::try_from(...).unwrap_or(u32::MAX)` — genuine narrowings per the project's
cast convention. The `#[allow(clippy::unnecessary_cast, clippy::cast_lossless)]`
and the `CString`/`MaybeUninit` machinery are gone.

- [ ] **Step 4: Replace the statvfs unsafe in the passthrough test**

In `musefs-latencyfs/tests/passthrough.rs`, the tail of
`mkdir_rmdir_and_statfs_through_the_mount` (lines 107-117) reads:

```rust
    // statfs returns real, non-empty filesystem stats for the mount (not the
    // benign all-zero fallback), exercising the passthrough statvfs path.
    let cpath = std::ffi::CString::new(mp.to_str().unwrap()).unwrap();
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: cpath is a valid NUL-terminated path; s is a valid out-param.
    assert_eq!(unsafe { libc::statvfs(cpath.as_ptr(), &raw mut s) }, 0);
    assert!(s.f_blocks > 0, "statfs should report real block counts");
```

Replace those lines with:

```rust
    // statfs returns real, non-empty filesystem stats for the mount (not the
    // benign all-zero fallback), exercising the passthrough statvfs path.
    let s = rustix::fs::statvfs(mp).unwrap();
    assert!(s.f_blocks > 0, "statfs should report real block counts");
```

- [ ] **Step 5: Verify it compiles and the (non-ignored) tests pass**

Run: `cargo test -p musefs-latencyfs`
Expected: PASS. (The real-mount `mkdir_rmdir_and_statfs_through_the_mount` test
is `#[ignore]` and will not run here; this step proves compilation.)

- [ ] **Step 6: (Best-effort) run the ignored e2e test if `/dev/fuse` is available**

Run: `cargo test -p musefs-latencyfs -- --ignored --test-threads=1`
Expected: PASS if `/dev/fuse` + libfuse present; otherwise the mount-dependent
test errors at mount and may be skipped — not a blocker for this task, but run it
if the environment supports FUSE since it is the only real exercise of the new
`statfs` path.

- [ ] **Step 7: Verify clippy is clean**

Run: `cargo clippy -p musefs-latencyfs --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 8: Commit**

```bash
git add musefs-latencyfs/Cargo.toml musefs-latencyfs/src/lib.rs musefs-latencyfs/tests/passthrough.rs
git commit -m "musefs-latencyfs: getuid/getgid + statvfs via rustix (drop unsafe)"
```

---

## Task 3: `musefs-core` metrics — add `set_fault_pread` setter

**Files:**
- Modify: `musefs-core/src/metrics.rs:26-78`
- Create: `musefs-core/tests/fault_injection.rs`

This adds a programmatic, env-free way to seed the per-pread fault duration, so
the concurrency test (Task 4) no longer needs `std::env::set_var`. The
`MUSEFS_FAULT_*_US` env-var path is preserved as the fallback. This task is
genuine TDD: write the test first, watch it fail, implement, watch it pass.

- [ ] **Step 1: Write the failing test (its own metrics-gated binary)**

Create `musefs-core/tests/fault_injection.rs`:

```rust
//! Verifies the programmatic per-pread fault setter. Its own single-test binary:
//! the per-pread fault cell is a process-global OnceLock, so a dedicated binary
//! guarantees `set_fault_pread` runs before any `on_pread` reads/seeds the cell.
#![cfg(feature = "metrics")]

use std::time::{Duration, Instant};

#[test]
fn set_fault_pread_injects_latency_without_env() {
    musefs_core::metrics::set_fault_pread(Some(Duration::from_millis(20)));
    let t = Instant::now();
    musefs_core::metrics::on_pread(0);
    assert!(
        t.elapsed() >= Duration::from_millis(15),
        "on_pread should sleep for the programmatically-set fault duration"
    );
}
```

- [ ] **Step 2: Run the test to verify it fails to compile**

Run: `cargo test -p musefs-core --features metrics --test fault_injection`
Expected: FAIL — compile error, `set_fault_pread` not found in `musefs_core::metrics`.

- [ ] **Step 3: Lift the per-pread fault cell to module scope and add the setter**

In `musefs-core/src/metrics.rs`, inside `#[cfg(feature = "metrics")] mod imp`,
the statics block (lines 32-40) currently ends at `SCAN_BYTES_READ`. Add a
module-scope fault cell after it:

```rust
    static SCAN_OPENS: AtomicU64 = AtomicU64::new(0);
    static SCAN_PREADS: AtomicU64 = AtomicU64::new(0);
    static SCAN_BYTES_READ: AtomicU64 = AtomicU64::new(0);
    static PREAD_FAULT: OnceLock<Option<Duration>> = OnceLock::new();
```

Then change `on_pread` (currently lines 68-73):

```rust
    pub fn on_pread(bytes: u64) {
        PREADS.fetch_add(1, Ordering::Relaxed);
        PREAD_BYTES.fetch_add(bytes, Ordering::Relaxed);
        static C: OnceLock<Option<Duration>> = OnceLock::new();
        fault("MUSEFS_FAULT_PREAD_US", &C);
    }
```

to use the module-scope cell, and add the setter immediately after it:

```rust
    pub fn on_pread(bytes: u64) {
        PREADS.fetch_add(1, Ordering::Relaxed);
        PREAD_BYTES.fetch_add(bytes, Ordering::Relaxed);
        fault("MUSEFS_FAULT_PREAD_US", &PREAD_FAULT);
    }

    /// Pre-seed the per-pread fault duration in-process, bypassing the
    /// `MUSEFS_FAULT_PREAD_US` env var (which stays as the benchmark fallback).
    ///
    /// Must be called before the first `on_pread`: the cell is a process-global
    /// `OnceLock`, so this is a no-op once the cell has been read or seeded.
    /// Intended for a single-test binary where that ordering is guaranteed.
    pub fn set_fault_pread(d: Option<Duration>) {
        let _ = PREAD_FAULT.set(d);
    }
```

Leave `on_open` and `on_stat` with their existing function-local `static C`
cells — only the pread cell needs a setter (YAGNI).

- [ ] **Step 4: Add the no-op stub to the non-metrics module**

So the symbol exists regardless of feature, in `musefs-core/src/metrics.rs`
inside `#[cfg(not(feature = "metrics"))] mod imp` (the stub block; `on_pread`
is the line `pub fn on_pread(_bytes: u64) {}`), add after it:

```rust
    pub fn on_pread(_bytes: u64) {}

    pub fn set_fault_pread(_d: Option<std::time::Duration>) {}
```

(This keeps `metrics::set_fault_pread` callable in non-metrics builds as a no-op,
matching how the other hooks are stubbed.)

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p musefs-core --features metrics --test fault_injection`
Expected: PASS.

- [ ] **Step 6: Verify the whole crate still builds both ways and is clippy-clean**

Run: `cargo clippy -p musefs-core --all-targets -- -D warnings`
Then: `cargo clippy -p musefs-core --all-targets --features metrics -- -D warnings`
Expected: no warnings in either.

- [ ] **Step 7: Commit**

```bash
git add musefs-core/src/metrics.rs musefs-core/tests/fault_injection.rs
git commit -m "musefs-core: add metrics::set_fault_pread (env-free fault seeding)"
```

---

## Task 4: `musefs-fuse` concurrency test — drop `set_var`

**Files:**
- Modify: `musefs-fuse/tests/concurrency.rs:118-127`

Swap the environment mutation for the new programmatic setter. This is the last
remaining `unsafe` site.

- [ ] **Step 1: Replace the `set_var` call and update the comment**

In `musefs-fuse/tests/concurrency.rs`, the test opening (lines 118-127) currently
reads:

```rust
fn slow_read_does_not_block_stat() {
    // 50 ms per backing pread call. The big file is >2 MiB; the kernel sends
    // ~128 KiB chunks, so there are ~16 FUSE read calls → ~800ms total.
    // The fault duration is parsed once into a process-global OnceLock on the
    // first on_pread. This is its own integration-test binary with a single test,
    // so no earlier on_pread can have initialized it — setting the env var here,
    // before any read, is guaranteed to be observed.
    unsafe { std::env::set_var("MUSEFS_FAULT_PREAD_US", "50000") };
```

Replace the comment + `set_var` line with:

```rust
fn slow_read_does_not_block_stat() {
    // 50 ms per backing pread call. The big file is >2 MiB; the kernel sends
    // ~128 KiB chunks, so there are ~16 FUSE read calls → ~800ms total.
    // The fault duration seeds a process-global OnceLock read on the first
    // on_pread. This is its own integration-test binary with a single test, so
    // no earlier on_pread can have seeded it — setting it here, before any read,
    // is guaranteed to be observed.
    musefs_core::metrics::set_fault_pread(Some(std::time::Duration::from_micros(50_000)));
```

(`musefs_core` is already imported in this file via
`use musefs_core::{scan_directory, Mode, MountConfig, Musefs};`; the fully-qualified
`musefs_core::metrics::set_fault_pread` call needs no new `use`.)

- [ ] **Step 2: Verify the test binary compiles under the metrics feature**

Run: `cargo test -p musefs-fuse --features metrics --test concurrency`
Expected: compiles; the single test reports as `ignored` (it is `#[ignore]` —
real mount needed). Compilation success is the goal here.

- [ ] **Step 3: (Best-effort) run the ignored concurrency test if FUSE is available**

Run: `cargo test -p musefs-fuse --features metrics --test concurrency -- --ignored --nocapture --test-threads=1`
Expected: PASS if `/dev/fuse` + libfuse present (a slow read must not block the
unrelated stat) — confirms the setter actually injects latency end-to-end. Skip
if the environment lacks FUSE; not a blocker for this task.

- [ ] **Step 4: Verify clippy is clean under the metrics feature**

Run: `cargo clippy -p musefs-fuse --all-targets --features metrics -- -D warnings`
Expected: no warnings. (The standard clippy gate omits `--features metrics`, so
this file is otherwise never linted — run it explicitly.)

- [ ] **Step 5: Commit**

```bash
git add musefs-fuse/tests/concurrency.rs
git commit -m "musefs-fuse: drive concurrency test via set_fault_pread (drop set_var)"
```

---

## Task 5: Enforce `unsafe_code = "deny"` workspace-wide + document it

**Files:**
- Modify: `Cargo.toml` (root)
- Modify: `CONTRIBUTING.md` (Code conventions section)

All five `unsafe` sites are now gone; add the lint that prevents regression, then
document the policy. This is the capstone and must be the final commit.

- [ ] **Step 1: Add the workspace rust-lint table**

In the root `Cargo.toml`, add a new `[workspace.lints.rust]` section immediately
before the existing `[workspace.lints.clippy]` section:

```toml
[workspace.lints.rust]
# unsafe is denied by default everywhere; a genuinely-necessary unsafe must be
# opted in per-site with `#[expect(unsafe_code, reason = "...")]` so it stays
# greppable and review-visible. Never relax this workspace lint for a one-off.
unsafe_code = "deny"

# Curated clippy policy: pedantic on, minus the groups that are intentional or
# stylistic in this codebase. Inherited by each crate via `[lints] workspace = true`.
[workspace.lints.clippy]
```

(Every crate already declares `[lints] workspace = true`, so this applies to all
seven workspace members at once. `fuzz/` is excluded from the workspace and is
unaffected.)

- [ ] **Step 2: Verify the whole workspace is clean under the new lint**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: no warnings, no `unsafe_code` errors.

Then verify the metrics-gated targets (which the default `--all-targets` run does
not compile) are also clean:

Run: `cargo clippy --all-targets --features metrics -- -D warnings`
Expected: no warnings, no `unsafe_code` errors — proves the concurrency test
(site 5) and the fault-injection test compile under `deny`.

- [ ] **Step 3: Run the full workspace test suite**

Run: `cargo test --workspace`
Expected: PASS (this is what the pre-commit hook will also run).

- [ ] **Step 4: Document the policy in CONTRIBUTING.md**

In `CONTRIBUTING.md`, under `## Code conventions`, add a new bullet immediately
after the existing `**Lint policy.**` bullet (which ends "The hook and CI deny
all warnings."):

```markdown
- **Unsafe code.** `unsafe_code = "deny"` is set workspace-wide in the root
  `Cargo.toml` (`[workspace.lints.rust]`). A genuinely-necessary `unsafe` is
  opted in per-site with `#[expect(unsafe_code, reason = "…")]` — never a bare
  `unsafe` block and never by relaxing the workspace lint — so every `unsafe`
  is greppable and shows up in review. Prefer a safe crate (e.g. `rustix` for
  syscalls) over hand-rolled FFI.
```

- [ ] **Step 5: Final full verification before commit**

Run: `cargo fmt --all --check`
Expected: clean (no diff).

Run: `grep -rn "unsafe" --include="*.rs" musefs-*/src musefs-*/tests musefs/src 2>/dev/null | grep -v "// " | grep "unsafe {"`
Expected: no output — confirms no `unsafe {` blocks remain in workspace src/tests.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml CONTRIBUTING.md
git commit -m "workspace: deny unsafe_code and document the opt-in policy"
```

---

## Self-Review Notes (for the implementer)

- **Spec coverage:** Task 1 covers fuse getuid/getgid; Task 2 covers latencyfs
  getuid/getgid, latencyfs `statfs`, and the latencyfs passthrough test; Task 3
  adds the metrics setter; Task 4 removes the `set_var`; Task 5 adds the lint and
  the CONTRIBUTING.md note. All five spec sites + enforcement + docs are covered.
- **rustix facts (verified against rustix 1.1.4 in the dep tree):**
  `rustix::process::getuid()/getgid()` exist; `.as_raw()` returns `u32`;
  `rustix::fs::statvfs<P: Arg>(path)` accepts `&PathBuf`; `StatVfs` fields
  (`f_blocks`, `f_bfree`, `f_bavail`, `f_files`, `f_ffree`, `f_bsize`,
  `f_namemax`, `f_frsize`) are all `u64`; `fuser`'s `ReplyStatfs::statfs` takes
  `(u64, u64, u64, u64, u64, u32, u32, u32)`.
- **Green-commit discipline:** the `deny` lint lands only in Task 5, after every
  `unsafe` is removed, so the pre-commit hook passes on every commit.
- **Metrics OnceLock constraint:** `set_fault_pread` is a no-op once the cell is
  read/seeded; both callers (the new `fault_injection.rs` test and the
  `concurrency.rs` test) are single-test, metrics-gated binaries where the
  set-before-first-`on_pread` ordering holds.
