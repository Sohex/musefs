# Fail on Orphaned `track_art` Rows Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn the silent art-drop in `track_art_to_inputs` into a hard `CoreError::OrphanedArt`, surfaced as `EIO` at the FUSE boundary, so an orphaned `track_art` row (an `art_id` with no `art` metadata row) reports DB corruption instead of serving a file with missing art.

**Architecture:** A new `CoreError::OrphanedArt { track_id, art_id }` variant is added in `musefs-core`. `track_art_to_inputs` returns it when `get_art_meta` yields `Ok(None)` instead of silently skipping the row. The FUSE errno mapper routes the variant to `EIO`, alongside the other structural-corruption errors. Two unit tests cover the new branch (one in core for the error, one in fuse for the errno mapping).

**Tech Stack:** Rust, `thiserror` (`CoreError`), `rusqlite` (raw connection for FK-off test setup), `fuser` (errno mapping).

**Spec:** `docs/superpowers/specs/2026-06-10-orphaned-track-art-hard-error-design.md`

---

## File Structure

- `musefs-core/src/error.rs` — add the `OrphanedArt` variant to `CoreError`.
- `musefs-fuse/src/lib.rs` — add the `OrphanedArt → EIO` arm in `errno`; extend the `maps_core_errors_to_errno` test.
- `musefs-core/src/mapping.rs` — change `track_art_to_inputs` to error on `None`; add the orphan test in the existing `tests` module.
- `ARCHITECTURE.md` — one sentence in the external-writer contract section.

Three commits: (1) variant + errno mapping + errno test, (2) the core fix + core test, (3) docs.

---

### Task 1: Add `CoreError::OrphanedArt` and map it to `EIO`

The variant and its errno arm land together: adding the variant alone makes `errno`'s exhaustive `match` fail to compile, so they are one atomic, compiling change. TDD here means the new errno assertion is written first and fails to compile until the variant and arm exist.

**Files:**
- Modify: `musefs-core/src/error.rs` (the `CoreError` enum, closes at line 35)
- Modify: `musefs-fuse/src/lib.rs:87-91` (the `errno` `EIO` arm) and `musefs-fuse/src/lib.rs:520-534` (the `maps_core_errors_to_errno` test)

- [ ] **Step 1: Add the failing errno assertion**

In `musefs-fuse/src/lib.rs`, inside `fn maps_core_errors_to_errno` (after the `io_other` assertion at line 533), add:

```rust
        assert_eq!(
            errno(&CoreError::OrphanedArt {
                track_id: 1,
                art_id: 2
            })
            .code(),
            libc::EIO
        );
```

- [ ] **Step 2: Run it to confirm it fails (compile error)**

Run: `cargo test -p musefs-fuse maps_core_errors_to_errno`
Expected: FAIL — compile error, `no variant named OrphanedArt found for enum CoreError`.

- [ ] **Step 3: Add the variant to `CoreError`**

In `musefs-core/src/error.rs`, add this variant inside the `CoreError` enum (e.g. just before `TrackNotFound`):

```rust
    #[error("track {track_id} references art {art_id}, which has no metadata row (orphaned track_art — DB contract violation)")]
    OrphanedArt { track_id: i64, art_id: i64 },
```

- [ ] **Step 4: Add the `EIO` arm in `errno`**

In `musefs-fuse/src/lib.rs`, extend the `EIO` match arm (currently lines 87-91) so it reads:

```rust
        CoreError::BackingChanged(_)
        | CoreError::Db(_)
        | CoreError::DbOpen { .. }
        | CoreError::Mp4MetadataTooLarge { .. }
        | CoreError::OrphanedArt { .. }
        | CoreError::Format(_) => fuser::Errno::EIO,
```

- [ ] **Step 5: Run the test to confirm it passes**

Run: `cargo test -p musefs-fuse maps_core_errors_to_errno`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add musefs-core/src/error.rs musefs-fuse/src/lib.rs
git commit -m "feat(core): add CoreError::OrphanedArt, map to EIO (#202)"
```

---

### Task 2: Reject orphaned rows in `track_art_to_inputs`

**Files:**
- Modify: `musefs-core/src/mapping.rs:33-54` (`track_art_to_inputs`)
- Test: `musefs-core/src/mapping.rs` — new test in the existing `mod tests` (after `track_art_to_inputs_errors_on_negative_byte_len`, around line 283)

- [ ] **Step 1: Write the failing test**

In `musefs-core/src/mapping.rs`, inside `mod tests`, add this test immediately after `track_art_to_inputs_errors_on_negative_byte_len`. It links a track to one valid art row, then orphans it by deleting the `art` row through a **raw** `rusqlite` connection (the production `Db` sets `PRAGMA foreign_keys=true` and `track_art.art_id` has no `ON DELETE`, so the delete must happen on a fresh raw connection where FK enforcement defaults off):

```rust
    #[test]
    fn track_art_to_inputs_errors_on_orphaned_row() {
        use crate::CoreError;
        use musefs_db::{NewArt, TrackArt}; // NewTrack already in scope at module level
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
        let orphan_id = db
            .upsert_art(&NewArt {
                mime: "image/png".into(),
                width: None,
                height: None,
                data: vec![1, 2, 3, 4],
            })
            .unwrap();
        db.set_track_art(
            tid,
            &[TrackArt {
                art_id: orphan_id,
                picture_type: 3,
                description: String::new(),
                ordinal: 0,
            }],
        )
        .unwrap();

        // Well-formed art resolves to one input (kills the "always error" mutant).
        let inputs = super::track_art_to_inputs(&db, tid).unwrap();
        assert_eq!(inputs.len(), 1);

        // Orphan the track_art row: delete the referenced art row on a raw
        // connection (FK enforcement off by default), leaving the track_art
        // link dangling. The production Db sets foreign_keys=true, so the
        // delete would RESTRICT-fail there.
        let raw = rusqlite::Connection::open(&path).unwrap();
        let deleted = raw
            .execute("DELETE FROM art WHERE id = ?1", [orphan_id])
            .unwrap();
        assert_eq!(deleted, 1, "delete must remove exactly one art row");
        drop(raw);

        let err = super::track_art_to_inputs(&db, tid).unwrap_err();
        assert!(
            matches!(
                err,
                CoreError::OrphanedArt { track_id, art_id }
                    if track_id == tid && art_id == orphan_id
            ),
            "orphaned track_art must yield OrphanedArt with the offending ids, got {err:?}"
        );
    }
