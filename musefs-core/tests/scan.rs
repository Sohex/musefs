mod common;
use common::{make_flac, set_mtime, streaminfo_body, vorbis_comment_body, write_flac};
use musefs_core::{ScanOptions, revalidate, scan_directory, scan_directory_with};
use musefs_db::{Db, Tag};

#[test]
fn scans_flac_files_seeding_tracks_and_tags() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();

    let a = make_flac(
        &[
            (0, streaminfo_body()),
            (4, vorbis_comment_body("v", &["TITLE=A", "ARTIST=X"])),
        ],
        &[0xAA; 30],
    );
    std::fs::write(dir.path().join("a.flac"), &a).unwrap();
    let b = make_flac(
        &[
            (0, streaminfo_body()),
            (4, vorbis_comment_body("v", &["TITLE=B"])),
        ],
        &[0xBB; 40],
    );
    std::fs::write(dir.path().join("sub/b.flac"), &b).unwrap();
    std::fs::write(dir.path().join("notes.txt"), b"hello").unwrap();

    let db = Db::open_in_memory().unwrap();
    let stats = scan_directory(&db, dir.path()).unwrap();
    assert_eq!(stats.scanned, 2);

    let tracks = db.list_tracks().unwrap();
    assert_eq!(tracks.len(), 2);

    let a_track = tracks
        .iter()
        .find(|t| t.backing_path.ends_with("a.flac"))
        .unwrap();
    let tags = db.get_tags(a_track.id).unwrap();
    assert!(tags.iter().any(|t| t.key == "title" && t.value == "A"));
    assert!(tags.iter().any(|t| t.key == "artist" && t.value == "X"));
    assert_eq!(a_track.bounds.audio_length(), 30);
}

#[test]
fn scans_a_single_file_path() {
    let dir = tempfile::tempdir().unwrap();
    let a = make_flac(
        &[
            (0, streaminfo_body()),
            (4, vorbis_comment_body("v", &["TITLE=Solo"])),
        ],
        &[0xAA; 30],
    );
    let path = dir.path().join("solo.flac");
    std::fs::write(&path, &a).unwrap();
    // A sibling in the same dir that must NOT be scanned when targeting a file.
    std::fs::write(dir.path().join("other.flac"), &a).unwrap();

    let db = Db::open_in_memory().unwrap();
    let stats = scan_directory(&db, &path).unwrap();
    assert_eq!(stats.scanned, 1);

    let tracks = db.list_tracks().unwrap();
    assert_eq!(tracks.len(), 1);
    assert!(tracks[0].backing_path.ends_with("solo.flac"));
}

#[test]
fn rescanning_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let a = make_flac(
        &[
            (0, streaminfo_body()),
            (4, vorbis_comment_body("v", &["TITLE=A"])),
        ],
        &[0xAA; 30],
    );
    std::fs::write(dir.path().join("a.flac"), &a).unwrap();

    let db = Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();
    scan_directory(&db, dir.path()).unwrap();
    assert_eq!(db.list_tracks().unwrap().len(), 1);
}

#[test]
fn bare_rescan_is_additive_and_preserves_db_edits() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_in_memory().unwrap();
    write_flac(
        &dir.path().join("a.flac"),
        &["TITLE=A-on-disk"],
        &[0xAA; 30],
    );

    scan_directory(&db, dir.path()).unwrap();
    let id = db.list_tracks().unwrap()[0].id;
    db.replace_tags(id, &[Tag::new("title", "Curated", 0)])
        .unwrap();

    write_flac(&dir.path().join("b.flac"), &["TITLE=B"], &[0xBB; 40]);
    let stats = scan_directory(&db, dir.path()).unwrap();

    assert_eq!(stats.scanned, 1);
    assert_eq!(stats.already_present, 1);
    let a = db
        .list_tracks()
        .unwrap()
        .into_iter()
        .find(|t| t.backing_path.ends_with("a.flac"))
        .unwrap();
    let tags = db.get_tags(a.id).unwrap();
    assert_eq!(tags[0].value, "Curated");
}

