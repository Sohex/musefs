# Validate Tag Keys Before Vorbis Synthesis — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop FLAC/Ogg synthesis from emitting malformed or boundary-shifting Vorbis comments by validating user-defined tag keys, with a universal hygiene floor at the DB and strict Vorbis grammar on the synthesis path.

**Architecture:** Three layers. `musefs-format` owns the strict Vorbis field-name grammar (`is_valid_key`) and `build()` defensively skips out-of-grammar keys (total for any caller, including fuzz). `musefs-core` mirrors a weaker universal floor (`key_passes_floor`) in the scanner and logs dropped keys at the FLAC/Ogg synthesis dispatch. `musefs-db` adds a `CHECK` to `tags.key` (non-empty, no control chars) in place in `MIGRATION_V4` — no new migration, since there are no deployed databases.

**Tech Stack:** Rust (workspace crates `musefs-format`, `musefs-core`, `musefs-db`), SQLite (rusqlite), `log` crate, `cargo`/`cargo +nightly fuzz`.

**Spec:** `docs/superpowers/specs/2026-06-12-validate-tag-keys-design.md`

---

## File Structure

- `musefs-format/src/vorbiscomment.rs` — add `is_valid_key`; change `build` to skip invalid keys; change `parse` to skip empty field names. New unit tests.
- `musefs-format/src/lib.rs` — re-export `is_valid_key` as `is_valid_vorbis_key`.
- `musefs-core/src/scan.rs` — add `key_passes_floor`; filter both tag-collection loops (`ingest` at :575, `ingest_bulk` at :658). New scanner tests.
- `musefs-core/src/reader.rs` — add `warn_invalid_vorbis_keys`; call it in the FLAC branch (:209) and Ogg branch (:285). New end-to-end Ogg regression test.
- `musefs-db/src/schema.rs` — add the `tags.key` `CHECK` to `MIGRATION_V4`'s `CREATE TABLE tags` (:177). New rejection test.
- `musefs-db/src/tags.rs` — new `replace_tags` rollback test.
- `contrib/python-musefs/src/musefs_common/schema.py` + `contrib/picard/musefs/_common/schema.py` — regenerated mirrors (generated, not hand-edited).
- `docs/FLAC.md`, `docs/OGG.md`, `ARCHITECTURE.md` — document the dropped-key behavior and the writer floor.

---

## Task 1: `is_valid_key` predicate + export

**Files:**
- Modify: `musefs-format/src/vorbiscomment.rs`
- Modify: `musefs-format/src/lib.rs:32`
- Test: `musefs-format/src/vorbiscomment.rs` (tests module)

- [ ] **Step 1: Write the failing test**

Add to the `tests` module at the bottom of `musefs-format/src/vorbiscomment.rs` (after `parse_canonicalizes_known_fields_and_preserves_unknown`):

```rust
    #[test]
    fn is_valid_key_enforces_vorbis_grammar() {
        // Legal: one-or-more ASCII 0x20..=0x7D, excluding '=' (0x3D).
        assert!(is_valid_key("title"));
        assert!(is_valid_key("CUSTOM_THING"));
        assert!(is_valid_key("}")); // 0x7D, upper bound
        assert!(is_valid_key(" ")); // 0x20, lower bound
        // Illegal.
        assert!(!is_valid_key("")); // empty
        assert!(!is_valid_key("a=b")); // contains '='
        assert!(!is_valid_key("a\u{1f}b")); // control char 0x1F
        assert!(!is_valid_key("a\u{7f}b")); // DEL 0x7F
        assert!(!is_valid_key("a~b")); // 0x7E, just past upper bound
        assert!(!is_valid_key("género")); // non-ASCII high bytes
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-format is_valid_key_enforces_vorbis_grammar`
Expected: FAIL to compile — `cannot find function 'is_valid_key'`.

- [ ] **Step 3: Write minimal implementation**

Insert this function in `musefs-format/src/vorbiscomment.rs` immediately above `pub(crate) fn build` (after the `VENDOR` const, around line 9):

