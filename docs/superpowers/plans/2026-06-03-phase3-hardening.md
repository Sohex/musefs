# Phase 3 — Safety net + small Rust hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close five independent low-risk hardening issues (#88, #91, #92, #93, #94) in one PR: a fuzz-coverage gap, a byte-budget overflow asymmetry, a missing non-negative guard on an externally-writable DB column, an unbounded MP4 metadata allocation, and two DbPool footguns.

**Architecture:** Five self-contained changes across `musefs-core`, `musefs-format`, and the out-of-workspace `fuzz` crate. No serve-path or byte-identical-audio semantics change. Each task is TDD where a behavior is observable; #93 and the fuzz target #88 are verified differently (existing tests / `cargo fuzz build`) because there is nothing new to assert with a Rust unit test.

**Tech Stack:** Rust (workspace), `thiserror`, `rusqlite` (dev-dep, already present), `cargo-fuzz` (nightly, for #88), `log`.

**Branch:** `phase3-hardening` (already created; the design spec is committed there).

**Reference spec:** `docs/superpowers/specs/2026-06-03-phase3-hardening-design.md`

---

## File Structure

- `musefs-core/src/byte_budget.rs` — Task 1 (#93): one-line saturating-add fix.
- `musefs-core/src/mapping.rs` — Task 2 (#92): non-negative guard on art `byte_len` + test.
- `musefs-format/src/mp4.rs` — Task 3 (#91): metadata-size cap const, `Mp4ScanError` variant, cap check, tests.
- `musefs-core/src/scan.rs` — Task 3 (#91): clear `log::warn!` on the new cap error at the swallow site.
- `musefs-core/src/error.rs` — Task 4 (#94b): new `CoreError::DbOpen` variant carrying the path.
- `musefs-core/src/db_pool.rs` — Task 4 (#94a/b/c): `Rc<Db>` thread-local + re-entrancy-safe `with`, path-context open error, Drop doc note + tests.
- `fuzz/fuzz_targets/ogg.rs` — Task 5 (#88): build `OggArt`s with image bytes so the art path is fuzzed.

---

## Task 1: #93 — `byte_budget` saturating-add symmetry

**Files:**
- Modify: `musefs-core/src/byte_budget.rs:32`

The guard at line 29 already uses `saturating_add`; the mutation at line 32 is a plain `+=`. This is an internal-consistency fix with **no observable behavior** (art weights are file-bounded, so `in_flight` never approaches `u64::MAX`). There is therefore nothing to assert in a new unit test — the existing tests in the file already pin additive accumulation and every guard mutant, and `saturating_add` remains additive below saturation. TDD's "write a failing test first" does not apply when no new behavior is observable; we make the change and prove the existing suite still passes.

- [ ] **Step 1: Make the change**

In `musefs-core/src/byte_budget.rs`, in `ByteBudget::acquire`, change the increment line:

```rust
        *in_flight = in_flight.saturating_add(n);
```

(was `*in_flight += n;`)

- [ ] **Step 2: Run the existing byte_budget tests to verify they still pass**

Run: `cargo test -p musefs-core byte_budget`
Expected: PASS — `oversized_item_admitted_when_idle`, `blocked_acquire_proceeds_after_release`, `accumulates_additively_then_blocks`, `exact_cap_is_admitted`, `over_cap_blocks` all green.

- [ ] **Step 3: Commit**

```bash
git add musefs-core/src/byte_budget.rs
git commit -m "fix(byte_budget): make acquire increment saturating to match its guard (#93)"
```

---

## Task 2: #92 — non-negative guard on art `byte_len`

**Files:**
- Modify: `musefs-core/src/mapping.rs` (`track_art_to_inputs`, ~lines 30-51, and its test module)
- Test: `musefs-core/src/mapping.rs` (`#[cfg(test)] mod tests`)

Only the art path is guarded. The binary-tag `byte_len` (`binary_tags_to_inputs`) is `length(value_blob)` in SQL (`tags.rs:127`), always ≥ 0, so it needs no guard. `art.byte_len` is a stored column (`schema.rs:31`) an external write can forge negative.

- [ ] **Step 1: Write the failing test**

Add this test inside `musefs-core/src/mapping.rs`'s `#[cfg(test)] mod tests` block. The negative value can't be made through the public API (`upsert_art` derives `byte_len` from `data.len()`), so we corrupt it via a second raw `rusqlite` connection — exactly the "external malformed write" the contract warns about. `rusqlite` is already a musefs-core dev-dependency.

```rust
    #[test]
    fn track_art_to_inputs_skips_negative_byte_len() {
        use musefs_db::{NewArt, NewTrack, TrackArt};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("art.db");
        let db = Db::open(&path).unwrap();
        let tid = db
            .upsert_track(&NewTrack {
                backing_path: "/a.flac".into(),
                format: Format::Flac,
                audio_offset: 0,
                audio_length: 0,
                backing_size: 0,
                backing_mtime: 0,
            })
            .unwrap();
        let good = db
            .upsert_art(&NewArt {
                mime: "image/png".into(),
                width: None,
                height: None,
                data: vec![1, 2, 3, 4],
            })
            .unwrap();
        let bad = db
            .upsert_art(&NewArt {
                mime: "image/png".into(),
                width: None,
                height: None,
                data: vec![9, 9, 9, 9, 9],
            })
            .unwrap();
        db.set_track_art(
            tid,
            &[
                TrackArt { art_id: good, picture_type: 3, description: String::new(), ordinal: 0 },
                TrackArt { art_id: bad, picture_type: 3, description: String::new(), ordinal: 1 },
            ],
        )
        .unwrap();

        // Simulate an external malformed write: byte_len is a stored column.
        let raw = rusqlite::Connection::open(&path).unwrap();
        raw.execute("UPDATE art SET byte_len = -1 WHERE id = ?1", [bad]).unwrap();
        drop(raw);

        let inputs = super::track_art_to_inputs(&db, tid).unwrap();
        assert_eq!(inputs.len(), 1, "the negative-byte_len art row must be skipped");
        assert_eq!(inputs[0].art_id, good);
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-core track_art_to_inputs_skips_negative_byte_len`
Expected: FAIL — currently both rows are included (`inputs.len() == 2`); the corrupted row casts `-1 → u64::MAX`.

- [ ] **Step 3: Add the guard**

In `musefs-core/src/mapping.rs`, in `track_art_to_inputs`, inside the existing `if let Some(meta) = db.get_art_meta(ta.art_id)?` arm, add the guard before the `inputs.push(...)`:

```rust
        if let Some(meta) = db.get_art_meta(ta.art_id)? {
            // A negative byte_len is a malformed external write to the contract
            // column; skip the row rather than cast it to a huge u64 segment
            // length (which would fail layout validation and break the track).
            if meta.byte_len < 0 {
                continue;
            }
            inputs.push(ArtInput {
                art_id: ta.art_id,
                mime: meta.mime,
                description: ta.description,
                picture_type: ta.picture_type as u32,
                width: meta.width.unwrap_or(0) as u32,
                height: meta.height.unwrap_or(0) as u32,
                data_len: meta.byte_len as u64,
            });
        }
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p musefs-core track_art_to_inputs_skips_negative_byte_len`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add musefs-core/src/mapping.rs
git commit -m "fix(mapping): skip art rows with negative byte_len from malformed DB writes (#92)"
```

---

## Task 3: #91 — cap MP4 `moov`/`ftyp` metadata allocation

**Files:**
- Modify: `musefs-format/src/mp4.rs` (module const, `Mp4ScanError` enum ~line 89, `read_structure_from` ~lines 244-313, test module ~line 841)
- Modify: `musefs-core/src/scan.rs` (`probe_file` MP4 arm, ~lines 219-222)

The cap is checked on the declared box length **before** `region()` allocates. The test exploits that `read_structure_from`'s `file_len` argument is independent of the reader's real length: a large `file_len` clears `box_header`'s `total_len > remaining` check, so the new cap (not `Malformed`) is what fires.

- [ ] **Step 1: Write the failing tests**

Add these two tests to `musefs-format/src/mp4.rs`'s `#[cfg(test)] mod tests` block (which has `use super::*;`):

```rust
    #[test]
    fn read_structure_from_rejects_oversized_moov() {
        use std::io::Cursor;
        // ftyp(16) + mdat(16) + moov(declares 600 MiB). The moov box "ends" exactly
        // at the (large, lied-about) file_len so the header walk terminates cleanly,
        // then the cap check fires before any payload region is allocated/read.
        let moov_size: u32 = 600 * 1024 * 1024; // 629_145_600 > 512 MiB cap
        let mut buf = Vec::new();
        buf.extend_from_slice(&16u32.to_be_bytes());
        buf.extend_from_slice(b"ftyp");
        buf.extend_from_slice(&[0u8; 8]); // ftyp payload (not read during the walk)
        buf.extend_from_slice(&16u32.to_be_bytes());
        buf.extend_from_slice(b"mdat");
        buf.extend_from_slice(&[0u8; 8]); // mdat payload
        buf.extend_from_slice(&moov_size.to_be_bytes());
        buf.extend_from_slice(b"moov");
        // No moov payload in the buffer: the cap check must return before region().
        assert_eq!(buf.len(), 40);
        let file_len = 32 + moov_size as u64;
        let mut cur = Cursor::new(buf);
        match read_structure_from(&mut cur, file_len).unwrap_err() {
            Mp4ScanError::MetadataTooLarge { box_kind, size, cap } => {
                assert_eq!(box_kind, "moov");
                assert_eq!(size, moov_size as u64);
                assert_eq!(cap, 512 * 1024 * 1024);
            }
            other => panic!("expected MetadataTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn read_structure_from_admits_box_at_exactly_the_cap() {
        use std::io::Cursor;
        // size == cap: the guard is strict `>`, so it does NOT trip. The read then
        // proceeds and hits EOF on the short buffer → Io, never MetadataTooLarge.
        let cap: u32 = 512 * 1024 * 1024;
        let mut buf = Vec::new();
        buf.extend_from_slice(&16u32.to_be_bytes());
        buf.extend_from_slice(b"ftyp");
        buf.extend_from_slice(&[0u8; 8]);
        buf.extend_from_slice(&16u32.to_be_bytes());
        buf.extend_from_slice(b"mdat");
        buf.extend_from_slice(&[0u8; 8]);
        buf.extend_from_slice(&cap.to_be_bytes());
        buf.extend_from_slice(b"moov");
        let file_len = 32 + cap as u64;
        let mut cur = Cursor::new(buf);
        let err = read_structure_from(&mut cur, file_len).unwrap_err();
        assert!(
            matches!(err, Mp4ScanError::Io(_)),
            "exact-cap box must pass the strict `>` guard (got {err:?})"
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail (compile error)**

Run: `cargo test -p musefs-format read_structure_from_rejects_oversized_moov`
Expected: FAIL to compile — `Mp4ScanError::MetadataTooLarge` does not exist yet.

- [ ] **Step 3: Add the const and the error variant**

In `musefs-format/src/mp4.rs`, add the module-level const (place it near the top of the file, after the imports / above the first item):

```rust
/// Upper bound on a single MP4 metadata box (`ftyp`/`moov`) allocation. A corrupt
/// or pathologically large box is rejected (the file is skipped at scan, logged)
/// rather than forcing a multi-hundred-MB allocation. 512 MiB leaves generous
/// headroom for the sample tables of very long audiobooks.
const MAX_MP4_METADATA_BYTES: u64 = 512 * 1024 * 1024;
```

Add the variant to the `Mp4ScanError` enum (after the existing `Format` variant):

```rust
#[derive(Debug, thiserror::Error)]
pub enum Mp4ScanError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Format(#[from] FormatError),
    #[error("MP4 {box_kind} box is {size} bytes, exceeds the {cap}-byte metadata cap")]
    MetadataTooLarge {
        box_kind: &'static str,
        size: u64,
        cap: u64,
    },
}
```

- [ ] **Step 4: Add the cap check in `read_structure_from`**

In `read_structure_from`, immediately after the three `ok_or` unwraps and **before** the `usize::try_from` / `region(...)` reads, insert:

```rust
    let (ftyp_s, ftyp_h) = ftyp.ok_or(FormatError::NotMp4)?;
    let (moov_s, moov_h) = moov.ok_or(FormatError::NotMp4)?;
    let (mdat_s, mdat_h) = mdat.ok_or(FormatError::NotMp4)?;

    // Reject an oversized metadata box before allocating for it (mirrors mp3.rs's
    // id3v2_alloc_safe philosophy). The check is on the declared length, so a
    // corrupt header never drives the allocation.
    for (box_kind, total_len) in [("ftyp", ftyp_h.total_len), ("moov", moov_h.total_len)] {
        if total_len > MAX_MP4_METADATA_BYTES {
            return Err(Mp4ScanError::MetadataTooLarge {
                box_kind,
                size: total_len,
                cap: MAX_MP4_METADATA_BYTES,
            });
        }
    }
```

(The existing `usize::try_from(...)` lines and the three `region(...)` reads follow unchanged.)

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p musefs-format read_structure_from`
Expected: PASS — both new tests plus the existing `read_structure_from_*` tests are green.

- [ ] **Step 6: Add the clear `log::warn!` at the scan swallow site**

In `musefs-core/src/scan.rs`, in `probe_file`'s MP4 arm, replace the silent swallow:

```rust
        let mut f = &file;
        let Ok(scan) = mp4::read_structure_from(&mut f, file_len) else {
            return Ok(None);
        };
```

with a match that loudly logs the new cap case (all other errors keep the existing silent skip, so the change is scoped to the new variant):

```rust
        let mut f = &file;
        let scan = match mp4::read_structure_from(&mut f, file_len) {
            Ok(s) => s,
            Err(e) => {
                if matches!(e, mp4::Mp4ScanError::MetadataTooLarge { .. }) {
                    log::warn!("skipping {}: {e}", path.display());
                }
                return Ok(None);
            }
        };
```

Note: the `MetadataTooLarge` branch at the scan site is only reachable for a genuinely >512 MiB file (scan passes the file's real length, so a small corrupt file hits `box_header`'s `Malformed` first). It is verified by inspection plus the unaffected existing MP4 scan tests, not a multi-hundred-MB fixture.

- [ ] **Step 7: Verify the whole crates build and scan tests are unaffected**

Run: `cargo test -p musefs-format -p musefs-core mp4`
Expected: PASS — new mp4 tests green, existing MP4 scan tests (normal files) unchanged.

- [ ] **Step 8: Commit**

```bash
git add musefs-format/src/mp4.rs musefs-core/src/scan.rs
git commit -m "fix(mp4): cap moov/ftyp metadata allocation at 512 MiB, log skips (#91)"
```

---

## Task 4: #94 — DbPool re-entrancy + open-error path context

**Files:**
- Modify: `musefs-core/src/error.rs` (add `CoreError::DbOpen`)
- Modify: `musefs-core/src/db_pool.rs` (imports, `PER_PATH` value type, `with`, `Drop` doc, tests)

Fixes (a) the re-entrant `with()` panic via `Rc<Db>`, and (b) the path-less open error via a new typed `CoreError` variant. (c) the cross-thread Drop leak is documented-and-accepted. The `Shared` variant's re-entrancy (a mutex deadlock) is out of scope; tests use `PerThread` (file-backed) only.

- [ ] **Step 1: Write the failing tests**

Add these two tests to `musefs-core/src/db_pool.rs`'s `#[cfg(test)] mod tests` block (which has `use super::*;` and `use musefs_db::Db;`):

```rust
    #[test]
    fn reentrant_with_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("re.db");
        Db::open(&path).unwrap(); // create + migrate (writer, sets WAL)
        let pool = DbPool::new(Db::open(&path).unwrap()).unwrap();
        // Nested with() on the same thread must not panic on a second borrow_mut.
        let r: Result<i64> = pool.with(|_outer| pool.with(|db| Ok(db.data_version()?)));
        assert!(r.is_ok(), "re-entrant with() must not panic or error");
    }

    #[test]
    fn with_open_failure_includes_path_in_error() {
        // Build a PerThread pool whose path can't be opened. `poll` is unused by
        // `with` (only by `with_poll`), so an in-memory Db satisfies the field.
        let bad = std::path::PathBuf::from("/nonexistent-musefs-dir/does-not-exist.db");
        let pool = DbPool::PerThread {
            id: u64::MAX, // unique key; won't collide with any real pool's thread-local
            path: bad.clone(),
            poll: Mutex::new(Db::open_in_memory().unwrap()),
        };
        let msg = pool.with(|_db| Ok(())).unwrap_err().to_string();
        assert!(
            msg.contains("/nonexistent-musefs-dir/does-not-exist.db"),
            "open error must name the failing path, got: {msg}"
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p musefs-core db_pool`
Expected: FAIL — `reentrant_with_does_not_panic` panics with `already borrowed: BorrowMutError`; `with_open_failure_includes_path_in_error` fails to compile (`CoreError::DbOpen` doesn't exist) or, once that compiles, the message lacks the path.

- [ ] **Step 3: Add the `CoreError::DbOpen` variant**

In `musefs-core/src/error.rs`, add a variant to the `CoreError` enum (after `Db`):

```rust
    #[error("failed to open database at {path}")]
    DbOpen {
        path: std::path::PathBuf,
        #[source]
        source: musefs_db::DbError,
    },
```

- [ ] **Step 4: Switch the thread-local to `Rc<Db>` and make `with` re-entrancy-safe**

In `musefs-core/src/db_pool.rs`, add the imports near the top (with the other `use` lines):

```rust
use std::rc::Rc;

use crate::error::CoreError;
```

Change the `PER_PATH` thread-local value type:

```rust
thread_local! {
    static PER_PATH: RefCell<HashMap<(PathBuf, u64), Rc<Db>>> = RefCell::new(HashMap::new());
}
```

Replace the `with` method body with the version that clones the `Rc` out before calling `f` and attaches the path to an open failure:

```rust
    /// Run `f` with a read connection.
    ///
    /// The `RefCell` borrow does not span `f`: the connection `Rc` is cloned out
    /// first, so a re-entrant `with()` on the same thread is safe. (The `Shared`
    /// variant remains non-reentrant — a second `m.lock()` deadlocks — but real
    /// mounts are always `PerThread`.)
    pub fn with<R>(&self, f: impl FnOnce(&Db) -> Result<R>) -> Result<R> {
        match self {
            DbPool::PerThread { id, path, .. } => PER_PATH.with(|cell| {
                let db = {
                    let mut map = cell.borrow_mut();
                    if let std::collections::hash_map::Entry::Vacant(e) =
                        map.entry((path.clone(), *id))
                    {
                        let db = Db::open_readonly(path).map_err(|source| CoreError::DbOpen {
                            path: path.clone(),
                            source,
                        })?;
                        e.insert(Rc::new(db));
                    }
                    Rc::clone(map.get(&(path.clone(), *id)).unwrap())
                };
                f(&db)
            }),
            DbPool::Shared(m) => {
                let db = m.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                f(&db)
            }
        }
    }
```

- [ ] **Step 5: Document the accepted cross-thread Drop limitation (#94c)**

In `musefs-core/src/db_pool.rs`, add a doc comment above `impl Drop for DbPool` (leave the body unchanged):

```rust
/// Clears only the *dropping thread's* thread-local connection for this pool.
/// Connections this pool opened on other worker threads persist until those
/// threads exit — accepted, because a mount's worker pool lives for the whole
/// mount, so a connection's lifetime already matches its thread's. A future
/// caller that creates and drops many pools over long-lived shared threads would
/// leak; closing that would need a cross-thread registry, intentionally not built
/// here.
impl Drop for DbPool {
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p musefs-core db_pool`
Expected: PASS — both new tests plus the existing `db_pool` tests (`shared_pool_for_in_memory_db`, `same_thread_two_pools_keyed_by_path`, `per_thread_pool_for_file_db`) green.

- [ ] **Step 7: Commit**

```bash
git add musefs-core/src/error.rs musefs-core/src/db_pool.rs
git commit -m "fix(db_pool): re-entrancy-safe with() via Rc<Db> + path-context open error (#94)"
```

---

## Task 5: #88 — fuzz the Ogg art-synthesis path

**Files:**
- Modify: `fuzz/fuzz_targets/ogg.rs`

The fuzz crate is **out of the workspace**, so `cargo build`/`clippy`/`test` do not compile it. Ogg's `synthesize_layout` takes `&[OggArt]` (each `OggArt { meta: &ArtInput, image: &[u8] }`), unlike flac/mp3 which take `&[ArtInput]` — so the fix fabricates image byte buffers and builds `OggArt` borrows, upholding `image.len() == meta.data_len` (the invariant `reader.rs:347` maintains).

- [ ] **Step 1: Rewrite the fuzz target to exercise the art path**

Replace the full contents of `fuzz/fuzz_targets/ogg.rs` with:

```rust
#![no_main]
use arbitrary::Unstructured;
use libfuzzer_sys::fuzz_target;
use musefs_format::ogg::OggArt;
use musefs_format::{fuzz_check::assert_backing_covers_audio, ogg, ArtInput};
use musefs_fuzz::{arb_tags, MAX_INPUT};

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT {
        return;
    }
    let _ = ogg::read_tags(data);
    let _ = ogg::read_pictures(data);
    let scan = match ogg::locate_audio(data) {
        Ok(s) => s,
        Err(_) => return,
    };
    let header = match ogg::read_metadata(data) {
        Ok(h) => h,
        Err(_) => return,
    };
    let mut u = Unstructured::new(data);
    let tags = arb_tags(&mut u).unwrap_or_default();

    // Fabricate embedded images so the page-renumber + CRC-recompute + base64
    // art path is reached. Each OggArt borrows an ArtInput and its raw bytes,
    // with data_len == image length (the invariant reader.rs upholds).
    let n = u.int_in_range(0..=2u8).unwrap_or(0);
    let mut images: Vec<Vec<u8>> = Vec::new();
    let mut inputs: Vec<ArtInput> = Vec::new();
    for i in 0..n {
        let len = u.int_in_range(0..=8192usize).unwrap_or(0);
        let bytes = u.bytes(len).map(<[u8]>::to_vec).unwrap_or_default();
        inputs.push(ArtInput {
            art_id: i as i64,
            mime: "image/png".to_string(),
            description: String::new(),
            picture_type: u.int_in_range(0..=20u32).unwrap_or(3),
            width: 0,
            height: 0,
            data_len: bytes.len() as u64,
        });
        images.push(bytes);
    }
    let arts: Vec<OggArt> = inputs
        .iter()
        .zip(images.iter())
        .map(|(meta, image)| OggArt { meta, image: image.as_slice() })
        .collect();

    if let Ok(layout) =
        ogg::synthesize_layout(&header, scan.audio_offset, scan.audio_length, &tags, &arts)
    {
        assert_backing_covers_audio(scan.audio_offset, scan.audio_length, &layout);
    }
});
```

- [ ] **Step 2: Verify the fuzz target compiles (requires nightly + cargo-fuzz)**

Run: `cargo +nightly fuzz build ogg`
Expected: builds cleanly. (This is the only thing that compiles the out-of-workspace fuzz crate; a normal `cargo build` will NOT catch a break here.)

- [ ] **Step 3: Short smoke run to confirm the art path is reached and panic-free**

Run: `cargo +nightly fuzz run ogg -- -runs=200000 -max_total_time=30`
Expected: completes with no crash/panic. (A few seconds; confirms the new art construction doesn't panic on adversarial input.)

- [ ] **Step 4: Commit**

```bash
git add fuzz/fuzz_targets/ogg.rs
git commit -m "test(fuzz): exercise Ogg art synthesis path with arbitrary images (#88)"
```

---

## Final verification (run after all five tasks)

- [ ] **Workspace tests, lint, format**

```bash
cargo test --workspace
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
```
Expected: all green. (`cargo fmt --all --check` is the CI fmt gate — a pre-push must.)

- [ ] **FUSE e2e regression (no serve-path semantics changed, but confirm)**

Run: `cargo test -p musefs-fuse -- --ignored`
Expected: PASS on `/dev/fuse` (requires libfuse). No read/synthesis semantics changed, so this is a regression check.

- [ ] **In-diff mutation gate (matches the `ci-ok`-required check on `main`)**

Run the repo's in-diff mutation gate locally (`-j$(nproc)`, `TMPDIR` under `/home`), and sanity-check that the diff it tests is non-empty so it isn't a silent false pass.

- [ ] **Open the PR**

```bash
git push -u origin phase3-hardening
gh pr create --base main --title "Phase 3: safety net + small Rust hardening (#88, #91-#94)" --body "$(cat <<'EOF'
Implements Roadmap Phase 3. Five independent low-risk hardening fixes:

- #93 byte_budget: saturating-add symmetry in `acquire`.
- #92 mapping: skip art rows with negative `byte_len` from malformed external writes.
- #91 mp4: cap `moov`/`ftyp` metadata allocation at 512 MiB; clearly logged skip at scan.
- #94 db_pool: re-entrancy-safe `with()` via `Rc<Db>` + path-context open error; cross-thread Drop leak documented-and-accepted.
- #88 fuzz: exercise the Ogg art-synthesis path (image bytes + `OggArt`).

Spec: `docs/superpowers/specs/2026-06-03-phase3-hardening-design.md`.
Reviewed by spec-plan-reviewer; byte-identical-audio invariant unchanged.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

---

## Notes for the implementer

- **Serena tools:** This project uses Serena for code reads/edits. Use `get_symbols_overview` / `find_symbol` to read, and `replace_symbol_body` / `replace_content` / `insert_*_symbol` to edit Rust files, rather than the built-in Read/Edit.
- **One commit per task** (five total), then the final-verification steps. Don't squash.
- **Don't `--no-verify`.** The pre-commit hook runs fmt + clippy + workspace tests; if it fails, fix and make a new commit.
- **#88 needs nightly** (`cargo +nightly fuzz`). If the toolchain is missing, install `cargo-fuzz` and a nightly toolchain; do not skip the build check — CI's smoke job is otherwise the only thing that compiles the fuzz crate.
