# Binary Tag Handling Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make binary tag data (ID3 `PRIV`/`POPM`/`UFID`/`GEOB`/`SYLT`, MP4 `----`, FLAC `APPLICATION`/`CUESHEET`) survive scan → DB → re-synthesis instead of being silently dropped, closing #66.

**Architecture:** Store binary tag payloads in a new nullable `tags.value_blob` column and stream them back into synthesized files via a new length-only `Segment::BinaryTag` (mirroring `ArtImage`/`read_art_chunk`, honoring the no-blobs-in-memory invariant). A narrow set of frames with universal text equivalents (`POPM`→`rating`/`playcount`, MusicBrainz `UFID`→`musicbrainz_trackid`) are promoted to editable text tags; everything else is opaque passthrough. FLAC additionally moves `APPLICATION`/`CUESHEET` into the editable DB and `STREAMINFO`/`SEEKTABLE` into a read-only `structural_blocks` table, eliminating the per-resolve FLAC front re-read.

**Tech Stack:** Rust workspace (`musefs-db` SQLite/rusqlite, `musefs-format` byte-surgery, `musefs-core` orchestration, `musefs-fuse`), the `id3` crate, proptest, cargo-fuzz, mutagen interop.

**Spec:** `docs/superpowers/specs/2026-06-01-binary-tags-design.md`

---

## Phasing overview

The spec defines five sequential phases. **Phase 1 (this plan, in full below) is the shared foundation** — schema, the streamed segment, the DB layer, the input type, and the query-split filter. It compiles green and is fully testable with **no format behavior change**. Phases 2–5 each build on Phase 1's merged types and ship as their own PR (matching the project's SP-per-PR convention); their task roadmaps are at the end of this document and will be expanded into their own plan files once Phase 1 lands.

- **Phase 1 — Foundation** (this plan): schema V2, `Segment::BinaryTag` + serving, DB binary-tag + structural-blocks layer, `BinaryTagInput`, `value_blob IS NULL` query filter.
- **Phase 2 — ID3 (MP3 + WAV):** opaque passthrough + `POPM`/`UFID` promotion. The primary gap.
- **Phase 3 — MP4:** `----` opaque passthrough (requires the `build_udta` segment-list refactor).
- **Phase 4 — FLAC:** `APPLICATION`/`CUESHEET` → DB; `STREAMINFO`/`SEEKTABLE` → structural store; resolve re-read elimination + legacy backfill/fallback.
- **Phase 5 — Test surface:** interop (mutagen), fuzz seeds, proptests, query-split + migration tests.

---

## Phase 1 — File structure

| File | Change | Responsibility |
| --- | --- | --- |
| `musefs-db/src/schema.rs` | Modify | Add `MIGRATION_V2` (column + table), append to `MIGRATIONS`. |
| `musefs-db/src/models.rs` | Modify | Add `BinaryTag`, `BinaryTagRow`, `StructuralBlock` structs; export. |
| `musefs-db/src/lib.rs` | Modify | Re-export the new models. |
| `musefs-db/src/tags.rs` | Modify | Filter text queries (`value_blob IS NULL`); add `set_binary_tags`, `get_binary_tags`, `read_binary_tag_chunk`. |
| `musefs-db/src/structural.rs` | Create | `set_structural_blocks`, `get_structural_blocks`. |
| `musefs-format/src/input.rs` | Modify | Add `BinaryTagInput`. |
| `musefs-format/src/lib.rs` | Modify | Re-export `BinaryTagInput`. |
| `musefs-format/src/layout.rs` | Modify | Add `Segment::BinaryTag { payload_id, len }` + `len()` arm. |
| `musefs-core/src/reader.rs` | Modify | Serve `Segment::BinaryTag` in `read_segments` (and any other exhaustive `Segment` match the compiler flags). |

All Phase-1 changes are additive: no parser or synthesis path emits a `BinaryTag` yet, so existing behavior is unchanged.

---

## Phase 1 — Tasks

### Task 1.1: Schema migration V2

**Files:**
- Modify: `musefs-db/src/schema.rs`
- Test: `musefs-db/src/schema.rs` (inline `#[cfg(test)]`)

- [ ] **Step 1: Write the failing test**

Add to a `#[cfg(test)] mod migration_v2_tests` at the end of `musefs-db/src/schema.rs`:

```rust
#[cfg(test)]
mod migration_v2_tests {
    use rusqlite::Connection;

    #[test]
    fn v2_adds_value_blob_and_structural_blocks_and_is_idempotent() {
        let mut conn = Connection::open_in_memory().unwrap();
        super::migrate(&mut conn).unwrap();
        // user_version reflects the number of migrations applied.
        let uv: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(uv, 2);

        // value_blob exists on tags and defaults to NULL.
        conn.execute(
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime, updated_at) \
             VALUES ('/a.flac','flac',0,1,1,0,0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1,'artist','A',0)",
            [],
        )
        .unwrap();
        let blob_is_null: bool = conn
            .query_row(
                "SELECT value_blob IS NULL FROM tags WHERE track_id=1 AND key='artist'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(blob_is_null);

        // structural_blocks table accepts a row.
        conn.execute(
            "INSERT INTO structural_blocks (track_id, kind, ordinal, body) \
             VALUES (1,'STREAMINFO',0,X'00')",
            [],
        )
        .unwrap();

        // Re-running migrate is a no-op (idempotent).
        super::migrate(&mut conn).unwrap();
        let uv2: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(uv2, 2);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-db v2_adds_value_blob -- --nocapture`
