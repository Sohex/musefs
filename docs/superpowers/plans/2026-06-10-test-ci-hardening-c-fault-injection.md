# Workstream C — Failure-path fault injection (#209) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Exercise the reader and DB failure paths under simulated runtime faults — an `EIO`/short backing read, a backing file changed mid-flight, and a corrupt SQLite database — asserting the error variant, its errno mapping, and post-failure cache state.

**Architecture:** Add a process-global, per-test-configurable fault seam at the single positioned-backing-read call site (`reader.rs`, the `BackingAudio` arm feeding `metrics::on_pread`), gated behind the existing `metrics` cargo feature so it compiles out of default/release builds and is visible across crate boundaries (a `musefs-fuse` mount test can drive it). `BackingChanged` and DB-corruption are driven by **real conditions** (mutating the backing file / corrupting the DB bytes), which need no seam and run in the default suite.

**Tech Stack:** Rust, `std::os::unix::fs::FileExt`, `std::sync::atomic`, the `metrics` feature, `tempfile`, FUSE integration tests (`--ignored`, need `/dev/fuse`).

**Scope decisions (read before starting):**
- The seam covers **EIO** and **short read** at the backing-read boundary. `BackingChanged` is NOT injected through the seam: the size/mtime re-validation lives in `HeaderCache::resolve` (`reader.rs:117-121`), not per-read, so it is triggered by mutating the real backing file before `resolve` — deterministic and seam-free.
- DB read faults are covered by **byte-corruption** of the SQLite file (deterministic, fast). The serve path is read-only, so `ENOSPC`/read-only-dir faults belong to the *write* (scan) path and are **out of scope here** — they are the spec's documented best-effort case and not part of this plan.
- **SQLITE_BUSY / exclusive-lock fault is intentionally not tested** (the spec lists it under "DB faults via real conditions"). The serve path opens WAL read-only connections, and WAL readers are **contention-free by design** — that is the entire point of `DbPool::PerThread` (a concurrent writer never makes a read return `SQLITE_BUSY`). The only way to force `SQLITE_BUSY` on a reader is a pathological `PRAGMA locking_mode=EXCLUSIVE`, a configuration musefs never sets. Writing such a test would assert the behaviour of a config the product doesn't use. Lock *contention* correctness (concurrent readers don't corrupt/deadlock) is instead proven by the Workstream B concurrency stress tests. **This is a deliberate deviation from the spec's "lock + corruption stay mandatory" wording and needs the maintainer's blessing; reconcile the spec's Workstream C / Open-risks sections to match once blessed.**
- The seam is process-global (an atomic gate + RAII reset guard). Fault tests must run single-threaded within their own test binary (`#![cfg(feature = "metrics")]` at file top, like the existing `musefs-core/tests/fault_injection.rs`), because two tests setting the global fault concurrently would interfere.

---

### Task 1: Add the backing-read fault seam to the metrics module

**Files:**
- Modify: `musefs-core/src/metrics.rs` (the `#[cfg(feature = "metrics")] mod imp`, the `#[cfg(not(feature = "metrics"))] mod imp`, and the `tests` mod)

- [ ] **Step 1: Write the failing unit test**

In `musefs-core/src/metrics.rs`, inside `#[cfg(all(test, feature = "metrics"))] mod tests`, add (after the existing `scan_counters_accumulate_and_reset` test):