```rust
/// True if `key` is a legal VorbisComment field name: one or more characters in
/// ASCII 0x20..=0x7D, excluding 0x3D (`=`). This is the Vorbis spec grammar and
/// matches what mutagen/TagLib enforce when writing. Non-ASCII, control chars,
/// `=`, and the empty string are all rejected.
pub fn is_valid_key(key: &str) -> bool {
    !key.is_empty() && key.bytes().all(|b| (0x20..=0x7D).contains(&b) && b != b'=')
}
```

- [ ] **Step 4: Export it from the crate (ungated)**

`musefs-core` calls this in normal (non-fuzzing) builds, so the re-export must
**not** be under the `#[cfg(feature = "fuzzing")]` attribute. The bottom block of
`lib.rs` (lines 29–32: `build_id3v2_segments`, `parse_vorbis_comment`) is all
fuzzing-gated — do not add it there. Add it to the ungated public-API block,
directly below `pub use probe::Extent;` (line 24):

```rust
pub use vorbiscomment::is_valid_key as is_valid_vorbis_key;
```

Verify it is ungated: `cargo build -p musefs-format` (no `--features fuzzing`) must
compile, and later `cargo build -p musefs-core` must see the symbol.

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p musefs-format is_valid_key_enforces_vorbis_grammar`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add musefs-format/src/vorbiscomment.rs musefs-format/src/lib.rs
git commit -m "feat(format): add is_valid_key Vorbis field-name predicate (#300)"
```

---

## Task 2: `build()` defensively skips invalid keys

**Files:**
- Modify: `musefs-format/src/vorbiscomment.rs:13-38` (the `build` function)
- Test: `musefs-format/src/vorbiscomment.rs` (tests module)

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `musefs-format/src/vorbiscomment.rs`:

```rust
    #[test]
    fn build_skips_keys_outside_vorbis_grammar() {
        // The issue's case: an `a=b` key would otherwise synthesize `A=B=c` and
        // shift the boundary on re-parse. Empty keys are also dropped. Valid keys
        // keep their order, and the comment count reflects only survivors.
        let tags = vec![
            TagInput::new("artist", "Alice"),
            TagInput::new("a=b", "c"),
            TagInput::new("", "x"),
            TagInput::new("title", "Song"),
        ];
        let body = build(&tags).unwrap();
        let parsed = parse(&body).unwrap();
        assert_eq!(
            parsed,
            vec![
                ("artist".to_string(), "Alice".to_string()),
                ("title".to_string(), "Song".to_string()),
            ]
        );
    }

    #[test]
    fn build_is_total_over_arbitrary_keys() {
        // build() must remain total: it has no assert guarding key validity, so the
        // fuzz harness can feed it arbitrary keys. It must never panic and must emit
        // only valid comments.
        let tags = vec![
            TagInput::new("a=b=c", "v"),
            TagInput::new("\u{0}\u{1}", "v"),
            TagInput::new("ok", "v"),
        ];
        let body = build(&tags).unwrap();
        let parsed = parse(&body).unwrap();
        assert_eq!(parsed, vec![("ok".to_string(), "v".to_string())]);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p musefs-format build_skips_keys_outside_vorbis_grammar build_is_total_over_arbitrary_keys`
Expected: FAIL — `build` currently writes `A=B=c` and `=x`, so the parsed vec contains extra/garbled entries.

- [ ] **Step 3: Rewrite `build` to filter**

Replace the body of `build` (`musefs-format/src/vorbiscomment.rs:13-38`) with:

```rust
pub(crate) fn build(tags: &[TagInput]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    out.extend_from_slice(
        &u32::try_from(VENDOR.len())
            .map_err(|_| FormatError::TooLarge)?
            .to_le_bytes(),
    );
    out.extend_from_slice(VENDOR.as_bytes());
    // Skip keys outside the Vorbis field-name grammar (e.g. an `=` key would shift
    // the key/value boundary on re-parse). build() is the single enforcement point,
    // so it stays total for any caller — including the fuzz harness.
    let valid: Vec<&TagInput> = tags.iter().filter(|t| is_valid_key(&t.key)).collect();
    out.extend_from_slice(
        &u32::try_from(valid.len())
            .map_err(|_| FormatError::TooLarge)?
            .to_le_bytes(),
    );
    for t in valid {
        let field = crate::tagmap::key_to_vorbis(&t.key)
            .map_or_else(|| t.key.to_ascii_uppercase(), str::to_string);
        let comment = format!("{field}={}", t.value);
        out.extend_from_slice(
            &u32::try_from(comment.len())
                .map_err(|_| FormatError::TooLarge)?
                .to_le_bytes(),
        );
        out.extend_from_slice(comment.as_bytes());
    }
    Ok(out)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p musefs-format vorbiscomment`