```

- [ ] **Step 2: Run it to confirm it fails**

Run: `cargo test -p musefs-core track_art_to_inputs_errors_on_orphaned_row`
Expected: FAIL — the second `track_art_to_inputs` call returns `Ok` (the row is silently skipped today), so `unwrap_err()` panics with "called `Result::unwrap_err()` on an `Ok` value".

- [ ] **Step 3: Make the orphan a hard error**

In `musefs-core/src/mapping.rs`, replace the whole `track_art_to_inputs` function (lines 37-55, from the `pub(crate) fn` signature through its closing `}`; the doc comment above it stays) with this self-contained block — it includes the signature, so paste it over the entire function rather than trusting the line range:

```rust
pub(crate) fn track_art_to_inputs<M>(db: &Db<M>, track_id: i64) -> Result<Vec<ArtInput>> {
    let mut inputs = Vec::new();
    for ta in db.get_track_art(track_id)? {
        // `track_art.art_id` is a foreign key into `art`, but SQLite FK
        // enforcement is per-connection and external writers can disable it or
        // import a partial DB. A missing `art` row is a contract violation we
        // surface (the read fails) rather than silently dropping the art.
        let Some(meta) = db.get_art_meta(ta.art_id)? else {
            return Err(crate::error::CoreError::OrphanedArt {
                track_id,
                art_id: ta.art_id,
            });
        };
        inputs.push(ArtInput {
            art_id: ta.art_id,
            mime: meta.mime,
            description: ta.description,
            picture_type: ta.picture_type,
            width: meta.width.unwrap_or(0),
            height: meta.height.unwrap_or(0),
            data_len: meta.byte_len,
        });
    }
    Ok(inputs)
}
```

- [ ] **Step 4: Run the new test to confirm it passes**

Run: `cargo test -p musefs-core track_art_to_inputs_errors_on_orphaned_row`
Expected: PASS.

- [ ] **Step 5: Run the full mapping test module to confirm no regressions**

Run: `cargo test -p musefs-core mapping`
Expected: PASS — including `track_art_to_inputs_errors_on_negative_byte_len` and `track_art_images_reads_stored_blob_bytes`.

- [ ] **Step 6: Commit**

```bash
git add musefs-core/src/mapping.rs
git commit -m "fix(core): fail on orphaned track_art rows instead of dropping art (#202)"
```

---

### Task 3: Document the behavior in the external-writer contract

**Files:**
- Modify: `ARCHITECTURE.md:143-146` (the controlled-degradation paragraph in "The external-writer contract")

- [ ] **Step 1: Add the sentence**

In `ARCHITECTURE.md`, in the paragraph ending at line 146 (`...never undefined behavior.`), append after that sentence:

```markdown
Referential gaps are treated the same way: a `track_art` row whose `art_id`
has no matching `art` row (an orphan an external writer can produce with FK
enforcement disabled) fails the serve with `EIO` rather than silently dropping
the art.
```

- [ ] **Step 2: Verify the wording reads correctly in context**

Run: `sed -n '143,150p' ARCHITECTURE.md`
Expected: the new sentence follows "...never undefined behavior." within the same paragraph.

- [ ] **Step 3: Commit**

```bash
git add ARCHITECTURE.md
git commit -m "docs: orphaned track_art rows fail the serve with EIO (#202)"
```

---

## Final verification

- [ ] **Run the full workspace test suite** (the pre-commit hook runs this too):

Run: `cargo test`
Expected: PASS.

- [ ] **Lint:**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Format check:**

Run: `cargo fmt --all --check`
Expected: clean (no diff).

## Notes

- No schema change → no Python schema-mirror regeneration.
- No format-layer signature change → fuzz targets unaffected (no `cargo +nightly fuzz build` needed).
- `track_art_images` (`mapping.rs:72`) is intentionally left unchanged: it runs only after `track_art_to_inputs` succeeds, so orphans are already rejected upstream before it is reached.
- No new logging code: `reply_errno` in `musefs-fuse/src/lib.rs` already routes non-routine `CoreError`s through a `warn!` arm, so an `OrphanedArt` is logged with its ids automatically.
