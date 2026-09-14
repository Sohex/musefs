//! Scanning, serving and revalidating MP3s whose ID3v2 metadata is not a single
//! prepended tag: a run of prepended tags (#767), or a tag appended after the
//! audio, alone or alongside a prepended one (#768).

use musefs_core::{HeaderCache, Mode, read_at, scan_directory};
use musefs_format::fuzz_check::fixtures::{
    MP3_FIXTURE_AUDIO, id3v1_trailer, id3v24_text_tag, mp3_with_front_and_back_tags,
    mp3_with_leading_tag_run,
};

/// Synchsafe size encoding, independent of the production encoder.
fn syncsafe(n: u32) -> [u8; 4] {
    [
        ((n >> 21) & 0x7F) as u8,
        ((n >> 14) & 0x7F) as u8,
        ((n >> 7) & 0x7F) as u8,
        (n & 0x7F) as u8,
    ]
}

/// `tag` as an appended tag: footer flag added to whatever flags it has, and a
/// `3DI` footer copied from the header (ID3v2.4 structure §3.4).
fn appended_form(tag: &[u8]) -> Vec<u8> {
    let mut out = tag.to_vec();
    out[5] |= 0x10;
    let header = out[3..10].to_vec();
    out.extend_from_slice(b"3DI");
    out.extend_from_slice(&header);
    out
}

/// `tag` (no flags, as `id3v24_text_tag` builds it) with an ID3v2.4 extended
/// header whose "tag is an update" flag is set (structure §3.2).
fn as_update(tag: &[u8]) -> Vec<u8> {
    let frames = &tag[10..];
    let mut out = vec![b'I', b'D', b'3', 4, 0, 0x40];
    out.extend_from_slice(&syncsafe(u32::try_from(frames.len() + 6).unwrap()));
    out.extend_from_slice(&[0, 0, 0, 6, 0x01, 0x40]); // size 6, one flag byte, flag b
    out.extend_from_slice(frames);
    out
}

fn sorted(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut v: Vec<_> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    v.sort();
    v
}

fn tags_of(db: &musefs_db::Db) -> Vec<(String, String)> {
    let t = &db.list_tracks().unwrap()[0];
    let mut tags: Vec<_> = db
        .get_tags(t.id)
        .unwrap()
        .into_iter()
        .map(|tg| (tg.key, tg.value))
        .collect();
    tags.sort();
    tags
}

/// Scan a directory holding just `bytes` as `a.mp3`.
fn scan_one(bytes: &[u8]) -> (tempfile::TempDir, musefs_db::Db) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.mp3"), bytes).unwrap();
    let db = musefs_db::Db::open_in_memory().unwrap();
    let stats = scan_directory(&db, dir.path()).unwrap();
    assert_eq!((stats.scanned, stats.failed), (1, 0), "the file scans");
    (dir, db)
}

fn assert_bounds(db: &musefs_db::Db, offset: usize) {
    let t = &db.list_tracks().unwrap()[0];
    assert_eq!(
        (t.bounds.audio_offset(), t.bounds.audio_length()),
        (offset as u64, MP3_FIXTURE_AUDIO.len() as u64),
        "the audio region is the audio and nothing else"
    );
}

#[test]
fn an_mp3_behind_two_leading_tags_scans_and_takes_the_later_tag() {
    let file = mp3_with_leading_tag_run();
    let (_dir, db) = scan_one(&file);
    assert_bounds(&db, file.len() - MP3_FIXTURE_AUDIO.len());
    // Neither tag is an update, so the later one discards the earlier (§5).
    assert_eq!(tags_of(&db), sorted(&[("title", "Second Tag")]));
}

#[test]
fn an_appended_only_tag_is_ingested() {
    let mut file = MP3_FIXTURE_AUDIO.to_vec();
    file.extend(appended_form(&id3v24_text_tag(&[
        ("title", "Back Title"),
        ("artist", "Back Artist"),
    ])));
    let (_dir, db) = scan_one(&file);
    assert_bounds(&db, 0);
    assert_eq!(
        tags_of(&db),
        sorted(&[("artist", "Back Artist"), ("title", "Back Title")])
    );
}

#[test]
fn a_later_tag_without_the_update_flag_replaces_the_earlier_one() {
    let file = mp3_with_front_and_back_tags();
    let (_dir, db) = scan_one(&file);
    let front = id3v24_text_tag(&[("title", "Front Title"), ("artist", "Front Artist")]);
    assert_bounds(&db, front.len());
    assert_eq!(tags_of(&db), sorted(&[("title", "Back Title")]));
}

#[test]
fn an_update_tag_overrides_only_the_frames_it_carries() {
    let front = id3v24_text_tag(&[("title", "Front Title"), ("artist", "Front Artist")]);
    let mut file = front.clone();
    file.extend_from_slice(MP3_FIXTURE_AUDIO);
    file.extend(appended_form(&as_update(&id3v24_text_tag(&[(
        "title",
        "Back Title",
    )]))));
    file.extend(id3v1_trailer());
    let (_dir, db) = scan_one(&file);
    assert_bounds(&db, front.len());
    assert_eq!(
        tags_of(&db),
        sorted(&[("artist", "Front Artist"), ("title", "Back Title")])
    );
}

