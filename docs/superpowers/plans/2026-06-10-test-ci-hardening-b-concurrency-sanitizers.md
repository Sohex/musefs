# Workstream B — Concurrency coverage + sanitizers (#208) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the serve path real concurrent-reader coverage (same file from many threads, many files in parallel, sustained load) and run it under AddressSanitizer as a required gate, with ThreadSanitizer as a best-effort non-required signal.

**Architecture:** Two test levels. A **core-level** concurrent test (`musefs-core`, no FUSE) drives the `quick_cache` `HeaderCache` and concurrent SQLite WAL reads through per-thread read-only connections — it is deterministic and is the body the required **ASan** job runs. A **mount-level** stress test (`musefs-fuse`, `--ignored`) drives the real `DbPool::PerThread` FUSE worker threads; it runs in the existing `e2e` job and the non-required **TSan** job. TSan cannot instrument the system C libs (libfuse, libsqlite3), so it stays out of the `ci-ok` aggregator.

**Tech Stack:** Rust, `std::thread`, `std::sync::Barrier`/`Arc`, `HeaderCache` (quick_cache), `Db::open_readonly` (WAL), FUSE integration tests, nightly `-Zsanitizer=address|thread`.

**Design decisions (read before starting):**
- The required sanitizer gate runs the **core-level** test, not the FUSE mount, because a sanitizer-instrumented FUSE mount in CI is the flaky part the spec flagged. ASan still interposes `malloc` globally, so it catches heap errors in the C deps reached from the core test too.
- **No `-Zbuild-std`** in the required ASan job (the spec's stated fallback: keep it fast and reliable). `-Zbuild-std` is left as an optional enhancement noted in Task 3, not implemented.
- Stress tests use **no cargo feature** so the existing `e2e` job (which runs `cargo test -p musefs-fuse -- --ignored`) picks up the mount-level test, and the default `check` job (`cargo test --workspace`) picks up the core-level test, with zero extra wiring.
- Determinism: bounded iteration counts, a `Barrier` to start threads together (no sleeps), assertions on **bytes/outcomes** (correct content, no panic, completes within the test harness — no timing thresholds).

---

### Task 1: Core-level concurrent-reader test (the ASan body)

**Files:**
- Create: `musefs-core/tests/concurrent_reads.rs` (no feature gate, NOT `#[ignore]` — runs in the default suite)
- May modify: `musefs-core/tests/common/mod.rs` (only if a needed helper is missing)

- [ ] **Step 1: Write the failing test**

Create `musefs-core/tests/concurrent_reads.rs`:

```rust
//! Concurrent-reader coverage for the serve path WITHOUT a FUSE mount: many
//! threads share one `HeaderCache` and each holds its own read-only WAL
//! connection, exercising the quick_cache header cache and concurrent SQLite
//! reads under contention. Deterministic (bounded, barrier-synchronized,
//! asserts on bytes) so it can gate CI and run under AddressSanitizer.
mod common;

use std::sync::{Arc, Barrier};

use musefs_core::reader::{read_at_with_file, HeaderCache, Mode};
use musefs_db::Db;

/// Build a file-backed store with `n` FLAC tracks (each a real backing file),
/// returning (db_path, track_ids, dir). Per-track audio differs so a
/// cross-wired read is detectable.
fn build_store(n: usize) -> (std::path::PathBuf, Vec<i64>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("musefs.db");
    let db = Db::open(&db_path).unwrap();
    let mut ids = Vec::new();
    for i in 0..n {
        let src = dir.path().join(format!("track{i}.flac"));
        let audio: Vec<u8> = (0..8192).map(|b| (b as u8).wrapping_add(i as u8)).collect();
        let (audio_offset, audio_length) =
            common::write_flac(&src, &[&format!("TITLE=T{i}")], &audio);
        let id = db
            .upsert_track(&musefs_db::NewTrack {
                backing_path: src.to_string_lossy().into_owned(),
                format: musefs_db::Format::Flac,
                audio_offset,
                audio_length,
                backing_size: std::fs::metadata(&src).unwrap().len(),
                backing_mtime: common::real_mtime(&src),
            })
            .unwrap();
        db.replace_tags(id, &[musefs_db::Tag::new("title", &format!("T{i}"), 0)])
            .unwrap();
        ids.push(id);
    }
    drop(db); // flush; threads reopen read-only
    (db_path, ids, dir)
}

/// Resolve + read one track fully on its own read-only connection + shared cache.
fn read_full(db_path: &std::path::Path, cache: &HeaderCache, id: i64) -> Vec<u8> {
    let db = Db::open_readonly(db_path).unwrap();
    let resolved = cache.resolve(&db, id).unwrap();
    let file = std::fs::File::open(&resolved.backing_path).unwrap();
    read_at_with_file(&resolved, &db, &file, 0, resolved.total_len).unwrap()
}

#[test]
fn same_file_from_many_threads_returns_identical_bytes() {
    let (db_path, ids, _dir) = build_store(1);
    let cache = Arc::new(HeaderCache::new(Mode::Synthesis));
    let reference = read_full(&db_path, &cache, ids[0]);

    const THREADS: usize = 16;
    const ITERS: usize = 50;
    let barrier = Arc::new(Barrier::new(THREADS));
    let db_path = Arc::new(db_path);
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let (cache, barrier, db_path, reference) =
                (cache.clone(), barrier.clone(), db_path.clone(), reference.clone());
            std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..ITERS {
                    let got = read_full(&db_path, &cache, 0 + /*id*/ 0);
                    assert_eq!(got, reference, "concurrent same-file read diverged");
                }
            })
        })
        .collect();
    // fix the id capture: read the real id
    for h in handles {
        h.join().unwrap();
    }
    let _ = ids;
}

#[test]
fn many_files_in_parallel_return_correct_bytes() {
    const N: usize = 12;
    let (db_path, ids, _dir) = build_store(N);
    let cache = Arc::new(HeaderCache::new(Mode::Synthesis));
    let references: Vec<Vec<u8>> = ids.iter().map(|&id| read_full(&db_path, &cache, id)).collect();

    let barrier = Arc::new(Barrier::new(N));
    let db_path = Arc::new(db_path);
    let ids = Arc::new(ids);
    let references = Arc::new(references);
    let handles: Vec<_> = (0..N)
        .map(|t| {
            let (cache, barrier, db_path, ids, references) = (
                cache.clone(),
                barrier.clone(),
                db_path.clone(),
                ids.clone(),
                references.clone(),
            );
            std::thread::spawn(move || {
                barrier.wait();
                for k in 0..30 {
                    let idx = (t + k) % ids.len();
                    let got = read_full(&db_path, &cache, ids[idx]);
                    assert_eq!(got, references[idx], "parallel read of track {idx} wrong");
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
}

#[test]
fn sustained_mixed_load_does_not_deadlock_or_corrupt() {
    const N: usize = 6;
    let (db_path, ids, _dir) = build_store(N);
    let cache = Arc::new(HeaderCache::new(Mode::Synthesis));
    let references: Vec<Vec<u8>> = ids.iter().map(|&id| read_full(&db_path, &cache, id)).collect();

    const THREADS: usize = 24;
    const ITERS: usize = 100;
    let barrier = Arc::new(Barrier::new(THREADS));
    let db_path = Arc::new(db_path);
    let ids = Arc::new(ids);
    let references = Arc::new(references);
    let handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let (cache, barrier, db_path, ids, references) = (
                cache.clone(),
                barrier.clone(),
                db_path.clone(),
                ids.clone(),
                references.clone(),
            );
            std::thread::spawn(move || {
                barrier.wait();
                for k in 0..ITERS {
                    let idx = (t * 7 + k) % ids.len();
                    let got = read_full(&db_path, &cache, ids[idx]);
                    assert_eq!(got, references[idx]);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
}
```

Fix the deliberate placeholder in `same_file_from_many_threads_returns_identical_bytes`: replace `read_full(&db_path, &cache, 0 + /*id*/ 0)` with the captured real id — restructure that test to clone `ids[0]` into the threads:

```rust
    let id0 = ids[0];
    // ... inside the closure:
                    let got = read_full(&db_path, &cache, id0);
```

(Keep the rest as written; `id0` is `Copy`, capture it by value.)

- [ ] **Step 2: Run the test to verify it compiles and passes**

Run: `cargo test -p musefs-core --test concurrent_reads`
Expected: PASS (3 tests). If it does not compile, reconcile the public names: `read_at_with_file` (`reader.rs:460`), `HeaderCache`/`Mode`/`ResolvedFile`, `Db::open`/`open_readonly`, `NewTrack`/`Tag`/`Format`, and `common::write_flac`/`common::real_mtime`. These are confirmed present except `real_mtime` — if it lives only in `interop_emit.rs`, lift it into `common/mod.rs` as `pub fn real_mtime(p: &std::path::Path) -> i64` first and commit that.

- [ ] **Step 3: Stress it locally to flush out nondeterminism**

Run: `for i in $(seq 1 20); do cargo test -p musefs-core --test concurrent_reads -- --test-threads=4 || break; done`
Expected: 20 clean passes. If any run fails or hangs, that is a real finding — STOP and use systematic-debugging before proceeding (the test must be a reliable gate).

- [ ] **Step 4: Commit**

```bash
git add musefs-core/tests/concurrent_reads.rs musefs-core/tests/common/mod.rs
git commit -m "test(core): concurrent HeaderCache + WAL-read coverage (#208)"
```

---

### Task 2: Mount-level concurrent FUSE-reader stress test

**Files:**
- Create: `musefs-fuse/tests/concurrent_reads.rs` (no feature gate, `#[ignore]` — needs `/dev/fuse`)

- [ ] **Step 1: Inspect the existing mount harness**

Read `musefs-fuse/tests/concurrency.rs` and `musefs-fuse/tests/mount.rs` for the exact private helpers (`make_flac`, `scan_directory` import, `Musefs::open`, `config()`, `musefs_fuse::spawn`, `BackgroundSession`). Reuse that pattern verbatim — the helpers are per-binary-private, so duplicate the minimal setup into the new file.

- [ ] **Step 2: Write the failing test**

Create `musefs-fuse/tests/concurrent_reads.rs` (fill in the harness helpers by copying from `concurrency.rs`/`mount.rs`):

```rust
//! Concurrent reads through a real FUSE mount: the same file from many threads
//! and many files in parallel, driving the DbPool::PerThread worker pool.
//! `--ignored` (needs /dev/fuse); runs in the e2e job and the TSan job.

use std::sync::{Arc, Barrier};

// --- copy these helpers verbatim from mount.rs / concurrency.rs ---
// fn make_flac(comments: &[&str], audio: &[u8]) -> Vec<u8> { ... }
// fn config() -> musefs_core::... { ... }
// use musefs_core::scan::scan_directory;  // adjust to the real path
// use musefs_fuse::Musefs;                // adjust to the real type/path
// ------------------------------------------------------------------

fn setup_mount() -> (tempfile::TempDir, tempfile::TempDir, /*session*/ impl Sized) {
    let backing = tempfile::tempdir().unwrap();
    for i in 0..8 {
        let audio: Vec<u8> = (0..(128 * 1024)).map(|b| (b as u8).wrapping_add(i)).collect();
        let flac = make_flac(&[&format!("ARTIST=A{i}"), &format!("TITLE=S{i}")], &audio);
        std::fs::write(backing.path().join(format!("t{i}.flac")), &flac).unwrap();
    }
    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, backing.path()).unwrap();
    let fs = Musefs::open(db, config()).unwrap();
    let mountpoint = tempfile::tempdir().unwrap();
    let session = musefs_fuse::spawn(fs, mountpoint.path(), "musefs-concurrent-reads").unwrap();
    (backing, mountpoint, session)
}

fn list_songs(mnt: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut v = Vec::new();
    for artist in std::fs::read_dir(mnt).unwrap() {
        let artist = artist.unwrap().path();
        if artist.is_dir() {
            for song in std::fs::read_dir(&artist).unwrap() {
                v.push(song.unwrap().path());
            }
        }
    }
    v.sort();
    v
}

#[test]
#[ignore = "requires /dev/fuse; run with: cargo test -p musefs-fuse -- --ignored"]
fn same_file_many_threads_through_mount() {
    let (_backing, mnt, _session) = setup_mount();
    let songs = list_songs(mnt.path());
    let target = songs[0].clone();
    let reference = std::fs::read(&target).unwrap();

    const THREADS: usize = 12;
    let barrier = Arc::new(Barrier::new(THREADS));
    let target = Arc::new(target);
    let reference = Arc::new(reference);
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let (barrier, target, reference) = (barrier.clone(), target.clone(), reference.clone());
            std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..20 {
                    assert_eq!(&std::fs::read(&*target).unwrap(), &*reference);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
}

#[test]
#[ignore = "requires /dev/fuse; run with: cargo test -p musefs-fuse -- --ignored"]
fn many_files_in_parallel_through_mount() {
    let (_backing, mnt, _session) = setup_mount();
    let songs = Arc::new(list_songs(mnt.path()));
    let references: Vec<Vec<u8>> = songs.iter().map(|p| std::fs::read(p).unwrap()).collect();
    let references = Arc::new(references);

    let n = songs.len();
    let barrier = Arc::new(Barrier::new(n));
    let handles: Vec<_> = (0..n)
        .map(|t| {
            let (songs, references, barrier) = (songs.clone(), references.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                for k in 0..15 {
                    let idx = (t + k) % songs.len();
                    assert_eq!(std::fs::read(&songs[idx]).unwrap(), references[idx]);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
}
```

- [ ] **Step 3: Run it to verify it compiles and passes**

Run: `cargo test -p musefs-fuse --test concurrent_reads -- --ignored --nocapture`
Expected: PASS (2 tests). Debug the harness against `concurrency.rs` if helper names differ.

- [ ] **Step 4: Stress it locally**

Run: `for i in $(seq 1 10); do cargo test -p musefs-fuse --test concurrent_reads -- --ignored || break; done`
Expected: 10 clean passes. A hang/failure is a real finding → systematic-debugging.

- [ ] **Step 5: Commit**

```bash
git add musefs-fuse/tests/concurrent_reads.rs
git commit -m "test(fuse): concurrent-reader stress through the mount (DbPool::PerThread) (#208)"
```

---

### Task 3: Required ASan CI job

**Files:**
- Modify: `.github/workflows/ci.yml` (add an `asan` job; add it to `ci-ok`'s `needs:`)

- [ ] **Step 1: Add the `asan` job**

In `.github/workflows/ci.yml`, after the `macos` job (before `freebsd`), add:

```yaml
  asan:
    # AddressSanitizer over the core-level concurrent test. Nightly + explicit
    # target are required for -Zsanitizer. We do NOT use -Zbuild-std (kept fast
    # and reliable); ASan still interposes malloc globally, so it catches heap
    # errors reached in the C deps (libsqlite3) from this test. detect_leaks=0:
    # this gate is about memory-safety (UAF/OOB), not leaks (sqlite/global
    # statics produce benign LSan noise).
    needs: changes
    if: needs.changes.outputs.src == 'true'
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@df4cb1c069e1874edd31b4311f1884172cec0e10
        with:
          persist-credentials: false
      - uses: dtolnay/rust-toolchain@29eef336d9b2848a0b548edc03f92a220660cdb8
        with:
          toolchain: nightly
          targets: x86_64-unknown-linux-gnu
      - uses: Swatinem/rust-cache@e18b497796c12c097a38f9edb9d0641fb99eee32
      - name: AddressSanitizer (core concurrent reads)
        env:
          RUSTFLAGS: "-Zsanitizer=address"
          ASAN_OPTIONS: "detect_leaks=0"
        run: >-
          cargo +nightly test -p musefs-core --test concurrent_reads
          --target x86_64-unknown-linux-gnu
```

- [ ] **Step 2: Add `asan` to the `ci-ok` aggregator**

In the `ci-ok` job's `needs:` list, add `asan`:

```yaml
  ci-ok:
    if: always()
    needs: [changes, check, interop, python-musefs, beets, lidarr, picard, e2e, macos, freebsd, asan]
```

- [ ] **Step 3: Validate the YAML**

Run: `python -c "import yaml; yaml.safe_load(open('.github/workflows/ci.yml')); print('ok')"`
Expected: `ok`.

- [ ] **Step 4: Reproduce the ASan run locally (the user's server has a nightly toolchain)**

Run:
```bash
rustup toolchain install nightly --profile minimal 2>/dev/null || true
RUSTFLAGS="-Zsanitizer=address" ASAN_OPTIONS="detect_leaks=0" \
  cargo +nightly test -p musefs-core --test concurrent_reads --target x86_64-unknown-linux-gnu
```
Expected: PASS, no ASan report. If ASan reports a real error, that is the kind of defect this gate exists to catch → systematic-debugging, do not suppress.

> Optional enhancement (NOT implemented here): adding `-Zbuild-std` (with `components: rust-src` and `-Z build-std`) instruments std for deeper coverage at a large build-time cost. Leave it out unless a defect demands it.

- [ ] **Step 5: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: required AddressSanitizer gate over the core concurrent test (#208)"
```

---

### Task 4: Non-required TSan CI job (best-effort)

**Files:**
- Modify: `.github/workflows/ci.yml` (add a `tsan` job; do NOT add it to `ci-ok`)

- [ ] **Step 1: Add the `tsan` job**

In `.github/workflows/ci.yml`, after the `asan` job, add:

```yaml
  tsan:
    # ThreadSanitizer is BEST-EFFORT and deliberately NOT in ci-ok: it cannot
    # instrument the system C libs (libfuse, libsqlite3), so it sees races in
    # our code around the FFI but may miss or false-positive inside the C deps.
    # continue-on-error keeps a noisy run from showing as a hard failure.
    needs: changes
    if: needs.changes.outputs.src == 'true'
    runs-on: ubuntu-latest
    continue-on-error: true
    steps:
      - uses: actions/checkout@df4cb1c069e1874edd31b4311f1884172cec0e10
        with:
          persist-credentials: false
      - name: Install libfuse3
        run: sudo apt-get update && sudo apt-get install -y fuse3 libfuse3-dev pkg-config
      - uses: dtolnay/rust-toolchain@29eef336d9b2848a0b548edc03f92a220660cdb8
        with:
          toolchain: nightly
          targets: x86_64-unknown-linux-gnu
      - uses: Swatinem/rust-cache@e18b497796c12c097a38f9edb9d0641fb99eee32
      - name: ThreadSanitizer (core concurrent reads)
        env:
          RUSTFLAGS: "-Zsanitizer=thread"
          TSAN_OPTIONS: "halt_on_error=0"
        run: >-
          cargo +nightly test -p musefs-core --test concurrent_reads
          --target x86_64-unknown-linux-gnu
      - name: ThreadSanitizer (mount concurrent reads, best-effort)
        env:
          RUSTFLAGS: "-Zsanitizer=thread"
          TSAN_OPTIONS: "halt_on_error=0"
        run: >-
          cargo +nightly test -p musefs-fuse --test concurrent_reads
          --target x86_64-unknown-linux-gnu -- --ignored
```

- [ ] **Step 2: Validate the YAML and confirm `tsan` is NOT in `ci-ok`**

Run: `python -c "import yaml; d=yaml.safe_load(open('.github/workflows/ci.yml')); n=d['jobs']['ci-ok']['needs']; assert 'tsan' not in n, n; assert 'asan' in n; print('ok:', n)"`
Expected: `ok: [... 'asan']` (asan present, tsan absent).

- [ ] **Step 3: Best-effort local check of the core TSan run**

Run:
```bash
RUSTFLAGS="-Zsanitizer=thread" TSAN_OPTIONS="halt_on_error=0" \
  cargo +nightly test -p musefs-core --test concurrent_reads --target x86_64-unknown-linux-gnu || true
```
Expected: completes (may print TSan warnings — that is acceptable for the non-required signal). A *workspace-code* data-race report is worth investigating; C-dep noise is expected.

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: best-effort ThreadSanitizer signal (non-required) (#208)"
```

---

### Task 5: Document the concurrency tier + sanitizers

**Files:**
- Modify: `CONTRIBUTING.md` (the "Test tiers beyond `cargo test`" section)

- [ ] **Step 1: Add a subsection**

In `CONTRIBUTING.md`, after the "Mutation testing" subsection (or adjacent to the interop/fault tiers), add:

```markdown
### Concurrency + sanitizers

Concurrent-reader coverage exists at two levels:

```bash
cargo test -p musefs-core --test concurrent_reads          # core: HeaderCache + WAL reads (default suite)
cargo test -p musefs-fuse --test concurrent_reads -- --ignored  # mount: DbPool::PerThread (needs /dev/fuse)
```

CI runs the core test under **AddressSanitizer** as a required gate (`asan` job)
and both tests under **ThreadSanitizer** as a non-required best-effort signal
(`tsan` job, `continue-on-error`). TSan cannot instrument the system C libraries
(libfuse, libsqlite3), so it is a signal, not a gate — reproduce locally with:

```bash
rustup toolchain install nightly
RUSTFLAGS="-Zsanitizer=address" ASAN_OPTIONS="detect_leaks=0" \
  cargo +nightly test -p musefs-core --test concurrent_reads --target x86_64-unknown-linux-gnu
RUSTFLAGS="-Zsanitizer=thread" \
  cargo +nightly test -p musefs-core --test concurrent_reads --target x86_64-unknown-linux-gnu
```
```

- [ ] **Step 2: Commit**

```bash
git add CONTRIBUTING.md
git commit -m "docs: document the concurrency + sanitizer test tier (#208)"
```

---

### Task 6: Final verification

- [ ] **Step 1: Run the new tests**

```bash
cargo test -p musefs-core --test concurrent_reads
cargo test -p musefs-fuse --test concurrent_reads -- --ignored
```
Expected: PASS.

- [ ] **Step 2: ASan parity with CI**

```bash
RUSTFLAGS="-Zsanitizer=address" ASAN_OPTIONS="detect_leaks=0" \
  cargo +nightly test -p musefs-core --test concurrent_reads --target x86_64-unknown-linux-gnu
```
Expected: PASS, no ASan report.

- [ ] **Step 3: Pre-commit parity**

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --workspace
```
Expected: all PASS (the core concurrent test runs here too).

---

## Self-review notes (for the implementer)

- **Spec coverage:** same-file/many-files/sustained-load → Tasks 1 (core) & 2 (mount); ASan required → Task 3; TSan non-required → Task 4; docs → Task 5. The spec's "stress tests must be deterministic to gate" is enforced by Steps 3 of Tasks 1–2 (repeat-run stress).
- **`ci-ok` discipline:** `asan` IS added to `needs:` (required); `tsan` is NOT (verified by Task 4 Step 2). Both gated on `needs.changes.outputs.src`.
- **Type consistency:** `read_at_with_file`, `HeaderCache`, `Mode`, `Db::open`/`open_readonly`, `NewTrack`, `Tag`, `Format`, `common::write_flac`, `common::real_mtime` used identically across tasks. The deliberate placeholder in Task 1 Step 1 is fixed in the same step (the `id0` capture).
- **Watch-out:** the mount-test harness helpers (`make_flac`, `config`, `scan_directory`, `Musefs`) must be copied from `concurrency.rs`/`mount.rs` — confirm their exact module paths before writing Task 2.