#[test]
fn force_rescan_reseeds_tags_from_file() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_in_memory().unwrap();
    write_flac(
        &dir.path().join("a.flac"),
        &["TITLE=A-on-disk"],
        &[0xAA; 30],
    );
    scan_directory(&db, dir.path()).unwrap();
    let id = db.list_tracks().unwrap()[0].id;
    db.replace_tags(id, &[Tag::new("title", "Curated", 0)])
        .unwrap();

    let mut opts = ScanOptions::default();
    opts.force = true;
    scan_directory_with(&db, dir.path(), &opts).unwrap();

    let tags = db.get_tags(id).unwrap();
    assert_eq!(tags[0].value, "A-on-disk");
}

#[test]
fn bare_scan_retargets_moved_file_preserving_tags() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_in_memory().unwrap();
    let a = dir.path().join("a.flac");
    write_flac(&a, &["TITLE=T"], &[0xAA; 30]);
    scan_directory(&db, dir.path()).unwrap();
    let id = db.list_tracks().unwrap()[0].id;
    db.replace_tags(id, &[Tag::new("title", "Curated", 0)])
        .unwrap();

    std::fs::rename(&a, dir.path().join("b.flac")).unwrap();
    scan_directory(&db, dir.path()).unwrap();

    let tracks = db.list_tracks().unwrap();
    assert_eq!(tracks.len(), 1);
    assert!(tracks[0].backing_path.ends_with("b.flac"));
    let tags = db.get_tags(tracks[0].id).unwrap();
    assert_eq!(tags[0].value, "Curated");
}

#[test]
fn mp3_id3v1_trailer_is_not_served_as_audio() {
    use musefs_db::Format;

    // An MP3 with both an ID3v2 tag and a 128-byte ID3v1 trailer. The trailer is
    // metadata, so the stored audio length must stop short of it — the scan path
    // only reads the file's tail for formats that can carry one.
    let dir = tempfile::tempdir().unwrap();
    let mut bytes = b"ID3".to_vec();
    bytes.extend_from_slice(&[0x04, 0x00, 0x00]); // v2.4.0, no flags
    bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // synchsafe size 0
    let audio_offset = bytes.len() as u64;
    let audio = [0xFFu8, 0xFB, 1, 2, 3, 4, 5, 6];
    bytes.extend_from_slice(&audio);
    bytes.extend_from_slice(b"TAG");
    bytes.extend(std::iter::repeat_n(0u8, 125));
    std::fs::write(dir.path().join("trailed.mp3"), &bytes).unwrap();

    let db = Db::open_in_memory().unwrap();
    assert_eq!(scan_directory(&db, dir.path()).unwrap().scanned, 1);

    let t = &db.list_tracks().unwrap()[0];
    assert_eq!(t.format, Format::Mp3);
    assert_eq!(t.bounds.audio_offset(), audio_offset);
    assert_eq!(
        t.bounds.audio_length(),
        audio.len() as u64,
        "the 128-byte ID3v1 trailer is metadata, not audio"
    );
}

#[test]
fn scans_mp3_files_seeding_tracks_and_tags() {
    use id3::TagLike;
    use musefs_db::Format;

    let dir = tempfile::tempdir().unwrap();

    // Build an MP3: a real ID3v2.4 tag (via the id3 crate) + a fake audio frame.
    let mut tag = id3::Tag::new();
    tag.set_artist("Bob");
    tag.set_title("Track");
    let mut bytes = Vec::new();
    tag.write_to(&mut bytes, id3::Version::Id3v24).unwrap();
    let audio_len = 8u64;
    bytes.extend_from_slice(&[0xFF, 0xFB, 1, 2, 3, 4, 5, 6]);
    let audio_offset = bytes.len() as u64 - audio_len;
    std::fs::write(dir.path().join("song.mp3"), &bytes).unwrap();

    let db = Db::open_in_memory().unwrap();
    let stats = scan_directory(&db, dir.path()).unwrap();
    assert_eq!(stats.scanned, 1);

    let tracks = db.list_tracks().unwrap();
    assert_eq!(tracks.len(), 1);
    let t = &tracks[0];
    assert_eq!(t.format, Format::Mp3);
    assert_eq!(t.bounds.audio_offset(), audio_offset);
    assert_eq!(t.bounds.audio_length(), audio_len);

    let tags = db.get_tags(t.id).unwrap();
    assert!(
        tags.iter()
            .any(|tag| tag.key == "artist" && tag.value == "Bob")
    );
    assert!(
        tags.iter()
            .any(|tag| tag.key == "title" && tag.value == "Track")
    );
}

