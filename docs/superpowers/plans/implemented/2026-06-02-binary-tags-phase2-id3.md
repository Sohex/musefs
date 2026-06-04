# Binary Tags Phase 2 — ID3 (MP3 + WAV) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make ID3v2 binary frames (`PRIV`/`GEOB`/`SYLT`/unpromoted `POPM`/`UFID`/…) survive scan → DB → re-synthesis as opaque passthrough, and promote the two frames with universal text equivalents (`POPM`→`rating`/`playcount`, MusicBrainz `UFID`→`musicbrainz_trackid`) to editable text tags — for both MP3 and WAV (WAV carries ID3 in its `id3 ` chunk).

**Architecture:** Opaque binary frames are extracted by a self-contained raw ID3 frame-body walker (byte-exact — no `id3`-crate re-encode), and synthesis stays hand-rolled (`push_frame_header` + body builders, emitting `Segment::BinaryTag` for streamed opaque payloads, exactly as `APIC` emits `ArtImage`). Binary payloads stream from `tags.value_blob` via the Phase-1 DB layer; nothing is materialized in memory. Phase 2 closes the open-handle `payload_id`-reuse gap (spec's "Open-handle gap (Phase-2 obligation)") with two cooperating mechanisms in `facade.rs`, landed before any task emits a `BinaryTag`: a lazy generation-gated handle re-resolve (freshness + size correctness), and a transactional `content_version` guard around reads of `BinaryTag`-bearing layouts (a single WAL read snapshot wraps the version check and the chunk reads, so a reused rowid can never be served).

**Tech Stack:** Rust workspace (`musefs-db`, `musefs-format` byte-surgery, `musefs-core` orchestration, `musefs-fuse`), the `id3` crate v1.17.0, `arc-swap`, `sharded_slab`, proptest.

**Spec:** `docs/superpowers/specs/2026-06-01-binary-tags-design.md` (§5 ID3, §6 query-split, the open-handle gap note). Phase 1 (merged): schema V2 + `Segment::BinaryTag` + DB binary-tag layer + `BinaryTagInput`.

---

## Key decisions locked before coding

These were resolved during planning (codebase + `id3`-crate research); do not re-litigate mid-implementation:

- **Opaque extraction uses a raw frame walker, NOT the `id3` crate.** `Content::to_unknown()` re-encodes typed frames with `Version::default()` + forced `Encoding::UTF8` (verified in `id3-1.17.0/src/frame/content.rs:289`), and the decoder dispatches `GEOB`/`SYLT`/`POPM`/`UFID`/`PRIV` to typed variants (`stream/frame/content.rs:420-447`). So `to_unknown` is **not byte-exact** for `GEOB`/`SYLT` (they carry a text-encoding byte) — it would silently mangle Serato `GEOB` analysis blobs, the spec's flagship case. Instead, `read_binary_tags` is a self-contained raw walker that slices each frame's post-header body verbatim. This is sound because `id3v2_alloc_safe` (`mp3.rs:335`) — already the gate on all ID3 parsing here — **rejects unsynchronised tags, extended headers, and non-zero frame flags**, so the on-disk body equals the logical body (no de-unsync needed). `read_tags`/`read_pictures` keep using the `id3` crate (text/art are unaffected by the re-encode issue). The walker handles v2.3/v2.4; v2.2 binary frames (3-char ids) are skipped (rare; text/art still parse via the crate).
- **POPM/UFID are parsed from the raw body** (not typed accessors), trivially: POPM body `<owner>\0<rating:u8>[<counter:be>]`, UFID body `<owner>\0<identifier>`.
- **Synthesis is hand-rolled** (matching `mp3.rs`'s existing `push_frame_header` + `*_frame_data` pattern). There is no public single-frame encoder in `id3` 1.17, and synthesis emits `Segment`s, not an `id3::Tag`. Opaque frames become `push_frame_header(id, len)` + `Segment::BinaryTag`; promoted frames are rebuilt by new `popm_frame_data`/`ufid_frame_data` from the `rating`/`playcount`/`musicbrainz_trackid` text tags.
- **Promotion is lossy by design** (spec §5): `POPM` owner-email dropped (rebuilt with empty owner); `POPM` counter capped at `u32` on rebuild; only `UFID` with owner `http://musicbrainz.org` promotes — any other owner is opaque. Round-trip proptests assert *semantic* survival (values), **not** byte-identical promoted frames.
- **Promoted keys must not double-emit.** `rating`/`playcount`/`musicbrainz_trackid` are excluded from the generic text/`TXXX` emission loop in `build_id3v2_segments` (otherwise they'd leak out as `TXXX:rating`, etc.).
- **Open-handle fix = gen-gated re-resolve + transactional `content_version` guard** (not content-addressing; the gap is fully closed). `sharded_slab::Slab` can't be iterated with `&self`, so a global `AtomicU64 refresh_gen` (bumped on a non-empty refresh) gates a per-handle re-resolve on the next read (`HeaderCache::resolve` is `content_version`-keyed, so unchanged tracks are a cheap cache hit). This alone shrinks but does not close the window — a refresh can land *between* the gen-check and the chunk read. So for `BinaryTag`-bearing layouts the read additionally runs inside one WAL read snapshot that first checks `live content_version == resolved.content_version` and bails (→ bounded re-resolve-and-retry) on mismatch; within a single SQLite read snapshot the `content_version` row and the `value_blob` rows are mutually consistent, so a reused rowid is impossible to serve. Plain `Inline`/`BackingAudio` layouts skip the guard (no per-read cost). The gen re-resolve also fixes a pre-existing size/content skew for handles held across a re-tag.

## Cross-layer types (consistency reference)

| Type | Where | Shape | Role |
| --- | --- | --- | --- |
| `EmbeddedBinaryTag` | `musefs-format/src/input.rs` (new) | `{ key: String, payload: Vec<u8> }` | parser → scan; opaque frame id + raw body |
| `BinaryTag` | `musefs-db` (Phase 1) | `{ key, payload, ordinal }` | scan → DB write |
| `BinaryTagRow` | `musefs-db` (Phase 1) | `{ rowid, key, byte_len }` | DB read → mapping |
| `BinaryTagInput` | `musefs-format` (Phase 1) | `{ key, payload_id, len }` | mapping → synthesis; `payload_id` == `tags` rowid |
| `Segment::BinaryTag` | `musefs-format` (Phase 1) | `{ payload_id, len }` | streamed at read time |

`read_binary_tags` returns `(Vec<EmbeddedBinaryTag>, Vec<(String, String)>)` — `(opaque, promoted)`. `promoted` pairs merge into the text-tag vector (written by `replace_tags`); `opaque` is written by `set_binary_tags`.

---

## File structure

| File | Change | Responsibility |
| --- | --- | --- |
| `musefs-core/src/facade.rs` | Modify | `refresh_gen`; `Handle { track_id, resolved: ArcSwap, gen }`; gen-gated re-resolve + transactional `content_version` guard in `read`; bump on non-empty refresh. |
| `musefs-db/src/tracks.rs` | Modify | `track_content_version`; `begin_read`/`end_read` read-snapshot delimiters. |
| `musefs-db/src/bulk.rs` | Modify | Scope `BulkWriter::replace_tags` DELETE to text rows (`value_blob IS NULL`); add `BulkWriter::set_binary_tags` batch mirror. |
| `musefs-format/src/input.rs` | Modify | Add `EmbeddedBinaryTag`; re-export. |
| `musefs-format/src/layout.rs` | Modify | `RegionLayout::has_binary_tag()` (gates the read-time guard). |
| `musefs-format/src/mp3.rs` | Modify | `read_binary_tags` (raw frame walker, byte-exact) + `classify_binary_frame`; `popm_frame_data`/`ufid_frame_data`; `build_id3v2_segments(tags, binary_tags, arts)`; thread `synthesize_layout`. |
| `musefs-format/src/wav.rs` | Modify | `read_binary_tags` over the `id3 ` chunk; thread `binary_tags` through `synthesize_layout`. |
| `musefs-core/src/mapping.rs` | Modify | `binary_tags_to_inputs(db, track_id)`. |
| `musefs-core/src/scan.rs` | Modify | `Probed.binary_tags`; `MAX_BINARY_TAG_BYTES`; populate in MP3/WAV probe arms; write via `set_binary_tags` after text replace; merge `promoted`. |
| `musefs-core/src/reader.rs` | Modify | `ResolvedFile.has_binary_tag` (set in `build`); MP3 + WAV resolve arms load `get_binary_tags` → `BinaryTagInput`, pass to `synthesize_layout`; `BinaryTag` serve arm emits the chunk metric. |
| `musefs-core/src/metrics.rs` | Modify | `binary_tag_chunks` counter + `on_binary_tag_chunk()` (parity with `art_chunks`). |

---

## Tasks

### Task 2.1: Close the open-handle `payload_id`-reuse gap (handle re-resolve)

Lands first so the gate is in place before any task emits a `BinaryTag`. Behavior-neutral on its own.

**Files:**
- Modify: `musefs-core/src/facade.rs` (`Handle` struct ~53, `Musefs` fields ~108, `read` ~608, `open_handle` ~638, `poll_refresh_notify` ~372)
- Modify: `musefs-db/src/tags.rs` or `tracks.rs` (`track_content_version`, `begin_read`/`end_read` snapshot helpers)
- Modify: `musefs-format/src/layout.rs` (`RegionLayout::has_binary_tag`)
- Test: `musefs-core/src/facade.rs` (inline)

This task lands two cooperating mechanisms: (A) the gen-gated re-resolve (freshness + size), and (B) the transactional `content_version` guard for `BinaryTag`-bearing reads (full close of the reuse window). Both are behavior-neutral in Phase 1 terms (no layout yet contains a `BinaryTag`), so they land safely first.

- [ ] **Step 1: Write the failing test (re-resolve / size correctness)**

Add to `facade.rs` a `#[cfg(test)]` test (mirror existing facade tests for `Musefs::open`/mount setup; they construct a `Db`, scan a temp file, and `Musefs::open`). The assertion: a handle held across a content_version bump re-resolves and serves the *new* layout length.

```rust
#[test]
fn open_handle_reresolves_after_content_version_bump() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let mp3 = dir.path().join("a.mp3");
    // Minimal MP3: tiny ID3v2 + 1 frame of silence is unnecessary here — scan only
    // needs a probe-able file. Reuse the crate's mp3 test fixture helper if present;
    // otherwise write known-good bytes from an existing facade/scan test.
    std::fs::write(&mp3, super::test_support::minimal_mp3()).unwrap();

    let db_path = dir.path().join("db.sqlite");
    let db = Db::open(&db_path).unwrap();
    crate::scan::scan_directory(&db, dir.path(), false).unwrap();
    let fs = Musefs::open(Db::open(&db_path).unwrap(), MountConfig::default()).unwrap();

    let ino = /* resolve the inode for a.mp3 via fs.lookup/readdir as other tests do */
        super::test_support::inode_of(&fs, "a.mp3");
    let fh = fs.open_handle(ino).unwrap();
    let len_before = fs.read(ino, fh, 0, 1 << 20).unwrap().len();

    // Out-of-band re-tag: add a long text tag so the synthesized region grows.
    let track_id = super::test_support::track_id_of(&db, "a.mp3");
    db.replace_tags(track_id, &[musefs_db::Tag::new("comment", &"x".repeat(4096), 0)])
        .unwrap();
    assert!(fs.poll_refresh().unwrap());

    // Same handle now serves the larger layout (re-resolved), not the stale snapshot.
    let len_after = fs.read(ino, fh, 0, 1 << 20).unwrap().len();
    assert!(len_after > len_before, "handle did not re-resolve: {len_before} -> {len_after}");
    fs.release_handle(fh);
}
```

> Implementer note: copy the exact mount/lookup/track-id helpers from the nearest existing `facade.rs` test (e.g. the `poll_refresh` tests). If no `test_support` helpers exist, inline the inode lookup (`fs.lookup(ROOT_INODE, "a.mp3")`) and `db.tracks()`-style track-id lookup the way those tests do. The behavioral assertion (`len_after > len_before`) is the point.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-core open_handle_reresolves_after_content_version_bump`
Expected: FAIL — `len_after == len_before` (handle keeps serving the stale snapshot).

- [ ] **Step 3: Add `refresh_gen` and extend `Handle`**

In `musefs-core/src/facade.rs`, change the `Handle` struct (currently `{ resolved: Arc<ResolvedFile>, file }`) and update its doc comment:

```rust
/// An open file handle: the resolved layout, the track it belongs to, the
/// generation at which `resolved` was last validated, and a backing fd opened
/// once at `open`.
///
/// A handle survives `poll_refresh`, but is **not** a frozen snapshot: when the
/// global `refresh_gen` advances (a refresh applied changes), the next `read`
/// re-resolves the track (a cheap `content_version`-keyed cache hit when the
/// track is unchanged) and swaps in the fresh layout. This keeps a re-tagged
/// file's handle consistent with the size the kernel sees via getattr, and
/// prevents a stale `Segment::BinaryTag { payload_id }` from serving reused-rowid
/// bytes after a re-tag.
struct Handle {
    track_id: i64,
    resolved: arc_swap::ArcSwap<ResolvedFile>,
    gen: std::sync::atomic::AtomicU64,
    file: std::fs::File,
}
```

Add a field to `Musefs` (near `handles`):

```rust
    refresh_gen: std::sync::atomic::AtomicU64,
```

Initialize it in `Musefs::open`/constructor alongside `handles: sharded_slab::Slab::new()`:

```rust
            refresh_gen: std::sync::atomic::AtomicU64::new(0),
```

- [ ] **Step 4: Build the handle with the new fields in `open_handle`**

Change the final line of `open_handle` (currently `fh_from_key(self.handles.insert(Arc::new(Handle { resolved, file })))`):

```rust
        let gen = self.refresh_gen.load(std::sync::atomic::Ordering::Acquire);
        fh_from_key(self.handles.insert(Arc::new(Handle {
            track_id,
            resolved: arc_swap::ArcSwap::from(resolved),
            gen: std::sync::atomic::AtomicU64::new(gen),
            file,
        })))
```

- [ ] **Step 5: Add the supporting helpers (`has_binary_tag`, DB snapshot + version read)**

In `musefs-format/src/layout.rs`, add to `impl RegionLayout`:

```rust
/// True if any segment streams an opaque binary tag payload from the DB. Used by
/// the reader to decide whether a read needs the transactional content_version
/// guard (plain Inline/BackingAudio layouts don't).
pub fn has_binary_tag(&self) -> bool {
    self.segments.iter().any(|s| matches!(s, Segment::BinaryTag { .. }))
}
```

In `musefs-db` (e.g. `tracks.rs`), add to `impl Db` a lightweight version read and a pair of read-snapshot delimiters (the facade can't touch `self.conn` from `musefs-core`, so these wrap it). The snapshot is connection-level, so reads issued on the same `Db` between `begin_read` and `end_read` share one consistent WAL snapshot:

```rust
/// The track's current `content_version` (cheap; no full-row fetch).
pub fn track_content_version(&self, track_id: i64) -> Result<i64> {
    Ok(self.conn.query_row(
        "SELECT content_version FROM tracks WHERE id = ?1",
        rusqlite::params![track_id],
        |r| r.get(0),
    )?)
}

/// Begin a deferred (read) transaction: subsequent reads on this connection see a
/// single consistent snapshot until `end_read`. Used to make a binary-tag read's
/// content_version check and its blob reads mutually consistent.
pub fn begin_read(&self) -> Result<()> {
    self.conn.execute_batch("BEGIN DEFERRED")?;
    Ok(())
}

/// End the read transaction opened by `begin_read` (rollback — it is read-only).
pub fn end_read(&self) -> Result<()> {
    self.conn.execute_batch("ROLLBACK")?;
    Ok(())
}
```

Add a one-line `musefs-db` test that `track_content_version` returns the value the triggers maintain (insert a tag, assert it incremented).

- [ ] **Step 6: Gen-gated re-resolve + transactional guard in the `read` fast path**

Replace the `fh != 0` fast-path body in `Musefs::read` (currently clones the handle and calls `read_at_with_file(&h.resolved, …)`). The loop re-resolves on a stale generation, then reads — guarding `BinaryTag` layouts inside a snapshot and retrying (bounded) if a refresh raced the read:

```rust
        if fh != 0 {
            let handle = self.handles.get((fh - 1) as usize).map(|g| Arc::clone(&g));
            if let Some(h) = handle {
                // Bounded retry absorbs a refresh landing mid-read; out-of-band
                // re-tags are human/batch-paced, so >1 attempt is already rare.
                for _attempt in 0..4 {
                    let cur = self.refresh_gen.load(std::sync::atomic::Ordering::Acquire);
                    if h.gen.load(std::sync::atomic::Ordering::Acquire) != cur {
                        // A refresh changed something; re-resolve (cheap content_version
                        // cache hit when this track is unchanged) and re-stamp.
                        let fresh = self.pool.with(|db| self.cache.resolve(db, h.track_id))?;
                        h.resolved.store(fresh);
                        h.gen.store(cur, std::sync::atomic::Ordering::Release);
                    }
                    let resolved = h.resolved.load();
                    let r: &ResolvedFile = &resolved;
                    let served = self.pool.with(|db| -> Result<Option<Vec<u8>>> {
                        if r.has_binary_tag {
                            // Snapshot-consistent: version check + blob reads see one
                            // WAL snapshot, so a reused rowid can't be served.
                            db.begin_read()?;
                            let res = (|| {
                                if db.track_content_version(h.track_id)? != r.content_version {
                                    return Ok(None); // stale layout — retry after re-resolve
                                }
                                Ok(Some(read_at_with_file(r, db, &h.file, offset, size)?))
                            })();
                            let _ = db.end_read(); // always release the snapshot
                            res
                        } else {
                            Ok(Some(read_at_with_file(r, db, &h.file, offset, size)?))
                        }
                    })?;
                    match served {
                        Some(bytes) => return Ok(bytes),
                        None => {
                            // Force a re-resolve next iteration against the live version.
                            let fresh = self.pool.with(|db| self.cache.resolve(db, h.track_id))?;
                            h.resolved.store(fresh);
                            h.gen.store(
                                self.refresh_gen.load(std::sync::atomic::Ordering::Acquire),
                                std::sync::atomic::Ordering::Release,
                            );
                        }
                    }
                }
                // Pathological constant re-tagging raced every attempt; surface a
                // retryable error rather than risk wrong bytes.
                return Err(CoreError::BackingChanged(
                    h.resolved.load().backing_path.to_string_lossy().to_string(),
                ));
            }
        }
```

Add a `has_binary_tag: bool` field to `ResolvedFile` (`reader.rs`), set once in `HeaderCache::build` from `layout.has_binary_tag()` — so the read path checks a precomputed bool, not the segment vector each call.

> Notes: `cache.resolve` re-stats/re-validates the backing file, so a moved/deleted backing surfaces as `BackingChanged` on the read. `let r: &ResolvedFile = &resolved;` pins the deref from the `ArcSwap` guard explicitly (the guard derefs `Guard → Arc → ResolvedFile`). `end_read` runs on every path via the inner closure so the snapshot is always released. If your `Result`/`CoreError` lacks a clean "retry" variant, `BackingChanged` is the closest existing retryable signal; consider a dedicated variant if one reads better.

- [ ] **Step 7: Bump `refresh_gen` on a non-empty refresh**

In `poll_refresh_notify`, the rebuild returns `(new_snapshot, change)` (currently `let (new_snapshot, _change) = …`). Capture `change` and, after the snapshot is committed (end of a successful refresh that applied changes), bump the generation. Locate the block that updates the stored snapshot (around line 442) and add, guarded by the change set being non-empty:

```rust
        if !change.is_empty() {
            self.refresh_gen.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        }
```

Rename `_change` to `change` at the `rebuild_incremental` call site. `ChangeSet::is_empty()` already exists (`refresh_diff.rs:31`).

- [ ] **Step 8: Run the size-correctness test to verify it passes**

Run: `cargo test -p musefs-core open_handle_reresolves_after_content_version_bump`
Expected: PASS.

- [ ] **Step 9: Write the rowid-reuse safety test (the property the guard actually protects)**

The Step-1 test only proves the layout re-resolves (size grows). Add a test that proves the real safety property: a handle holding a `BinaryTag` never serves a *different* row's bytes after the original payload's rowid is reused. Because the guard makes the read snapshot-consistent, the held handle must serve either the original bytes or a clean `BackingChanged` error — never another payload.

```rust
#[test]
fn binary_tag_handle_never_serves_reused_rowid_bytes() {
    // Scan an MP3 carrying a PRIV frame; open a handle; read the original payload.
    // Then churn the tags rowids by re-tagging binary rows (delete the PRIV, insert
    // a different same-length payload that reuses the freed rowid), poll_refresh,
    // and read again on the SAME handle. Assert the result is either the original
    // payload bytes or an Err — but never the new (reused-rowid) payload's bytes.
    // (Construct the fixture + handle exactly as in Step 1; force rowid reuse by
    // set_binary_tags([]) then set_binary_tags([<same-length different payload>]).)
}
```

> Implementer: the deterministic way to force reuse is to make the PRIV row the highest rowid, delete it (`set_binary_tags(tid, &[])`), then insert one new binary row — SQLite reclaims the freed max rowid. Assert the handle read ≠ the new payload bytes.

- [ ] **Step 10: Confirm `arc-swap` is a direct dependency of `musefs-core`**

Run: `cargo tree -p musefs-core -i arc-swap --depth 0`
Expected: shows `arc-swap` (the tree already uses `ArcSwap<VirtualTree>`). If it is only transitive, add `arc-swap = "1"` to `musefs-core/Cargo.toml` `[dependencies]` and re-run.

- [ ] **Step 11: Full crate test + commit**

Run: `cargo test -p musefs-core -p musefs-db -p musefs-format`
Expected: PASS (existing handle/poll tests still green; new `track_content_version` + `has_binary_tag` tests pass; both handle tests pass).

```bash
git add musefs-core/src/facade.rs musefs-core/src/reader.rs musefs-core/Cargo.toml \
        musefs-db/src/tracks.rs musefs-format/src/layout.rs
git commit -m "fix(core): close open-handle payload_id reuse gap (re-resolve + snapshot guard)"
```

---

### Task 2.2: Bulk-writer binary-tag support (`replace_tags` scoping + `set_binary_tags`)

Two `bulk.rs` changes the scan fast path needs: (a) scope `BulkWriter::replace_tags` to text rows so it stops wiping binary rows, and (b) add a `BulkWriter::set_binary_tags` batch mirror of `Db::set_binary_tags` (Phase 1 added only the `Db` method). `ingest_bulk` (Task 2.8) calls both.

**Files:**
- Modify: `musefs-db/src/bulk.rs` (`replace_tags` ~68; new `set_binary_tags`)
- Test: `musefs-db/src/bulk.rs` (inline)

- [ ] **Step 1: Write the failing test**

Add to `bulk.rs` tests (mirror the existing bulk test setup that opens an in-memory `Db` and a `BulkWriter`):

```rust
#[test]
fn bulk_replace_tags_preserves_binary_rows() {
    let db = Db::open_in_memory().unwrap();
    let tid = db.upsert_track(&crate::NewTrack {
        backing_path: "/a.mp3".into(),
        format: crate::Format::Mp3,
        audio_offset: 0, audio_length: 0, backing_size: 0, backing_mtime: 0,
    }).unwrap();
    db.set_binary_tags(tid, &[crate::BinaryTag { key: "PRIV".into(), payload: vec![1,2,3], ordinal: 0 }]).unwrap();

    {
        let mut bw = db.bulk_writer().unwrap();
        bw.replace_tags(tid, &[crate::Tag::new("artist", "A", 0)]).unwrap();
        bw.commit().unwrap();
    }

    assert_eq!(db.get_binary_tags(tid).unwrap().len(), 1, "bulk replace_tags wiped binary rows");
    assert_eq!(db.get_tags(tid).unwrap(), vec![crate::Tag::new("artist", "A", 0)]);
}
```

> Use the actual `BulkWriter` construction + commit API from the nearest existing `bulk.rs` test (the exact constructor name may be `db.bulk_writer()` / `BulkWriter::new`). The assertion is the point.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-db bulk_replace_tags_preserves_binary_rows`
Expected: FAIL — `get_binary_tags` returns 0 (the unscoped DELETE removed the `PRIV` row).

- [ ] **Step 3: Scope the DELETE**

In `musefs-db/src/bulk.rs`, change the `replace_tags` DELETE (currently `DELETE FROM tags WHERE track_id = ?1`):

```rust
        self.tx.execute(
            "DELETE FROM tags WHERE track_id = ?1 AND value_blob IS NULL",
            params![track_id],
        )?;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p musefs-db bulk_replace_tags_preserves_binary_rows`
Expected: PASS.

- [ ] **Step 5: Full crate test + commit the scoping fix**

Run: `cargo test -p musefs-db`

```bash
git add musefs-db/src/bulk.rs
git commit -m "fix(db): scope BulkWriter::replace_tags to text rows"
```

- [ ] **Step 6: Write the failing test for `BulkWriter::set_binary_tags`**

Add to `bulk.rs` tests:

```rust
#[test]
fn bulk_set_binary_tags_round_trips_and_scopes_to_binary_rows() {
    let db = Db::open_in_memory().unwrap();
    let tid = db.upsert_track(&crate::NewTrack {
        backing_path: "/a.mp3".into(), format: crate::Format::Mp3,
        audio_offset: 0, audio_length: 0, backing_size: 0, backing_mtime: 0,
    }).unwrap();
    {
        let mut bw = db.bulk_writer().unwrap();
        bw.replace_tags(tid, &[crate::Tag::new("artist", "A", 0)]).unwrap();
        bw.set_binary_tags(tid, &[crate::BinaryTag { key: "PRIV".into(), payload: vec![7, 7, 7], ordinal: 0 }]).unwrap();
        bw.commit().unwrap();
    }
    let rows = db.get_binary_tags(tid).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key, "PRIV");
    assert_eq!(rows[0].byte_len, 3);
    // Text row untouched by the binary write.
    assert_eq!(db.get_tags(tid).unwrap(), vec![crate::Tag::new("artist", "A", 0)]);
}
```

- [ ] **Step 7: Run test to verify it fails**

Run: `cargo test -p musefs-db bulk_set_binary_tags_round_trips`
Expected: FAIL — `BulkWriter::set_binary_tags` not found.

- [ ] **Step 8: Implement `BulkWriter::set_binary_tags`**

In `musefs-db/src/bulk.rs`, add to `impl BulkWriter` (mirror `Db::set_binary_tags`, but using the batch transaction `self.tx`). Ensure `BinaryTag` is imported (`use crate::models::BinaryTag;` or `crate::BinaryTag`):

```rust
/// Replace the track's binary tag rows (`value_blob IS NOT NULL`) through the
/// batch transaction; text rows (managed by `replace_tags`) are untouched. Binary
/// rows store '' in `value`. The batch mirror of `Db::set_binary_tags`.
pub fn set_binary_tags(&mut self, track_id: i64, tags: &[BinaryTag]) -> Result<()> {
    self.tx.execute(
        "DELETE FROM tags WHERE track_id = ?1 AND value_blob IS NOT NULL",
        params![track_id],
    )?;
    let mut stmt = self.tx.prepare(
        "INSERT INTO tags (track_id, key, value, value_blob, ordinal) \
         VALUES (?1, ?2, '', ?3, ?4)",
    )?;
    for t in tags {
        stmt.execute(params![track_id, t.key, t.payload, t.ordinal])?;
    }
    Ok(())
}
```

> Match the exact borrow shape of the existing `replace_tags` in this file — if it scopes the prepared `stmt` in a `{ … }` block to drop the `self.tx` borrow before `commit`, do the same here.

- [ ] **Step 9: Run test + commit**

Run: `cargo test -p musefs-db bulk_set_binary_tags_round_trips && cargo test -p musefs-db`

```bash
git add musefs-db/src/bulk.rs
git commit -m "feat(db): BulkWriter::set_binary_tags batch mirror"
```

---

### Task 2.3: `EmbeddedBinaryTag` parser return type

**Files:**
- Modify: `musefs-format/src/input.rs`
- Modify: `musefs-format/src/lib.rs` (re-export)
- Test: `musefs-format/src/input.rs` (inline)

- [ ] **Step 1: Write the failing test**

Add to `input.rs` tests:

```rust
#[test]
fn embedded_binary_tag_constructs() {
    let e = super::EmbeddedBinaryTag { key: "PRIV".into(), payload: vec![1, 2, 3] };
    assert_eq!(e.key, "PRIV");
    assert_eq!(e.payload.len(), 3);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-format embedded_binary_tag_constructs`
Expected: FAIL — `EmbeddedBinaryTag` not found.

- [ ] **Step 3: Add the struct**

Append to `musefs-format/src/input.rs`:

```rust
/// A binary tag frame extracted at scan time: the format-private identifier
/// (`key` — an ID3 4-char frame id, a FLAC `APPLICATION`/`CUESHEET`, or an MP4
/// `----:<mean>:<name>`) and the raw post-header body (`payload`). Unlike
/// `BinaryTagInput`, this owns the bytes (scan ingests them into the DB).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddedBinaryTag {
    pub key: String,
    pub payload: Vec<u8>,
}
```

- [ ] **Step 4: Re-export**

In `musefs-format/src/lib.rs`, extend the input re-export (currently `pub use input::{ArtInput, BinaryTagInput, EmbeddedPicture, TagInput};`):

```rust
pub use input::{ArtInput, BinaryTagInput, EmbeddedBinaryTag, EmbeddedPicture, TagInput};
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p musefs-format embedded_binary_tag_constructs`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add musefs-format/src/input.rs musefs-format/src/lib.rs
git commit -m "feat(format): EmbeddedBinaryTag parser return type"
```

---

### Task 2.4: `read_binary_tags` — classify + promote (mp3.rs)

**Files:**
- Modify: `musefs-format/src/mp3.rs`
- Test: `musefs-format/src/mp3.rs` (inline)

- [ ] **Step 1: Write the failing tests (classification + byte-exact GEOB)**

Add to `mp3.rs` tests. The first uses the `id3` crate's `Encoder` to build a v2.4 fixture (default encoder emits no unsync / no extended header / zero frame flags, so it passes `id3v2_alloc_safe`). The second builds a `GEOB` frame **by hand** with a Latin1 (encoding byte `0x00`) description and asserts the opaque payload is byte-identical — this is the fidelity property `to_unknown` would have broken.

```rust
#[test]
fn read_binary_tags_promotes_popm_and_mbid_and_passes_through_priv() {
    use id3::frame::{Content, Popularimeter, UniqueFileIdentifier, Unknown};
    use id3::{Frame, Tag, TagLike, Version};

    let mut tag = Tag::new();
    tag.add_frame(Popularimeter { user: "a@b.c".into(), rating: 200, counter: 7 });
    tag.add_frame(UniqueFileIdentifier {
        owner_identifier: "http://musicbrainz.org".into(),
        identifier: b"mbid-123".to_vec(),
    });
    tag.add_frame(UniqueFileIdentifier {
        owner_identifier: "http://other.example".into(),
        identifier: b"other".to_vec(),
    });
    tag.add_frame(Frame::with_content(
        "PRIV",
        Content::Unknown(Unknown { data: vec![9, 8, 7], version: Version::Id3v24 }),
    ));
    let mut buf = Vec::new();
    id3::Encoder::new().version(Version::Id3v24).encode(&tag, &mut buf).unwrap();

    let (opaque, promoted) = super::read_binary_tags(&buf);
    assert!(promoted.contains(&("rating".to_string(), "200".to_string())));
    assert!(promoted.contains(&("playcount".to_string(), "7".to_string())));
    assert!(promoted.contains(&("musicbrainz_trackid".to_string(), "mbid-123".to_string())));
    let keys: Vec<&str> = opaque.iter().map(|e| e.key.as_str()).collect();
    assert!(keys.contains(&"PRIV"));
    assert_eq!(keys.iter().filter(|k| **k == "UFID").count(), 1); // non-MB UFID opaque
    assert_eq!(opaque.iter().find(|e| e.key == "PRIV").unwrap().payload, vec![9, 8, 7]);
}

#[test]
fn read_binary_tags_preserves_geob_body_byte_exact() {
    // A GEOB body with a Latin1 (encoding 0x00) description — the exact case the
    // crate's to_unknown() would re-encode to UTF-8. Build a minimal v2.4 tag by
    // hand: header + one GEOB frame header + body.
    let geob_body: Vec<u8> = {
        let mut b = vec![0x00];                 // text encoding: ISO-8859-1
        b.extend_from_slice(b"application/octet-stream\0"); // mime
        b.extend_from_slice(b"Serato Overview\0");          // filename (latin1)
        b.extend_from_slice(b"\0");                          // description
        b.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);      // object data
        b
    };
    let tag = build_v24_tag(&[(b"GEOB", &geob_body)]); // small test helper (Step 3 note)

    let (opaque, _promoted) = super::read_binary_tags(&tag);
    let geob = opaque.iter().find(|e| e.key == "GEOB").expect("GEOB preserved");
    assert_eq!(geob.payload, geob_body, "GEOB body must survive byte-identical");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p musefs-format read_binary_tags`
Expected: FAIL — `read_binary_tags` not found.

- [ ] **Step 3: Implement `read_binary_tags` as a raw frame walker**

Add to `musefs-format/src/mp3.rs` (after `read_tags`). This does **not** use the `id3` crate: it walks frames with the same size-decode logic as `id3v2_alloc_safe` (which has already guaranteed no unsync / no extended header / zero frame flags, so on-disk bodies are byte-exact). The MusicBrainz owner constant is reused by synthesis (Task 2.5), so make it `pub(crate)`.

```rust
pub(crate) const MUSICBRAINZ_UFID_OWNER: &str = "http://musicbrainz.org";

/// Extract an ID3v2.3/2.4 tag's binary frames. Returns `(opaque, promoted)`:
/// - `opaque`: frames preserved **byte-exact** — `(frame-id, raw post-header body)`.
///   `PRIV`/`GEOB`/`SYLT`/`MCDI`/unknown frames and any non-MusicBrainz `UFID`.
/// - `promoted`: `(key, value)` text pairs — `POPM` → `rating` (raw 0–255) `+ playcount`
///   (counter, omitted when 0); MusicBrainz `UFID` → `musicbrainz_trackid`. Promoted
///   frames are NOT in `opaque`.
///
/// Text (`T***`), `COMM`, `USLT`, `APIC` are handled by `read_tags`/`read_pictures`
/// and skipped. Gated by `id3v2_alloc_safe`, so the tag is well-formed, has no
/// unsynchronisation/extended header/frame flags, and bodies are sliced verbatim.
/// v2.2 (3-char ids) is not processed (rare; text/art still parse via the crate).
pub fn read_binary_tags(data: &[u8]) -> (Vec<EmbeddedBinaryTag>, Vec<(String, String)>) {
    let mut opaque = Vec::new();
    let mut promoted = Vec::new();
    if !id3v2_alloc_safe(data) || data[3] < 3 {
        return (opaque, promoted);
    }
    let tag_end = 10 + synchsafe_decode(&data[6..10]) as usize; // bounds validated upstream
    let mut pos = 10usize;
    while pos + 10 <= tag_end {
        if data[pos] == 0 {
            break; // padding
        }
        let id = &data[pos..pos + 4];
        let size = if data[3] == 3 {
            u32::from_be_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]]) as usize
        } else {
            synchsafe_decode(&data[pos + 4..pos + 8]) as usize
        };
        let body_start = pos + 10;
        if body_start + size > tag_end {
            break; // defensive; id3v2_alloc_safe already validated bounds
        }
        classify_binary_frame(id, &data[body_start..body_start + size], &mut opaque, &mut promoted);
        pos = body_start + size;
    }
    (opaque, promoted)
}