```rust
    #[test]
    fn backing_fault_injects_eio_then_clears_on_drop() {
        use std::io::Write;
        use std::os::unix::fs::FileExt;

        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"hello world").unwrap();
        let f = std::fs::File::open(tmp.path()).unwrap();

        // No fault: real read succeeds.
        let mut buf = [0u8; 5];
        backing_read_exact_at(&f, &mut buf, 0).unwrap();
        assert_eq!(&buf, b"hello");

        {
            let _guard = set_backing_fault(BackingFault::Eio);
            let err = backing_read_exact_at(&f, &mut buf, 0).unwrap_err();
            assert_eq!(err.raw_os_error(), Some(5), "EIO == 5");
        }

        // Guard dropped: fault cleared, real read works again.
        let mut buf2 = [0u8; 5];
        backing_read_exact_at(&f, &mut buf2, 6).unwrap();
        assert_eq!(&buf2, b"world");

        // Sanity: the std read path still fills the same bytes.
        let mut direct = [0u8; 5];
        f.read_exact_at(&mut direct, 0).unwrap();
        assert_eq!(&direct, b"hello");
    }

    #[test]
    fn backing_fault_short_read_fills_prefix_then_errors() {
        use std::io::Write;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"abcdefgh").unwrap();
        let f = std::fs::File::open(tmp.path()).unwrap();

        let mut buf = [0u8; 8];
        let _guard = set_backing_fault(BackingFault::ShortRead { prefix: 3 });
        let err = backing_read_exact_at(&f, &mut buf, 0).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
        assert_eq!(&buf[..3], b"abc", "prefix bytes were filled before the fault");
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-core --features metrics backing_fault`
Expected: FAIL to **compile** — `cannot find function backing_read_exact_at`, `cannot find type BackingFault`, `cannot find function set_backing_fault`.

- [ ] **Step 3: Implement the seam in the `metrics`-on module**

In `musefs-core/src/metrics.rs`, in `#[cfg(feature = "metrics")] mod imp`, change the atomic imports line:

```rust
    use std::sync::atomic::{AtomicU64, Ordering};
```

to:

```rust
    use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};
```

Then add, immediately after the `static PREAD_FAULT: OnceLock<Option<Duration>> = OnceLock::new();` line:

```rust
    // Backing-read fault seam (test-only; process-global so it reaches the FUSE
    // worker thread that actually performs the read — a thread-local set on the
    // test thread would not). Kind: 0=none, 1=EIO, 2=short read. Distinct from
    // the latency-only `set_fault_pread` hook above.
    static BACKING_FAULT_KIND: AtomicU8 = AtomicU8::new(0);
    static BACKING_FAULT_PREFIX: AtomicUsize = AtomicUsize::new(0);

    /// A simulated backing-read failure, set per test via [`set_backing_fault`].
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum BackingFault {
        /// Return `EIO` instead of reading any bytes.
        Eio,
        /// Fill the first `prefix` bytes from the file, then return
        /// `UnexpectedEof` (simulating a truncated/short pread).
        ShortRead { prefix: usize },
    }

    /// Clears the global backing fault when dropped, so a fault never leaks past
    /// the test that set it.
    #[must_use = "the fault is cleared when this guard drops; bind it to a name"]
    pub struct BackingFaultGuard(());

    impl Drop for BackingFaultGuard {
        fn drop(&mut self) {
            BACKING_FAULT_KIND.store(0, Ordering::SeqCst);
        }
    }

    /// Install a backing-read fault for the current test scope. Process-global:
    /// tests using it must run single-threaded (their own `metrics`-gated test
    /// binary), like `fault_injection.rs`.
    pub fn set_backing_fault(fault: BackingFault) -> BackingFaultGuard {
        match fault {
            BackingFault::Eio => {
                BACKING_FAULT_KIND.store(1, Ordering::SeqCst);
            }
            BackingFault::ShortRead { prefix } => {
                BACKING_FAULT_PREFIX.store(prefix, Ordering::SeqCst);
                BACKING_FAULT_KIND.store(2, Ordering::SeqCst);
            }
        }
        BackingFaultGuard(())
    }

    /// Positioned backing read used by the serve path. Honors an injected fault
    /// when one is set; otherwise a plain `read_exact_at`. The no-fault path is a
    /// single relaxed atomic load.
    pub fn backing_read_exact_at(
        f: &std::fs::File,
        buf: &mut [u8],
        offset: u64,
    ) -> std::io::Result<()> {
        use std::os::unix::fs::FileExt;
        match BACKING_FAULT_KIND.load(Ordering::SeqCst) {
            // EIO is 5 on Linux, macOS, and FreeBSD.
            1 => return Err(std::io::Error::from_raw_os_error(5)),
            2 => {
                let p = BACKING_FAULT_PREFIX.load(Ordering::SeqCst).min(buf.len());
                f.read_exact_at(&mut buf[..p], offset)?;
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "injected short backing read",
                ));
            }
            _ => {}
        }
        f.read_exact_at(buf, offset)
    }
```