#[test]
fn scans_m4a_files_seeding_tracks() {
    use musefs_db::Format;

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.m4a"), common::minimal_m4a(b"AUDIODATA")).unwrap();

    let db = Db::open_in_memory().unwrap();
    let stats = scan_directory(&db, dir.path()).unwrap();
    assert_eq!(stats.scanned, 1);

    let tracks = db.list_tracks().unwrap();
    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].format, Format::M4a);

    let tags = db.get_tags(tracks[0].id).unwrap();
    assert!(
        tags.iter()
            .any(|t| t.key == "title" && t.value == "Orig M4A")
    );
}

#[test]
fn revalidate_skips_unchanged_prunes_missing_and_gcs_art() {
    let dir = tempfile::tempdir().unwrap();
    let a = make_flac(
        &[
            (0, streaminfo_body()),
            (4, vorbis_comment_body("v", &["TITLE=A"])),
        ],
        &[0xAA; 30],
    );
    std::fs::write(dir.path().join("a.flac"), &a).unwrap();
    let gone = make_flac(
        &[
            (0, streaminfo_body()),
            (4, vorbis_comment_body("v", &["TITLE=G"])),
        ],
        &[0xBB; 30],
    );
    std::fs::write(dir.path().join("gone.flac"), &gone).unwrap();

    let db = Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();
    assert_eq!(db.list_tracks().unwrap().len(), 2);

    // An external edit to a's tags that a revalidate must NOT clobber (the file is
    // unchanged on disk, so revalidate should skip re-reading it).
    let a_id = db
        .list_tracks()
        .unwrap()
        .into_iter()
        .find(|t| t.backing_path.ends_with("a.flac"))
        .unwrap()
        .id;
    db.replace_tags(a_id, &[Tag::new("title", "Edited", 0)])
        .unwrap();

    // Delete gone.flac from disk; default revalidate keeps the row now.
    std::fs::remove_file(dir.path().join("gone.flac")).unwrap();

    let stats = revalidate(&db, dir.path()).unwrap();
    assert_eq!(stats.unchanged, 1); // a.flac (size+mtime match) is skipped
    assert_eq!(stats.updated, 0); // nothing on disk changed, so nothing re-ingested
    assert_eq!(stats.pruned, 0); // prune is opt-in now

    let tracks = db.list_tracks().unwrap();
    assert_eq!(tracks.len(), 2);
    // The skipped file kept its externally-edited tag (not re-seeded from disk).
    let tags = db
        .get_tags(
            tracks
                .iter()
                .find(|t| t.backing_path.ends_with("a.flac"))
                .unwrap()
                .id,
        )
        .unwrap();
    assert!(tags.iter().any(|t| t.key == "title" && t.value == "Edited"));

    let mut options = ScanOptions::default();
    options.prune = true;
    let stats = musefs_core::revalidate_with(&db, dir.path(), &options).unwrap();
    assert_eq!(stats.pruned, 1); // gone.flac's track is removed
    assert_eq!(db.list_tracks().unwrap().len(), 1);
}

#[test]
fn scans_flac_with_many_mixed_tags_into_db() {
    let dir = tempfile::tempdir().unwrap();
    let comments = [
        "TITLE=Song",
        "ARTIST=A",
        "ARTIST=B", // multi-value artist
        "ALBUM=Alb",
        "ALBUMARTIST=VA",
        "DATE=2020",
        "GENRE=Electronic",
        "TRACKNUMBER=3",
        "REPLAYGAIN_TRACK_GAIN=-6.5 dB",
        "MUSICBRAINZ_ALBUMID=abc-123",
        "MYCUSTOMFIELD=custom", // user-defined: not in vocabulary, kept verbatim
    ];
    let flac = make_flac(
        &[
            (0, streaminfo_body()),
            (4, vorbis_comment_body("v", &comments)),
        ],
        &[0xAA; 30],
    );
    std::fs::write(dir.path().join("many.flac"), &flac).unwrap();

    let db = Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();
    let track = &db.list_tracks().unwrap()[0];
    let tags = db.get_tags(track.id).unwrap();

    // Known Vorbis fields fold to canonical lowercase keys.
    let has = |k: &str, v: &str| tags.iter().any(|t| t.key == k && t.value == v);
    assert!(has("title", "Song"));
    assert!(has("albumartist", "VA"));
    assert!(has("date", "2020"));
    assert!(has("genre", "Electronic"));
    assert!(has("tracknumber", "3"));
    assert!(has("replaygain_track_gain", "-6.5 dB"));
    assert!(has("musicbrainz_albumid", "abc-123"));

    // A user-defined field keeps its verbatim (uppercase) key.
    assert!(has("MYCUSTOMFIELD", "custom"));

    // Multi-value artist: both values survive as separate ordinals under "artist".
    let artists: Vec<&str> = tags
        .iter()
        .filter(|t| t.key == "artist")
        .map(|t| t.value.as_str())
        .collect();
    assert!(
        artists.contains(&"A") && artists.contains(&"B"),
        "artists: {artists:?}"
    );
}

