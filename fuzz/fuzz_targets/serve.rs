#![no_main]
use libfuzzer_sys::fuzz_target;
use musefs_core::{read_at_with_file, HeaderCache, Mode};
use musefs_db::{Db, Format, NewTrack, Tag};
use musefs_fuzz::MAX_INPUT;
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
    std::fs::File::create(&path)
        .ok()?
        .write_all(backing)
        .ok()?;
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
    db.replace_tags(id, &[Tag::new("title", "T", 0)])
        .ok()?;
    Some((dir, db, id))
}

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT {
        return;
    }
    // Skeleton: serve a fixed Opus fixture and read the whole virtual file.
    let backing = musefs_format::fuzz_check::fixtures::ogg_opus();
    let scan = match musefs_format::ogg::locate_audio(&backing) {
        Ok(s) => s,
        Err(_) => return,
    };
    let Some((_dir, db, id)) =
        setup(&backing, Format::Opus, scan.audio_offset, scan.audio_length)
    else {
        return;
    };
    let resolved = match HeaderCache::new(Mode::Synthesis).resolve(&db, id) {
        Ok(r) => r,
        Err(_) => return,
    };
    let file = std::fs::File::open(&resolved.backing_path).expect("backing file opens");
    let whole =
        read_at_with_file(&resolved, &db, &file, 0, resolved.total_len).unwrap();
    assert_eq!(
        whole.len() as u64,
        resolved.total_len,
        "whole read length != total_len"
    );
});