- [ ] **Step 4: Implement the no-op passthrough in the `metrics`-off module**

In `musefs-core/src/metrics.rs`, in `#[cfg(not(feature = "metrics"))] mod imp`, add (next to the other `#[inline(always)]` stubs, e.g. after `pub fn set_fault_pread(_d: ...) {}`):

```rust
    #[inline(always)]
    pub fn backing_read_exact_at(
        f: &std::fs::File,
        buf: &mut [u8],
        offset: u64,
    ) -> std::io::Result<()> {
        use std::os::unix::fs::FileExt;
        f.read_exact_at(buf, offset)
    }
```

Note: `BackingFault`, `BackingFaultGuard`, and `set_backing_fault` are intentionally NOT defined in the off arm — they are only referenced from `metrics`-gated tests.

- [ ] **Step 5: Run the unit tests to verify they pass**

Run: `cargo test -p musefs-core --features metrics backing_fault`
Expected: PASS (2 tests).

- [ ] **Step 6: Verify the metrics-off build still compiles**

Run: `cargo build -p musefs-core`
Expected: success (the off-arm `backing_read_exact_at` exists; nothing references the gated types).

- [ ] **Step 7: Commit**

```bash
git add musefs-core/src/metrics.rs
git commit -m "feat(core): add metrics-gated backing-read fault seam (#209)"
```

---

### Task 2: Route the serve path through the seam

**Files:**
- Modify: `musefs-core/src/reader.rs` (the `BackingAudio` arm in `read_segments_into`, around line 381; and the `use std::os::unix::fs::FileExt;` at line 350)

- [ ] **Step 1: Write the failing integration test**

Create `musefs-core/tests/reader_faults.rs`:

```rust
//! Reader failure paths under injected backing-read faults. Own single-test
//! binary gated on `metrics`: the fault seam is process-global, so these run
//! single-threaded (one test binary, default serial within it).
#![cfg(feature = "metrics")]

mod common;

use musefs_core::metrics::{set_backing_fault, BackingFault};
use musefs_core::reader::{read_at, HeaderCache};
use musefs_core::{CoreError, Mode}; // Mode is re-exported at the crate root, NOT in `reader`
use musefs_db::Db;

fn resolve_one_flac() -> (Db, std::sync::Arc<musefs_core::reader::ResolvedFile>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("a.flac");
    let (audio_offset, audio_length) = common::write_flac(&src, &["TITLE=Faulty"], &[0xAB; 4096]);
    let db = Db::open_in_memory().unwrap();
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
    db.replace_tags(id, &[musefs_db::Tag::new("title", "Faulty", 0)])
        .unwrap();
    let resolved = HeaderCache::new(Mode::Synthesis).resolve(&db, id).unwrap();
    (db, resolved, dir)
}

#[test]
fn eio_on_backing_read_surfaces_as_core_io_error() {
    let (db, resolved, _dir) = resolve_one_flac();
    let _guard = set_backing_fault(BackingFault::Eio);
    // Read a range that lands in the audio (BackingAudio) segment.
    let err = read_at(&resolved, &db, resolved.total_len - 16, 16).unwrap_err();
    match err {
        CoreError::Io(e) => assert_eq!(e.raw_os_error(), Some(5), "EIO maps to CoreError::Io(EIO)"),
        other => panic!("expected CoreError::Io, got {other:?}"),
    }
}

#[test]
fn short_backing_read_surfaces_as_core_io_error() {
    let (db, resolved, _dir) = resolve_one_flac();
    let _guard = set_backing_fault(BackingFault::ShortRead { prefix: 2 });
    let err = read_at(&resolved, &db, resolved.total_len - 16, 16).unwrap_err();
    match err {
        CoreError::Io(e) => assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof),
        other => panic!("expected CoreError::Io(UnexpectedEof), got {other:?}"),
    }
}
```