fn flac_with_picture(comments: &[&str], img: &[u8]) -> Vec<u8> {
    use common::{flac_block, streaminfo_body, vorbis_comment_body};
    fn picture_body(pic_type: u32, mime: &str, data: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&pic_type.to_be_bytes());
        b.extend_from_slice(&u32::try_from(mime.len()).unwrap().to_be_bytes());
        b.extend_from_slice(mime.as_bytes());
        b.extend_from_slice(&0u32.to_be_bytes()); // empty description
        b.extend_from_slice(&0u32.to_be_bytes()); // width
        b.extend_from_slice(&0u32.to_be_bytes()); // height
        b.extend_from_slice(&0u32.to_be_bytes()); // depth
        b.extend_from_slice(&0u32.to_be_bytes()); // colors
        b.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
        b.extend_from_slice(data);
        b
    }
    let mut out = Vec::new();
    out.extend_from_slice(b"fLaC");
    out.extend_from_slice(&flac_block(0, &streaminfo_body(), false));
    out.extend_from_slice(&flac_block(4, &vorbis_comment_body("v", comments), false));
    out.extend_from_slice(&flac_block(6, &picture_body(3, "image/png", img), true));
    out.extend_from_slice(&[0xAAu8; 24]);
    out
}

#[test]
fn scan_ingests_and_dedups_embedded_art() {
    let dir = tempfile::tempdir().unwrap();
    let img = vec![0x42u8; 100];
    std::fs::write(
        dir.path().join("a.flac"),
        flac_with_picture(&["TITLE=A"], &img),
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(
        dir.path().join("sub/b.flac"),
        flac_with_picture(&["TITLE=B"], &img),
    )
    .unwrap();

    let db = Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();

    let tracks = db.list_tracks().unwrap();
    assert_eq!(tracks.len(), 2);

    // Both tracks link art, and the identical image is stored once (dedup by sha256).
    let mut art_ids = std::collections::HashSet::new();
    for t in &tracks {
        let ta = db.get_track_art(t.id).unwrap();
        assert_eq!(ta.len(), 1);
        assert_eq!(ta[0].picture_type, 3);
        art_ids.insert(ta[0].art_id);
    }
    assert_eq!(art_ids.len(), 1, "identical art should dedup to one row");

    let only = *art_ids.iter().next().unwrap();
    assert_eq!(db.get_art_meta(only).unwrap().unwrap().byte_len, 100);
}

/// The raise itself (#644): 300 KB of lyrics is over the old 256 KiB cap and
/// well under the new one, so the file that prompted the report now scans and
/// round-trips its tag.
#[test]
fn scan_stores_a_tag_value_the_old_cap_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let lyrics_body = "z".repeat(300_000);
    std::fs::write(
        dir.path().join("a.flac"),
        flac_with_pictures(&["TITLE=A", &format!("LYRICS={lyrics_body}")], &[]),
    )
    .unwrap();

    let db = Db::open_in_memory().unwrap();
    let stats = scan_directory(&db, dir.path()).unwrap();
    assert_eq!((stats.scanned, stats.failed), (1, 0));

    let t = &db.list_tracks().unwrap()[0];
    let tags = db.get_tags(t.id).unwrap();
    let lyrics = tags
        .iter()
        .find(|t| t.key.eq_ignore_ascii_case("LYRICS"))
        .expect("the 300 KB lyrics tag is stored");
    assert_eq!(lyrics.value, lyrics_body);
}