#[test]
fn the_served_file_carries_only_the_synthesized_front_tag() {
    let (_dir, db) = scan_one(&mp3_with_front_and_back_tags());
    let id = db.list_tracks().unwrap()[0].id;
    // An edit in the store: the served tag must say this, and nothing stale may
    // survive at either end of the file.
    db.replace_tags(id, &[musefs_db::Tag::new("title", "Edited", 0)])
        .unwrap();
    let resolved = HeaderCache::new(Mode::Synthesis).resolve(&db, id).unwrap();
    let served = read_at(&resolved, &db, 0, resolved.total_len).unwrap();

    let tag = id3::Tag::read_from2(std::io::Cursor::new(&served)).expect("a front ID3v2 tag");
    assert_eq!(id3::TagLike::title(&tag), Some("Edited"));
    let located = musefs_format::mp3::locate_audio(&served).unwrap();
    assert_eq!(
        (located.audio_offset, located.audio_length),
        (
            served.len() as u64 - MP3_FIXTURE_AUDIO.len() as u64,
            MP3_FIXTURE_AUDIO.len() as u64
        ),
        "one tag, then the audio, then nothing"
    );
    assert!(served.ends_with(MP3_FIXTURE_AUDIO));
    for stale in [&b"3DI"[..], b"Front Title", b"Back Title", b"TAG"] {
        assert!(
            !served.windows(stale.len()).any(|w| w == stale),
            "{:?} survived into the served file",
            String::from_utf8_lossy(stale)
        );
    }
}

#[test]
fn a_bounded_scan_agrees_with_the_full_probe_over_a_large_appended_tag() {
    // An appended tag far larger than the tail read's first window, so the
    // footer's declared extent drives a second, exact read.
    let mut back = id3v24_text_tag(&[("title", "Wide"), ("artist", "Alice")]);
    back.extend(std::iter::repeat_n(0u8, 8192)); // padding inside the tag body
    let body_len = u32::try_from(back.len() - 10).unwrap();
    back[6..10].copy_from_slice(&syncsafe(body_len));
    let mut file = id3v24_text_tag(&[("title", "Front")]);
    let front_len = file.len();
    file.extend_from_slice(MP3_FIXTURE_AUDIO);
    file.extend(appended_form(&back));
    file.extend(id3v1_trailer());
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("wide.mp3"), &file).unwrap();

    let oracle_db = musefs_db::Db::open_in_memory().unwrap();
    musefs_core::scan_directory_full_oracle(&oracle_db, dir.path()).unwrap();
    let bounded_db = musefs_db::Db::open_in_memory().unwrap();
    let mut options = musefs_core::ScanOptions::default();
    options.window = 64;
    musefs_core::scan_directory_with(&bounded_db, dir.path(), &options).unwrap();

    let rows = |db: &musefs_db::Db| {
        let t = &db.list_tracks().unwrap()[0];
        (
            t.bounds.audio_offset(),
            t.bounds.audio_length(),
            tags_of(db),
        )
    };
    let oracle = rows(&oracle_db);
    assert_eq!(
        oracle,
        (
            front_len as u64,
            MP3_FIXTURE_AUDIO.len() as u64,
            sorted(&[("artist", "Alice"), ("title", "Wide")])
        )
    );
    assert_eq!(oracle, rows(&bounded_db));
}

/// A store upgraded from an older release holds MP3 rows whose audio region
/// runs over an appended tag. The upgrade leaves every row without a
/// fingerprint, which is what makes the owed revalidate re-probe it, and that
/// re-probe is what corrects the region. It refreshes structure only, so the
/// curated tags survive it.
#[test]
fn the_owed_revalidate_corrects_an_upgraded_row_that_served_an_appended_tag() {
    let music = tempfile::tempdir().unwrap();
    let mut file = MP3_FIXTURE_AUDIO.to_vec();
    file.extend(appended_form(&id3v24_text_tag(&[("title", "Back")])));
    file.extend(id3v1_trailer());
    std::fs::write(music.path().join("a.mp3"), &file).unwrap();
    let store = tempfile::tempdir().unwrap();
    let db_path = store.path().join("musefs.db");
    let db = musefs_db::Db::open(&db_path).unwrap();
    scan_directory(&db, music.path()).unwrap();

    // What an older build stored (only the ID3v1 trailer trimmed), in the shape
    // the V4 migration leaves every row: no fingerprint, no inode.
    let id = db.list_tracks().unwrap()[0].id;
    rusqlite::Connection::open(&db_path)
        .unwrap()
        .execute(
            "UPDATE tracks SET audio_length = ?1, fingerprint = NULL, backing_ino = 0 \
             WHERE id = ?2",
            rusqlite::params![i64::try_from(file.len() - 128).unwrap(), id],
        )
        .unwrap();
    assert_eq!(db.count_tracks_awaiting_revalidate().unwrap(), 1);
    // What a tagger curated since, which is not what the file carries.
    db.replace_tags(id, &[musefs_db::Tag::new("title", "Curated", 0)])
        .unwrap();

    let stats = musefs_core::revalidate(&db, music.path()).unwrap();
    assert_eq!(stats.updated, 1, "the owed revalidate re-probes the row");
    let t = &db.list_tracks().unwrap()[0];
    assert_eq!(
        (t.bounds.audio_offset(), t.bounds.audio_length()),
        (0, MP3_FIXTURE_AUDIO.len() as u64)
    );
    assert_eq!(
        tags_of(&db),
        sorted(&[("title", "Curated")]),
        "a structural refresh keeps the curated tags"
    );
    assert_eq!(db.count_tracks_awaiting_revalidate().unwrap(), 0);
}
