# Fh File-Handle Newtype Implementation Plan (#134)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move the FUSE file-handle non-zero invariant out of `+1`/`-1` arithmetic scattered across three sites in `musefs-core/src/facade.rs` and into an `Fh(NonZeroU64)` newtype whose two private conversion methods are the only places the offset exists.

**Architecture:** `musefs-core` gains a public `Fh` newtype; the facade API changes shape (`open_handle → Result<Fh>`, `read`/`read_into` take `Option<Fh>` replacing the `fh == 0` sentinel, `release_handle` takes `Fh`). `musefs-fuse` converts at the wire boundary: the kernel still sees raw `u64`s, identical to today. Stale-handle semantics are unchanged — a `Some(fh)` whose slab slot was reused still misses the lookup and falls back to inode resolution.

**Tech Stack:** Rust, `std::num::NonZeroU64`, `sharded_slab`, fuser.

**Spec:** `docs/superpowers/specs/2026-06-05-cli-mount-args-fh-newtype-design.md` (Part 2).

**Branch:** create `fh-newtype` off `main` before Task 1 (`git checkout -b fh-newtype main`). Independent of the #132 branch — no ordering dependency. If the #132 PR has not merged yet when this branch is cut, the spec document is not on `main`; that's fine — this plan does not touch the spec.

---

### Task 1: The `Fh` newtype and core facade API in `musefs-core`

**Files:**
- Modify: `musefs-core/src/facade.rs` (imports at :1, `fh_from_key` at :158, `read_into` at :875, `read` at :962, `open_handle` at :968, `release_handle` at :1000, unit tests at :1013 and the two handle-reuse tests at ~:1085 and ~:1180)
- Modify: `musefs-core/src/lib.rs` (re-export at :17)

The signature changes and their in-crate consumers must land together to compile; TDD here means rewriting the handle unit test against the newtype first, watching it fail to compile, then implementing.

- [ ] **Step 1: Rewrite the failing unit test**

In `musefs-core/src/facade.rs`'s `#[cfg(test)] mod tests`, replace the whole `fh_from_key_offsets_by_one_and_maps_full_to_error` test with:

```rust
    #[test]
    fn fh_round_trips_slab_key_and_maps_full_to_error() {
        // None (slab at capacity) -> HandleTableFull.
        assert!(matches!(fh_from_key(None), Err(CoreError::HandleTableFull)));
        // Wire value is the slab key + 1, so the kernel never sees 0 ("no
        // handle"). Non-zero needs no runtime assertion — NonZeroU64 makes a
        // zero handle unrepresentable.
        assert_eq!(fh_from_key(Some(0)).unwrap().get(), 1);
        assert_eq!(fh_from_key(Some(41)).unwrap().get(), 42);
        // The two private conversion methods invert each other.
        assert_eq!(Fh::from_slab_key(0).slab_key(), 0);
        assert_eq!(Fh::from_slab_key(41).slab_key(), 41);
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-core --lib fh_round_trips`
Expected: compile error — `Fh` does not exist and `fh_from_key` returns `Result<u64>`, which has no `.get()`.

- [ ] **Step 3: Implement `Fh` and the facade API change**

All in `musefs-core/src/facade.rs`:

**(a)** Add to the imports at the top:

```rust
use std::num::NonZeroU64;
```

**(b)** Replace `fh_from_key` (at :158) with the newtype and the rewritten helper. The helper stays standalone because its `None` arm is only unit-testable as a function — a full slab can't practically be produced in a test:

```rust
/// A FUSE file handle: the sharded-slab key offset by one, so the wire value
/// is never 0 (`0` on the wire means "no handle" — `read` falls back to inode
/// resolution).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fh(NonZeroU64);

impl Fh {
    /// Sole site of the `+1`: slab key → wire-safe non-zero handle.
    /// `NonZeroU64::MIN.saturating_add` is panic-free, overflow-proof, and
    /// non-zero by construction.
    fn from_slab_key(key: usize) -> Fh {
        Fh(NonZeroU64::MIN.saturating_add(key as u64))
    }

    /// Sole site of the `-1`: handle → slab key.
    fn slab_key(self) -> usize {
        (self.0.get() - 1) as usize
    }

    /// The raw wire value handed to the kernel.
    pub fn get(self) -> u64 {
        self.0.get()
    }
}

/// Wire → type, for the FUSE layer's boundary conversion.
impl From<NonZeroU64> for Fh {
    fn from(raw: NonZeroU64) -> Fh {
        Fh(raw)
    }
}

/// Map a `sharded_slab::Slab` insert result to a file handle. `None` means the
/// slab is at capacity, surfaced as an explicit error rather than a panic.
fn fh_from_key(key: Option<usize>) -> Result<Fh> {
    key.map(Fh::from_slab_key).ok_or(CoreError::HandleTableFull)
}
```

**(c)** `open_handle` (at :968): change only the signature's return type — the body, including the final `fh_from_key(self.handles.insert(...))` expression, is unchanged:

```rust
    pub fn open_handle(&self, inode: u64) -> Result<Fh> {
```

Also update the last line of its doc comment from "return a non-zero handle id" to "return a handle" (the type now says non-zero).

**(d)** `read_into` (at :875): change the parameter `fh: u64` to `fh: Option<Fh>`, and the fast-path guard from:

```rust
        if fh != 0 {
            let handle = self.handles.get((fh - 1) as usize).map(|g| Arc::clone(&g));
```

to:

```rust
        if let Some(fh) = fh {
            let handle = self.handles.get(fh.slab_key()).map(|g| Arc::clone(&g));
```

Everything else in the body (the retry loop, the fallback path) is untouched. The trailing comment "Fallback (no prior open, or unknown handle)" still reads correctly.

**(e)** `read` (at :962): change the parameter the same way — body unchanged:

```rust
    pub fn read(&self, inode: u64, fh: Option<Fh>, offset: u64, size: u64) -> Result<Vec<u8>> {
```

**(f)** `release_handle` (at :1000): the sentinel guard disappears — an absent handle is now unrepresentable at the call site:

```rust
    /// Drop an open handle (closes its backing fd when the last reference goes).
    pub fn release_handle(&self, fh: Fh) {
        self.handles.remove(fh.slab_key());
    }
```

**(g)** In `musefs-core/src/lib.rs:17`, add `Fh` to the facade re-export:

```rust
pub use facade::{Attr, Fh, Mode, MountConfig, Musefs};
```

**(h)** Update the two handle-reuse tests in `facade.rs`'s own tests module — wrap the handle in `Some(...)` at each `fs.read` call (five sites; `open_handle`/`release_handle` calls need no change):

- ~:1086 and ~:1105 (the refresh/re-resolve test): `fs.read(file_inode, fh, 0, 1 << 20)` → `fs.read(file_inode, Some(fh), 0, 1 << 20)`
- ~:1181, ~:1209, ~:1228 (the rowid-reuse test): same wrap for `fh` and `fh2`.

- [ ] **Step 4: Run the unit test to verify it passes**

Run: `cargo test -p musefs-core --lib fh_round_trips`
Expected: PASS.

**Do not commit yet.** The repo's pre-commit hook (`.githooks/pre-commit`) runs `cargo test --workspace`, which cannot compile until the integration tests (Task 2) and the FUSE boundary (Task 3) are updated. The single commit happens at the end of Task 3.

### Task 2: Integration-test ripple in `musefs-core/tests/`

**Files:**
- Modify: `musefs-core/tests/facade.rs`
- Modify: `musefs-core/tests/metrics.rs`
- Modify: `musefs-core/tests/flac_binary_tags.rs`
- Modify: `musefs-core/tests/bench_ingest.rs`
- Modify: `musefs-core/benches/read_throughput.rs` (a declared `[[bench]]` with `harness = false` — NOT compiled by `cargo build` or `cargo test -p musefs-core`, but IS compiled by `cargo clippy --all-targets` and therefore by the pre-commit hook; missing it makes the first commit fail)

- [ ] **Step 1: Apply the mechanical substitutions**

Sentinel `0` → `None` (the fallback-path reads):

- `tests/facade.rs:54, :140, :173, :228, :266, :349, :367` — `fs.read(file_inode, 0, …)` → `fs.read(file_inode, None, …)`
- `tests/metrics.rs:40, :95` — same
- `tests/bench_ingest.rs:225, :233` — same
- `benches/read_throughput.rs:77, :195, :236` — same

Live handle → `Some(…)`:

- `tests/facade.rs:348` `Some(fh)`, `:354` `Some(fh)` (stale-after-release read — keep the `// unknown fh → fallback` comment), `:377` `Some(fh_a)`, `:380` `Some(fh_b)`, `:988` `Some(fh)`, `:1012` `Some(fh)`
- `tests/metrics.rs:142` `Some(fh)`
- `tests/flac_binary_tags.rs:61` `Some(fh)`
- `benches/read_throughput.rs:137` `Some(fh)` (the `open_handle`/`release_handle` calls at `:133`/`:143` need no change)

`release_handle`/`open_handle` call sites need no change (`fh` variables are now `Fh` values throughout).

- [ ] **Step 2: Delete the two now-untypeable non-zero assertions**