Expected: PASS — including the pre-existing `build_then_parse_round_trips` and `parse_canonicalizes_known_fields_and_preserves_unknown` (their keys are all valid).

- [ ] **Step 5: Commit**

```bash
git add musefs-format/src/vorbiscomment.rs
git commit -m "fix(format): build skips out-of-grammar Vorbis keys (#300)"
```

---

## Task 3: `parse()` skips empty field names

**Files:**
- Modify: `musefs-format/src/vorbiscomment.rs:70-74` (inside `parse`)
- Test: `musefs-format/src/vorbiscomment.rs` (tests module)

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `musefs-format/src/vorbiscomment.rs`:

```rust
    fn body_with_one_comment(comment: &str) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&u32::try_from(VENDOR.len()).unwrap().to_le_bytes());
        body.extend_from_slice(VENDOR.as_bytes());
        body.extend_from_slice(&1u32.to_le_bytes()); // count = 1
        body.extend_from_slice(&u32::try_from(comment.len()).unwrap().to_le_bytes());
        body.extend_from_slice(comment.as_bytes());
        body
    }

    #[test]
    fn parse_skips_empty_field_name() {
        // A "=value" comment has no field name; it must not become an empty-key tag.
        assert!(parse(&body_with_one_comment("=value")).unwrap().is_empty());
    }

    #[test]
    fn parse_splits_on_first_equals() {
        // The exact boundary the issue is about: A=B=c -> key "A", value "B=c".
        let parsed = parse(&body_with_one_comment("A=B=c")).unwrap();
        assert_eq!(parsed, vec![("A".to_string(), "B=c".to_string())]);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p musefs-format parse_skips_empty_field_name parse_splits_on_first_equals`
Expected: `parse_skips_empty_field_name` FAILS (currently yields `("", "value")`); `parse_splits_on_first_equals` already PASSES (first-`=` split is existing behavior — keep it green as a regression guard).

- [ ] **Step 3: Guard the empty field name**

In `parse` (`musefs-format/src/vorbiscomment.rs`), replace the block:

```rust
        if let Some((field, value)) = comment.split_once('=') {
            let key = crate::tagmap::vorbis_to_key(field)
                .map_or_else(|| field.to_string(), str::to_string);
            out.push((key, value.to_string()));
        }
```

with:

```rust
        if let Some((field, value)) = comment.split_once('=') {
            // A comment whose field name is empty (e.g. "=value") is malformed and
            // must not become an empty-key tag. The first-`=` split is preserved.
            if !field.is_empty() {
                let key = crate::tagmap::vorbis_to_key(field)
                    .map_or_else(|| field.to_string(), str::to_string);
                out.push((key, value.to_string()));
            }
        }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p musefs-format vorbiscomment`
Expected: PASS (all vorbiscomment tests).

- [ ] **Step 5: Commit**

```bash
git add musefs-format/src/vorbiscomment.rs
git commit -m "fix(format): parse skips empty Vorbis field names (#300)"
```

---

## Task 4: Scanner universal floor (both loops)

**Files:**
- Modify: `musefs-core/src/scan.rs:575` (`ingest` loop) and `:658` (`ingest_bulk` loop)
- Test: `musefs-core/src/scan.rs` (tests module)

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `musefs-core/src/scan.rs` (near the existing `ingest_filters_empty_and_oversize_binary_tags` test, ~line 1845):

