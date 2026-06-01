# SP3 — Read/serve residuals Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove the three read/serve-path residuals from the prior optimization pass — the `read_segments` backing-audio double-allocation, the global `handles` mutex, and the global `size_cache` mutex — without any observable behavior change (served audio stays byte-identical).

**Architecture:** Three independent, narrowly-scoped edits to existing hot paths. (1) In `musefs-core/src/reader.rs`, read backing audio directly into the output `Vec`'s reserved tail instead of a throwaway buffer. (2) In `musefs-core/src/facade.rs`, replace `handles: Mutex<HashMap<u64, Arc<Handle>>>` + `next_fh: AtomicU64` with a lock-free `sharded_slab::Slab<Arc<Handle>>` whose key (offset by one) is the FUSE `fh`. (3) Replace `size_cache: Mutex<HashMap<i64, SizeEntry>>` with `dashmap::DashMap<i64, SizeEntry>`. A new `CoreError::HandleTableFull` variant (→ `ENFILE`) handles slab exhaustion.

**Tech Stack:** Rust, `sharded-slab` (lock-free slab, generation-keyed for ABA safety), `dashmap` (sharded concurrent map), existing `proptest` byte-identity suite + `criterion` benches (`sequential_read`, `concurrent_read_walk`).

**Spec:** `docs/superpowers/specs/2026-05-30-optimization-pass/SP3-read-serve-residuals.md`

**Cardinal invariant (hard gate):** Original audio bytes are never copied or modified, and served audio stays byte-identical. The existing byte-identity proptests and the `#[ignore]`d FUSE e2e mount tests are the gate; they must stay green.

---

## File Structure

| File | Change |
|---|---|
| `musefs-core/src/error.rs` | Add `CoreError::HandleTableFull` variant |
| `musefs-fuse/src/lib.rs` | Add `errno()` arm `HandleTableFull => ENFILE`; add a unit-test module |
| `musefs-core/Cargo.toml` | Add `sharded-slab` and `dashmap` dependencies |
| `musefs-core/src/facade.rs` | Swap `handles`→slab and `size_cache`→DashMap; remove `next_fh`, the two `MutexGuard` accessors, and the `AtomicU64` import; add `fh_from_key` helper; rewrite the lock-order comment; update `read`/`open_handle`/`release_handle`/`getattr`/`poll_refresh` call sites |
| `musefs-core/src/reader.rs` | Read backing audio into `out`'s tail (the `Segment::BackingAudio` arm of `read_segments`) |
| `musefs-core/tests/facade.rs` | Add the release+reopen ABA-fallback integration test |
| `docs/superpowers/specs/2026-05-30-optimization-pass/README.md` | Fill SP3 Plan column; record results |

---

## Task 1: Add `CoreError::HandleTableFull` + FUSE `ENFILE` mapping

This must land first: Task 2's `open_handle` references the variant, and the FUSE `errno()` match is exhaustive (won't compile without the arm).

**Files:**
- Modify: `musefs-core/src/error.rs`
- Modify: `musefs-fuse/src/lib.rs:64-72` (the `errno` match) + add a test module
- Test: `musefs-fuse/src/lib.rs` (new inline `#[cfg(test)] mod errno_tests`)

- [ ] **Step 1: Write the failing test**

Append to `musefs-fuse/src/lib.rs`:

```rust
#[cfg(test)]
mod errno_tests {
    use super::errno;
    use musefs_core::CoreError;

    #[test]
    fn handle_table_full_maps_to_enfile() {
        assert_eq!(errno(&CoreError::HandleTableFull), fuser::Errno::ENFILE);
    }
}
```

- [ ] **Step 2: Run test to verify it fails (to compile)**

Run: `cargo test -p musefs-fuse errno_tests`
Expected: FAIL — compile error `no variant named HandleTableFull found for enum CoreError` (and a non-exhaustive-match error once the variant exists).

- [ ] **Step 3: Add the error variant**

In `musefs-core/src/error.rs`, add the variant to `CoreError` (after `NotADir`, before the closing brace at line 20-21):