/// #696, the corpus entry: a backing file whose mtime predates the Unix epoch.
/// Archival rips, restored backups and anything whose mtime was set from the
/// original media's metadata can carry one, and `tar`/`rsync` preserve it, so
/// this is legitimate input rather than a crafted one.
///
/// It used to probe cleanly, produce a valid unit, and then die at the store's
/// `CHECK (backing_mtime_ns >= 0)` — counted under `failed` and absent from the
/// mount. The `tracks` rebuild drops that lower bound (#696), so the file now
/// reaches the store with its negative stamp intact, which is what this asserts.
#[test]
fn a_pre_epoch_backing_mtime_reaches_the_store_intact() {
    let dir = tempfile::tempdir().unwrap();
    let old = dir.path().join("archival.flac");
    write_flac(&old, &["TITLE=Archival"], &[0xAA; 30]);
    // 1969-12-31 23:59:58.5 UTC: negative whole seconds with a non-negative
    // `tv_nsec` fraction, which is the shape the kernel actually stores.
    set_mtime(&old, -2, 500_000_000);

    // A sibling with an ordinary mtime proves the failure is contained to the
    // one file rather than aborting the run.
    let modern = dir.path().join("modern.flac");
    write_flac(&modern, &["TITLE=Modern"], &[0xBB; 30]);

    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(&old).unwrap();
    assert_eq!(
        meta.mtime(),
        -2,
        "the filesystem must preserve a pre-epoch mtime for this test to mean anything"
    );

    // The Rust half of #696, against a real filesystem stamp: the stamp keeps
    // the negative nanosecond value, and the `getattr` display second is the
    // file's own `st_mtime` rather than the truncation toward zero.
    let stamp = musefs_core::freshness::BackingStamp::from_metadata(&meta);
    assert_eq!(stamp.mtime_ns, -1_500_000_000);
    assert_eq!(stamp.display_secs(), meta.mtime());

    let db = Db::open_in_memory().unwrap();
    let stats = scan_directory(&db, dir.path()).unwrap();

    assert_eq!(stats.scanned, 2, "both files reach the store");
    assert_eq!(stats.failed, 0);

    let archival = db
        .list_tracks()
        .unwrap()
        .into_iter()
        .find(|t| t.backing_path.ends_with("archival.flac"))
        .expect("the pre-epoch file is stored, not rejected");
    assert_eq!(
        archival.backing_mtime_ns, -1_500_000_000,
        "stored verbatim: the stamp is the file's, not a clamped stand-in"
    );
}

fn flac_with_pictures(comments: &[&str], pics: &[(u32, &[u8])]) -> Vec<u8> {
    use common::{flac_block, streaminfo_body, vorbis_comment_body};
    fn picture_body(pic_type: u32, mime: &str, data: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&pic_type.to_be_bytes());
        b.extend_from_slice(&u32::try_from(mime.len()).unwrap().to_be_bytes());
        b.extend_from_slice(mime.as_bytes());
        b.extend_from_slice(&0u32.to_be_bytes()); // description
        b.extend_from_slice(&0u32.to_be_bytes()); // width
        b.extend_from_slice(&0u32.to_be_bytes()); // height
        b.extend_from_slice(&0u32.to_be_bytes()); // depth
        b.extend_from_slice(&0u32.to_be_bytes()); // colors
        b.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
        b.extend_from_slice(data);
        b
    }
    let mut out = Vec::new();
    out.extend_from_slice(b"fLaC");
    out.extend_from_slice(&flac_block(0, &streaminfo_body(), false));
    out.extend_from_slice(&flac_block(
        4,
        &vorbis_comment_body("v", comments),
        pics.is_empty(),
    ));
    for (i, (pt, data)) in pics.iter().enumerate() {
        out.extend_from_slice(&flac_block(
            6,
            &picture_body(*pt, "image/png", data),
            i == pics.len() - 1,
        ));
    }
    out.extend_from_slice(&[0xAAu8; 24]);
    out
}