The `NonZeroU64` wrapper subsumes them; the adjacent `assert_ne!` distinctness checks are retained (they rely on `Fh`'s `PartialEq, Eq` derives):

- `tests/facade.rs:347`: delete `assert!(fh != 0);`
- `tests/facade.rs:965`: delete `assert!(fh1 != 0 && fh2 != 0);` (the test `open_handle_returns_distinct_ids_and_rejects_dirs` keeps its name and its `assert_ne!(fh1, fh2, …)`)

- [ ] **Step 3: Run the crate's full test suite, then verify the bench compiles**

Run: `cargo test -p musefs-core`
Expected: PASS — unit tests, integration tests, and proptests all compile and pass.

Run: `cargo clippy -p musefs-core --all-targets`
Expected: clean — this is what compiles `benches/read_throughput.rs` (`cargo test`/`cargo build` skip it; the pre-commit hook's workspace clippy does not).

**Do not commit yet** — `musefs-fuse` still doesn't compile against the new facade API; the workspace-wide pre-commit hook would fail. Commit comes at the end of Task 3.

### Task 3: FUSE wire-boundary conversion in `musefs-fuse`

**Files:**
- Modify: `musefs-fuse/src/lib.rs` (imports at :19-21, `open` at :278, `release` at :287, `read` at :317)

- [ ] **Step 1: Apply the boundary conversions**

**(a)** Add to the imports (next to the existing `musefs_core` imports at :19-21):

```rust
use std::num::NonZeroU64;
```

```rust
use musefs_core::Fh;
```

**(b)** `open` (:278): the core now returns `Fh`; unwrap to the wire value:

```rust
            Ok(fh) => reply.opened(FileHandle(fh.get()), flags),
```

**(c)** `release` (:287): convert and skip the core call on `None`, preserving today's `fh == 0` no-op:

```rust
        // Cheap (a map remove); no need to offload to the pool.
        if let Some(fh) = NonZeroU64::new(fh.0) {
            self.core.release_handle(Fh::from(fh));
        }
        reply.ok();
```

**(d)** `read` (:317): convert the raw wire value to `Option<Fh>` — `None` (wire 0) preserves today's inode-resolution fallback:

```rust
                match core.read_into(ino.0, NonZeroU64::new(fh.0).map(Fh::from), offset, size as u64, &mut buf) {
```

(If rustfmt wraps this line, let it.)

- [ ] **Step 2: Run the FUSE crate tests and workspace build**

Run: `cargo test -p musefs-fuse && cargo build`
Expected: PASS / clean build (the `#[ignore]`d e2e tests compile but don't run here).

- [ ] **Step 3: Commit the whole change**

The first commit on the branch — Tasks 1–3 together, because the pre-commit hook runs `cargo fmt --check`, workspace clippy `-D warnings`, and `cargo test --workspace`, all of which need the full change present:

```bash
git add musefs-core/src/facade.rs musefs-core/src/lib.rs musefs-core/tests/facade.rs musefs-core/tests/metrics.rs musefs-core/tests/flac_binary_tags.rs musefs-core/tests/bench_ingest.rs musefs-core/benches/read_throughput.rs musefs-fuse/src/lib.rs
git commit -m "$(cat <<'EOF'
Fh newtype: file-handle non-zero invariant in the type (#134)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

If the hook fails, the commit didn't happen — fix the issue, re-stage, and create a NEW commit (never `--amend`, never `--no-verify`).

### Task 4: Full validation gate

**Files:** none (verification only).

- [ ] **Step 1: Workspace tests**

Run: `cargo test`
Expected: PASS across all crates.

- [ ] **Step 2: FUSE end-to-end tests (real mounts)**

This PR touches the open/read/release path, so run the `#[ignore]`d e2e suite (needs `/dev/fuse` + libfuse; available on this machine):

Run: `cargo test -p musefs-fuse -- --ignored`
Expected: PASS (e.g. `end_to_end_read_through_mount`).

- [ ] **Step 3: Workspace clippy + fmt**

Run: `cargo clippy --all-targets && cargo fmt --all && cargo fmt --all --check`
Expected: clean, exit 0 (check the exit status directly — CI gates on it).

- [ ] **Step 4: In-diff mutation gate (CI parity)**

Always `-j2`, output on /tmp, do NOT set TMPDIR. Sanity-check the diff is non-empty first — an empty diff mutates nothing and exits 0, a silent false pass:

```bash
git diff "$(git merge-base main HEAD)...HEAD" -- '*.rs' > mutants.diff
grep -q '^@@ ' mutants.diff
cargo mutants --in-diff mutants.diff -j2 --exclude 'musefs-latencyfs/**' --output /tmp/mutants-out/in-diff
```

Expected: zero missed mutants. The `fh_round_trips_slab_key_and_maps_full_to_error` unit test pins the `±1` arithmetic in `from_slab_key`/`slab_key`; the handle-roundtrip integration tests (`open_handle_read_and_release_roundtrip`, `stale_fh_after_release_and_reopen_falls_back`) catch mutations of the `read_into` fast-path guard and `release_handle`. If a mutant survives, strengthen the relevant test rather than excluding the mutant.

- [ ] **Step 5: Fuzz targets still build**

The `fuzz/` crate is outside the workspace, so format-layer/core signature changes break it only in CI's smoke job. `Fh` is not used by fuzz targets, but verify cheaply:

Run: `cargo +nightly fuzz build flac`
Expected: builds clean.
