#![no_main]
use arbitrary::Unstructured;
use libfuzzer_sys::fuzz_target;
use musefs_core::{HeaderCache, Mode, read_at_with_file};
use musefs_db::{Db, Format, NewArt, NewTrack, Tag, TrackArt};
use musefs_fuzz::{MAX_INPUT, arb_arts, arb_tags};
use std::io::Write;

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
            backing_mtime: i64::try_from(
                meta.modified()
                    .ok()?
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()?
                    .as_secs(),
            )
            .ok()?,
        })
        .ok()?;
    db.replace_tags(id, &[Tag::new("title", "T", 0)]).ok()?;
    Some((dir, db, id))
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
        _ => {
            let b = musefs_format::fuzz_check::fixtures::ogg_opus();
            let s = match musefs_format::ogg::locate_audio(&b) {
                Ok(s) => s,
                Err(_) => return,
            };
            (b, Format::Opus, s.audio_offset, s.audio_length)
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

    let resolved = match HeaderCache::new(Mode::Synthesis).resolve(&db, id) {
        Ok(r) => r,
        Err(_) => return,
    };
    let total = resolved.total_len;
    let file = std::fs::File::open(&resolved.backing_path).expect("backing file opens");

    // The single whole read every window is checked against (splice consistency).
    let whole = read_at_with_file(&resolved, &db, &file, 0, total).unwrap();
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
        let got = read_at_with_file(&resolved, &db, &file, offset, size).unwrap();
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