Note: confirm the re-exports used here exist — `musefs_core::reader::{read_at, HeaderCache, Mode, ResolvedFile}`, `musefs_core::CoreError`, `musefs_db::{Db, NewTrack, Format, Tag}`, and `common::write_flac` / `common::real_mtime` (the latter is used by `interop_emit.rs`; if `real_mtime` is not in `tests/common/mod.rs`, copy its one-line body — `std::fs::metadata(p).unwrap().modified()...` — from `interop_emit.rs` into `common/mod.rs` as a `pub fn` in a separate prep step and commit it). Adjust paths to match the actual public surface if a name differs.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-core --features metrics --test reader_faults`
Expected: FAIL — both tests get `Ok` (the seam isn't wired in yet, so the real read succeeds and `unwrap_err` panics).

- [ ] **Step 3: Wire the seam into the read loop**

In `musefs-core/src/reader.rs`, in the `BackingAudio` arm of `read_segments_into` (around line 381), change:

```rust
                    f.read_exact_at(&mut out[start..], bo + within)?;
                    crate::metrics::on_pread(n as u64);
```

to:

```rust
                    crate::metrics::backing_read_exact_at(f, &mut out[start..], bo + within)?;
                    crate::metrics::on_pread(n as u64);
```

- [ ] **Step 4: Remove the now-unused `FileExt` import**

`read_exact_at` was the only `FileExt` method called in `read_segments_into`. Delete the line at `musefs-core/src/reader.rs:350`:

```rust
    use std::os::unix::fs::FileExt;
```

(If `cargo build` later reports `FileExt` still used elsewhere in the function, keep the import instead — verify with the next step.)

- [ ] **Step 5: Verify the default build/clippy is clean**

Run: `cargo clippy -p musefs-core --all-targets -- -D warnings`
Expected: PASS, no `unused_imports` warning.

- [ ] **Step 6: Run the fault tests to verify they pass**

Run: `cargo test -p musefs-core --features metrics --test reader_faults`
Expected: PASS (2 tests).

- [ ] **Step 7: Run the full core suite both ways (no regression)**

Run: `cargo test -p musefs-core && cargo test -p musefs-core --features metrics`
Expected: PASS both.

- [ ] **Step 8: Commit**

```bash
git add musefs-core/src/reader.rs musefs-core/tests/reader_faults.rs musefs-core/tests/common/mod.rs
git commit -m "feat(core): route serve path through the fault seam; reader EIO/short-read tests (#209)"
```

---

### Task 3: BackingChanged on a mid-flight backing-file change (real condition)

**Files:**
- Create: `musefs-core/tests/backing_changed_fault.rs` (no feature gate — runs in the default suite)

- [ ] **Step 1: Write the failing test**

Create `musefs-core/tests/backing_changed_fault.rs`:

```rust
//! `HeaderCache::resolve` re-stats the backing file and rejects a track whose
//! file changed size/mtime since scan. Driven by a real file mutation — no
//! fault seam needed.
mod common;

use musefs_core::reader::HeaderCache;
use musefs_core::{CoreError, Mode}; // Mode is re-exported at the crate root, NOT in `reader`
use musefs_db::Db;

