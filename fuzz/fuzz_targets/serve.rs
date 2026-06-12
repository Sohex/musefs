#![no_main]
use arbitrary::Unstructured;
use libfuzzer_sys::fuzz_target;
use musefs_core::{HeaderCache, Mode, read_at_with_file};
use musefs_db::{BinaryTag, Db, Format, NewArt, NewTrack, Tag, TrackArt};
use musefs_fuzz::{MAX_INPUT, arb_arts, arb_tags};
use std::io::Write;
use std::os::unix::fs::MetadataExt;

/// Build a one-track in-memory DB over `backing` written to a temp file, and
/// return (tempdir, db, track_id). Returns None on any setup error.
fn setup(
    backing: &[u8],
    format: Format,
    audio_offset: u64,
    audio_length: u64,
) -> Option<(tempfile::TempDir, Db, i64)> {
    let dir = tempfile::tempdir().ok()?;
    let path = dir.path().join("backing");
    std::fs::File::create(&path).ok()?.write_all(backing).ok()?;
    let meta = std::fs::metadata(&path).ok()?;
    let db = Db::open_in_memory().ok()?;
    let id = db
        .upsert_track(&NewTrack {
            backing_path: path.to_string_lossy().into_owned(),
            format,
            audio_offset,
            audio_length,
            backing_size: meta.len(),
            backing_mtime_ns: meta.mtime() * 1_000_000_000 + meta.mtime_nsec(),
            backing_ctime_ns: meta.ctime() * 1_000_000_000 + meta.ctime_nsec(),
        })
        .ok()?;
    db.replace_tags(id, &[Tag::new("title", "T", 0)]).ok()?;
    Some((dir, db, id))
}