```rust
    fn probed_with_text_tags(tags: &[(&str, &str)]) -> Probed {
        Probed {
            format: musefs_db::Format::Mp3,
            audio_offset: 0,
            audio_length: 0,
            tags: tags
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            pictures: Vec::new(),
            binary_tags: Vec::new(),
            structural_blocks: Vec::new(),
        }
    }

    #[test]
    fn ingest_skips_empty_and_control_char_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.mp3");
        std::fs::write(&path, b"x").unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let db = Db::open_in_memory().unwrap();

        ingest(
            &db,
            &path.to_string_lossy(),
            &meta,
            probed_with_text_tags(&[
                ("artist", "Alice"),
                ("", "dropped"),       // empty key
                ("a\u{7}b", "dropped"), // control char
                ("a=b", "kept"),       // '=' is NOT a floor violation
            ]),
        )
        .unwrap();

        let tid = db.list_tracks().unwrap()[0].id;
        let keys: Vec<String> = db.get_tags(tid).unwrap().into_iter().map(|t| t.key).collect();
        // get_tags is ORDER BY key, ordinal: '=' (0x3D) sorts before 'a' (0x61).
        assert_eq!(keys, vec!["a=b".to_string(), "artist".to_string()]);
    }

    #[test]
    fn ingest_bulk_skips_empty_and_control_char_keys() {
        let db = Db::open_in_memory().unwrap();
        {
            let mut bw = db.bulk_writer().unwrap();
            ingest_bulk(
                &mut bw,
                "/a.mp3",
                BackingStamp {
                    size: 1,
                    mtime_ns: 0,
                    ctime_ns: 0,
                },
                probed_with_text_tags(&[
                    ("artist", "Alice"),
                    ("", "dropped"),
                    ("a\u{7}b", "dropped"),
                    ("a=b", "kept"),
                ]),
            )
            .unwrap();
            bw.commit().unwrap();
        }
        let tid = db.list_tracks().unwrap()[0].id;
        let keys: Vec<String> = db.get_tags(tid).unwrap().into_iter().map(|t| t.key).collect();
        assert_eq!(keys, vec!["a=b".to_string(), "artist".to_string()]);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p musefs-core ingest_skips_empty_and_control_char_keys ingest_bulk_skips_empty_and_control_char_keys`
Expected: FAIL — empty/control keys are currently ingested, so `keys` has 4 entries, not 2.

- [ ] **Step 3: Add the `key_passes_floor` predicate**

Add this free function in `musefs-core/src/scan.rs` immediately above `fn ingest(` (around line 560):

```rust
/// The universal `tags.key` floor, mirrored from the DB `CHECK` exactly: a key
/// must be non-empty and contain no byte below 0x20 (the control chars the DB
/// rejects via its GLOB range; NUL also fails here, the DB's documented blind
/// spot). DEL (0x7F) and high/non-ASCII bytes are accepted, matching the DB.
/// Distinct from the strict Vorbis `is_valid_key` (which also bars `=`, 0x7E,
/// 0x7F, and non-ASCII) — applying that here would wrongly drop legal MP3/M4A
/// custom keys containing `=`/`:`/space.
fn key_passes_floor(key: &str) -> bool {
    !key.is_empty() && key.bytes().all(|b| b >= 0x20)
}
```

- [ ] **Step 4: Filter both loops**

In `ingest` (`scan.rs:575`), change:

```rust
    for (key, value) in probed.tags {
        let ord = ordinals.entry(key.clone()).or_insert(0);
        tags.push(Tag::new(&key, &value, *ord));
        *ord += 1;
    }
```

to:

```rust
    for (key, value) in probed.tags {
        if !key_passes_floor(&key) {
            continue;
        }
        let ord = ordinals.entry(key.clone()).or_insert(0);
        tags.push(Tag::new(&key, &value, *ord));
        *ord += 1;
    }
```

In `ingest_bulk` (`scan.rs:658`), change:

```rust
    for (key, value) in &probed.tags {
        let ord = ordinals.entry(key.clone()).or_insert(0);
        tags.push(Tag::new(key, value, *ord));
        *ord += 1;
    }
```

to:

```rust
    for (key, value) in &probed.tags {
        if !key_passes_floor(key) {
            continue;
        }
        let ord = ordinals.entry(key.clone()).or_insert(0);
        tags.push(Tag::new(key, value, *ord));
        *ord += 1;
    }
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p musefs-core ingest_skips_empty_and_control_char_keys ingest_bulk_skips_empty_and_control_char_keys`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add musefs-core/src/scan.rs
git commit -m "fix(scan): drop empty/control-char tag keys before insert (#300)"
```

---

## Task 5: DB `CHECK` on `tags.key` + Python mirror regen

**Files:**
- Modify: `musefs-db/src/schema.rs:177-189` (MIGRATION_V4 `CREATE TABLE tags`)
- Modify (generated): `contrib/python-musefs/src/musefs_common/schema.py`, `contrib/picard/musefs/_common/schema.py`
- Test: `musefs-db/src/schema.rs` (tests module), `musefs-db/src/tags.rs` (tests module)

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `musefs-db/src/schema.rs` (near `v4_tracks_rejects_unknown_format`, which uses the existing `fresh` and `rejected` helpers):

```rust
    #[test]
    fn v4_tags_rejects_empty_and_control_char_keys() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        conn.execute(
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, updated_at) \
             VALUES ('/x','flac',0,0,0,0,0)",
            [],
        )
        .unwrap();
        rejected(
            &conn,
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1,'','v',0)",
        );
        rejected(
            &conn,
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1,char(7),'v',0)",
        );
        // '=' is NOT a DB-floor violation — only Vorbis synthesis bars it.
        conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1,'a=b','c',0)",
            [],
        )
        .unwrap();
    }
```

Add to the `tests` module in `musefs-db/src/tags.rs` (near the existing `replace_tags` tests, which use `open_mem`, `new_track`, `Tag`):

```rust
    #[test]
    fn replace_tags_rejects_floor_violating_keys() {
        let db = open_mem();
        let t = db.upsert_track(&new_track("/a.flac")).unwrap();
        // A row violating the floor aborts the whole row-by-row transactional insert.
        assert!(db.replace_tags(t, &[Tag::new("", "v", 0)]).is_err());
        assert!(db.replace_tags(t, &[Tag::new("\u{7}", "v", 0)]).is_err());
        // '=' passes the DB floor (only the Vorbis path bars it).
        db.replace_tags(t, &[Tag::new("a=b", "c", 0)]).unwrap();
        let got = db.get_tags(t).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].key, "a=b");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p musefs-db v4_tags_rejects_empty_and_control_char_keys replace_tags_rejects_floor_violating_keys`
Expected: FAIL — empty/control-char inserts currently succeed, so the `is_err()` assertions fail.

- [ ] **Step 3: Add the `CHECK` to MIGRATION_V4**

In `musefs-db/src/schema.rs`, inside `MIGRATION_V4`'s `CREATE TABLE tags` (the occurrence at line 177, **not** the V1 copy at line 17 or the test fixture at line 1569), change:

```sql
    CHECK (length(key) <= 256),
```

to:

```sql
    CHECK (length(key) <= 256),
    -- A field name must be non-empty and free of ASCII control chars (0x01-0x1F).
    -- length()/GLOB stop at an embedded NUL, so a NUL inside a key evades this; the
    -- Rust is_valid_key filter on the Vorbis path is the backstop for that case.
    CHECK (length(key) >= 1
           AND key NOT GLOB '*[' || char(1) || '-' || char(31) || ']*'),
```

- [ ] **Step 4: Regenerate and re-vendor the Python schema mirror**

Run both steps (the `schema_py_fixture_is_fresh` test fails the full-workspace pre-commit gate until the canonical mirror is fresh):

```bash
MUSEFS_REGEN_SCHEMA_PY=1 cargo test -p musefs-db schema_py
python contrib/python-musefs/vendor_to_picard.py
```

Verify both mirror files changed and the new `CHECK` appears in the **V4 rendering** (the second `CREATE TABLE tags` in each file):

```bash
git status --short contrib/
grep -n "key NOT GLOB" contrib/python-musefs/src/musefs_common/schema.py contrib/picard/musefs/_common/schema.py
```

Expected: both files modified; `grep` shows the new CHECK in each.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p musefs-db`
Expected: PASS — including the new rejection tests, `schema_py_fixture_is_fresh`, and the V3→V4 migration data-preservation test (`v4_rebuild_preserves_fk_children`, whose seeded key `'artist'` passes the floor).