#[test]
fn shrinking_the_backing_file_after_scan_yields_backing_changed() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("a.flac");
    let (audio_offset, audio_length) = common::write_flac(&src, &["TITLE=T"], &[0xAB; 4096]);
    let db = Db::open_in_memory().unwrap();
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
    db.replace_tags(id, &[musefs_db::Tag::new("title", "T", 0)]).unwrap();

    // First resolve succeeds (file matches the scanned stat).
    let cache = HeaderCache::new(Mode::Synthesis);
    cache.resolve(&db, id).unwrap();

    // Truncate the backing file: its size now disagrees with the stored
    // backing_size. A fresh resolve (new HeaderCache, no cache hit) must error.
    let f = std::fs::OpenOptions::new().write(true).open(&src).unwrap();
    f.set_len(10).unwrap();
    drop(f);

    let err = HeaderCache::new(Mode::Synthesis).resolve(&db, id).unwrap_err();
    match err {
        CoreError::BackingChanged(path) => assert!(path.ends_with("a.flac")),
        other => panic!("expected BackingChanged, got {other:?}"),
    }
}
```

- [ ] **Step 2: Run to verify it fails, then passes**

Run: `cargo test -p musefs-core --test backing_changed_fault`
Expected: PASS immediately if `resolve`'s guard already works (this test *characterizes* an existing invariant that had no coverage). If it does not compile, fix the `HeaderCache`/`Mode` import paths to the real public names, then it should pass. (This task adds the missing test for an already-correct path; there is no production change.)

- [ ] **Step 3: Commit**

```bash
git add musefs-core/tests/backing_changed_fault.rs
git commit -m "test(core): cover BackingChanged on a mid-flight backing-file change (#209)"
```

---

### Task 4: DB corruption surfaces as a mapped error

**Files:**
- Create: `musefs-core/tests/db_corruption_fault.rs` (no feature gate)

- [ ] **Step 1: Write the failing test**

Create `musefs-core/tests/db_corruption_fault.rs`:

```rust
//! A byte-corrupted SQLite store must surface as a mapped error from the DB
//! layer, not a panic, when the serve path reads it.
use std::io::{Seek, SeekFrom, Write};

use musefs_db::Db;

#[test]
fn corrupt_db_header_errors_instead_of_panicking() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("musefs.db");

    // Build a valid store with one track.
    {
        let db = Db::open(&db_path).unwrap();
        db.upsert_track(&musefs_db::NewTrack {
            backing_path: "/nonexistent/a.flac".into(),
            format: musefs_db::Format::Flac,
            audio_offset: 0,
            audio_length: 1,
            backing_size: 1,
            backing_mtime: 0,
        })
        .unwrap();
    } // connection dropped, file flushed

    // Clobber the 16-byte SQLite magic header ("SQLite format 3\0").
    {
        let mut f = std::fs::OpenOptions::new().write(true).open(&db_path).unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();
        f.write_all(&[0u8; 16]).unwrap();
        f.flush().unwrap();
    }

    // Opening read-only and listing must be an Err (whether the failure lands at
    // open or at first query), never a panic or a wrong-but-Ok result.
    let result = Db::open_readonly(&db_path).and_then(|db| db.list_tracks());
    assert!(result.is_err(), "corrupt DB must yield a DbError, got {result:?}");
}
```

- [ ] **Step 2: Run to verify it passes**

Run: `cargo test -p musefs-core --test db_corruption_fault`
Expected: PASS. If `Db::open_readonly`/`list_tracks`/`Db::open`/`NewTrack` names differ, correct them to the real `musefs-db` surface (confirmed present: `Db::open_readonly` at `musefs-db/src/lib.rs:108`, `list_tracks` at `musefs-db/src/tracks.rs:67`).

- [ ] **Step 3: Commit**

```bash
git add musefs-core/tests/db_corruption_fault.rs
git commit -m "test(core): cover corrupt-DB error mapping on the read path (#209)"
```

---

### Task 5: FUSE-level fault test — the global seam reaches the worker thread

**Files:**
- Create: `musefs-fuse/tests/fault_injection.rs` (`#![cfg(feature = "metrics")]`, `--ignored`, needs `/dev/fuse`)

This proves the process-global seam is visible on the FUSE worker thread (a thread-local would not be) and that an `EIO` backing read surfaces as an I/O error through the mount.

- [ ] **Step 1: Inspect the existing mount harness to reuse it**

Read `musefs-fuse/tests/concurrency.rs` and `musefs-fuse/tests/mount.rs` for the exact mount setup helpers (`make_flac`, `scan_directory`, `Musefs::open`, `config()`, `musefs_fuse::spawn`). Reuse the same pattern — do not invent a new harness.