/// Plant one hostile mutation via the fuzzing-only raw accessor. The production
/// read path must reject the resulting state with `Err`, never UB. Each fuzz
/// iteration uses a fresh in-memory DB (`setup`), so dropping a trigger / leaving
/// a pragma toggled is scoped to that iteration's connection.
///
/// Variants whose target row ALWAYS exists (0, 1, 5 — the single `tracks` row)
/// assert `n == 1` so a future schema rename cannot silently turn the mutation
/// into a swallowed no-op (this exact trap hid a `backing_mtime` -> `backing_mtime_ns`
/// rename during planning). Variants 2/3/4 are genuinely conditional (they need an
/// art row or binary-tag row that the earlier stages may not have created), so
/// they stay best-effort (`let _ =`).
fn apply_hostile(db: &Db, id: i64, variant: u8, val: i64) {
    db.with_raw_conn(|conn| match variant {
        // 0: negative/oversized integer geometry (resolve rejects at the bounds check).
        0 => {
            conn.execute_batch("PRAGMA ignore_check_constraints = ON")
                .unwrap();
            let n = conn
                .execute(
                    "UPDATE tracks SET audio_offset = ?1, audio_length = ?1 WHERE id = ?2",
                    rusqlite::params![val, id],
                )
                .unwrap();
            assert_eq!(n, 1, "variant 0 must mutate the tracks row");
            conn.execute_batch("PRAGMA ignore_check_constraints = OFF")
                .unwrap();
        }
        // 1: invalid format discriminant (model deserialization must Err, not panic).
        1 => {
            conn.execute_batch("PRAGMA ignore_check_constraints = ON")
                .unwrap();
            let n = conn
                .execute(
                    "UPDATE tracks SET format = 'bogus' WHERE id = ?1",
                    rusqlite::params![id],
                )
                .unwrap();
            assert_eq!(n, 1, "variant 1 must mutate the tracks row");
            conn.execute_batch("PRAGMA ignore_check_constraints = OFF")
                .unwrap();
        }
        // 2: orphaned track_art (art_id -> no art row) under FK off. No-op when no
        // art row was attached this iteration.
        2 => {
            conn.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
            let _ = conn.execute(
                "UPDATE track_art SET art_id = 999999 WHERE track_id = ?1",
                rusqlite::params![id],
            );
            conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();
        }
        // 3: oversized art mime. `art` rows are immutable via the
        // `art_reject_content_update` trigger, and `ignore_check_constraints` does
        // NOT disable triggers, so the trigger must be dropped first (the length(mime)
        // CHECK still needs the pragma). No-op when no art row was attached.
        3 => {
            conn.execute_batch(
                "PRAGMA ignore_check_constraints = ON; DROP TRIGGER art_reject_content_update;",
            )
            .unwrap();
            let _ = conn.execute(
                "UPDATE art SET mime = ?1 \
                 WHERE id IN (SELECT art_id FROM track_art WHERE track_id = ?2)",
                rusqlite::params!["x".repeat(100_000), id],
            );
            conn.execute_batch("PRAGMA ignore_check_constraints = OFF")
                .unwrap();
        }
        // 4: stale binary-tag handle (delete the blob rows the layout will read).
        // No-op unless the binary-tag opt-in fired for an MP3/M4a track.
        4 => {
            let _ = conn.execute(
                "DELETE FROM tags WHERE track_id = ?1 AND value_blob IS NOT NULL",
                rusqlite::params![id],
            );
        }
        // 5: backing-geometry / content-version mismatch (per-read freshness guard).
        _ => {
            let n = conn
                .execute(
                    "UPDATE tracks SET backing_size = backing_size + 1, \
                     backing_mtime_ns = backing_mtime_ns + 1, \
                     content_version = content_version + 1 WHERE id = ?1",
                    rusqlite::params![id],
                )
                .unwrap();
            assert_eq!(n, 1, "variant 5 must mutate the tracks row");
        }
    });
}

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT || data.is_empty() {
        return;
    }
    let mut u = Unstructured::new(data);

    // A fixed small audio payload keeps fixtures cheap and deterministic; the
    // adversarial surface is the selector, the DB tags/art, and the read windows.
    const AUDIO: &[u8] = &[1u8, 2, 3, 4, 5, 6, 7, 8];

    // Selector biases Opus so serve_ogg_window / OggArtSlice are well covered.
    let sel = u.int_in_range(0..=6u8).unwrap_or(6);
    let (backing, format, audio_offset, audio_length) = match sel {
        0 => {
            let b = musefs_format::fuzz_check::fixtures::flac(AUDIO);
            let s = match musefs_format::flac::locate_audio(&b) {
                Ok(s) => s,
                Err(_) => return,
            };
            (b, Format::Flac, s.audio_offset, s.audio_length)
        }
        1 => {
            let b = musefs_format::fuzz_check::fixtures::wav(&[0i16, 1, -1, 100]);
            let s = match musefs_format::wav::locate_audio(&b) {
                Ok(s) => s,
                Err(_) => return,
            };
            (b, Format::Wav, s.audio_offset, s.audio_length)
        }
        2 => {
            let b = musefs_format::fuzz_check::fixtures::mp3();
            let s = match musefs_format::mp3::locate_audio(&b) {
                Ok(s) => s,
                Err(_) => return,
            };
            (b, Format::Mp3, s.audio_offset, s.audio_length)
        }
        3 => {
            let b = musefs_format::fuzz_check::fixtures::m4a(&[9u8; 32]);
            let s = match musefs_format::mp4::read_structure(&b) {
                Ok(s) => s,
                Err(_) => return,
            };
            (b, Format::M4a, s.mdat_payload_offset, s.mdat_payload_len)
        }
        4 => {
            let b = musefs_format::fuzz_check::fixtures::ogg_opus();
            let s = match musefs_format::ogg::locate_audio(&b) {
                Ok(s) => s,
                Err(_) => return,
            };
            (b, Format::Opus, s.audio_offset, s.audio_length)
        }
        5 => {
            let b = musefs_format::fuzz_check::fixtures::ogg_vorbis();
            let s = match musefs_format::ogg::locate_audio(&b) {
                Ok(s) => s,
                Err(_) => return,
            };
            (b, Format::Vorbis, s.audio_offset, s.audio_length)
        }
        _ => {
            let b = musefs_format::fuzz_check::fixtures::ogg_flac();
            let s = match musefs_format::ogg::locate_audio(&b) {
                Ok(s) => s,
                Err(_) => return,
            };
            (b, Format::OggFlac, s.audio_offset, s.audio_length)
        }
    };

    let Some((_dir, db, id)) = setup(&backing, format, audio_offset, audio_length) else {
        return;
    };

    // Optionally attach fuzzer-chosen tags and a DB art blob (the art produces an
    // ArtImage / OggArtSlice segment, depending on format).
    let tags = arb_tags(&mut u).unwrap_or_default();
    if !tags.is_empty() {
        let db_tags: Vec<Tag> = tags
            .iter()
            .enumerate()
            .map(|(i, t)| Tag::new(&t.key, &t.value, i as u64))
            .collect();
        let _ = db.replace_tags(id, &db_tags);
    }
    let arts = arb_arts(&mut u).unwrap_or_default();
    if let Some(a) = arts.first() {
        let blob = vec![0xABu8; usize::try_from(a.data_len.get().min(4096)).unwrap_or(0)];
        if !blob.is_empty()
            && let Ok(art_id) = db.upsert_art(&NewArt {
                mime: a.mime.clone(),
                width: Some(8),
                height: Some(8),
                data: blob,
            })
        {
            let _ = db.set_track_art(
                id,
                &[TrackArt {
                    art_id,
                    picture_type: 3,
                    description: String::new(),
                    ordinal: 0,
                }],
            );
        }
    }

    // Optionally attach DB binary tags so synthesis materializes a
    // Segment::BinaryTag and the read windows exercise read_binary_tag_chunk_into.
    // Only MP3 (4-byte frame id) and M4a (`----:<mean>:<name>` freeform atom)
    // synthesize opaque binary tags from the DB; for any other format the row is
    // silently dropped at synthesis, so restrict the opt-in to those two.
    if matches!(format, Format::Mp3 | Format::M4a) && u.arbitrary::<bool>().unwrap_or(false) {
        let key = match format {
            Format::Mp3 => "GEOB".to_string(),
            _ => "----:com.apple.iTunes:FUZZ".to_string(),
        };
        let _ = db.set_binary_tags(
            id,
            &[BinaryTag {
                key,
                payload: vec![0xCDu8; 64],
                ordinal: 0,
            }],
        );
    }

    // Hostile-row stage. Variants 0/1/3 corrupt geometry/format/art-metadata that
    // `resolve` validates (it returns Err -> the existing early-return below
    // handles them). Variants 2/4/5 are read-time hostilities applied AFTER a
    // successful resolve.
    let hostile = if u.arbitrary::<bool>().unwrap_or(false) {
        Some(u.int_in_range(0..=5u8).unwrap_or(0))
    } else {
        None
    };
    let hostile_val = u.arbitrary::<i64>().unwrap_or(i64::MAX);
    if matches!(hostile, Some(0 | 1 | 3)) {
        apply_hostile(&db, id, hostile.unwrap(), hostile_val);
    }

    let resolved = match HeaderCache::new(Mode::Synthesis).resolve(&db, id) {
        Ok(r) => r,
        Err(_) => return,
    };

    // Read-time hostilities: apply only after a successful resolve.
    let hostile_post = matches!(hostile, Some(2 | 4 | 5));
    if hostile_post {
        apply_hostile(&db, id, hostile.unwrap(), hostile_val);
    }

    let total = resolved.total_len;
    let file = std::fs::File::open(&resolved.backing_path).expect("backing file opens");

    // Splice-consistency invariants. A successfully-resolved layout is internally
    // consistent regardless of how its rows were planted, so whenever the read
    // returns Ok these MUST hold and are asserted. The only relaxation: when ANY
    // hostile mutation was applied, a read may return Err -> return/break, do not
    // assert. This must key on `hostile.is_some()`, NOT `hostile_post`: resolve
    // trusts the DB geometry for Ogg (it defers page parsing to read time), so a
    // pre-resolve geometry corruption (variant 0) can survive resolve and surface
    // as a clean Err(Format(Malformed)) at read time — that is the production code
    // correctly rejecting hostile state, not a clean-path failure. Only a
    // genuinely clean input (hostile == None) must read without error.
    let whole = match read_at_with_file(&resolved, &db, &file, 0, total) {
        Ok(w) => w,
        Err(_) if hostile.is_some() => return,
        Err(e) => panic!("clean-path whole read failed: {e:?}"),
    };
    assert_eq!(whole.len() as u64, total, "whole read length != total_len");

    // Draw up to 8 windows, including ranges that start at/after EOF or run past
    // it (offset/size range up to total+64). read_segments_into clamps the read
    // to [offset, total); an oversized/past-EOF range must not panic and must
    // return the clamped length (0 when offset >= total), and the bytes must
    // equal the clamped slice of the whole read.
    let slack = total.saturating_add(64);
    for _ in 0..8 {
        let offset = match u.int_in_range(0..=slack) {
            Ok(v) => v,
            Err(_) => break,
        };
        let size = match u.int_in_range(0..=slack) {
            Ok(v) => v,
            Err(_) => break,
        };
        let got = match read_at_with_file(&resolved, &db, &file, offset, size) {
            Ok(g) => g,
            Err(_) if hostile.is_some() => break,
            Err(e) => panic!("clean-path window read failed: {e:?}"),
        };
        // Mirror read_segments_into's clamp: served = [min(offset,total), min(offset+size,total)).
        let end = offset.saturating_add(size).min(total);
        let expected = end.saturating_sub(offset.min(total));
        assert_eq!(got.len() as u64, expected, "clamped window length mismatch");
        if expected > 0 {
            assert_eq!(
                got.as_slice(),
                &whole
                    [usize::try_from(offset).unwrap()..usize::try_from(offset + expected).unwrap()],
                "window != clamped slice of whole read",
            );
        }
    }
});