/// Classify one ID3v2 frame body into opaque-passthrough or promoted-text.
fn classify_binary_frame(
    id: &[u8],
    body: &[u8],
    opaque: &mut Vec<EmbeddedBinaryTag>,
    promoted: &mut Vec<(String, String)>,
) {
    // Handled by read_tags/read_pictures: text frames (T***), COMM, USLT, APIC.
    if id[0] == b'T' || id == b"COMM" || id == b"USLT" || id == b"APIC" {
        return;
    }
    match id {
        b"POPM" => {
            // <owner>\0<rating:u8>[<counter: big-endian>]
            if let Some(nul) = body.iter().position(|&b| b == 0) {
                if let Some((&rating, counter)) = body[nul + 1..].split_first() {
                    promoted.push(("rating".to_string(), rating.to_string()));
                    let c = counter.iter().take(8).fold(0u64, |a, &b| (a << 8) | b as u64);
                    if c > 0 {
                        promoted.push(("playcount".to_string(), c.to_string()));
                    }
                }
            }
        }
        b"UFID" => {
            // <owner>\0<identifier>. MusicBrainz owner promotes; others opaque.
            match body.iter().position(|&b| b == 0) {
                Some(nul) if &body[..nul] == MUSICBRAINZ_UFID_OWNER.as_bytes() => {
                    promoted.push((
                        "musicbrainz_trackid".to_string(),
                        String::from_utf8_lossy(&body[nul + 1..]).into_owned(),
                    ));
                }
                _ => opaque.push(EmbeddedBinaryTag { key: "UFID".to_string(), payload: body.to_vec() }),
            }
        }
        _ => {
            // Opaque verbatim: PRIV, GEOB, SYLT, MCDI, W***, unknown, … (4-byte ids).
            if id.iter().all(|b| b.is_ascii_graphic()) {
                opaque.push(EmbeddedBinaryTag {
                    key: String::from_utf8_lossy(id).into_owned(),
                    payload: body.to_vec(),
                });
            }
        }
    }
}
```

Add `use crate::input::EmbeddedBinaryTag;` to `mp3.rs` if not in scope. Note `synchsafe_decode` already exists in this file (used by `id3v2_alloc_safe`). For the byte-exact test, add a small `#[cfg(test)] fn build_v24_tag(frames: &[(&[u8;4], &[u8])]) -> Vec<u8>` helper that writes `b"ID3\x04\x00\x00"` + a synchsafe total size + each frame's 10-byte header (id + synchsafe size + `\0\0`) + body. (Use the file's `syncsafe` helper for the size fields.)