- [ ] **Step 6: Commit**

```bash
git add musefs-db/src/schema.rs musefs-db/src/tags.rs \
  contrib/python-musefs/src/musefs_common/schema.py \
  contrib/picard/musefs/_common/schema.py
git commit -m "feat(db): reject empty/control-char tag keys via CHECK (#300)"
```

---

## Task 6: Core logging + end-to-end Ogg regression

**Files:**
- Modify: `musefs-core/src/reader.rs` (add helper; call in FLAC branch :209 and Ogg branch :285)
- Test: `musefs-core/src/reader.rs` (tests module)

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `musefs-core/src/reader.rs` (next to `resolves_and_reads_opus_with_identical_audio`, reusing its `build_opus_file` helper and imports):

```rust
    #[test]
    fn synthesis_drops_invalid_vorbis_key_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("track.opus");
        let (audio_offset, audio_length) = build_opus_file(&path);

        let db = Db::open_in_memory().unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let track_id = db
            .upsert_track(&NewTrack {
                backing_path: path.to_string_lossy().into_owned(),
                format: Format::Opus,
                audio_offset,
                audio_length,
                backing_size: meta.len(),
                backing_mtime_ns: meta.mtime() * 1_000_000_000 + meta.mtime_nsec(),
                backing_ctime_ns: meta.ctime() * 1_000_000_000 + meta.ctime_nsec(),
            })
            .unwrap();
        // `a=b` passes the DB floor but is not a valid Vorbis field name. Without the
        // fix it would synthesize `A=B=c` and re-parse as key "A", value "B=c".
        db.replace_tags(
            track_id,
            &[
                Tag::new("artist", "Alice", 0),
                Tag::new("a=b", "c", 0),
                Tag::new("title", "Song", 0),
            ],
        )
        .unwrap();

        let cache = HeaderCache::new(Mode::Synthesis);
        let resolved = cache.resolve(&db, track_id).unwrap();
        let out = read_at(&resolved, &db, 0, resolved.total_len).unwrap();

        let tags = musefs_format::ogg::read_tags(&out).unwrap();
        assert!(tags.iter().any(|(k, v)| k == "artist" && v == "Alice"));
        assert!(tags.iter().any(|(k, v)| k == "title" && v == "Song"));
        assert!(
            !tags.iter().any(|(k, _)| k == "A" || k.contains('=')),
            "the a=b key must be dropped, not synthesized as A=B=c: {tags:?}"
        );
    }
```

- [ ] **Step 2: Run the test to verify it passes (behavior already correct from Task 2)**

Run: `cargo test -p musefs-core synthesis_drops_invalid_vorbis_key_end_to_end`
Expected: PASS — Task 2 made `build` drop the key; this test is the end-to-end Ogg guard the issue asks for. (If it fails, Task 2 regressed.)

- [ ] **Step 3: Add the logging helper**

Add this free function in `musefs-core/src/reader.rs` near the other read helpers (e.g. just above `pub fn read_at`):

```rust
/// Warn about user-defined tag keys that the Vorbis synthesis path will drop,
/// so a silently-dropped key is observable. Runs during layout resolution (a
/// cache miss), not per `read_at`, so a malformed key warns once per resolution.
fn warn_invalid_vorbis_keys(track_id: i64, inputs: &[musefs_format::TagInput]) {
    for t in inputs {
        if !musefs_format::is_valid_vorbis_key(&t.key) {
            log::warn!(
                "track {track_id}: dropping tag key {:?} from Vorbis synthesis \
                 (not a valid field name)",
                t.key
            );
        }
    }
}
```

- [ ] **Step 4: Call it in the FLAC and Ogg branches**

In `reader.rs`, in the `Format::Flac` arm, immediately before the `flac::synthesize_layout(` call (line ~209), add:

```rust
                        warn_invalid_vorbis_keys(track.id, &inputs);
```

In the `Format::Opus | Format::Vorbis | Format::OggFlac` arm, immediately before the `musefs_format::ogg::synthesize_layout(` call (line ~285), add:

```rust
                        warn_invalid_vorbis_keys(track.id, &inputs);
```

- [ ] **Step 5: Run tests and lints to verify**

Run: `cargo test -p musefs-core && cargo clippy -p musefs-core --all-targets`
Expected: PASS, no clippy warnings.

- [ ] **Step 6: Commit**

```bash
git add musefs-core/src/reader.rs
git commit -m "feat(core): log tag keys dropped from Vorbis synthesis (#300)"
```

---

## Task 7: Documentation, fuzz check, and final verification

**Files:**
- Modify: `docs/FLAC.md`, `docs/OGG.md`, `ARCHITECTURE.md`

- [ ] **Step 1: Document the dropped-key behavior (FLAC)**

In `docs/FLAC.md`, in the "What round-trips" section (around the user-defined-fields bullet, line ~11-13), add a sentence:

```markdown
User-defined keys that are not legal Vorbis field names (empty, containing `=`,
control characters, or non-ASCII bytes — i.e. outside ASCII `0x20`–`0x7D` minus
`=`) are dropped on synthesis and logged; they cannot round-trip by name.
```

- [ ] **Step 2: Document the dropped-key behavior (Ogg)**

In `docs/OGG.md`, in its "What round-trips" section (around line 29-31), add the same note:

```markdown
User-defined keys outside the Vorbis field-name grammar (empty, containing `=`,
control characters, or non-ASCII — outside ASCII `0x20`–`0x7D` minus `=`) are
dropped on synthesis and logged.
```

- [ ] **Step 3: Document the writer floor (ARCHITECTURE)**

In `ARCHITECTURE.md`, in the external-writer contract section, add a bullet describing the `tags.key` floor:

```markdown
- `tags.key` must be non-empty and contain no ASCII control characters (a DB
  `CHECK` enforces this; violating writes are rejected). Additionally, only keys
  within the Vorbis field-name grammar (ASCII `0x20`–`0x7D`, excluding `=`)
  survive FLAC/Ogg synthesis — others are dropped and logged. MP3/M4A custom
  keys may use the wider set (e.g. `=`, `:`, spaces, non-ASCII).
```

- [ ] **Step 4: Commit the docs**

```bash
git add docs/FLAC.md docs/OGG.md ARCHITECTURE.md
git commit -m "docs: document tag-key validation and writer floor (#300)"
```

- [ ] **Step 5: Final full-workspace verification**

Run each and confirm success:

```bash
cargo fmt --all --check
cargo clippy --all-targets
cargo test
cargo +nightly fuzz build
```

Expected: `fmt` clean; `clippy` no warnings; full test suite green; fuzz targets (`vorbiscomment`, `flac`, `ogg`) build — they feed `arb_tags` (arbitrary keys) into the changed `build`/`parse`, which are now total, so no signature break and no panic path.

- [ ] **Step 6: Metrics-feature check (CI parity)**

Run: `cargo test -p musefs-core --features metrics`
Expected: PASS — the local default `cargo test` skips this feature; the change does not touch getattr/read counts, but run it to match the CI `check` job before pushing.

---

## Notes for the implementer

- **Commit order matters for the green pre-commit gate.** Each task's commit must leave the full workspace test suite green (the pre-commit hook runs it). The order here (format → scanner floor → DB CHECK → core logging) ensures the scanner never feeds the new `CHECK` a violating key, and the DB-CHECK commit includes the regenerated Python mirror so `schema_py_fixture_is_fresh` stays green.
- **Do not add a new migration.** The `CHECK` goes into `MIGRATION_V4` in place. `user_version` stays at 5, so the Picard `test_conftest_sanity.py` version assertion is unaffected.
- **Two predicates, deliberately different.** `is_valid_vorbis_key` (format, strict: `0x20`–`0x7D`, no `=`) governs synthesis; `key_passes_floor` (core scanner, weaker: non-empty, no control char) mirrors the DB `CHECK`. Do not unify them — the scanner must keep legal MP3/M4A keys containing `=`/`:`/space.
- The `fuzz/` crate is outside the workspace; `cargo +nightly fuzz build` is the only thing that compiles it.