```rust
    #[error("inode {0} is not a directory")]
    NotADir(u64),
    #[error("handle table full")]
    HandleTableFull,
}
```

- [ ] **Step 4: Add the `errno` arm**

In `musefs-fuse/src/lib.rs`, the `errno` match (lines 64-72) is exhaustive; add the arm:

```rust
pub fn errno(err: &CoreError) -> fuser::Errno {
    match err {
        CoreError::NoEntry(_) | CoreError::TrackNotFound(_) => fuser::Errno::ENOENT,
        CoreError::IsDir(_) => fuser::Errno::EISDIR,
        CoreError::NotADir(_) => fuser::Errno::ENOTDIR,
        CoreError::HandleTableFull => fuser::Errno::ENFILE,
        CoreError::Io(e) => fuser::Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)),
        CoreError::BackingChanged(_) | CoreError::Db(_) | CoreError::Format(_) => fuser::Errno::EIO,
    }
}
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p musefs-fuse errno_tests`
Expected: PASS (`handle_table_full_maps_to_enfile ... ok`).

- [ ] **Step 6: Commit**

```bash
git add musefs-core/src/error.rs musefs-fuse/src/lib.rs
git commit -m "feat(SP3): add CoreError::HandleTableFull mapped to ENFILE"
```

---

## Task 2: Swap `handles` to a lock-free `sharded-slab`

The whole crate must compile after this task, so all `handles` sites change together: struct field, `open`, the `handles()` accessor, `read`, `open_handle`, `release_handle`, the `AtomicU64` import, the `next_fh` field, and the lock-order comment. A new private `fh_from_key` helper isolates the key→`fh` mapping (the only custom logic) and is the TDD seam.

**Files:**
- Modify: `musefs-core/Cargo.toml` (add `sharded-slab`)
- Modify: `musefs-core/src/facade.rs` (imports line 2; struct 114-115; `open` 152-153; lock-order comment 340-348; accessor 350-354; `read` 608-617; `open_handle` 649-656; `release_handle` 660-662; add `fh_from_key`)
- Test: `musefs-core/src/facade.rs` (inline `#[cfg(test)] mod tests` at line 665)

- [ ] **Step 1: Write the failing test**

In `musefs-core/src/facade.rs`, inside the existing `#[cfg(test)] mod tests { use super::*; … }` block (starts line 665), add:

```rust
    #[test]
    fn fh_from_key_offsets_by_one_and_maps_full_to_error() {
        // None (slab at capacity) -> HandleTableFull.
        assert!(matches!(fh_from_key(None), Err(CoreError::HandleTableFull)));
        // Some(key) -> key + 1, so the fh is always non-zero (0 == "no handle").
        assert_eq!(fh_from_key(Some(0)).unwrap(), 1);
        assert_eq!(fh_from_key(Some(41)).unwrap(), 42);
    }
```

- [ ] **Step 2: Run test to verify it fails (to compile)**

Run: `cargo test -p musefs-core --lib fh_from_key`
Expected: FAIL — compile error `cannot find function fh_from_key in this scope`.

- [ ] **Step 3: Add the `sharded-slab` dependency**

In `musefs-core/Cargo.toml`, add to `[dependencies]` (keep the list alphabetical-ish, matching the existing block):

```toml
[dependencies]
arc-swap = "1"
dashmap = "6"
im = "15"
musefs-db = { path = "../musefs-db", version = "0.2.0" }
musefs-format = { path = "../musefs-format", version = "0.2.0" }
once_cell = "1"
sharded-slab = "0.1"
thiserror = "1"
```