Expected: FAIL — `user_version` is `1`, and `value_blob`/`structural_blocks` do not exist (`no such column: value_blob`).

- [ ] **Step 3: Add the V2 migration**

In `musefs-db/src/schema.rs`, after the `MIGRATION_V1` constant, add:

```rust
const MIGRATION_V2: &str = r"
-- Binary tag payloads live alongside text tags. A row is binary iff
-- value_blob IS NOT NULL; binary rows store '' in value.
ALTER TABLE tags ADD COLUMN value_blob BLOB;

-- Read-only, derived-from-file structural metadata (FLAC STREAMINFO/SEEKTABLE).
-- NOT part of the editable `tags` contract: external tools never touch it.
CREATE TABLE structural_blocks (
    track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    kind     TEXT NOT NULL,
    ordinal  INTEGER NOT NULL DEFAULT 0,
    body     BLOB NOT NULL,
    PRIMARY KEY (track_id, kind, ordinal)
);
";
```

Then change the `MIGRATIONS` constant:

```rust
const MIGRATIONS: &[&str] = &[MIGRATION_V1, MIGRATION_V2];
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p musefs-db v2_adds_value_blob -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Run the full db crate to confirm no regressions**

Run: `cargo test -p musefs-db`
Expected: PASS (existing migration/idempotency tests still green).

- [ ] **Step 6: Commit**

```bash
git add musefs-db/src/schema.rs
git commit -m "feat(db): migration V2 — tags.value_blob + structural_blocks"
```

---

### Task 1.2: Binary-tag DB models

**Files:**
- Modify: `musefs-db/src/models.rs`
- Modify: `musefs-db/src/lib.rs:11` (re-export)

- [ ] **Step 1: Write the failing test**

Add to `musefs-db/src/models.rs` a `#[cfg(test)] mod tests` (create the module if absent — `models.rs` currently has no test module):

```rust
#[cfg(test)]
mod tests {
#[test]
fn binary_tag_constructs() {
    let bt = super::BinaryTag {
        key: "PRIV".to_string(),
        payload: vec![1, 2, 3],
        ordinal: 0,
    };
    assert_eq!(bt.payload.len(), 3);
    let row = super::BinaryTagRow {
        rowid: 7,
        key: "PRIV".to_string(),
        byte_len: 3,
    };
    assert_eq!(row.rowid, 7);
    let sb = super::StructuralBlock {
        kind: "STREAMINFO".to_string(),
        ordinal: 0,
        body: vec![0u8; 34],
    };
    assert_eq!(sb.body.len(), 34);
}
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-db binary_tag_constructs`
Expected: FAIL — `BinaryTag`/`BinaryTagRow`/`StructuralBlock` not found.

- [ ] **Step 3: Add the structs**

Append to `musefs-db/src/models.rs`:

```rust
/// A binary tag payload to write (e.g. an opaque ID3 `PRIV` frame body). `key` is
/// the format-private identifier (ID3 frame id, `APPLICATION`/`CUESHEET`,
/// `----:<mean>:<name>`); `payload` is the post-header frame/block body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryTag {
    pub key: String,
    pub payload: Vec<u8>,
    pub ordinal: i64,
}

/// A binary tag row read back for synthesis: the streaming handle (`rowid`), the
/// key, and the payload length — the bytes themselves stream at read time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryTagRow {
    pub rowid: i64,
    pub key: String,
    pub byte_len: i64,
}

/// A read-only structural metadata block derived from the backing file
/// (FLAC `STREAMINFO`/`SEEKTABLE`). Stored outside the editable `tags` contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuralBlock {
    pub kind: String,
    pub ordinal: i64,
    pub body: Vec<u8>,
}
```

- [ ] **Step 4: Re-export from the crate root**

In `musefs-db/src/lib.rs`, extend the models re-export (currently `pub use models::{Art, ArtMeta, Format, NewArt, NewTrack, Tag, Track, TrackArt};`):

```rust
pub use models::{
    Art, ArtMeta, BinaryTag, BinaryTagRow, Format, NewArt, NewTrack, StructuralBlock, Tag, Track,
    TrackArt,
};
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p musefs-db binary_tag_constructs`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add musefs-db/src/models.rs musefs-db/src/lib.rs
git commit -m "feat(db): BinaryTag/BinaryTagRow/StructuralBlock models"
```

---

### Task 1.3: Filter text queries to exclude binary rows

**Files:**
- Modify: `musefs-db/src/tags.rs` (`get_tags:23`, `tags_for_tracks:49`, `tags_grouped:77`)
- Test: `musefs-db/src/tags.rs` (inline)

After migration, a binary row stores `value=''`; the text-reading queries must not return it (it would pollute template field rendering and text synthesis).

- [ ] **Step 1: Write the failing test**

Add to `musefs-db/src/tags.rs` `#[cfg(test)] mod tags_for_tracks_tests`:

```rust
#[test]
fn text_queries_exclude_binary_rows() {
    let db = open_mem();
    let a = db.upsert_track(&new_track("/a.flac")).unwrap();
    db.replace_tags(a, &[Tag::new("artist", "Alice", 0)]).unwrap();
    // Insert a binary row directly (set_binary_tags lands in Task 1.4).
    db.conn
        .execute(
            "INSERT INTO tags (track_id, key, value, value_blob, ordinal) \
             VALUES (?1, 'PRIV', '', X'DEADBEEF', 0)",
            rusqlite::params![a],
        )
        .unwrap();

    // get_tags returns only the text row.
    let got = db.get_tags(a).unwrap();
    assert_eq!(got, vec![Tag::new("artist", "Alice", 0)]);
    // tags_grouped likewise.
    let grouped = db.tags_grouped().unwrap();
    assert_eq!(grouped[&a], vec![Tag::new("artist", "Alice", 0)]);
    // tags_for_tracks likewise.
    let for_tracks = db.tags_for_tracks(&[a]).unwrap();
    assert_eq!(for_tracks[&a], vec![Tag::new("artist", "Alice", 0)]);
}
```

Note: this test uses `db.conn`. If `conn` is private to the crate, the test is in-crate (`tags.rs`) so it has access; confirm by compiling.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-db text_queries_exclude_binary_rows`
Expected: FAIL — each query returns the `PRIV` row too (2 rows, with an empty-string value).

- [ ] **Step 3: Add the filter to all three text queries**

In `musefs-db/src/tags.rs`:

`get_tags` SELECT becomes:
```rust
"SELECT key, value, ordinal FROM tags \
 WHERE track_id = ?1 AND value_blob IS NULL ORDER BY key, ordinal",
```

`tags_for_tracks` SELECT becomes:
```rust
let sql = format!(
    "SELECT track_id, key, value, ordinal FROM tags \
     WHERE track_id IN ({placeholders}) AND value_blob IS NULL \
     ORDER BY track_id, key, ordinal"
);
```

`tags_grouped` SELECT becomes:
```rust
"SELECT track_id, key, value, ordinal FROM tags \
 WHERE value_blob IS NULL ORDER BY track_id, key, ordinal",
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p musefs-db text_queries_exclude_binary_rows`
Expected: PASS.

- [ ] **Step 5: Run the full db crate**

Run: `cargo test -p musefs-db`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add musefs-db/src/tags.rs
git commit -m "feat(db): exclude binary rows from text tag queries"
```

---

### Task 1.4: Binary-tag write/read DB methods

**Files:**
- Modify: `musefs-db/src/tags.rs`
- Test: `musefs-db/src/tags.rs` (inline)

`replace_tags` is scoped to **text** rows (delete `value_blob IS NULL`) and `set_binary_tags` to **binary** rows (delete `value_blob IS NOT NULL`), so the two are independent and order-free.

- [ ] **Step 1: Write the failing test**

Add to `musefs-db/src/tags.rs` tests:

```rust
#[test]
fn binary_tags_round_trip_and_are_independent_of_text() {
    let db = open_mem();
    let a = db.upsert_track(&new_track("/a.flac")).unwrap();
    db.replace_tags(a, &[Tag::new("artist", "Alice", 0)]).unwrap();
    // Insert in an order that does NOT match (key, ordinal) sort order, so the
    // ORDER BY is actually exercised: GEOB is inserted last but must sort first.
    db.set_binary_tags(
        a,
        &[
            crate::BinaryTag { key: "PRIV".into(), payload: vec![1, 2, 3], ordinal: 0 },
            crate::BinaryTag { key: "PRIV".into(), payload: vec![9, 9], ordinal: 1 },
            crate::BinaryTag { key: "GEOB".into(), payload: vec![7], ordinal: 0 },
        ],
    )
    .unwrap();

    // Text query is unaffected.
    assert_eq!(db.get_tags(a).unwrap(), vec![Tag::new("artist", "Alice", 0)]);

    // Binary rows come back ordered by (key, ordinal): GEOB(0), PRIV(0), PRIV(1).
    let rows = db.get_binary_tags(a).unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].key, "GEOB");
    assert_eq!(rows[0].byte_len, 1);
    assert_eq!(rows[1].key, "PRIV");
    assert_eq!(rows[1].byte_len, 3);
    assert_eq!(rows[2].byte_len, 2);

    // Streamed chunk reads return exact bytes from the PRIV(0) payload.
    let full = db.read_binary_tag_chunk(rows[1].rowid, 0, 3).unwrap();
    assert_eq!(full, vec![1, 2, 3]);
    let mid = db.read_binary_tag_chunk(rows[1].rowid, 1, 2).unwrap();
    assert_eq!(mid, vec![2, 3]);

    // Re-setting binary tags replaces only binary rows; text survives.
    db.set_binary_tags(a, &[]).unwrap();
    assert!(db.get_binary_tags(a).unwrap().is_empty());
    assert_eq!(db.get_tags(a).unwrap(), vec![Tag::new("artist", "Alice", 0)]);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-db binary_tags_round_trip`