#[test]
fn scan_clamps_out_of_range_picture_type() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.flac"),
        flac_with_pictures(&["TITLE=A"], &[(99, &[0x11u8; 40])]),
    )
    .unwrap();

    let db = Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();

    let t = &db.list_tracks().unwrap()[0];
    let ta = db.get_track_art(t.id).unwrap();
    assert_eq!(ta.len(), 1);
    assert_eq!(ta[0].picture_type, 0); // 99 is out of range (0..=20) -> clamped to 0
    assert_eq!(ta[0].ordinal, 0);
}

/// #644: a file carrying oversize art fails outright rather than being stored
/// with that art quietly missing. Its neighbours in the same directory are
/// unaffected — the whole point of failing the file instead of the scan.
#[test]
fn scan_fails_only_the_file_with_oversized_art() {
    let dir = tempfile::tempdir().unwrap();
    // Over MAX_ART_BYTES (16 MiB - 64 KiB) but still within FLAC's 24-bit block limit.
    let big = vec![0u8; 16_776_500];
    let small = vec![0x22u8; 50];
    std::fs::write(
        dir.path().join("a.flac"),
        flac_with_pictures(&["TITLE=A"], &[(3, &big), (4, &small)]),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("b.flac"),
        flac_with_pictures(&["TITLE=B"], &[(3, &small)]),
    )
    .unwrap();

    let db = Db::open_in_memory().unwrap();
    let stats = scan_directory(&db, dir.path()).unwrap();

    assert_eq!(stats.failed, 1, "the oversize file is counted as failed");
    assert_eq!(stats.scanned, 1, "its neighbour still lands");
    let tracks = db.list_tracks().unwrap();
    assert_eq!(tracks.len(), 1);
    assert!(
        tracks[0].backing_path.ends_with("b.flac"),
        "only the clean file is stored, got {}",
        tracks[0].backing_path.display()
    );
    // No partial row for the failed file, and no orphan art from it either.
    let ta = db.get_track_art(tracks[0].id).unwrap();
    assert_eq!(ta.len(), 1);
    assert_eq!(db.get_art_meta(ta[0].art_id).unwrap().unwrap().byte_len, 50);
}

/// Two files holding byte-identical art that describe it differently.
///
/// This is #716's reproduction. `art` is deduplicated on `sha256(data)`, and it
/// used to own the mime, the dimensions and (in the format, but nowhere in the
/// store) the depth and colour count. `upsert_art` is `ON CONFLICT DO NOTHING`,
/// so whichever occurrence was scanned first chose all of them for every track
/// referencing that blob — including the *declared MIME type*, so a file whose
/// own block said JPEG could be served one saying PNG. Scan order decided it.
///
/// The blob is still shared; the description of it is not.
#[test]
fn two_files_sharing_one_blob_keep_their_own_picture_metadata() {
    /// A FLAC `PICTURE` block declaring its own mime, geometry, depth and colours.
    fn picture_block(mime: &str, w: u32, h: u32, depth: u32, colors: u32, data: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&3u32.to_be_bytes()); // front cover
        b.extend_from_slice(&u32::try_from(mime.len()).unwrap().to_be_bytes());
        b.extend_from_slice(mime.as_bytes());
        b.extend_from_slice(&0u32.to_be_bytes()); // description
        b.extend_from_slice(&w.to_be_bytes());
        b.extend_from_slice(&h.to_be_bytes());
        b.extend_from_slice(&depth.to_be_bytes());
        b.extend_from_slice(&colors.to_be_bytes());
        b.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
        b.extend_from_slice(data);
        b
    }

    // One image, two descriptions of it.
    let image = [0xABu8; 64];
    let dir = tempfile::tempdir().unwrap();
    for (name, title, block) in [
        (
            "a.flac",
            "TITLE=A",
            picture_block("image/jpeg", 1200, 1200, 24, 0, &image),
        ),
        (
            "b.flac",
            "TITLE=B",
            picture_block("image/png", 64, 64, 8, 256, &image),
        ),
    ] {
        let bytes = make_flac(
            &[
                (0, streaminfo_body()),
                (4, vorbis_comment_body("v", &[title])),
                (6, block),
            ],
            &[0xCC; 30],
        );
        std::fs::write(dir.path().join(name), bytes).unwrap();
    }

    let db = Db::open_in_memory().unwrap();
    let stats = scan_directory(&db, dir.path()).unwrap();
    assert_eq!(stats.scanned, 2);

    let link_of = |file: &str| {
        let track = db
            .list_tracks()
            .unwrap()
            .into_iter()
            .find(|t| t.backing_path.ends_with(file))
            .expect("both tracks stored");
        db.get_track_art(track.id)
            .unwrap()
            .into_iter()
            .next()
            .expect("each track links its own picture")
    };
    let a = link_of("a.flac");
    let b = link_of("b.flac");

    // One blob: the deduplication that made this a problem still happens.
    assert_eq!(a.art_id, b.art_id, "byte-identical art is still one row");

    // Two descriptions of it, each its own.
    assert_eq!(
        (a.mime.as_str(), a.width, a.height),
        ("image/jpeg", Some(1200), Some(1200))
    );
    assert_eq!(
        (b.mime.as_str(), b.width, b.height),
        ("image/png", Some(64), Some(64))
    );
    // Depth and colours too: the FLAC parser used to read and discard both.
    assert_eq!((a.depth, a.colors), (24, 0));
    assert_eq!((b.depth, b.colors), (8, 256));
}

