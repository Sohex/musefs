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
            backing_path: flac.clone(),
            format: Format::Flac,
            audio_offset,
            audio_length,
            backing_size: meta.len(),
            backing_mtime_ns: common::real_mtime_ns(&flac),
            backing_ctime_ns: common::real_ctime_ns(&flac),
            backing_ino: None,
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
    let mtime_ns = common::real_mtime_ns(&flac);
    let ctime_ns = common::real_ctime_ns(&flac);

    let db = Db::open_in_memory().unwrap();
    let overrun = db.upsert_track(&NewTrack {
        backing_path: flac.clone(),
        format: Format::Flac,
        audio_offset: 0,
        audio_length: meta.len() + 1,
        backing_size: meta.len(),
        backing_mtime_ns: mtime_ns,
        backing_ctime_ns: ctime_ns,
        backing_ino: None,
    });
    assert!(
        overrun.is_err(),
        "bounds CHECK must reject audio_length overrunning backing_size"
    );
}

#[test]
fn resolve_includes_art_image_segments() {
    use musefs_db::{NewArt, TrackArt};
    use musefs_format::Segment;

    let (_dir, db, id) = setup();
    let art_id = db
        .upsert_art(&NewArt {
            data: vec![0x9u8; 80],
        })
        .unwrap();
    db.set_track_art(
        id,
        &[TrackArt {
            art_id,
            picture_type: 3,
            description: String::new(),
            mime: "image/png".into(),
            width: None,
            height: None,
            depth: 0,
            colors: 0,
            ordinal: 0,
        }],
    )
    .unwrap();

    let cache = HeaderCache::new(Mode::Synthesis);
    let resolved = cache.resolve(&db, id).unwrap();
    assert!(resolved.layout.segments().iter().any(
        |s| matches!(s, Segment::ArtImage { art_id: a, len, .. } if *a == art_id && len.get() == 80)
    ));
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
        resolved.layout.segments(),
        &[Segment::BackingAudio {
            offset: 0,
            len: original.len() as u64
        }]
    );

    // Reading the whole file yields the original bytes unchanged (not synthesized).
    let whole = read_at(&resolved, &db, 0, resolved.total_len).unwrap();
    assert_eq!(whole, original);
}

/// #725 puts the store's `content_version` in the reported timestamp's
/// nanoseconds so a metadata edit moves the mtime. That is right only where the
/// served bytes are synthesized from the store.
///
/// In `StructureOnly` the served bytes *are* the backing file, so a tag edit
/// changes nothing about them — and signalling a change would send every
/// size-plus-mtime consumer to re-copy a byte-identical file. The inverse of the
/// bug #725 fixes, and just as wrong.
#[test]
fn a_tag_edit_does_not_move_the_passthrough_mtime() {
    let (dir, db, id) = setup();
    let _ = dir;

    let before = HeaderCache::new(Mode::StructureOnly)
        .resolve(&db, id)
        .unwrap()
        .mtime;

    // A real edit: it bumps content_version, which is what Synthesis reports.
    db.replace_tags(id, &[musefs_db::Tag::new("title", "Edited", 0)])
        .unwrap();
    let bumped = db.get_track(id).unwrap().unwrap().content_version;
    assert!(bumped > 0, "the edit must have bumped content_version");

    let after = HeaderCache::new(Mode::StructureOnly)
        .resolve(&db, id)
        .unwrap()
        .mtime;
    assert_eq!(
        after, before,
        "passthrough bytes did not change, so the timestamp must not either"
    );
    assert_eq!(
        after.content_version, 0,
        "no sub-second signal in passthrough"
    );

    // And the same edit *does* move it under synthesis, where the bytes really
    // are rebuilt from the store — so this is a mode distinction, not a dead
    // mechanism.
    let synth = HeaderCache::new(Mode::Synthesis)
        .resolve(&db, id)
        .unwrap()
        .mtime;
    assert_eq!(synth.content_version, bumped);
    assert_ne!(synth.nanos(), after.nanos());
}

/// Rebuild `id`'s row as the scanner would write it, with `backing_ino` and the
/// backing timestamps replaced. Every other column is carried over unchanged.
fn restamp(db: &Db, id: i64, backing_ino: Option<u64>, mtime_ns: i64, ctime_ns: i64) {
    let track = db.get_track(id).unwrap().unwrap();
    db.upsert_track(&NewTrack {
        backing_path: track.backing_path,
        format: track.format,
        audio_offset: track.bounds.audio_offset(),
        audio_length: track.bounds.audio_length(),
        backing_size: track.backing_size,
        backing_mtime_ns: mtime_ns,
        backing_ctime_ns: ctime_ns,
        backing_ino,
    })
    .unwrap();
}

/// #725's promise is wider than tag edits: any store change to what synthesis
/// serves bumps `content_version`, and the reported timestamp has to move with
/// it. Here nothing but the geometry trigger fires, recording the file's inode
/// for the first time. That is what the first revalidate after the V4 upgrade
/// does to every row, with no tag, art or structural block touched.
#[test]
fn a_version_bump_with_no_tag_edit_moves_the_synthesis_mtime() {
    use std::os::unix::fs::MetadataExt;

    let (dir, db, id) = setup();
    let flac = dir.path().join("song.flac");
    let cache = HeaderCache::new(Mode::Synthesis);
    let before = cache.resolve(&db, id).unwrap().mtime;
    let tags = db.get_tags(id).unwrap();

    let track = db.get_track(id).unwrap().unwrap();
    let ino = std::fs::metadata(&flac).unwrap().ino();
    restamp(
        &db,
        id,
        Some(ino),
        track.backing_mtime_ns,
        track.backing_ctime_ns,
    );

    let after = cache.resolve(&db, id).unwrap().mtime;
    assert_eq!(db.get_tags(id).unwrap(), tags, "no tag was touched");
    assert!(
        after.content_version > before.content_version,
        "recording the inode bumps content_version"
    );
    assert_ne!(
        after.nanos(),
        before.nanos(),
        "and the reported timestamp moves with it"
    );
}

/// The limit #725 documents rather than fixes. The reported second is the later
/// of the backing file's mtime and the row's `updated_at`, so a backing file
/// dated in the future holds that second still across a tag edit, and only the
/// nanoseconds move. A consumer comparing whole seconds misses the edit; one
/// comparing the full timestamp sees it. Pinned so the serving guide's
/// description of the limit stays true, and any change to it is deliberate.
#[test]
fn a_future_backing_mtime_holds_the_second_and_only_the_nanoseconds_move() {
    let (dir, db, id) = setup();
    let flac = dir.path().join("song.flac");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    // Ten years ahead: later than any `updated_at` this run can stamp.
    let future = i64::try_from(now).unwrap() + 10 * 365 * 86_400;
    common::set_mtime(&flac, future, 0);
    restamp(
        &db,
        id,
        None,
        common::real_mtime_ns(&flac),
        common::real_ctime_ns(&flac),
    );

    let cache = HeaderCache::new(Mode::Synthesis);
    let before = cache.resolve(&db, id).unwrap().mtime;
    assert_eq!(before.secs, future, "the backing file's future second wins");

    db.replace_tags(id, &[Tag::new("title", "Edited", 0)])
        .unwrap();
    let after = cache.resolve(&db, id).unwrap().mtime;
    assert_eq!(
        after.secs, before.secs,
        "a whole-second comparison cannot see the edit"
    );
    assert_ne!(after.nanos(), before.nanos(), "the full timestamp can");
}