- [ ] **Step 2: Write the failing test**

Create `musefs-fuse/tests/fault_injection.rs`, mirroring `concurrency.rs`'s setup (adjust helper names/paths to match that file exactly):

```rust
//! An injected EIO backing read surfaces as an I/O error through a real FUSE
//! mount, proving the process-global fault seam reaches the worker thread.
#![cfg(feature = "metrics")]

// Reuse the harness helpers from the sibling integration tests. Copy the small
// `make_flac` / `config` / scan+mount setup from `mount.rs`/`concurrency.rs`
// (those helpers are private to each test binary, so duplicate the minimal
// setup here rather than importing).

use musefs_core::metrics::{set_backing_fault, BackingFault};

#[test]
#[ignore = "requires /dev/fuse; run with: cargo test -p musefs-fuse --features metrics -- --ignored"]
fn eio_backing_read_surfaces_through_the_mount() {
    let backing = tempfile::tempdir().unwrap();
    let flac = make_flac(&["ARTIST=Alice", "TITLE=Song"], &vec![0xAB; 256 * 1024]);
    std::fs::write(backing.path().join("a.flac"), &flac).unwrap();

    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, backing.path()).unwrap();
    let fs = Musefs::open(db, config()).unwrap();
    let mountpoint = tempfile::tempdir().unwrap();
    let session =
        musefs_fuse::spawn(fs, mountpoint.path(), "musefs-fault-test").unwrap();

    let song = mountpoint.path().join("Alice").join("Song.flac");

    // With EIO injected on the next backing read, reading the (audio-bearing)
    // file through the mount must fail with an I/O error, not succeed or hang.
    let _guard = set_backing_fault(BackingFault::Eio);
    let err = std::fs::read(&song).expect_err("read should fail under injected EIO");
    // FUSE maps the reader's CoreError::Io(EIO) straight back to errno EIO, so a
    // tight assertion guards against a false pass from an unrelated failure.
    assert_eq!(
        err.raw_os_error(),
        Some(5),
        "injected EIO should surface as EIO through the mount, got {err:?}"
    );

    drop(session);
}
```

(Copy `make_flac`, `scan_directory` import, `Musefs`, `config()` from `mount.rs`/`concurrency.rs` verbatim so this compiles. Keep the audio payload large enough — 256 KiB — that the kernel issues a real backing read for the audio region rather than serving entirely from synthesized header bytes.)

- [ ] **Step 3: Confirm the test compiles and fails for the right reason without the seam wired**

(The seam is already wired from Task 2; this step confirms the harness compiles.)

Run: `cargo test -p musefs-fuse --features metrics --test fault_injection -- --ignored --nocapture`
Expected: PASS. If it does not, first run without `--ignored` to confirm compilation, then debug the mount setup against `concurrency.rs`.

- [ ] **Step 4: Sanity-check that without the fault the same file reads fine**

Temporarily comment out the `let _guard = ...` line and confirm `std::fs::read(&song)` succeeds (then restore it). This guards against a false pass where the read fails for an unrelated reason. Document the check; do not commit the commented-out variant.

- [ ] **Step 5: Commit**

```bash
git add musefs-fuse/tests/fault_injection.rs
git commit -m "test(fuse): EIO backing read surfaces through the mount via the global seam (#209)"
```

---

### Task 6: Run the FUSE metrics-feature ignored tests in CI

The `e2e` job runs `cargo test -p musefs-fuse -- --ignored` **without** `--features metrics`, so metrics-gated FUSE tests (the new `fault_injection.rs`, and the pre-existing `concurrency.rs`) never run in CI. Add a step that does.

**Files:**
- Modify: `.github/workflows/ci.yml` (the `e2e` job)

- [ ] **Step 1: Add the metrics-feature ignored step to the `e2e` job**

In `.github/workflows/ci.yml`, in the `e2e` job, after the existing step:

```yaml
      - name: FUSE end-to-end tests
        run: cargo test -p musefs-fuse -- --ignored
```

add:

```yaml
      - name: FUSE fault-injection + concurrency (metrics feature)
        run: cargo test -p musefs-fuse --features metrics -- --ignored
```

(The `e2e` job already installs `fuse3 libfuse3-dev pkg-config ffmpeg` and is in `ci-ok`'s `needs:`, gated on `needs.changes.outputs.src == 'true'` — no wiring change needed. This is a required gate.)

- [ ] **Step 2: Validate the workflow YAML locally**

Run: `python -c "import yaml,sys; yaml.safe_load(open('.github/workflows/ci.yml')); print('ok')"`
Expected: `ok`.

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: run FUSE metrics-feature ignored tests (fault + concurrency) in e2e (#209)"
```

---

### Task 7: Document the fault seam

**Files:**
- Modify: `CONTRIBUTING.md` (the "Test tiers beyond `cargo test`" section, around line 134)

- [ ] **Step 1: Add a fault-injection subsection**

In `CONTRIBUTING.md`, after the "Independent-reader interop (mutagen)" subsection, add:

```markdown
### Failure-path fault injection

The reader and DB error paths are exercised under simulated runtime faults.
`musefs_core::metrics::set_backing_fault(BackingFault::{Eio,ShortRead})`
(behind the `metrics` feature) installs a process-global fault at the positioned
backing-read site, cleared by the returned RAII guard. Because it is global, the
tests run in their own `metrics`-gated binaries.

```bash
cargo test -p musefs-core --features metrics --test reader_faults
cargo test -p musefs-core --test backing_changed_fault   # real file mutation
cargo test -p musefs-core --test db_corruption_fault      # byte-corrupt DB
cargo test -p musefs-fuse --features metrics -- --ignored # EIO through the mount (needs /dev/fuse)
```

`BackingChanged` (re-validated in `HeaderCache::resolve`) and DB corruption are
driven by real conditions, not the seam. `ENOSPC`/read-only faults are write-path
concerns and are out of scope for the read-time suite.
```

- [ ] **Step 2: Lint the docs (ruff is unaffected; just confirm no broken build)**

Run: `cargo fmt --all -- --check`
Expected: PASS (no Rust changed here, but the pre-commit hook will run it).

- [ ] **Step 3: Commit**

```bash
git add CONTRIBUTING.md
git commit -m "docs: document the failure-path fault-injection tier (#209)"
```

---

### Task 8: Final verification

- [ ] **Step 1: Run every new test path**

```bash
cargo test -p musefs-core --features metrics
cargo test -p musefs-core
cargo test -p musefs-fuse --features metrics -- --ignored
```
Expected: all PASS.

- [ ] **Step 2: Full pre-commit parity (fmt + clippy + workspace tests)**

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --workspace
```
Expected: all PASS. (These are what the pre-commit hook enforces; each commit above must already be green.)

- [ ] **Step 3: Confirm the seam is compiled out of the default build**

Run: `cargo build --release -p musefs-core && cargo build --release -p musefs-fuse`
Expected: success; `BackingFault`/`set_backing_fault` absent from the release build (they live only in the `metrics`-on arm).

---

## Self-review notes (for the implementer)

- **Spec coverage:** seam (EIO/short read) → Tasks 1–2, 5; BackingChanged → Task 3; DB fault → Task 4; CI visibility → Task 6; docs → Task 7. The spec's `ENOSPC`/read-only case is explicitly out of scope (write-path; documented best-effort).
- **Type consistency:** `BackingFault`, `BackingFaultGuard`, `set_backing_fault`, `backing_read_exact_at` are named identically in every task. `CoreError::Io`/`CoreError::BackingChanged` per `musefs-core/src/error.rs`.
- **Watch-outs:** confirm `common::real_mtime` exists in `musefs-core/tests/common/mod.rs` before Task 2 (it is used by `interop_emit.rs`; if it lives only there, lift it into `common/mod.rs` first). Confirm the public re-export path of `HeaderCache`/`Mode`/`ResolvedFile`/`read_at` and adjust `use` lines to match.