Expected: FAIL — `set_binary_tags`/`get_binary_tags`/`read_binary_tag_chunk` not found.

- [ ] **Step 3: Scope `replace_tags` to text rows + add the binary methods**

In `musefs-db/src/tags.rs`, change `replace_tags`'s DELETE to text-only:

```rust
tx.execute(
    "DELETE FROM tags WHERE track_id = ?1 AND value_blob IS NULL",
    params![track_id],
)?;
```

Then add these methods inside `impl Db`:

```rust
/// Replace the track's binary tag rows (value_blob IS NOT NULL); text rows
/// (managed by `replace_tags`) are untouched. Binary rows store '' in `value`.
pub fn set_binary_tags(&self, track_id: i64, tags: &[BinaryTag]) -> Result<()> {
    let tx = self.conn.unchecked_transaction()?;
    tx.execute(
        "DELETE FROM tags WHERE track_id = ?1 AND value_blob IS NOT NULL",
        params![track_id],
    )?;
    {
        let mut stmt = tx.prepare(
            "INSERT INTO tags (track_id, key, value, value_blob, ordinal) \
             VALUES (?1, ?2, '', ?3, ?4)",
        )?;
        for t in tags {
            stmt.execute(params![track_id, t.key, t.payload, t.ordinal])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Binary tag rows for a track: streaming handle (rowid), key, and payload
/// length. Ordered by (key, ordinal) to match `get_binary_tags`/synthesis order.
pub fn get_binary_tags(&self, track_id: i64) -> Result<Vec<BinaryTagRow>> {
    let mut stmt = self.conn.prepare(
        "SELECT rowid, key, length(value_blob) FROM tags \
         WHERE track_id = ?1 AND value_blob IS NOT NULL ORDER BY key, ordinal",
    )?;
    let rows = stmt.query_map(params![track_id], |r| {
        Ok(BinaryTagRow {
            rowid: r.get(0)?,
            key: r.get(1)?,
            byte_len: r.get(2)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Stream `len` bytes of a binary tag payload at `offset` via incremental blob
/// I/O — payloads are never fully materialized. A short read means the row
/// changed underneath the resolved layout; `read_at_exact` surfaces it as an
/// error rather than zero-filling. (`payload_id` is the `tags` rowid; see the
/// spec's "payload_id validity invariant".)
pub fn read_binary_tag_chunk(&self, payload_id: i64, offset: u64, len: usize) -> Result<Vec<u8>> {
    let blob = self.conn.blob_open("main", "tags", "value_blob", payload_id, true)?;
    let mut buf = vec![0u8; len];
    blob.read_at_exact(&mut buf, offset as usize)?;
    Ok(buf)
}
```

Add `BinaryTag, BinaryTagRow` to the `use crate::models::...` import at the top of `tags.rs` (currently `use crate::models::Tag;`):

```rust
use crate::models::{BinaryTag, BinaryTagRow, Tag};
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p musefs-db binary_tags_round_trip`
Expected: PASS.

- [ ] **Step 5: Run the full db crate**

Run: `cargo test -p musefs-db`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add musefs-db/src/tags.rs
git commit -m "feat(db): set_binary_tags/get_binary_tags/read_binary_tag_chunk"
```

---

### Task 1.5: Structural-blocks DB layer

**Files:**
- Create: `musefs-db/src/structural.rs`
- Modify: `musefs-db/src/lib.rs` (add `mod structural;`)
- Test: `musefs-db/src/structural.rs` (inline)

- [ ] **Step 1: Create the module with a failing test**

Create `musefs-db/src/structural.rs`:

```rust
use crate::models::StructuralBlock;
use crate::{Db, Result};
use rusqlite::params;