> One definition of `MUSICBRAINZ_UFID_OWNER` (a `&str`): the parser compares against `.as_bytes()`, synthesis (Task 2.5) passes it straight to `ufid_frame_data(owner: &str, …)`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p musefs-format read_binary_tags`
Expected: PASS (both classification and byte-exact GEOB).

- [ ] **Step 5: Commit**

```bash
git add musefs-format/src/mp3.rs
git commit -m "feat(format): read_binary_tags — ID3 opaque passthrough + POPM/UFID promotion"
```

---

### Task 2.5: Synthesis — rebuild POPM/UFID + emit opaque binary frames

Changes `build_id3v2_segments`'s signature to `(tags, binary_tags, arts)`; updates its two callers (`mp3::synthesize_layout`, `wav::synthesize_layout`) to pass `binary_tags` through (the reader passes `&[]` until Task 2.9, keeping the workspace compiling). Adds the two body builders and the promoted-key exclusion.

**Files:**
- Modify: `musefs-format/src/mp3.rs` (`build_id3v2_segments`, `synthesize_layout`, new builders)
- Modify: `musefs-format/src/wav.rs` (`synthesize_layout` call site)
- Modify: `musefs-core/src/reader.rs` (MP3 + WAV arms: pass `&[]` for now)
- Test: `musefs-format/src/mp3.rs` (inline)

- [ ] **Step 1: Write the failing test**

Add to `mp3.rs` tests. Asserts: promoted text tags rebuild a `POPM`/`UFID` frame (and don't leak as `TXXX`), and an opaque `BinaryTagInput` becomes a header + `Segment::BinaryTag`.

```rust
#[test]
fn build_id3v2_segments_rebuilds_popm_ufid_and_streams_opaque() {
    use crate::{BinaryTagInput, TagInput};
    let tags = vec![
        TagInput::new("artist", "A"),
        TagInput::new("rating", "200"),
        TagInput::new("playcount", "7"),
        TagInput::new("musicbrainz_trackid", "mbid-123"),
    ];
    let bin = vec![BinaryTagInput { key: "PRIV".into(), payload_id: 42, len: 3 }];
    let (segments, _len) = super::build_id3v2_segments(&tags, &bin, &[]).unwrap();

    // The opaque PRIV streams as a BinaryTag carrying its payload_id.
    assert!(segments.iter().any(|s| matches!(s, Segment::BinaryTag { payload_id: 42, len: 3 })));

    // Materialize the inline bytes and confirm POPM + UFID frame ids are present
    // and rating/playcount/musicbrainz_trackid did NOT leak as TXXX descriptors.
    let inline: Vec<u8> = segments.iter().flat_map(|s| match s {
        Segment::Inline(b) => b.clone(),
        _ => Vec::new(),
    }).collect();
    assert!(find_sub(&inline, b"POPM"), "POPM not rebuilt");
    assert!(find_sub(&inline, b"UFID"), "UFID not rebuilt");
    assert!(find_sub(&inline, b"http://musicbrainz.org"), "UFID owner missing");
    assert!(!find_sub(&inline, b"rating"), "promoted key leaked as TXXX");
    assert!(!find_sub(&inline, b"musicbrainz_trackid"), "promoted key leaked as TXXX");
}