/// #746: the V4 migration could only copy each blob's shared metadata onto
/// every link, with FLAC's depth and colours at 0, and promised the real values
/// back from `migrate`'s offer — a revalidate, whose structural pass never
/// touched art. A link the file supplied is not curated metadata, so what the
/// file declares about the picture comes back; a link an external writer made,
/// and the tags, stay as they are.
#[test]
fn revalidate_restores_a_files_own_picture_metadata_and_leaves_curated_art() {
    fn picture_block(mime: &str, side: u32, depth: u32, colors: u32, data: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&3u32.to_be_bytes()); // front cover
        b.extend_from_slice(&u32::try_from(mime.len()).unwrap().to_be_bytes());
        b.extend_from_slice(mime.as_bytes());
        b.extend_from_slice(&0u32.to_be_bytes()); // description
        b.extend_from_slice(&side.to_be_bytes());
        b.extend_from_slice(&side.to_be_bytes());
        b.extend_from_slice(&depth.to_be_bytes());
        b.extend_from_slice(&colors.to_be_bytes());
        b.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
        b.extend_from_slice(data);
        b
    }

    // One image embedded twice and described two ways, so restoring it has to
    // pair the file's pictures with their links by order.
    let image = [0xABu8; 64];
    let dir = tempfile::tempdir().unwrap();
    let bytes = make_flac(
        &[
            (0, streaminfo_body()),
            (4, vorbis_comment_body("v", &["TITLE=A"])),
            (6, picture_block("image/jpeg", 1200, 24, 0, &image)),
            (6, picture_block("image/png", 64, 8, 256, &image)),
        ],
        &[0xCC; 30],
    );
    std::fs::write(dir.path().join("a.flac"), bytes).unwrap();

    let db = Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();
    let track = db.list_tracks().unwrap().remove(0);
    let as_scanned = db.get_track_art(track.id).unwrap();
    assert_eq!(as_scanned.len(), 2);

    // What V4 leaves: one occurrence's values on both links, depth and colours
    // zeroed. Plus a cover an external writer linked, and a curated title.
    let mut links: Vec<musefs_db::TrackArt> = as_scanned
        .iter()
        .map(|link| musefs_db::TrackArt {
            mime: "image/jpeg".to_string(),
            width: Some(1200),
            height: Some(1200),
            depth: 0,
            colors: 0,
            ..link.clone()
        })
        .collect();
    let written = db
        .upsert_art(&musefs_db::NewArt {
            data: vec![0xEE; 16],
        })
        .unwrap();
    links.push(musefs_db::TrackArt {
        art_id: written,
        picture_type: 3,
        description: String::new(),
        mime: "image/webp".to_string(),
        width: None,
        height: None,
        depth: 0,
        colors: 0,
        ordinal: 2,
    });
    db.set_track_art(track.id, &links).unwrap();
    db.replace_tags(track.id, &[Tag::new("title", "Curated", 0)])
        .unwrap();
    // And no recorded inode, as V4 leaves every row, which is what makes
    // revalidate re-probe a file that has not changed.
    db.upsert_track(&musefs_db::NewTrack {
        backing_path: track.backing_path.clone(),
        format: track.format,
        audio_offset: track.bounds.audio_offset(),
        audio_length: track.bounds.audio_length(),
        backing_size: track.backing_size,
        backing_mtime_ns: track.backing_mtime_ns,
        backing_ctime_ns: track.backing_ctime_ns,
        backing_ino: None,
    })
    .unwrap();

    let stats = revalidate(&db, dir.path()).unwrap();
    assert_eq!(stats.updated, 1);

    let restored = db.get_track_art(track.id).unwrap();
    assert_eq!(
        restored[..2],
        as_scanned[..],
        "the file's own links read as a fresh scan wrote them"
    );
    assert_eq!(
        restored[2], links[2],
        "the external writer's link is untouched"
    );
    assert_eq!(db.get_tags(track.id).unwrap()[0].value, "Curated");
}