(`dashmap` is listed now too so Task 3 doesn't re-touch the file; it is harmless-unused until Task 3.) `dashmap = "6"` resolves to 6.0.1 and `sharded-slab = "0.1"` to 0.1.7; `cargo build` updates `Cargo.lock`. This step needs crates.io registry access — if the execution box is offline, fetch these first.

- [ ] **Step 4: Add the `fh_from_key` helper**

In `musefs-core/src/facade.rs`, add a free function near the top of the `impl Musefs` block's neighborhood — place it just above `impl Musefs` (after the `SizeEntry` struct, before the `RefreshGuard` doc at line 66) so it is module-private:

```rust
/// Map a `sharded_slab::Slab` insert result to a FUSE file handle. The slab key
/// is offset by one so the wire `fh` is always non-zero (`fh == 0` means "no
/// handle" — `read` falls back to inode resolution). `None` means the slab is at
/// capacity, surfaced as an explicit error rather than a panic.
fn fh_from_key(key: Option<usize>) -> Result<u64> {
    key.map(|k| k as u64 + 1).ok_or(CoreError::HandleTableFull)
}
```

- [ ] **Step 5: Change the imports and struct fields**

In `musefs-core/src/facade.rs`, line 2, drop `AtomicU64` (no longer used):

```rust
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
```

In `struct Musefs` (lines 114-115), replace the two fields:

```rust
    handles: sharded_slab::Slab<Arc<Handle>>,
```

(Delete the `next_fh: AtomicU64,` line entirely. Leave `size_cache` as-is in this task.)

- [ ] **Step 6: Update the `open` constructor**

In `Musefs::open` (lines 152-153), replace the `handles`/`next_fh` initializers:

```rust
            handles: sharded_slab::Slab::new(),
```

(Delete the `next_fh: AtomicU64::new(0),` line.)

- [ ] **Step 7: Remove the `handles()` accessor**

Delete the `fn handles(&self) -> …` helper (lines 350-354):

```rust
    fn handles(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Arc<Handle>>> {
        self.handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
```

- [ ] **Step 8: Update the `read` fast-path**

In `read` (lines 610-616), replace the handle lookup. `Slab::get` returns a borrow guard; clone the `Arc` out and let the guard drop before `pool.with` (preserves "no in-memory lock held across I/O"):

```rust
        // Fast path: serve from the per-handle fd + cached layout (no open/stat).
        if fh != 0 {
            let handle = self.handles.get((fh - 1) as usize).map(|g| Arc::clone(&g));
            if let Some(h) = handle {
                return self
                    .pool
                    .with(|db| read_at_with_file(&h.resolved, db, &h.file, offset, size));
            }
        }
```

(Note: this corrects the spec's `(**g).clone()` — that would deref to `Handle`, which is not `Clone` because it holds a `File`. `Arc::clone(&g)` clones the `Arc`, the intended behavior.)

- [ ] **Step 9: Update `open_handle`**

In `open_handle` (lines 653-656), replace the `next_fh`/insert tail with the slab insert through `fh_from_key`:

```rust
        let resolved = self.pool.with(|db| self.cache.resolve(db, track_id))?;
        crate::metrics::on_open();
        let file = std::fs::File::open(&resolved.backing_path)?;
        validate_opened_backing(&file, &resolved)?;
        fh_from_key(self.handles.insert(Arc::new(Handle { resolved, file })))
```

- [ ] **Step 10: Update `release_handle`**

Replace `release_handle` (lines 660-662). Guard `fh != 0` so `fh - 1` cannot underflow:

```rust
    /// Drop an open handle (closes its backing fd when the last reference goes).
    pub fn release_handle(&self, fh: u64) {
        if fh != 0 {
            self.handles.remove((fh - 1) as usize);
        }
    }
```

- [ ] **Step 11: Update the lock-order comment (handles portion)**

In the comment block at lines 340-348, the `handles` mention must reflect that it is no longer a lock. Replace the block with (the `size_cache` line is finalized in Task 3 — for now state it is becoming a `DashMap`):

```rust
    // Lock order: acquire a DbPool connection (`pool.with`/`with_poll`) FIRST, then
    // any in-memory lock (`inodes`, the header cache's shards). `inodes` is held
    // inside `pool.with` during `refresh` — the one intentional exception where a
    // pool connection is held around an in-memory lock. `handles` is a lock-free
    // `sharded_slab::Slab`: its `get` guard is cloned-from and dropped before any
    // pool call, so it never participates in lock ordering. `size_cache` is a
    // `DashMap`; its per-shard guards are taken and released per op (never held
    // across a DB call), so it imposes no global ordering either.
```

- [ ] **Step 12: Run the helper test + the whole crate**

Run: `cargo test -p musefs-core --lib fh_from_key`
Expected: PASS.

Run: `cargo test -p musefs-core`
Expected: PASS — in particular `open_handle_read_and_release_roundtrip`, `open_handle_returns_distinct_ids_and_rejects_dirs`, and `release_handle_forces_fallback_on_next_read` (these exercise open/read-via-fh/release-then-stale-read against the new slab).

- [ ] **Step 13: Commit**

```bash
git add musefs-core/Cargo.toml musefs-core/src/facade.rs
git commit -m "perf(SP3): replace handles mutex with lock-free sharded-slab"
```

---

## Task 3: Swap `size_cache` to `DashMap`

Independent of Task 2; the crate compiles after this. Behavior-preserving refactor — the existing `getattr`/`poll_refresh` tests are the regression guard.

**Files:**
- Modify: `musefs-core/src/facade.rs` (struct 119; `open` 154; accessor 356-360; `getattr` 563+574; `poll_refresh` retain 435)

- [ ] **Step 1: Confirm the existing guard tests pass first (refactor baseline)**

Run: `cargo test -p musefs-core getattr_reresolves_size_after_content_version_bump poll_refresh_keeps_unchanged_entries_and_prunes_vanished`
Expected: PASS (establishes the behavior we must preserve).

- [ ] **Step 2: Change the struct field**

In `struct Musefs`, replace line 119:

```rust
    size_cache: dashmap::DashMap<i64, SizeEntry>,
```

- [ ] **Step 3: Update the `open` constructor**

In `Musefs::open`, replace line 154:

```rust
            size_cache: dashmap::DashMap::new(),
```

- [ ] **Step 4: Remove the `size_cache()` accessor**

Delete the helper (lines 356-360):

```rust
    fn size_cache(&self) -> std::sync::MutexGuard<'_, HashMap<i64, SizeEntry>> {
        self.size_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
```

- [ ] **Step 5: Update `getattr`**

In `getattr`, replace the cache read (line 563) so the `Ref` guard is dropped before the later `insert` on the same shard (copy out via `*e`):

```rust
            if let Some(e) = self.size_cache.get(&track_id).map(|e| *e) {
                if e.content_version == track.content_version {
```

and the cache write (line 574):

```rust
            self.size_cache.insert(
                track_id,
                SizeEntry {
                    content_version: track.content_version,
                    total_len: resolved.total_len,
                    mtime_secs: resolved.mtime_secs,
                },
            );
```

(`insert` is `self.size_cache.insert(...)`, no longer `self.size_cache().insert(...)`.)

- [ ] **Step 6: Update the `poll_refresh` prune**

Replace line 435:

```rust
        self.size_cache.retain(|k, _| live.contains(k));
```

`DashMap::retain` takes `|&K, &mut V| -> bool`; the closure binds `k: &i64` and ignores the value. It locks each shard in turn, so it must not run while a `Ref` is held — it does not (the surrounding code holds no `get` guard here).

- [ ] **Step 7: Run the guard tests + the whole crate**

Run: `cargo test -p musefs-core getattr_reresolves_size_after_content_version_bump poll_refresh_keeps_unchanged_entries_and_prunes_vanished`
Expected: PASS.

Run: `cargo test -p musefs-core`
Expected: PASS (whole crate).

- [ ] **Step 8: Commit**

```bash
git add musefs-core/src/facade.rs
git commit -m "perf(SP3): replace size_cache mutex with DashMap"
```

---

## Task 4: Read backing audio into `out`'s tail (kill the double-allocation)

**Files:**
- Modify: `musefs-core/src/reader.rs:423-434` (the `Segment::BackingAudio` arm of `read_segments`)
- Test (existing, run as gate): `musefs-core/tests/proptest_read_fidelity.rs`

- [ ] **Step 1: Run the byte-identity proptest first (baseline green)**

Run: `cargo test -p musefs-core --test proptest_read_fidelity`
Expected: PASS — this proptest asserts the spliced output is byte-identical; it is the gate for this change.

- [ ] **Step 2: Replace the BackingAudio arm**

In `musefs-core/src/reader.rs`, the `Segment::BackingAudio` arm currently reads into a throwaway `buf` then copies. Replace it (preserve the existing comment about ESTALE):

```rust
                Segment::BackingAudio { offset: bo, .. } => {
                    let f = file.expect("backing segment requires an open backing file");
                    // Finding #15 (ESTALE, untested by design): on an NFS-backed mount a stale file
                    // handle surfaces here as a raw io::Error from the positioned read (or as
                    // BackingChanged from the size/mtime re-validation) and is propagated verbatim
                    // through the FUSE layer. There is no test-framework support to inject NFS ESTALE,
                    // so this path is documented rather than covered.
                    let start = out.len();
                    out.resize(start + n, 0);
                    f.read_exact_at(&mut out[start..], bo + within)?;
                    crate::metrics::on_pread(n as u64);
                }
```

`out` is `Vec::with_capacity(end - offset)` (reader.rs:407) and segment lengths sum to exactly that, so `resize` never reallocates; `read_exact_at` fully overwrites the zero-fill or returns `Err`.

- [ ] **Step 3: Run the byte-identity proptest to verify it still passes**

Run: `cargo test -p musefs-core --test proptest_read_fidelity`
Expected: PASS (byte-identical output unchanged).

- [ ] **Step 4: Run the reader unit tests**

Run: `cargo test -p musefs-core --lib reader`
Expected: PASS (incl. `read_segments_returns_empty_past_end_of_range` and the ogg serve tests).

- [ ] **Step 5: Commit**

```bash
git add musefs-core/src/reader.rs
git commit -m "perf(SP3): read backing audio into out's tail (no temp buffer)"
```

---

## Task 5: ABA release+reopen fallback integration test

The existing `open_handle_read_and_release_roundtrip` covers release→stale-read→fallback. This adds the **reopen-between** case: after a handle is released and a new handle opened (slab slot may be reused with a bumped generation), a read on the *stale* `fh` must still fall back to inode resolution and serve correct bytes — never alias the new handle.

**Files:**
- Test: `musefs-core/tests/facade.rs` (add a `#[test]` near the other handle tests, ~line 357)

- [ ] **Step 1: Write the test**

Add to `musefs-core/tests/facade.rs`:

```rust
#[test]
fn stale_fh_after_release_and_reopen_falls_back() {
    let dir = tempfile::tempdir().unwrap();
    let db = scanned_db(dir.path());
    let fs = Musefs::open(db, config()).unwrap();

    let artist = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let (_, file_inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();
    let size = fs.getattr(file_inode).unwrap().size;
    let canonical = fs.read(file_inode, 0, 0, size).unwrap(); // inode fallback bytes

    let fh_a = fs.open_handle(file_inode).unwrap();
    fs.release_handle(fh_a);
    let fh_b = fs.open_handle(file_inode).unwrap();

    // Generation-encoded keys: the reissued handle must not collide with the stale one.
    assert_ne!(fh_a, fh_b);
    // A read on the stale fh_a misses the slab (None) and falls back to inode
    // resolution — correct bytes, never fh_b's handle, never a panic.
    let via_stale = fs.read(file_inode, fh_a, 0, size).unwrap();
    assert_eq!(via_stale, canonical);
    // The live handle still serves correctly.
    let via_live = fs.read(file_inode, fh_b, 0, size).unwrap();
    assert_eq!(via_live, canonical);

    fs.release_handle(fh_b);
}
```

(`scanned_db`, `config`, and the `Alice` track come from the existing test harness used by `open_handle_read_and_release_roundtrip` at line 337-340.)

- [ ] **Step 2: Run the test**

Run: `cargo test -p musefs-core --test facade stale_fh_after_release_and_reopen_falls_back`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add musefs-core/tests/facade.rs
git commit -m "test(SP3): stale fh after release+reopen falls back (slab generation)"
```

---

## Task 6: Full validation, benches, and docs

**Files:**
- Modify: `docs/superpowers/specs/2026-05-30-optimization-pass/README.md` (SP3 Plan column + results log)

- [ ] **Step 1: Format + lint**

Run: `cargo fmt && cargo clippy --all-targets`
Expected: no diff from `fmt`; clippy clean (no new warnings — confirm `AtomicU64`-unused is gone).

- [ ] **Step 2: Full workspace test suite**

Run: `cargo test`
Expected: PASS, all crates.

- [ ] **Step 3: Format-layer byte-identity proptests (fuzzing feature)**

Run: `cargo test -p musefs-format --features fuzzing && cargo test -p musefs-core --test proptest_read_fidelity`
Expected: PASS (byte-identical invariant + tag round-trip).

- [ ] **Step 4: FUSE end-to-end mount tests (the hard byte-identical gate)**

Run: `cargo test -p musefs-fuse -- --ignored`
Expected: PASS — `end_to_end_read_through_mount` and the other real-mount tests serve byte-identical audio. (Requires `/dev/fuse` + libfuse; on a box without them, record that this gate must run on the VPS before merge.)

- [ ] **Step 5: Regression bench — `sequential_read` (>10% gate)**

**Baseline source:** capture the pre-SP3 `sequential_read` median *before* execution begins, on the current `cb8441f` commit (the last commit before any code change), and record it — that is the comparison point. (No bench baseline for SP3 is pre-recorded in `BENCHMARKS.md`; SP2's entries are tree-refresh, not read-path.)

Run: `cargo bench -p musefs-core --bench read_throughput -- sequential_read`
Expected: the `ci` median must not rise >10% vs that captured baseline (per README convention; the alloc fix should hold or improve it). Record the median.

- [ ] **Step 6: Contention signal — `concurrent_read_walk`**

Run: `cargo bench -p musefs-core --bench read_throughput -- concurrent_read_walk`
Expected: record before/after medians (this bench names `handles`/`size_cache` contention as the SP3 target). Improvement or parity is the SP3 signal.

- [ ] **Step 7: Update the tracking README**

In `docs/superpowers/specs/2026-05-30-optimization-pass/README.md`, change the SP3 status-row State from `Plan` to `Implemented` (the Plan-column link is already populated — leave it as-is). Add a results-log bullet under "## Results log" with the format `tier · storage class · wall time · op counts · fsyncs · peak RSS`, citing the `sequential_read` and `concurrent_read_walk` medians from Steps 5-6.

- [ ] **Step 8: Commit**

```bash
git add docs/superpowers/specs/2026-05-30-optimization-pass/README.md
git commit -m "docs(SP3): record results, mark Implemented, link plan"
```

---

## Self-Review notes (for the executor)

- **Spec coverage:** Task 4 = change (1); Task 2 = change (2) + `HandleTableFull`/`ENFILE` (Task 1) + lock-order comment (handles); Task 3 = change (3) + lock-order comment (size_cache); Task 5 + Task 1 test = the two new tests the spec's Validation section asks for; Task 6 = the four validation gates + benches + README results.
- **`HandleTableFull` coverage is split intentionally:** the at-capacity `insert -> None` → `HandleTableFull` branch is covered by the `fh_from_key(None)` unit test (Task 2 Step 1), and the variant→`ENFILE` mapping by `handle_table_full_maps_to_enfile` (Task 1). A test that forces *real* slab exhaustion is deliberately not written — SP3 keeps sharded-slab's default (effectively unbounded) `Config`, so exhausting it in a test is infeasible and not worth a custom tiny-`Config` seam. The two tests together cover the full construction→errno path. The deferred residuals (Options 2/3) and YAGNI items are intentionally untouched.
- **Type consistency:** `fh_from_key(Option<usize>) -> Result<u64>` is defined in Task 2 Step 4 and used in Task 2 Step 9 and tested in Task 2 Step 1. `handles: sharded_slab::Slab<Arc<Handle>>` and `size_cache: dashmap::DashMap<i64, SizeEntry>` are consistent across struct/`open`/call sites. `Arc::clone(&g)` (not `(**g).clone()`) is used for the slab guard.
- **Ordering rationale:** Task 1 before Task 2 (variant referenced by `open_handle`); Tasks 2/3/4 mutually independent (each leaves the crate compiling); Task 5 after Task 2 (uses the slab); Task 6 last.