// Tiny substring search helper for the test module (add once if absent).
fn find_sub(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-format build_id3v2_segments_rebuilds_popm_ufid`
Expected: FAIL — `build_id3v2_segments` takes 2 args (arity mismatch / won't compile).

- [ ] **Step 3: Add the body builders**

In `musefs-format/src/mp3.rs`, near the other `*_frame_data` helpers (after `apic_framing`):

```rust
/// POPM body: `<owner>\0<rating:u8>[<counter: 4-byte big-endian>]`. Owner is empty
/// by design (spec §5 — the original tagger identity is dropped). The counter is
/// emitted as 4 bytes when `playcount > 0` and omitted when 0; values above
/// `u32::MAX` are clamped (the typed read path caps at u64, the common case fits
/// u32).
fn popm_frame_data(rating: u8, playcount: u64) -> Vec<u8> {
    let mut d = Vec::new();
    d.push(0x00); // empty owner, NUL-terminated
    d.push(rating);
    if playcount > 0 {
        let c = u32::try_from(playcount).unwrap_or(u32::MAX);
        d.extend_from_slice(&c.to_be_bytes());
    }
    d
}

/// UFID body: `<owner>\0<identifier bytes>`.
fn ufid_frame_data(owner: &str, identifier: &[u8]) -> Vec<u8> {
    let mut d = Vec::new();
    d.extend_from_slice(owner.as_bytes());
    d.push(0x00);
    d.extend_from_slice(identifier);
    d
}

/// True for the canonical text keys that are rebuilt as POPM/UFID frames and must
/// therefore be excluded from the generic text/TXXX emission (no double-store).
fn is_promoted_key(key: &str) -> bool {
    matches!(key, "rating" | "playcount" | "musicbrainz_trackid")
}
```

- [ ] **Step 4: Change `build_id3v2_segments`**

Replace the signature and weave in promotion + opaque emission. The new signature:

```rust
pub(crate) fn build_id3v2_segments(
    tags: &[TagInput],
    binary_tags: &[BinaryTagInput],
    arts: &[ArtInput],
) -> Result<(Vec<Segment>, u64)> {
```

Add `use crate::input::BinaryTagInput;` at the top of `mp3.rs` if not already in scope.

Inside, **before** the `groups` build, pull the promoted scalar values out of `tags` (first value wins per key):

```rust
    let mut popm_rating: Option<u8> = None;
    let mut popm_playcount: u64 = 0;
    let mut mbid: Option<String> = None;
    for t in tags {
        match t.key.as_str() {
            "rating" if popm_rating.is_none() => popm_rating = t.value.parse().ok(),
            "playcount" => popm_playcount = t.value.parse().unwrap_or(popm_playcount),
            "musicbrainz_trackid" if mbid.is_none() => mbid = Some(t.value.clone()),
            _ => {}
        }
    }
```

In the `for t in tags` grouping loop, skip promoted keys so they don't enter the text/`TXXX` path:

```rust
    for t in tags {
        if is_promoted_key(&t.key) {
            continue;
        }
        match groups.last_mut() {
            Some(g) if g.0 == t.key => g.1.push(t.value.clone()),
            _ => groups.push((t.key.clone(), vec![t.value.clone()])),
        }
    }
```

**After** the existing text-frame `for (key, values) in &groups { … }` loop and **before** the `for art in arts` loop, emit the rebuilt promoted frames and the opaque binary frames:

```rust
    // Rebuilt promoted frames (POPM from rating/playcount, UFID from MBID).
    if let Some(rating) = popm_rating {
        let data = popm_frame_data(rating, popm_playcount);
        push_frame_header(&mut buf, b"POPM", data.len())?;
        buf.extend_from_slice(&data);
        frames_len += 10 + data.len() as u64;
    }
    if let Some(id) = &mbid {
        let data = ufid_frame_data(MUSICBRAINZ_UFID_OWNER, id.as_bytes());
        push_frame_header(&mut buf, b"UFID", data.len())?;
        buf.extend_from_slice(&data);
        frames_len += 10 + data.len() as u64;
    }

    // Opaque binary frames: header (inline) + streamed body (BinaryTag segment).
    for bt in binary_tags {
        if bt.len == 0 {
            continue; // an empty BinaryTag fails RegionLayout::validate.
        }
        let Ok(id): std::result::Result<[u8; 4], _> = bt.key.as_bytes().try_into() else {
            continue; // defensive: ID3 opaque keys are 4-byte frame ids.
        };
        push_frame_header(&mut buf, &id, bt.len as usize)?;
        segments.push(Segment::Inline(std::mem::take(&mut buf)));
        segments.push(Segment::BinaryTag { payload_id: bt.payload_id, len: bt.len });
        frames_len += 10 + bt.len;
    }
```

> The existing 28-bit syncsafe guards in `push_frame_header` and the final `frames_len` check now bound binary frame lengths too — no new guard needed.

- [ ] **Step 5: Thread the new parameter through callers (keep compiling)**

`mp3::synthesize_layout` — add `binary_tags` and forward it:

```rust
pub fn synthesize_layout(
    audio_offset: u64,
    audio_length: u64,
    tags: &[TagInput],
    binary_tags: &[BinaryTagInput],
    arts: &[ArtInput],
) -> Result<RegionLayout> {
    let (mut segments, _tag_len) = build_id3v2_segments(tags, binary_tags, arts)?;
    segments.push(Segment::BackingAudio { offset: audio_offset, len: audio_length });
    RegionLayout::validated(segments).map_err(|_| FormatError::InvalidLayout)
}
```

`wav::synthesize_layout` — add `binary_tags` and pass through (it calls `mp3::build_id3v2_segments`):

```rust
pub fn synthesize_layout(
    scan: &WavScan,
    audio_offset: u64,
    audio_length: u64,
    tags: &[TagInput],
    binary_tags: &[BinaryTagInput],
    arts: &[ArtInput],
) -> Result<RegionLayout> {
    // ... existing body, but change the build call:
    let (tag_segments, tag_len) = crate::mp3::build_id3v2_segments(tags, binary_tags, arts)?;
    // ... rest unchanged.
}
```

In `musefs-core/src/reader.rs`, update the MP3 and WAV arms to pass an empty slice for now (Task 2.9 replaces it):

```rust
        Format::Mp3 => mp3::synthesize_layout(
            track.audio_offset as u64,
            track.audio_length as u64,
            &inputs,
            &[], // binary_tags — wired in Task 2.9
            &art_inputs,
        )?,
```

```rust
        Format::Wav => {
            let front = read_front(Path::new(&track.backing_path), track.audio_offset as u64)?;
            let scan = wav::read_structure(&front)?;
            wav::synthesize_layout(
                &scan,
                track.audio_offset as u64,
                track.audio_length as u64,
                &inputs,
                &[], // binary_tags — wired in Task 2.9
                &art_inputs,
            )?
        }
```

- [ ] **Step 6: Run test + workspace build**

Run: `cargo test -p musefs-format build_id3v2_segments_rebuilds_popm_ufid && cargo build --workspace`
Expected: PASS / builds (all callers updated).

- [ ] **Step 7: Commit**

```bash
git add musefs-format/src/mp3.rs musefs-format/src/wav.rs musefs-core/src/reader.rs
git commit -m "feat(format): synthesize ID3 POPM/UFID + stream opaque binary frames"
```

---

### Task 2.6: WAV `id3 ` chunk → `read_binary_tags`

Expose a WAV-side `read_binary_tags` that extracts the embedded `id3 ` chunk and runs the MP3 classifier over it (the scan path uses it in Task 2.8).

**Files:**
- Modify: `musefs-format/src/wav.rs`
- Test: `musefs-format/src/wav.rs` (inline)

- [ ] **Step 1: Write the failing test**

Add to `wav.rs` tests (reuse the existing WAV fixture builder that wraps an ID3v2 tag in an `id3 ` chunk; the file already has `find_id3_chunk` + `read_tags` tests to copy structure from):

```rust
#[test]
fn wav_read_binary_tags_extracts_id3_chunk_frames() {
    // Build an ID3v2.4 tag with a PRIV frame, wrap it in an `id3 ` chunk inside a
    // minimal RIFF/WAVE container (mirror the existing wav read_tags test fixture).
    let id3 = build_id3_with_priv(&[5, 6, 7]); // helper used by existing wav tests
    let wav = wrap_in_wav_with_id3_chunk(&id3);  // helper used by existing wav tests

    let (opaque, _promoted) = super::read_binary_tags(&wav);
    let priv_tag = opaque.iter().find(|e| e.key == "PRIV").expect("PRIV preserved");
    assert_eq!(priv_tag.payload, vec![5, 6, 7]);
}
```

> If the WAV test module lacks `build_id3_with_priv`/`wrap_in_wav_with_id3_chunk`, construct the bytes inline the way the existing `wav.rs` `read_tags` test builds its `id3 ` chunk fixture (ID3 tag via the `id3` crate `Encoder`, then an 8-byte `id3 ` + LE-size chunk header inside `RIFF....WAVE`).

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-format wav_read_binary_tags_extracts_id3_chunk`
Expected: FAIL — `wav::read_binary_tags` not found.

- [ ] **Step 3: Implement `wav::read_binary_tags`**

Add to `musefs-format/src/wav.rs` (near `read_tags`, reusing the existing chunk-walk + `find_id3_chunk`):

```rust
/// Extract binary ID3 frames from a WAV's embedded `id3 ` chunk. Classification is
/// identical to MP3 (`mp3::read_binary_tags`); only the chunk extraction differs.
/// Returns `(opaque, promoted)`; empty when there is no `id3 ` chunk.
pub fn read_binary_tags(data: &[u8]) -> (Vec<EmbeddedBinaryTag>, Vec<(String, String)>) {
    let Some(chunks) = list_chunks(data) else {
        return (Vec::new(), Vec::new());
    };
    match find_id3_chunk(data, &chunks) {
        Some(id3_bytes) => crate::mp3::read_binary_tags(id3_bytes),
        None => (Vec::new(), Vec::new()),
    }
}
```

> Use whatever chunk-enumeration the existing `read_tags` uses to feed `find_id3_chunk` (the agent map shows `read_tags` walks chunks then calls `find_id3_chunk(buf, &chunks)`). Match that exact call shape — if the helper is named differently than `list_chunks`, use the real one. Add `use crate::input::EmbeddedBinaryTag;` if not in scope.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p musefs-format wav_read_binary_tags_extracts_id3_chunk`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add musefs-format/src/wav.rs
git commit -m "feat(format): wav::read_binary_tags over the id3 chunk"
```

---

### Task 2.7: `binary_tags_to_inputs` (mapping.rs)

**Files:**
- Modify: `musefs-core/src/mapping.rs`
- Test: `musefs-core/src/mapping.rs` (inline)

- [ ] **Step 1: Write the failing test**

Add to `mapping.rs` tests (mirror the `track_art_to_inputs` test):

```rust
#[test]
fn binary_tags_to_inputs_maps_rows() {
    let db = Db::open_in_memory().unwrap();
    let tid = db.upsert_track(&NewTrack {
        backing_path: "/a.mp3".into(), format: Format::Mp3,
        audio_offset: 0, audio_length: 0, backing_size: 0, backing_mtime: 0,
    }).unwrap();
    db.set_binary_tags(tid, &[musefs_db::BinaryTag { key: "PRIV".into(), payload: vec![1,2,3,4], ordinal: 0 }]).unwrap();

    let inputs = super::binary_tags_to_inputs(&db, tid).unwrap();
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0].key, "PRIV");
    assert_eq!(inputs[0].len, 4);
    // payload_id is the streaming handle (the tags rowid).
    let rowid = db.get_binary_tags(tid).unwrap()[0].rowid;
    assert_eq!(inputs[0].payload_id, rowid);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-core binary_tags_to_inputs_maps_rows`
Expected: FAIL — `binary_tags_to_inputs` not found.

- [ ] **Step 3: Implement**

Add to `musefs-core/src/mapping.rs` (next to `track_art_to_inputs`). Ensure `BinaryTagInput` is imported (`use musefs_format::BinaryTagInput;`):

```rust
/// Map a track's binary tag rows to `BinaryTagInput`s for synthesis. Never reads
/// the payload bytes — only `(rowid, key, byte_len)`; the bytes stream at read
/// time. Ordered by (key, ordinal), matching `get_binary_tags`.
pub(crate) fn binary_tags_to_inputs(db: &Db, track_id: i64) -> Result<Vec<BinaryTagInput>> {
    Ok(db
        .get_binary_tags(track_id)?
        .into_iter()
        .map(|row| BinaryTagInput {
            key: row.key,
            payload_id: row.rowid,
            len: row.byte_len as u64,
        })
        .collect())
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p musefs-core binary_tags_to_inputs_maps_rows`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add musefs-core/src/mapping.rs
git commit -m "feat(core): binary_tags_to_inputs row->input mapping"
```

---

### Task 2.8: Scan ingestion (scan.rs)

Add `Probed.binary_tags`, the size cap, populate the MP3 + WAV probe arms, and write binary rows via `set_binary_tags` **after** the text replace (so the scoped deletes don't race) in both the single-track and bulk paths. Merge `promoted` text pairs into the text tags.

**Files:**
- Modify: `musefs-core/src/scan.rs` (`Probed` ~82, probe arms ~108/140/265/296, `ingest` ~346, `ingest_bulk` ~397, `MAX_ART_BYTES` ~24)
- Test: `musefs-core/src/scan.rs` (inline)

- [ ] **Step 1: Write the failing test**

Add to `scan.rs` tests (mirror the existing scan tests that write a fixture file and assert DB state):

```rust
#[test]
fn scan_ingests_binary_tags_and_promotes() {
    use id3::frame::{Content, Popularimeter, Unknown};
    use id3::{Frame, Tag, TagLike, Version};
    let dir = tempfile::tempdir().unwrap();

    // Build an MP3 with a PRIV (opaque) + POPM (promoted) tag.
    let mut tag = Tag::new();
    tag.add_frame(Popularimeter { user: "u".into(), rating: 128, counter: 3 });
    tag.add_frame(Frame::with_content("PRIV",
        Content::Unknown(Unknown { data: vec![1, 1, 2, 3, 5], version: Version::Id3v24 })));
    let mut bytes = Vec::new();
    id3::Encoder::new().version(Version::Id3v24).encode(&tag, &mut bytes).unwrap();
    bytes.extend_from_slice(&crate::scan::tests::silent_mpeg_frame()); // existing audio helper
    std::fs::write(dir.path().join("a.mp3"), &bytes).unwrap();

    let db = Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path(), false).unwrap();
    let tid = /* track id for a.mp3 */ db_first_track_id(&db);

    // Opaque PRIV survives as a binary row.
    let bin = db.get_binary_tags(tid).unwrap();
    assert!(bin.iter().any(|r| r.key == "PRIV" && r.byte_len == 5));

    // POPM promoted into editable text tags.
    let texts = db.get_tags(tid).unwrap();
    assert!(texts.iter().any(|t| t.key == "rating" && t.value == "128"));
    assert!(texts.iter().any(|t| t.key == "playcount" && t.value == "3"));
}
```

> Use the existing scan-test helpers for the audio frame and track-id lookup (copy from the nearest scan test). The assertions are the point.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-core scan_ingests_binary_tags_and_promotes`
Expected: FAIL — binary rows empty / no `rating` text tag (scan doesn't parse binary frames yet).

- [ ] **Step 3: Add the cap and the `Probed` field**

In `musefs-core/src/scan.rs`, after `MAX_ART_BYTES` (~24):

```rust
/// Per-frame cap for opaque binary tags, mirroring `MAX_ART_BYTES`. Oversize
/// payloads (e.g. a GEOB embedding a multi-MB file) are logged-and-skipped.
const MAX_BINARY_TAG_BYTES: usize = MAX_ART_BYTES;
```

Add a field to `Probed` (~82):

```rust
    binary_tags: Vec<EmbeddedBinaryTag>,
```

Import it: `use musefs_format::EmbeddedBinaryTag;` (alongside `EmbeddedPicture`).

- [ ] **Step 4: Populate the MP3 + WAV probe arms**

Every `Probed { … }` literal must now set `binary_tags`. For the **MP3** and **WAV** arms (probe_full ~104–110 & ~136–142; probe_prefix ~261–266 & ~292–297), compute the binaries and merge promoted text into `tags`. Because the current arms use struct-literal `tags:` initializers, restructure each MP3/WAV arm into a block. MP3 example (`probe_full`):

```rust
        } else if /* mp3 detection */ {
            let (binary_tags, promoted) = mp3::read_binary_tags(bytes);
            let mut tags = mp3::read_tags(bytes);
            tags.extend(promoted);
            Some(Probed {
                format: Format::Mp3,
                audio_offset, // (existing values)
                audio_length,
                tags,
                pictures: mp3::read_pictures(bytes),
                binary_tags,
            })
        }
```

WAV arm (analogous), using `wav::read_binary_tags(bytes)` and `wav::read_tags(bytes)`.

For **all other** `Probed { … }` literals (FLAC, MP4, Ogg, and the `probe_file` MP4 arm, plus the test fixtures around ~846/856), add `binary_tags: Vec::new(),` — Phase 2 does not parse their binary frames (FLAC is Phase 4, MP4 is Phase 3). The compiler will flag every missing field; add the empty default to each.

- [ ] **Step 5: Write binary tags in `ingest` (single-track path)**

In `ingest` (~346), after `db.replace_tags(track_id, &tags)?;` (line 363) and before/after art — order relative to art is irrelevant, but it MUST be after the text replace. Add:

```rust
    let binary_tags: Vec<musefs_db::BinaryTag> = probed
        .binary_tags
        .into_iter()
        .filter(|b| !b.payload.is_empty() && b.payload.len() <= MAX_BINARY_TAG_BYTES)
        .enumerate()
        .map(|(ordinal, b)| musefs_db::BinaryTag { key: b.key, payload: b.payload, ordinal: ordinal as i64 })
        .collect();
    db.set_binary_tags(track_id, &binary_tags)?;
```

> `ordinal` is a gap-free index over accepted (non-empty, non-oversize) frames, mirroring the art ordinal scheme. Note `probed.binary_tags` is moved here; ensure this runs after any other use of `probed` fields (in `ingest`, `probed.tags`/`probed.pictures` are consumed earlier, so the move is fine — confirm by compiling).

- [ ] **Step 6: Write binary tags in `ingest_bulk` (bulk path)**

In `ingest_bulk` (~397), after `bw.replace_tags(track_id, &tags)?;` (line 420). `ingest_bulk` takes `probed: &Probed` (borrowed), so clone:

```rust
    let binary_tags: Vec<musefs_db::BinaryTag> = probed
        .binary_tags
        .iter()
        .filter(|b| !b.payload.is_empty() && b.payload.len() <= MAX_BINARY_TAG_BYTES)
        .enumerate()
        .map(|(ordinal, b)| musefs_db::BinaryTag {
            key: b.key.clone(), payload: b.payload.clone(), ordinal: ordinal as i64,
        })
        .collect();
    bw.set_binary_tags(track_id, &binary_tags)?;
```

> `bw.set_binary_tags` was added in Task 2.2 (Steps 6–9), so it is available here.

- [ ] **Step 7: Run test to verify it passes**

Run: `cargo test -p musefs-core scan_ingests_binary_tags_and_promotes`
Expected: PASS.

- [ ] **Step 8: Full crate test + commit**

Run: `cargo test -p musefs-core -p musefs-db`

```bash
git add musefs-core/src/scan.rs musefs-db/src/bulk.rs
git commit -m "feat(core): ingest ID3 binary tags (opaque + promoted) during scan"
```

---

### Task 2.9: Wire binary tags into the reader resolve arms

Replace the `&[]` placeholders (Task 2.5) with real `BinaryTagInput`s loaded from the DB, so synthesized MP3/WAV files emit their binary frames.

**Files:**
- Modify: `musefs-core/src/reader.rs` (MP3 + WAV arms in `HeaderCache::build`)
- Test: `musefs-core/src/reader.rs` (inline) — end-to-end resolve→read

- [ ] **Step 1: Write the failing test**

Add to `reader.rs` tests: scan an MP3 with a PRIV frame into a DB, resolve it, read the full synthesized file, and assert the PRIV body bytes appear in the output (proving the reader threaded the binary tag through synthesis + served it from the DB).

```rust
#[test]
fn resolve_mp3_emits_binary_tag_in_synthesized_region() {
    use id3::frame::{Content, Unknown};
    use id3::{Frame, Tag, Version};
    let dir = tempfile::tempdir().unwrap();
    let mut tag = Tag::new();
    let needle = [0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02];
    tag.add_frame(Frame::with_content("PRIV",
        Content::Unknown(Unknown { data: needle.to_vec(), version: Version::Id3v24 })));
    let mut bytes = Vec::new();
    id3::Encoder::new().version(Version::Id3v24).encode(&tag, &mut bytes).unwrap();
    bytes.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]); // tiny audio tail
    let path = dir.path().join("a.mp3");
    std::fs::write(&path, &bytes).unwrap();

    let db = Db::open_in_memory().unwrap();
    crate::scan::scan_directory(&db, dir.path(), false).unwrap();
    let tid = /* track id for a.mp3 */;
    let cache = HeaderCache::new(/* args as other reader tests */);
    let resolved = cache.resolve(&db, tid).unwrap();
    let whole = read_at(&resolved, &db, 0, resolved.total_len).unwrap();
    assert!(whole.windows(needle.len()).any(|w| w == needle), "PRIV body not in synthesized file");
}
```

> Copy `HeaderCache::new` construction + track-id lookup from the nearest existing reader test. The assertion (needle present) is the point.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-core resolve_mp3_emits_binary_tag`
Expected: FAIL — the synthesized region has no PRIV (reader passes `&[]`).

- [ ] **Step 3: Load and pass binary tags**

In `musefs-core/src/reader.rs`, in `HeaderCache::build` (where `art_inputs` is computed ~251), add:

```rust
        let binary_tag_inputs = crate::mapping::binary_tags_to_inputs(db, track.id)?;
```

Then in the MP3 arm, replace `&[]` with `&binary_tag_inputs`:

```rust
        Format::Mp3 => mp3::synthesize_layout(
            track.audio_offset as u64,
            track.audio_length as u64,
            &inputs,
            &binary_tag_inputs,
            &art_inputs,
        )?,
```

And the WAV arm likewise (`&binary_tag_inputs` in place of `&[]`).

> Only MP3 + WAV consume `binary_tag_inputs` in Phase 2. FLAC/MP4/Ogg arms ignore it (their binary handling is Phase 3/4); computing it for every track is a single cheap query (rows only, no blobs), and is empty for tracks with no binary frames.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p musefs-core resolve_mp3_emits_binary_tag`
Expected: PASS.

- [ ] **Step 5: Add the binary-tag-chunk serve metric (parity with art)**

The Phase-1 `Segment::BinaryTag` serve arm streams from the DB but emits no metric, unlike `ArtImage` (`metrics::on_art_chunk`). Now that binary tags are actually served, add a counter for observability parity.

In `musefs-core/src/metrics.rs`, mirror the `art_chunks`/`on_art_chunk` plumbing for `binary_tag_chunks` in **both** the enabled-feature block and the no-op block:
- add `static BINARY_TAG_CHUNKS: AtomicU64 = AtomicU64::new(0);` beside `ART_CHUNKS`;
- add `pub fn on_binary_tag_chunk() { BINARY_TAG_CHUNKS.fetch_add(1, Ordering::Relaxed); }` beside `on_art_chunk` (and a no-op `pub fn on_binary_tag_chunk() {}` in the disabled block);
- add `pub binary_tag_chunks: u64,` to the stats struct (both blocks), populate it in the snapshot constructor (`binary_tag_chunks: BINARY_TAG_CHUNKS.load(Ordering::Relaxed),`), and reset it (`BINARY_TAG_CHUNKS.store(0, Ordering::Relaxed);`) wherever `ART_CHUNKS` is reset.

Then in `musefs-core/src/reader.rs`, in `read_segments`'s `Segment::BinaryTag` arm (Phase-1 code), add the metric call after the chunk read:

```rust
                Segment::BinaryTag { payload_id, .. } => {
                    let chunk = db.read_binary_tag_chunk(*payload_id, within, n)?;
                    crate::metrics::on_binary_tag_chunk();
                    out.extend_from_slice(&chunk);
                }
```

If `metrics.rs` has a test asserting a snapshot's fields (e.g. `art_chunks == 1`), extend it to cover `on_binary_tag_chunk()` → `binary_tag_chunks == 1`.

- [ ] **Step 6: Full workspace test + commit**

Run: `cargo test --workspace`

```bash
git add musefs-core/src/reader.rs musefs-core/src/metrics.rs
git commit -m "feat(core): emit ID3 binary tags from MP3/WAV resolve arms + serve metric"
```

---

### Task 2.10: Round-trip proptest + Phase-2 gate

**Files:**
- Modify: `musefs-format/tests/proptest_mp3.rs` (or create if absent)
- Validation only otherwise

- [ ] **Step 1: Write the round-trip property test**

Add to `musefs-format/tests/proptest_mp3.rs` a property over arbitrary opaque frames + POPM/UFID, asserting: opaque payloads (incl. `GEOB`/`SYLT` bodies with **non-UTF-8 encoding bytes** — the case that motivated the raw walker) survive **byte-identical** through `read_binary_tags` → DB-shaped round-trip → `build_id3v2_segments` (materialized) → `read_binary_tags`; and `POPM`/`UFID` promote to the right `rating`/`playcount`/`musicbrainz_trackid`. Assert opaque-payload **byte-identity**, but for the (regenerated) promoted frames assert only **value** survival, not byte-identical framing/ordering. Cover these boundaries explicitly: a `POPM` with `counter == 0` (promotes `rating` only, rebuilds with no counter); the **dual-UFID** case — a MusicBrainz UFID (promoted) + a non-MusicBrainz UFID (opaque) re-synthesize to two distinct `UFID` frames with distinct owners (ID3v2.4 distinct-owner rule). Model the round-trip with an in-memory `Db` (`set_binary_tags`/`get_binary_tags`) so `payload_id`s are real, then materialize the inline segments and resolve `Segment::BinaryTag` lengths against the DB.

> Constrain the proptest generators to what `id3v2_alloc_safe` accepts (no unsync/extended header/frame flags) and to v2.3/v2.4 — that's the domain `read_binary_tags` operates on.

> Use the Phase-1 `resolve_layout(layout, backing, art, binary_tags)` test helper (`musefs-format/tests/common/mod.rs`) to splice a layout into concrete bytes, feeding a `payload_id -> bytes` map.

- [ ] **Step 2: Run the property test**

Run: `cargo test -p musefs-format --features fuzzing proptest_mp3`
Expected: PASS.

- [ ] **Step 3: Query-split correctness regression**

Add a test asserting a `PRIV`-bearing track renders the same template path as the same track without the `PRIV` frame (the binary row must not pollute `tags_to_fields`). Place it in `musefs-core` (it needs the DB + mapping). This guards the Phase-1 `value_blob IS NULL` filter against Phase-2 writes.

Run: `cargo test -p musefs-core`

- [ ] **Step 4: Format + clippy**

Run: `cargo fmt --all --check` (expect clean) and `cargo clippy --all-targets` (expect no warnings).

> Generate any in-diff artifacts via `rtk proxy` if the rtk hook is active — see memory `sp-validation-expectations` (plain `git diff` is token-compacted and breaks tooling).

- [ ] **Step 5: Full workspace test**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 6: In-diff mutation gate**

Per memory `sp-validation-expectations`, mirror `.github/workflows/mutants.yml` — and generate the diff with `rtk proxy git diff` (plain `git diff` is rtk-compacted into an invalid unified diff → silent false pass):

```bash
rtk proxy git diff "$(git merge-base main HEAD)...HEAD" -- '*.rs' > mutants.diff
rtk proxy grep -c '^+++ b/.*\.rs' mutants.diff   # sanity: matches changed .rs count
rtk proxy cargo mutants --in-diff mutants.diff -j"$(nproc)" --exclude 'musefs-latencyfs/**'
```

Expected: 0 missed (add tests if any survive).

- [ ] **Step 7: Open the Phase-2 PR**

```bash
git push -u origin <branch>
gh pr create --base main --title "feat: binary tags Phase 2 — ID3 (MP3 + WAV)" --body-file <body>
```

---

## Deferred to later phases (do NOT do here)

- **Phase 3 (MP4 `----`):** the `build_udta` (prefix,len) → `Vec<Segment>` refactor and multi-`----` streaming. Phase 2 leaves the MP4 probe arm's `binary_tags: Vec::new()`.
- **Phase 4 (FLAC):** `APPLICATION`/`CUESHEET` → DB, `STREAMINFO`/`SEEKTABLE` → `structural_blocks`, resolve re-read elimination. Phase 2 leaves the FLAC probe arm's `binary_tags: Vec::new()`.
- **Phase 5 (test surface):** mutagen interop fixtures + fuzz seeds for binary frames.

## Phase 2 Self-Review

**Spec coverage (§5 ID3 + §6 query-split + open-handle obligation):** opaque passthrough — **byte-exact** via the raw frame walker, incl. GEOB/SYLT/Serato (2.4/2.5/2.8/2.9 ✓), POPM/UFID promotion incl. owner-drop + non-MB-UFID-opaque (2.4/2.5 ✓), WAV `id3 ` chunk parity (2.6/2.8 ✓), `MAX_BINARY_TAG_BYTES` + skip-zero-length (2.8 ✓), bulk delete scoping + `BulkWriter::set_binary_tags` (2.2 ✓), serve-metric parity `on_binary_tag_chunk` (2.9 ✓), query-split regression (2.10 ✓), dual-UFID distinct-owner + POPM counter==0 boundary (2.10 ✓), open-handle gap **fully closed** via gen re-resolve + transactional `content_version` snapshot guard, with a rowid-reuse safety test (2.1 ✓). Production query-split is already complete (Phase 1 filtered all three text read methods; no other production `FROM tags` read exists). MP4/FLAC are out of scope by design.

**Review fixes folded in (spec-plan-reviewer, 2026-06-02):** the `Content::to_unknown()` fidelity blocker → replaced with a byte-exact raw walker (2.4); the open-handle residual-race → closed with the transactional snapshot guard + bounded retry, and the overstated "closes the gap" claim corrected (2.1). Minors: `ArcSwap` guard deref pinned (2.1 Step 6), POPM `counter==0` + dual-UFID boundaries added to the proptest (2.10).

**Type consistency:** `read_binary_tags -> (Vec<EmbeddedBinaryTag>, Vec<(String,String)>)` (opaque, promoted) consumed by scan (2.8); `EmbeddedBinaryTag {key,payload}` → `BinaryTag {key,payload,ordinal}` (DB write) → `BinaryTagRow {rowid,key,byte_len}` (read) → `BinaryTagInput {key,payload_id,len}` (synthesis) → `Segment::BinaryTag {payload_id,len}`. `build_id3v2_segments(tags, binary_tags, arts)` and `{mp3,wav}::synthesize_layout(..., binary_tags, arts)` arg order is consistent across 2.5/2.6/2.9. Promoted keys (`rating`/`playcount`/`musicbrainz_trackid`) are excluded in synthesis (2.5) exactly where they're produced in parse (2.4).

**Placeholder scan:** the only "fill from the nearest existing test" notes are for test *scaffolding* (mount/lookup/track-id/fixture helpers that already exist in each module's test file) — the assertions and production code are fully specified. No production-code placeholders.