impl Db {
    /// Replace the track's structural blocks (FLAC STREAMINFO/SEEKTABLE).
    pub fn set_structural_blocks(&self, track_id: i64, blocks: &[StructuralBlock]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM structural_blocks WHERE track_id = ?1",
            params![track_id],
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO structural_blocks (track_id, kind, ordinal, body) \
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for b in blocks {
                stmt.execute(params![track_id, b.kind, b.ordinal, b.body])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Structural blocks for a track, ordered by (kind, ordinal). Empty when a
    /// FLAC track has not been (re)scanned under V2 — callers fall back to a
    /// front read in that case.
    pub fn get_structural_blocks(&self, track_id: i64) -> Result<Vec<StructuralBlock>> {
        let mut stmt = self.conn.prepare(
            "SELECT kind, ordinal, body FROM structural_blocks \
             WHERE track_id = ?1 ORDER BY kind, ordinal",
        )?;
        let rows = stmt.query_map(params![track_id], |r| {
            Ok(StructuralBlock {
                kind: r.get(0)?,
                ordinal: r.get(1)?,
                body: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

#[cfg(test)]
mod tests {
    use crate::{Db, Format, NewTrack, StructuralBlock};

    #[test]
    fn structural_blocks_round_trip_and_replace() {
        let db = Db::open_in_memory().unwrap();
        let id = db
            .upsert_track(&NewTrack {
                backing_path: "/a.flac".into(),
                format: Format::Flac,
                audio_offset: 0,
                audio_length: 1,
                backing_size: 1,
                backing_mtime: 0,
            })
            .unwrap();
        db.set_structural_blocks(
            id,
            &[
                StructuralBlock { kind: "STREAMINFO".into(), ordinal: 0, body: vec![1, 2] },
                StructuralBlock { kind: "SEEKTABLE".into(), ordinal: 0, body: vec![3] },
            ],
        )
        .unwrap();
        let got = db.get_structural_blocks(id).unwrap();
        assert_eq!(got.len(), 2);
        // ordered by kind: SEEKTABLE before STREAMINFO
        assert_eq!(got[0].kind, "SEEKTABLE");
        assert_eq!(got[1].body, vec![1, 2]);

        db.set_structural_blocks(id, &[]).unwrap();
        assert!(db.get_structural_blocks(id).unwrap().is_empty());
    }
}
```

- [ ] **Step 2: Register the module**

In `musefs-db/src/lib.rs`, add alongside the other `mod` declarations (e.g. near `mod tags;`):

```rust
mod structural;
```

- [ ] **Step 3: Run test to verify it fails, then passes**

Run: `cargo test -p musefs-db structural_blocks_round_trip`
Expected: before Step 2 — compile error (`structural.rs` is not in the module tree, so its `impl Db` methods don't exist); after Step 2 (`mod structural;` added) — PASS.

- [ ] **Step 4: Run the full db crate**

Run: `cargo test -p musefs-db`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add musefs-db/src/structural.rs musefs-db/src/lib.rs
git commit -m "feat(db): structural_blocks read/write layer"
```

---

### Task 1.6: `BinaryTagInput` in the format layer

**Files:**
- Modify: `musefs-format/src/input.rs`
- Modify: `musefs-format/src/lib.rs:17` (re-export)
- Test: `musefs-format/src/input.rs` (inline)

- [ ] **Step 1: Write the failing test**

Add to `musefs-format/src/input.rs` (create a `#[cfg(test)] mod tests` if absent):

```rust
#[cfg(test)]
mod tests {
    use super::BinaryTagInput;

    #[test]
    fn binary_tag_input_constructs() {
        let b = BinaryTagInput { key: "PRIV".into(), payload_id: 7, len: 3 };
        assert_eq!(b.payload_id, 7);
        assert_eq!(b.len, 3);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-format binary_tag_input_constructs`
Expected: FAIL — `BinaryTagInput` not found.

- [ ] **Step 3: Add the struct**

Append to `musefs-format/src/input.rs`:

```rust
/// A reference to one opaque binary tag payload to synthesize. Like `ArtInput`,
/// the bytes are NOT held here — only `len` and `payload_id`, an opaque handle the
/// caller (musefs-core) maps to the `tags` rowid it streams from. `key` is the
/// format-private identifier the synthesis path decodes (ID3 frame id,
/// `APPLICATION`/`CUESHEET`, `----:<mean>:<name>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryTagInput {
    pub key: String,
    pub payload_id: i64,
    pub len: u64,
}
```

- [ ] **Step 4: Re-export**

In `musefs-format/src/lib.rs`, extend the input re-export (currently `pub use input::{ArtInput, EmbeddedPicture, TagInput};`):

```rust
pub use input::{ArtInput, BinaryTagInput, EmbeddedPicture, TagInput};
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p musefs-format binary_tag_input_constructs`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add musefs-format/src/input.rs musefs-format/src/lib.rs
git commit -m "feat(format): BinaryTagInput synthesis input"
```

---

### Task 1.7: `Segment::BinaryTag` variant + length

**Files:**
- Modify: `musefs-format/src/layout.rs`
- Test: `musefs-format/src/layout.rs` (inline)

- [ ] **Step 1: Write the failing test**

Add to `musefs-format/src/layout.rs` tests (or create the test module):

```rust
#[test]
fn binary_tag_segment_len_and_validate() {
    let seg = Segment::BinaryTag { payload_id: 5, len: 12 };
    assert_eq!(seg.len(), 12);
    // Non-empty binary tag passes validation.
    RegionLayout::validated(vec![
        seg,
        Segment::BackingAudio { offset: 0, len: 1 },
    ])
    .unwrap();
    // Empty binary tag is rejected (EmptySegment), like empty art.
    let err = RegionLayout::validated(vec![Segment::BinaryTag { payload_id: 5, len: 0 }]);
    assert!(matches!(err, Err(LayoutError::EmptySegment)));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-format binary_tag_segment_len`
Expected: FAIL — `Segment::BinaryTag` not found.

- [ ] **Step 3: Add the variant and its `len()` arm**

In `musefs-format/src/layout.rs`, add a variant to `enum Segment` (after `OggArtSlice`):

```rust
    /// An opaque binary tag payload (e.g. an ID3 `PRIV` frame body or a FLAC
    /// `APPLICATION` block body) streamed from the DB at read time; only the
    /// length is known here. `payload_id` is the caller's `tags` rowid handle.
    BinaryTag { payload_id: i64, len: u64 },
```

In `impl Segment`'s `len()`, add `BinaryTag` to the combined `*len` arm:

```rust
            Segment::ArtImage { len, .. }
            | Segment::BackingAudio { len, .. }
            | Segment::OggAudio { len, .. }
            | Segment::OggArtSlice { len, .. }
            | Segment::BinaryTag { len, .. } => *len,
```

`validate()` needs no change: a zero-length `BinaryTag` falls through the
`!matches!(seg, BackingAudio | OggAudio)` guard and correctly errors `EmptySegment`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p musefs-format binary_tag_segment_len`
Expected: PASS.

- [ ] **Step 5: Build the format crate**

Run: `cargo build -p musefs-format`
Expected: PASS. The `Segment::len()` arm above is the only non-test exhaustive `Segment` match in this crate (the `ogg/page.rs` `=> Some(*len)` match is test-only and has a `_` wildcard, so it is not flagged). The one *serving* match that needs an arm lives in `musefs-core` and is handled in Task 1.8.

- [ ] **Step 6: Commit**

```bash
git add musefs-format/src/layout.rs musefs-format/src/ogg/page.rs
git commit -m "feat(format): Segment::BinaryTag streamed segment variant"
```

---

### Task 1.8: Serve `Segment::BinaryTag` in the reader

**Files:**
- Modify: `musefs-core/src/reader.rs` (`read_segments`, and any other exhaustive `Segment` match)
- Test: `musefs-core/src/reader.rs` (inline)

- [ ] **Step 1: Write the failing test**

Add a test to `musefs-core/src/reader.rs` `#[cfg(test)]` that builds a `ResolvedFile` whose layout is a single `BinaryTag` and asserts `read_at` streams the DB payload. Mirror the existing art-segment reader tests in this file for `ResolvedFile` construction; the key assertion:

```rust
#[test]
fn read_at_serves_binary_tag_segment() {
    use musefs_db::{BinaryTag, Db, Format, NewTrack};
    let db = Db::open_in_memory().unwrap();
    let id = db
        .upsert_track(&NewTrack {
            backing_path: "/x.mp3".into(),
            format: Format::Mp3,
            audio_offset: 0,
            audio_length: 0,
            backing_size: 0,
            backing_mtime: 0,
        })
        .unwrap();
    db.set_binary_tags(id, &[BinaryTag { key: "PRIV".into(), payload: vec![10, 20, 30, 40], ordinal: 0 }])
        .unwrap();
    let rowid = db.get_binary_tags(id).unwrap()[0].rowid;

    let resolved = ResolvedFile {
        layout: RegionLayout::new(vec![Segment::BinaryTag { payload_id: rowid, len: 4 }]),
        total_len: 4,
        content_version: 0,
        backing_path: PathBuf::from("/x.mp3"),
        backing_size: 0,
        backing_mtime_secs: 0,
        mtime_secs: 0,
        last_page: std::sync::Mutex::new(None),
        cache_bytes: 0,
    };
    // No BackingAudio segment, so read_at opens no file.
    let got = read_at(&resolved, &db, 1, 2).unwrap();
    assert_eq!(got, vec![20, 30]);
}
```

(If `ResolvedFile`'s fields differ at implementation time, copy them from an existing reader test in the same file — the point is the `BinaryTag` layout + the `read_at` slice assertion.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-core read_at_serves_binary_tag_segment`
Expected: FAIL — non-exhaustive match in `read_segments` (`Segment::BinaryTag` not covered) → compile error.

- [ ] **Step 3: Add the serving arm**

In `musefs-core/src/reader.rs`, inside `read_segments`'s `match seg { … }` (after the `ArtImage` arm), add:

```rust
                Segment::BinaryTag { payload_id, .. } => {
                    let chunk = db.read_binary_tag_chunk(*payload_id, within, n)?;
                    out.extend_from_slice(&chunk);
                }
```

This is the **only** non-test exhaustive `Segment` match in the workspace: `read_at_with_file` (`reader.rs:484`) is a one-line delegate to `read_segments`, not a second match site. The `cache_bytes` computation (`reader.rs:337`) uses `_ => 0`, so `BinaryTag` contributes 0 automatically — no change.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p musefs-core read_at_serves_binary_tag_segment`
Expected: PASS.

- [ ] **Step 5: Build + test the workspace**

Run: `cargo build --workspace && cargo test -p musefs-core`
Expected: PASS (no other exhaustive `Segment` match left uncovered).

- [ ] **Step 6: Commit**

```bash
git add musefs-core/src/reader.rs
git commit -m "feat(core): serve Segment::BinaryTag by streaming from the DB"
```

---

### Task 1.9: Phase-1 gate — fmt, clippy, full test, mutation

**Files:** none (validation only)

- [ ] **Step 1: Format check**

Run: `cargo fmt --all --check`
Expected: clean exit (CI fmt gate; see memory `musefs-prepush-checks`).

- [ ] **Step 2: Clippy**

Run: `cargo clippy --all-targets`
Expected: no warnings.

- [ ] **Step 3: Full workspace test**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 4: In-diff mutation gate**

Run the CI in-diff mutation gate locally over the Phase-1 diff (per memory `sp-validation-expectations`), e.g.:
`cargo mutants --in-diff <(git diff main...HEAD) -j$(nproc)`
Expected: no surviving mutants in the changed lines (add tests if any survive).

- [ ] **Step 5: Open the Phase-1 PR**

Phase 1 is a self-contained, behavior-neutral foundation. Open it as its own PR before starting Phase 2.

---

## Phases 2–5 — Roadmap

Each phase below builds on Phase 1's merged types and ships as its own PR. These outlines fix the files, interfaces, and test obligations; each will be expanded into a full task-by-task plan file (`docs/superpowers/plans/2026-06-01-binary-tags-phaseN-*.md`) when its predecessor lands, so its code references the real merged signatures.

### Phase 2 — ID3 (MP3 + WAV): opaque passthrough + POPM/UFID promotion

**Files:** `musefs-format/src/mp3.rs` (new `read_binary_tags`, `popm_frame_data`, `ufid_frame_data`; extend `build_id3v2_segments` signature with `binary_tags: &[BinaryTagInput]`; promotion logic), `musefs-format/src/mp3.rs` `synthesize_layout` (thread `binary_tags`), `musefs-format/src/wav.rs` (extract the `id3 ` chunk, run `read_binary_tags`; thread `binary_tags` through `wav::synthesize_layout`), `musefs-core/src/scan.rs` (ingest binary frames via `db.set_binary_tags`, `MAX_BINARY_TAG_BYTES` cap mirroring `MAX_ART_BYTES`, skip zero-length), `musefs-core/src/mapping.rs` (new `binary_tags_to_inputs(rows) -> Vec<BinaryTagInput>`), `musefs-core/src/reader.rs` (MP3/WAV resolve arms load `db.get_binary_tags` → `BinaryTagInput` and pass them in).

**Key interface decisions to lock in the Phase-2 plan:**
- **Scope `BulkWriter::replace_tags` (`musefs-db/src/bulk.rs:69`) to text rows.** The scan fast path uses the bulk writer, whose `replace_tags` currently does `DELETE FROM tags WHERE track_id = ?1` (ALL rows). Task 1.4 scoped only `Db::replace_tags`; the bulk path must get the same `AND value_blob IS NULL` clause, **or** the binary write must be ordered after the bulk text-replace, or it will wipe the binary rows `set_binary_tags` just wrote. Lock the scan write ordering (text first, then `set_binary_tags`) and the scoped bulk delete in the Phase-2 plan. (Harmless in Phase 1: no binary rows exist yet.)
- `EmbeddedBinaryTag { key: String, payload: Vec<u8> }` parser return type (lands in `musefs-format/src/input.rs`).
- `build_id3v2_segments(tags, binary_tags, arts)` emits, after text/`TXXX`/`APIC`: (a) rebuilt `POPM`/`UFID` frames from the promoted text keys (`rating`/`playcount`/`musicbrainz_trackid`), then (b) one `push_frame_header(frame_id, len)` + `Segment::BinaryTag` per `BinaryTagInput`. The existing 28-bit syncsafe guard already bounds these.
- **POPM parse/build (from raw frame body, version-stable):** body = `<owner>\0<rating:u8>[<counter:big-endian>]`. Parse → `rating` = rating byte (0–255, raw), `playcount` = counter (default 0). Build → owner = `""`, rating byte, 4-byte counter. **Owner dropped by design** (spec §5).
- **UFID parse/build:** body = `<owner>\0<identifier bytes>`. Owner `http://musicbrainz.org` → promote to `musicbrainz_trackid` (identifier as UTF-8); any other owner → opaque `(UFID, full-body)` passthrough.
- **Research item:** confirm how the `id3` crate exposes `POPM`/`UFID`/unknown-frame **raw bytes** at parse time (typed `Content` accessors vs `Content::Unknown`). If typed accessors don't expose the raw body, parse from the frame's encoded bytes. Resolve this before writing Phase-2 code.
- **Close the open-handle `payload_id` reuse gap (blocking for Phase 2).** The `content_version` cache-invalidation that protects `payload_id` (spec's "payload_id validity invariant") does **not** reach open FUSE handles: `Handle` holds its own `Arc<ResolvedFile>` that outlives `poll_refresh`, and `Musefs::read`'s `fh != 0` fast path (`facade.rs:610`) serves it via `read_at_with_file` without re-resolving. Once a handle can hold a `Segment::BinaryTag`, a re-tag (`set_binary_tags` = `DELETE`+`INSERT`) can have SQLite reuse the old `tags` rowid for a different, same-length payload, which the handle then serves silently as snapshot bytes (`read_binary_tag_chunk` errors only on a *short* read). Lock the fix in the Phase-2 plan — **preferred:** content-address binary payloads (stable, never-deleted id, like `art`) so an open handle's `payload_id` can't be reused out from under it; a read-time `content_version` staleness check is the fallback but breaks the open-fd snapshot semantics `Inline`/`BackingAudio` depend on. Add a regression test: open a handle on a binary-tag-bearing track, re-tag it (forcing rowid churn), and assert the handle still serves the original payload (or a clean error) — never another row's bytes. (Harmless in Phase 1: no synthesis path emits `BinaryTag`, so no handle can hold one. Flagged by CodeRabbit on PR #74.)

**Tests:** round-trip proptest `proptest_mp3.rs` (opaque payload byte-identical; `POPM`/`UFID` promotion correct); the dual-`UFID` owner-uniqueness case (spec §Testing); WAV `id3 ` chunk binary-frame extraction; query-split correctness (a `PRIV`-bearing track renders the same tree path).

### Phase 3 — MP4 `----` opaque passthrough

**Files:** `musefs-format/src/mp4.rs` (`build_udta` contract change + `----` parse), `musefs-core/src/scan.rs` (ingest), `musefs-core/src/reader.rs` (M4a resolve arm threads `binary_tags`).

**Key change (the riskiest in the feature):** `build_udta` currently returns `(Vec<u8> prefix, u64 art_len)` assuming a single streamed (art) segment last in `ilst`. It must change to return **an ordered `Vec<Segment>`** (like `build_id3v2_segments`) so N `----` payloads stream as interleaved `Segment::BinaryTag` inside `udta`, with every enclosing box length (`----`/`mean`/`name` wrapper → `ilst` → `meta` → `udta`) summing all binary payload lengths at the correct depth. `new_moov_size`/`delta` then sum binary lengths into `udta_total` before computing `delta`; `stco`/`co64` patching is unchanged. Parse: `----:<mean>:<name>` key, payload = the `data` atom value bytes; multi-`----` (N>1) in scope. **No inline materialization** (invariant).

**Tests:** round-trip proptest `proptest_mp4.rs` (multi-`----` byte-identical); exact box-size assertions extending `build_udta_art_box_sizes_are_exact`; fuzz seed with a `----` atom.

### Phase 4 — FLAC: DB-backed blocks + structural store + resolve re-read elimination

**Files:** `musefs-format/src/flac.rs` (`parse_blocks` splits block types into structural vs DB-backed; `synthesize_layout` signature: `FlacScan` → structural-block bodies + `tags`/`binary_tags`/`arts`; canonical block order; is-last rule), `musefs-core/src/scan.rs` (write `STREAMINFO`/`SEEKTABLE` → `db.set_structural_blocks`, `APPLICATION`/`CUESHEET` → `db.set_binary_tags`; `--revalidate` backfills legacy FLAC tracks), `musefs-core/src/reader.rs` (FLAC arm: build from `db.get_structural_blocks` + `db.get_binary_tags`; **front-read fallback** when `structural_blocks` empty; `audio_offset`/`audio_length` from the `tracks` row).

**Key decisions (spec §1/§5/§7):** canonical block order `fLaC + STREAMINFO + SEEKTABLE + VORBIS_COMMENT + APPLICATION/CUESHEET + PICTURE + audio`; is-last set on the header preceding the final body (streamed or inline); total count = structural + 1 + binary blocks + nonempty pictures. `read_front` survives (WAV/Ogg). Legacy migration: empty `structural_blocks` → front-read fallback until first rescan; `revalidate` backfills.

**Tests:** FLAC round-trip proptest asserting payload fidelity **not** block order; legacy-FLAC migration test (V1 DB → migrate → resolve via fallback → revalidate → no front read); `APPLICATION`/`CUESHEET` 24-bit guard.

### Phase 5 — Test surface expansion

**Files:** `musefs-core/tests/interop_emit.rs` + `tests/interop` (mutagen fixtures: `POPM`/`UFID`/`PRIV`/`GEOB` ID3 + a `----` atom; assert survival), `fuzz/fuzz_targets/{mp3,mp4}.rs` + `generate_seeds` (binary-frame seeds), `musefs-format/tests/proptest_*.rs` (consolidate round-trip properties), the byte-identical-invariant proptest (fixtures carry binary frames).

**Tests:** Property 5 interop is the real-world proof; the byte-identical audio invariant must hold with binary tags present.

---

## Phase 1 Self-Review

**Spec coverage (Phase 1 scope only):** schema V2 (Task 1.1 ✓), `value_blob`/`structural_blocks` (1.1 ✓), models (1.2 ✓), query-split filter on all three text queries (1.3 ✓ — `get_tags`/`tags_for_tracks`/`tags_grouped`), binary write/read + `read_binary_tag_chunk` (1.4 ✓), structural store (1.5 ✓), `BinaryTagInput` (1.6 ✓), `Segment::BinaryTag` + len + validate (1.7 ✓), reader serving + exhaustive-match coverage + cache_bytes note (1.8 ✓). Phases 2–5 are roadmapped, not yet task-detailed by design.

**Placeholder scan:** none — every Phase-1 step has concrete code/commands.

**Type consistency:** `BinaryTag { key, payload, ordinal }` (db model, write side) vs `BinaryTagRow { rowid, key, byte_len }` (db read side) vs `BinaryTagInput { key, payload_id, len }` (format input) — three distinct types at three layers, used consistently: scan writes `BinaryTag`; `get_binary_tags` returns `BinaryTagRow`; `mapping` (Phase 2) converts `BinaryTagRow` → `BinaryTagInput`; `Segment::BinaryTag { payload_id, len }` carries the rowid. `read_binary_tag_chunk(payload_id, …)` matches the segment field name. `set_binary_tags`/`get_binary_tags` names are consistent across Tasks 1.4 and 1.8.
