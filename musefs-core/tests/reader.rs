mod common;
use common::write_flac;
use musefs_core::{HeaderCache, Mode, read_at};
use musefs_db::{Db, Format, NewTrack, Tag};

fn setup() -> (tempfile::TempDir, Db, i64) {
    let dir = tempfile::tempdir().unwrap();
    let flac = dir.path().join("song.flac");
    let audio = vec![0x5A; 120];
    let (audio_offset, audio_length) = write_flac(&flac, &["TITLE=Orig"], &audio);
    let meta = std::fs::metadata(&flac).unwrap();

    let db = Db::open_in_memory().unwrap();
    let id = db
        .upsert_track(&NewTrack {
            backing_path: flac.to_string_lossy().into_owned(),
            format: Format::Flac,
            audio_offset,
            audio_length,
            backing_size: meta.len(),
            backing_mtime: i64::try_from(
                meta.modified()
                    .unwrap()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
            )
            .unwrap(),
        })
        .unwrap();
    db.replace_tags(id, &[Tag::new("title", "Real Title", 0)])
        .unwrap();
    (dir, db, id)
}

#[test]
fn resolve_builds_layout_and_total_len() {
    let (_dir, db, id) = setup();
    let cache = HeaderCache::new(Mode::Synthesis);
    let resolved = cache.resolve(&db, id).unwrap();
    assert!(resolved.total_len > 0);
    assert_eq!(resolved.total_len, resolved.layout.total_len());
    assert_eq!(resolved.total_len, resolved.layout.header_len() + 120);
}

#[test]
fn resolve_caches_until_content_version_changes() {
    let (_dir, db, id) = setup();
    let cache = HeaderCache::new(Mode::Synthesis);
    let first = cache.resolve(&db, id).unwrap();
    let first_version = first.content_version;

    let again = cache.resolve(&db, id).unwrap();
    assert!(std::sync::Arc::ptr_eq(&first, &again));

    db.replace_tags(id, &[Tag::new("title", "Different", 0)])
        .unwrap();
    let updated = cache.resolve(&db, id).unwrap();
    assert!(updated.content_version > first_version);
    assert!(!std::sync::Arc::ptr_eq(&first, &updated));
}

#[test]
fn bounds_check_rejects_audio_region_overrunning_the_file() {
    // An audio range that overruns the backing file can no longer be committed:
    // the V4 `audio_offset + audio_length <= backing_size` CHECK rejects it at
    // write time, so synthesis never sees an out-of-file region.
    let dir = tempfile::tempdir().unwrap();
    let flac = dir.path().join("song.flac");
    let audio = vec![0x5A; 120];
    let _ = write_flac(&flac, &["TITLE=Orig"], &audio);
    let meta = std::fs::metadata(&flac).unwrap();
    let mtime = i64::try_from(
        meta.modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap();

    let db = Db::open_in_memory().unwrap();
    let overrun = db.upsert_track(&NewTrack {
        backing_path: flac.to_string_lossy().into_owned(),
        format: Format::Flac,
        audio_offset: 0,
        audio_length: meta.len() + 1,
        backing_size: meta.len(),
        backing_mtime: mtime,
    });
    assert!(
        overrun.is_err(),
        "bounds CHECK must reject audio_length overrunning backing_size"
    );
}

#[test]
fn resolve_errors_when_backing_file_changes() {
    let (dir, db, id) = setup();
    let cache = HeaderCache::new(Mode::Synthesis);
    cache.resolve(&db, id).unwrap();

    std::fs::write(dir.path().join("song.flac"), b"fLaC truncated").unwrap();
    let err = cache.resolve(&db, id);
    assert!(matches!(
        err,
        Err(musefs_core::CoreError::BackingChanged(_))
    ));
}

#[test]
fn resolve_includes_art_image_segments() {
    use musefs_db::{NewArt, TrackArt};
    use musefs_format::Segment;

    let (_dir, db, id) = setup();
    let art_id = db
        .upsert_art(&NewArt {
            mime: "image/png".to_string(),
            width: None,
            height: None,
            data: vec![0x9u8; 80],
        })
        .unwrap();
    db.set_track_art(
        id,
        &[TrackArt {
            art_id,
            picture_type: 3,
            description: String::new(),
            ordinal: 0,
        }],
    )
    .unwrap();

    let cache = HeaderCache::new(Mode::Synthesis);
    let resolved = cache.resolve(&db, id).unwrap();
    assert!(
        resolved.layout.segments.iter().any(
            |s| matches!(s, Segment::ArtImage { art_id: a, len } if *a == art_id && *len == 80)
        )
    );
}

#[test]
fn structure_only_resolves_to_whole_backing_file() {
    use musefs_format::Segment;

    let (dir, db, id) = setup();
    let backing = dir.path().join("song.flac");
    let original = std::fs::read(&backing).unwrap();

    let cache = HeaderCache::new(Mode::StructureOnly);
    let resolved = cache.resolve(&db, id).unwrap();

    // Passthrough: one whole-file backing segment, size == the real file.
    assert_eq!(resolved.total_len, original.len() as u64);
    assert_eq!(
        resolved.layout.segments,
        vec![Segment::BackingAudio {
            offset: 0,
            len: original.len() as u64
        }]
    );

    // Reading the whole file yields the original bytes unchanged (not synthesized).
    let whole = read_at(&resolved, &db, 0, resolved.total_len).unwrap();
    assert_eq!(whole, original);
}