/// #680: a filename is an arbitrary byte string on Unix, and the scanner used
/// to store `to_string_lossy()` as the row's identity.
///
/// Two failures came out of that, and the second is the serious one. The stored
/// path did not exist on disk, so the track could never be served. And two
/// distinct byte paths converged on one `U+FFFD`-bearing string, where
/// `ON CONFLICT(backing_path) DO UPDATE` silently merged them into a single row
/// — one file's identity carrying the other's metadata, with nothing in the
/// skipped or failed counts to say so.
#[test]
fn two_paths_differing_only_in_invalid_utf8_stay_two_tracks() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let dir = tempfile::tempdir().unwrap();
    // `bad\x80name.flac` and `bad\x81name.flac`: distinct byte strings, and
    // both render as the same `bad<U+FFFD>name.flac` once converted lossily.
    let a = dir.path().join(OsStr::from_bytes(b"bad\x80name.flac"));
    let b = dir.path().join(OsStr::from_bytes(b"bad\x81name.flac"));
    assert_ne!(a, b);

    // Not every filesystem will host such a name. APFS and HFS+ enforce valid
    // UTF-8 and refuse the create with `EILSEQ`, so macOS cannot run this;
    // Linux and FreeBSD take arbitrary bytes and do. Detected by trying rather
    // than by an OS allowlist, because it is a property of the filesystem under
    // the temp directory, not of the platform — the same Linux build hits it on
    // a FAT32 or a network mount.
    if let Err(e) = std::fs::write(&a, b"") {
        eprintln!(
            "skipping two_paths_differing_only_in_invalid_utf8_stay_two_tracks: \
             this filesystem will not accept a non-UTF-8 filename ({e})"
        );
        return;
    }
    assert_eq!(
        a.to_string_lossy(),
        b.to_string_lossy(),
        "the fixture only tests anything if these collide lossily"
    );
    common::write_flac(&a, &["TITLE=A"], &[0xAA; 512]);
    common::write_flac(&b, &["TITLE=B"], &[0xBB; 512]);

    let db = Db::open_in_memory().unwrap();
    let stats = scan_directory(&db, dir.path()).unwrap();
    assert_eq!(stats.scanned, 2, "both files must be stored: {stats:?}");
    assert_eq!(stats.failed, 0, "{stats:?}");

    let tracks = db.list_tracks().unwrap();
    assert_eq!(tracks.len(), 2, "one row each, not one row total");

    // Each row's path is the bytes that were on disk, so it still names a real
    // file — the half that made a mangled row unservable.
    for t in &tracks {
        assert!(
            t.backing_path.exists(),
            "stored path must exist on disk: {:?}",
            t.backing_path
        );
    }
    let mut stored: Vec<_> = tracks.iter().map(|t| t.backing_path.clone()).collect();
    stored.sort();
    let mut expected = vec![a.clone(), b.clone()];
    expected.sort();
    assert_eq!(stored, expected);

    // And the two rows kept their own tags rather than one overwriting the other.
    let title = |p: &std::path::Path| {
        let t = db.get_track_by_path(p).unwrap().expect("row by byte path");
        db.get_tags(t.id)
            .unwrap()
            .into_iter()
            .find(|tag| tag.key == "title")
            .map(|tag| tag.value)
    };
    assert_eq!(title(&a).as_deref(), Some("A"));
    assert_eq!(title(&b).as_deref(), Some("B"));
}
