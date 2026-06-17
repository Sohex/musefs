use super::*;
use musefs_format::{RegionLayout, Segment};

#[test]
fn fh_round_trips_slab_key_and_maps_full_to_error() {
    // None (slab at capacity) -> HandleTableFull.
    assert!(matches!(fh_from_key(None), Err(CoreError::HandleTableFull)));
    // Wire value is the slab key + 1, so the kernel never sees 0 ("no
    // handle"). Non-zero needs no runtime assertion — NonZeroU64 makes a
    // zero handle unrepresentable.
    assert_eq!(fh_from_key(Some(0)).unwrap().get(), 1);
    assert_eq!(fh_from_key(Some(41)).unwrap().get(), 42);
    // The two private conversion methods invert each other.
    assert_eq!(Fh::from_slab_key(0).slab_key(), 0);
    assert_eq!(Fh::from_slab_key(41).slab_key(), 41);
}

#[test]
fn validate_opened_backing_rejects_mismatched_descriptor_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let expected_path = dir.path().join("expected.flac");
    let replacement_path = dir.path().join("replacement.flac");
    std::fs::write(&expected_path, [1_u8; 8]).unwrap();
    std::fs::write(&replacement_path, [2_u8; 16]).unwrap();
    let expected_meta = std::fs::metadata(&expected_path).unwrap();
    let replacement = std::fs::File::open(&replacement_path).unwrap();

    let resolved = ResolvedFile {
        layout: RegionLayout::validated(vec![Segment::BackingAudio { offset: 0, len: 8 }]).unwrap(),
        total_len: 8,
        track_id: 1,
        content_version: 1,
        backing_path: expected_path,
        stamp: crate::freshness::BackingStamp::from_metadata(&expected_meta),
        mtime_secs: crate::freshness::BackingStamp::from_metadata(&expected_meta).display_secs(),
        last_page: std::sync::Mutex::new(None),
        cache_bytes: 0,
        streams_db_rowid: false,
    };

    assert!(matches!(
        validate_opened_backing(&replacement, &resolved),
        Err(CoreError::BackingChanged(_))
    ));
}

#[test]
fn open_handle_reresolves_after_content_version_bump() {
    use crate::scan::scan_directory;
    use id3::TagLike;
    use std::collections::BTreeMap;

    let dir = tempfile::tempdir().unwrap();
    {
        let mut tag = id3::Tag::new();
        tag.set_artist("Pix");
        tag.set_title("Song");
        let mut bytes = Vec::new();
        tag.write_to(&mut bytes, id3::Version::Id3v24).unwrap();
        bytes.extend_from_slice(&[0xFF, 0xFB, 1, 2, 3, 4]);
        std::fs::write(dir.path().join("a.mp3"), &bytes).unwrap();
    }

    let db_path = dir.path().join("m.db");
    {
        let db = musefs_db::Db::open(&db_path).unwrap();
        scan_directory(&db, dir.path()).unwrap();
    }
    let cfg = MountConfig {
        template: "$artist/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode: Mode::Synthesis,
        poll_interval: std::time::Duration::ZERO,
        case_insensitive: false,
        read_ahead_budget: 64 * 1024 * 1024,
        read_ahead_prefetch: false,
        skip_on_missing: false,
    };
    let fs = Musefs::open(musefs_db::Db::open(&db_path).unwrap(), cfg).unwrap();

    let artist = fs.lookup(VirtualTree::ROOT, "Pix").expect("artist dir");
    let (_, file_inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();
    let fh = fs.open_handle(file_inode).unwrap();
    let len_before = fs.read(file_inode, Some(fh), 0, 1 << 20).unwrap().len();
    assert!(len_before > 0, "baseline read must be non-empty");

    // Out-of-band re-tag: a long comment grows the synthesized ID3v2 region.
    {
        let db = musefs_db::Db::open(&db_path).unwrap();
        let track_id = db.list_tracks().unwrap().into_iter().next().unwrap().id;
        db.replace_tags(
            track_id,
            &[musefs_db::Tag::new("comment", &"x".repeat(4096), 0)],
        )
        .unwrap();
    }
    assert!(
        fs.poll_refresh().unwrap(),
        "poll_refresh must detect the change"
    );

    // Same handle: must re-resolve and serve the larger layout.
    let len_after = fs.read(file_inode, Some(fh), 0, 1 << 20).unwrap().len();
    assert!(
        len_after > len_before,
        "handle did not re-resolve: {len_before} -> {len_after}"
    );
    fs.release_handle(fh);
}

#[test]
fn prefetch_workers_created_only_with_budget_and_flag() {
    use std::collections::BTreeMap;
    let mk = |budget: u64, prefetch: bool| {
        let cfg = MountConfig {
            template: "$artist/$title".to_string(),
            fallbacks: BTreeMap::new(),
            default_fallback: "Unknown".to_string(),
            mode: Mode::Synthesis,
            poll_interval: std::time::Duration::ZERO,
            case_insensitive: false,
            read_ahead_budget: budget,
            read_ahead_prefetch: prefetch,
            skip_on_missing: false,
        };
        Musefs::open(musefs_db::Db::open_in_memory().unwrap(), cfg).unwrap()
    };
    assert!(
        !mk(64 << 20, false).prefetch_workers_active(),
        "default is Phase-1 amplification only"
    );
    assert!(mk(64 << 20, true).prefetch_workers_active(), "flag opts in");
    assert!(
        !mk(0, true).prefetch_workers_active(),
        "budget 0 disables read-ahead entirely"
    );
    assert!(!mk(0, false).prefetch_workers_active());
}

#[test]
fn read_then_release_does_not_leak_budget() {
    use crate::scan::scan_directory;
    use id3::TagLike;
    use std::collections::BTreeMap;

    let dir = tempfile::tempdir().unwrap();
    {
        let mut tag = id3::Tag::new();
        tag.set_artist("Pix");
        tag.set_title("Song");
        let mut bytes = Vec::new();
        tag.write_to(&mut bytes, id3::Version::Id3v24).unwrap();
        bytes.extend_from_slice(&[0xFF, 0xFB, 1, 2, 3, 4]);
        std::fs::write(dir.path().join("a.mp3"), &bytes).unwrap();
    }
    let db_path = dir.path().join("m.db");
    {
        let db = musefs_db::Db::open(&db_path).unwrap();
        scan_directory(&db, dir.path()).unwrap();
    }
    let cfg = MountConfig {
        template: "$artist/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode: Mode::Synthesis,
        poll_interval: std::time::Duration::ZERO,
        case_insensitive: false,
        read_ahead_budget: 64 * 1024 * 1024,
        read_ahead_prefetch: false,
        skip_on_missing: false,
    };
    let fs = Musefs::open(musefs_db::Db::open(&db_path).unwrap(), cfg).unwrap();
    let artist = fs.lookup(VirtualTree::ROOT, "Pix").expect("artist dir");
    let (_, file_inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();
    let fh = fs.open_handle(file_inode).unwrap();
    assert!(
        !fs.read(file_inode, Some(fh), 0, 1 << 20)
            .unwrap()
            .is_empty()
    );
    // The read registers the stream and charges its window; release must
    // deregister and uncharge it. A registration that does not fire on the
    // first read leaks the charge (the buffer is never in the pool to free).
    fs.release_handle(fh);
    assert_eq!(fs.pool_charged(), 0, "release leaked the read-ahead charge");
}

#[test]
fn two_handles_get_distinct_pool_keys() {
    use crate::scan::scan_directory;
    use id3::TagLike;
    use std::collections::BTreeMap;

    let dir = tempfile::tempdir().unwrap();
    {
        let mut tag = id3::Tag::new();
        tag.set_artist("Pix");
        tag.set_title("Song");
        let mut bytes = Vec::new();
        tag.write_to(&mut bytes, id3::Version::Id3v24).unwrap();
        bytes.extend_from_slice(&[0xFF, 0xFB, 1, 2, 3, 4]);
        std::fs::write(dir.path().join("a.mp3"), &bytes).unwrap();
    }
    let db_path = dir.path().join("m.db");
    {
        let db = musefs_db::Db::open(&db_path).unwrap();
        scan_directory(&db, dir.path()).unwrap();
    }
    let cfg = MountConfig {
        template: "$artist/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode: Mode::Synthesis,
        poll_interval: std::time::Duration::ZERO,
        case_insensitive: false,
        read_ahead_budget: 64 * 1024 * 1024,
        read_ahead_prefetch: false,
        skip_on_missing: false,
    };
    let fs = Musefs::open(musefs_db::Db::open(&db_path).unwrap(), cfg).unwrap();
    let artist = fs.lookup(VirtualTree::ROOT, "Pix").expect("artist dir");
    let (_, inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();
    // Two handles on the same inode hold DISTINCT read-ahead buffers, so their
    // pool keys (buffer addresses) must differ. A constant pool_key collides
    // both onto one registry entry: the second registration overwrites the
    // first, so only one of the two charges is freed on release — a leak.
    let fh1 = fs.open_handle(inode).unwrap();
    let fh2 = fs.open_handle(inode).unwrap();
    assert!(!fs.read(inode, Some(fh1), 0, 1 << 20).unwrap().is_empty());
    assert!(!fs.read(inode, Some(fh2), 0, 1 << 20).unwrap().is_empty());
    fs.release_handle(fh1);
    fs.release_handle(fh2);
    assert_eq!(
        fs.pool_charged(),
        0,
        "distinct keys must each free their charge"
    );
}

/// The safety property the transactional `content_version` guard exists to
/// protect: a handle holding a `Segment::BinaryTag { payload_id }` must never
/// serve the bytes of a *different* row that later reused that rowid under the
/// stale layout's framing.
///
/// We free the original PRIV row's rowid and reuse it with a different-length
/// payload **without** calling `poll_refresh`, so `refresh_gen` does not move
/// and the gen-gated re-resolve cannot mask the bug — the content_version
/// guard is the only thing standing between the read and torn bytes. With the
/// guard, a successful read is byte-identical to a fresh resolve of the new DB
/// state (the guard forces a re-resolve on the version mismatch); a clean
/// `Err` is the only other acceptable outcome. Without the guard the stale
/// handle would serve `len_a` bytes off the reused rowid, framed by the old
/// header — neither the original nor a valid new file.
#[test]
fn binary_tag_handle_never_serves_reused_rowid_bytes() {
    use crate::scan::scan_directory;
    use id3::frame::{Content, Unknown};
    use id3::{Encoder, Frame, TagLike, Version};
    use std::collections::BTreeMap;

    let needle_a = [0xDEu8, 0xAD, 0xBE, 0xEF, 0x01, 0x02];
    let needle_b = [0x11u8, 0x22, 0x33]; // different bytes AND different length

    let dir = tempfile::tempdir().unwrap();
    {
        // PRIV-only tag: text frames are omitted because the `id3` crate's
        // reader errors on a `Content::Unknown` frame it round-tripped, which
        // would drop the text tags (the raw binary walker is unaffected). The
        // track therefore renders under the `$artist/$title` fallback path.
        let mut tag = id3::Tag::new();
        tag.add_frame(Frame::with_content(
            "PRIV",
            Content::Unknown(Unknown {
                data: needle_a.to_vec(),
                version: Version::Id3v24,
            }),
        ));
        let mut bytes = Vec::new();
        Encoder::new()
            .version(Version::Id3v24)
            .encode(&tag, &mut bytes)
            .unwrap();
        bytes.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00, 0, 0, 0, 0]);
        std::fs::write(dir.path().join("a.mp3"), &bytes).unwrap();
    }

    let db_path = dir.path().join("m.db");
    {
        let db = musefs_db::Db::open(&db_path).unwrap();
        scan_directory(&db, dir.path()).unwrap();
    }
    let cfg = MountConfig {
        template: "$artist/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode: Mode::Synthesis,
        poll_interval: std::time::Duration::ZERO,
        case_insensitive: false,
        read_ahead_budget: 64 * 1024 * 1024,
        read_ahead_prefetch: false,
        skip_on_missing: false,
    };
    let fs = Musefs::open(musefs_db::Db::open(&db_path).unwrap(), cfg).unwrap();

    let artist = fs
        .lookup(VirtualTree::ROOT, "Unknown")
        .expect("fallback artist dir");
    let (_, file_inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();

    // Open the handle and read the original synthesized file (carries needle_a).
    let fh = fs.open_handle(file_inode).unwrap();
    let whole_a = fs.read(file_inode, Some(fh), 0, 1 << 20).unwrap();
    assert!(
        whole_a.windows(needle_a.len()).any(|w| w == needle_a),
        "baseline must carry the original PRIV body"
    );

    // Out-of-band: free the PRIV row's rowid, then reuse it with a different
    // payload. With no other tag rows present, deleting the PRIV row empties
    // `tags` and the next insert reclaims the freed rowid (plain INTEGER
    // PRIMARY KEY, no AUTOINCREMENT). Both writes bump content_version. No
    // poll_refresh, so refresh_gen stays put — only the guard can catch this.
    {
        let db = musefs_db::Db::open(&db_path).unwrap();
        let track_id = db.list_tracks().unwrap().into_iter().next().unwrap().id;
        db.set_binary_tags(track_id, &[]).unwrap();
        db.set_binary_tags(
            track_id,
            &[musefs_db::BinaryTag {
                key: "PRIV".into(),
                payload: needle_b.to_vec(),
                ordinal: 0,
            }],
        )
        .unwrap();
    }

    // What a freshly resolved handle serves for the *current* DB state.
    let fh2 = fs.open_handle(file_inode).unwrap();
    let whole_b = fs.read(file_inode, Some(fh2), 0, 1 << 20).unwrap();
    fs.release_handle(fh2);
    assert!(
        whole_b.windows(needle_b.len()).any(|w| w == needle_b),
        "fresh resolve must carry the new PRIV body"
    );
    assert!(
        !whole_b.windows(needle_a.len()).any(|w| w == needle_a),
        "fresh resolve must not carry the freed payload"
    );
    assert_ne!(
        whole_a.len(),
        whole_b.len(),
        "test setup: payloads must differ in length to expose stale framing"
    );

    // The stale handle: either a clean error, or — via the guard's forced
    // re-resolve — byte-identical to the fresh resolve. Never torn bytes.
    // Err is acceptable too (the guard can surface a retryable error).
    if let Ok(bytes) = fs.read(file_inode, Some(fh), 0, 1 << 20) {
        assert_eq!(
            bytes, whole_b,
            "stale handle served torn/reused-rowid bytes instead of re-resolving"
        );
    }
    fs.release_handle(fh);
}

/// The per-handle fast-path read loop retries a stale binary-tag layout a
/// bounded number of times (`0..4`) before surfacing a retryable
/// `BackingChanged`, which the FUSE layer maps to `EIO`. A writer
/// tight-looping commits to one track can lose the `content_version` race on
/// every attempt; this pins the exact bound — three forced same-track misses
/// still serve on the final attempt, a fourth exhausts the loop and errors.
/// (#187)
#[test]
fn same_track_retag_storm_exhausts_read_retry_into_backing_changed() {
    use crate::scan::scan_directory;
    use id3::frame::{Content, Unknown};
    use id3::{Encoder, Frame, TagLike, Version};
    use std::collections::BTreeMap;

    let needle = [0xDEu8, 0xAD, 0xBE, 0xEF];
    let dir = tempfile::tempdir().unwrap();
    {
        // PRIV-only tag → a binary-tag layout under the fallback path, so the
        // transactional `content_version` guard (and its test seam) is live.
        let mut tag = id3::Tag::new();
        tag.add_frame(Frame::with_content(
            "PRIV",
            Content::Unknown(Unknown {
                data: needle.to_vec(),
                version: Version::Id3v24,
            }),
        ));
        let mut bytes = Vec::new();
        Encoder::new()
            .version(Version::Id3v24)
            .encode(&tag, &mut bytes)
            .unwrap();
        bytes.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00, 0, 0, 0, 0]);
        std::fs::write(dir.path().join("a.mp3"), &bytes).unwrap();
    }

    let db_path = dir.path().join("m.db");
    {
        let db = musefs_db::Db::open(&db_path).unwrap();
        scan_directory(&db, dir.path()).unwrap();
    }
    let cfg = MountConfig {
        template: "$artist/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode: Mode::Synthesis,
        poll_interval: std::time::Duration::ZERO,
        case_insensitive: false,
        read_ahead_budget: 64 * 1024 * 1024,
        read_ahead_prefetch: false,
        skip_on_missing: false,
    };
    let fs = Musefs::open(musefs_db::Db::open(&db_path).unwrap(), cfg).unwrap();

    let artist = fs
        .lookup(VirtualTree::ROOT, "Unknown")
        .expect("fallback artist dir");
    let (_, file_inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();
    let fh = fs.open_handle(file_inode).unwrap();

    let baseline = fs.read(file_inode, Some(fh), 0, 1 << 20).unwrap();
    assert!(
        baseline.windows(needle.len()).any(|w| w == needle),
        "baseline read must serve the binary-tag layout"
    );

    // bound-1 same-track misses: attempts retry, the final attempt serves.
    fs.force_version_mismatches_for_test(3);
    let after_three = fs
        .read(file_inode, Some(fh), 0, 1 << 20)
        .expect("three retries must still serve on the final attempt");
    assert_eq!(
        after_three, baseline,
        "bytes served after surviving the retries must match the layout"
    );

    // One miss per attempt with none left over: the loop exhausts.
    fs.force_version_mismatches_for_test(4);
    match fs.read(file_inode, Some(fh), 0, 1 << 20) {
        Err(CoreError::BackingChanged(_)) => {}
        other => panic!("exhausted retry must return BackingChanged, got {other:?}"),
    }

    // Seam drained: the handle is otherwise healthy and serves again.
    let recovered = fs.read(file_inode, Some(fh), 0, 1 << 20).unwrap();
    assert_eq!(recovered, baseline, "handle must recover after the storm");
    fs.release_handle(fh);
}

#[test]
fn render_entries_returns_paths_and_snapshot() {
    use crate::scan::scan_directory;
    use id3::TagLike;

    let dir = tempfile::tempdir().unwrap();
    {
        let mut tag = id3::Tag::new();
        tag.set_artist("Pix");
        tag.set_title("Song");
        let mut bytes = Vec::new();
        tag.write_to(&mut bytes, id3::Version::Id3v24).unwrap();
        bytes.extend_from_slice(&[0xFF, 0xFB, 1, 2, 3, 4]);
        std::fs::write(dir.path().join("a.mp3"), &bytes).unwrap();
    }
    let db = musefs_db::Db::open(dir.path().join("m.db")).unwrap();
    scan_directory(&db, dir.path()).unwrap();

    let cfg = MountConfig {
        template: "$artist/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode: Mode::Synthesis,
        poll_interval: std::time::Duration::ZERO,
        case_insensitive: false,
        read_ahead_budget: 64 * 1024 * 1024,
        read_ahead_prefetch: false,
        skip_on_missing: false,
    };

    let (entries, snapshot) = Musefs::render_entries(
        &db,
        &Template::parse(&cfg.template).expect("valid template"),
        &cfg,
    )
    .unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].1, "Pix/Song.mp3");
    let id = entries[0].0;
    assert_eq!(snapshot[&id].path, "Pix/Song.mp3");
    assert!(snapshot[&id].content_version >= 1);
}

#[test]
fn render_entries_skips_tracks_missing_top_level_field_when_enabled() {
    use crate::scan::scan_directory;
    use id3::TagLike;

    let dir = tempfile::tempdir().unwrap();
    let mk = |name: &str, artist: Option<&str>, title: &str| {
        let mut tag = id3::Tag::new();
        if let Some(a) = artist {
            tag.set_artist(a);
        }
        tag.set_title(title);
        let mut bytes = Vec::new();
        tag.write_to(&mut bytes, id3::Version::Id3v24).unwrap();
        bytes.extend_from_slice(&[0xFF, 0xFB, 1, 2, 3, 4]);
        std::fs::write(dir.path().join(name), &bytes).unwrap();
    };
    mk("full.mp3", Some("Pix"), "Song");
    mk("partial.mp3", None, "Lonely");

    let db = musefs_db::Db::open(dir.path().join("m.db")).unwrap();
    scan_directory(&db, dir.path()).unwrap();

    let cfg = MountConfig {
        template: "$artist/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode: Mode::Synthesis,
        poll_interval: std::time::Duration::ZERO,
        case_insensitive: false,
        read_ahead_budget: 64 * 1024 * 1024,
        read_ahead_prefetch: false,
        skip_on_missing: true,
    };

    let (entries, snapshot) = Musefs::render_entries(
        &db,
        &Template::parse(&cfg.template).expect("valid template"),
        &cfg,
    )
    .unwrap();
    assert_eq!(entries.len(), 1, "the artist-less track must be skipped");
    assert_eq!(entries[0].1, "Pix/Song.mp3");
    let id = entries[0].0;
    assert_eq!(snapshot.len(), 1);
    assert!(snapshot.contains_key(&id));
}

#[test]
fn needs_rebuild_flag_forces_full_rebuild_on_next_poll() {
    use crate::scan::scan_directory;
    use id3::TagLike;
    use std::collections::BTreeMap;

    let dir = tempfile::tempdir().unwrap();
    {
        let mut tag = id3::Tag::new();
        tag.set_artist("Pix");
        tag.set_title("Song");
        let mut bytes = Vec::new();
        tag.write_to(&mut bytes, id3::Version::Id3v24).unwrap();
        bytes.extend_from_slice(&[0xFF, 0xFB, 1, 2, 3, 4]);
        std::fs::write(dir.path().join("a.mp3"), &bytes).unwrap();
    }
    let db_path = dir.path().join("m.db");
    {
        let db = musefs_db::Db::open(&db_path).unwrap();
        scan_directory(&db, dir.path()).unwrap();
    }
    let cfg = MountConfig {
        template: "$artist/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode: Mode::Synthesis,
        poll_interval: std::time::Duration::ZERO,
        case_insensitive: false,
        read_ahead_budget: 64 * 1024 * 1024,
        read_ahead_prefetch: false,
        skip_on_missing: false,
    };
    let fs = Musefs::open(musefs_db::Db::open(&db_path).unwrap(), cfg).unwrap();

    // data_version is unchanged since open, so a normal poll is a no-op.
    assert!(!fs.poll_refresh().unwrap(), "baseline poll must be a no-op");

    // Advance data_version out-of-band so the forced rebuild has newer DB state
    // to incorporate and stamp; the trailing normal poll then proves it stamped.
    {
        let db = musefs_db::Db::open(&db_path).unwrap();
        let track_id = db.list_tracks().unwrap().into_iter().next().unwrap().id;
        db.replace_tags(track_id, &[musefs_db::Tag::new("comment", "hi", 0)])
            .unwrap();
    }

    // Simulate recovery from a poisoned VFS-state lock.
    fs.mark_needs_rebuild_for_test();
    assert!(
        fs.needs_rebuild_is_set_for_test(),
        "flag reads set after marking"
    );
    assert!(
        fs.poll_refresh().unwrap(),
        "a set needs_rebuild flag must force a rebuild"
    );
    assert!(
        !fs.needs_rebuild_is_set_for_test(),
        "flag cleared after rebuild"
    );

    // The forced rebuild incorporated the out-of-band write and stamped its
    // data_version, so a subsequent normal poll detects no change.
    assert!(
        !fs.poll_refresh().unwrap(),
        "forced rebuild must stamp data_version (next poll is a no-op)"
    );
}

#[test]
fn failed_forced_rebuild_keeps_needs_rebuild_set() {
    // A forced rebuild that fails must re-arm the request it consumed up front,
    // so the next poll retries instead of leaving the poisoned VFS state only
    // re-validated by a data_version change a quiescent DB may never produce (#369).
    let (_d, fs) = fs_with_poll_interval(std::time::Duration::ZERO);
    fs.mark_needs_rebuild_for_test();
    fs.force_rebuild_errors_for_test(true);
    assert!(
        fs.poll_refresh().is_err(),
        "forced rebuild propagates the rebuild error"
    );
    assert!(
        fs.needs_rebuild_is_set_for_test(),
        "a failed forced rebuild must leave needs_rebuild set for retry"
    );
}

#[test]
fn failed_case_insensitive_rebuild_does_not_arm_needs_rebuild() {
    use crate::scan::scan_directory;
    use id3::TagLike;
    use std::collections::BTreeMap;

    // The case-insensitive poll path also routes through force_full_rebuild, but
    // with needs_rebuild unset. A rebuild failure there must NOT raise the flag:
    // only a request we actually consumed is re-armed, so a transient error
    // can't pin the bypass-backoff rebuild branch into a busy retry (#369).
    let dir = tempfile::tempdir().unwrap();
    {
        let mut tag = id3::Tag::new();
        tag.set_artist("Pix");
        tag.set_title("Song");
        let mut bytes = Vec::new();
        tag.write_to(&mut bytes, id3::Version::Id3v24).unwrap();
        bytes.extend_from_slice(&[0xFF, 0xFB, 1, 2, 3, 4]);
        std::fs::write(dir.path().join("a.mp3"), &bytes).unwrap();
    }
    let db_path = dir.path().join("m.db");
    {
        let db = musefs_db::Db::open(&db_path).unwrap();
        scan_directory(&db, dir.path()).unwrap();
    }
    let cfg = MountConfig {
        template: "$artist/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode: Mode::Synthesis,
        poll_interval: std::time::Duration::ZERO,
        case_insensitive: true,
        read_ahead_budget: 64 * 1024 * 1024,
        read_ahead_prefetch: false,
        skip_on_missing: false,
    };
    let fs = Musefs::open(musefs_db::Db::open(&db_path).unwrap(), cfg).unwrap();

    // Bump data_version so the poll enters the version-changed (case-insensitive)
    // rebuild branch instead of short-circuiting as a no-op.
    {
        let db = musefs_db::Db::open(&db_path).unwrap();
        let track_id = db.list_tracks().unwrap().into_iter().next().unwrap().id;
        db.replace_tags(track_id, &[musefs_db::Tag::new("comment", "hi", 0)])
            .unwrap();
    }
    assert!(
        !fs.needs_rebuild_is_set_for_test(),
        "precondition: needs_rebuild is unset"
    );
    fs.force_rebuild_errors_for_test(true);
    assert!(fs.poll_refresh().is_err(), "rebuild error propagates");
    assert!(
        !fs.needs_rebuild_is_set_for_test(),
        "a failed rebuild with no pending request must not raise needs_rebuild"
    );
}

#[test]
fn poll_read_error_arms_backoff() {
    // A failing data_version read propagates before the `refreshing` CAS; it must
    // still stamp the failed-refresh time so the existing backoff gate suppresses
    // an immediate re-dispatch on the next metadata op (#369).
    let (_d, fs) = fs_with_poll_interval(std::time::Duration::from_hours(1));
    fs.expire_poll_debounce_for_test(); // past the debounce so the read is reached
    fs.force_poll_read_errors_for_test(true);
    assert!(
        fs.poll_refresh().is_err(),
        "a broken poll read propagates the error"
    );
    fs.force_poll_read_errors_for_test(false);
    assert!(
        !fs.poll_due(),
        "the read error must arm the retry backoff, suppressing an immediate re-poll"
    );
    fs.expire_refresh_backoff_for_test();
    assert!(
        fs.poll_due(),
        "past the backoff window the poll is due again"
    );
}

fn fs_with_poll_interval(interval: std::time::Duration) -> (tempfile::TempDir, Musefs) {
    let dir = tempfile::tempdir().unwrap();
    let cfg = MountConfig {
        template: "$artist/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode: Mode::Synthesis,
        poll_interval: interval,
        case_insensitive: false,
        read_ahead_budget: 64 * 1024 * 1024,
        read_ahead_prefetch: false,
        skip_on_missing: false,
    };
    let fs = Musefs::open(musefs_db::Db::open(dir.path().join("m.db")).unwrap(), cfg).unwrap();
    (dir, fs)
}

#[test]
fn poll_due_false_within_interval_true_after_expiry() {
    let (_d, fs) = fs_with_poll_interval(std::time::Duration::from_hours(1));
    assert!(!fs.poll_due(), "fresh open is within the debounce window");
    fs.expire_poll_debounce_for_test();
    assert!(fs.poll_due(), "past the debounce window");
}

#[test]
fn poll_due_true_when_needs_rebuild_regardless_of_interval() {
    let (_d, fs) = fs_with_poll_interval(std::time::Duration::from_hours(1));
    assert!(!fs.poll_due());
    fs.mark_needs_rebuild_for_test();
    assert!(fs.poll_due(), "needs_rebuild bypasses the debounce");
}

#[test]
fn poll_due_true_when_interval_zero() {
    let (_d, fs) = fs_with_poll_interval(std::time::Duration::ZERO);
    assert!(fs.poll_due(), "zero interval disables the debounce");
}

#[test]
fn poll_due_respects_failure_backoff_window() {
    let (_d, fs) = fs_with_poll_interval(std::time::Duration::from_hours(1));
    fs.expire_poll_debounce_for_test(); // get past the debounce gate first
    fs.fail_refresh_now_for_test();
    assert!(!fs.poll_due(), "inside the retry backoff window");
    fs.expire_refresh_backoff_for_test();
    assert!(fs.poll_due(), "past the retry backoff window");
}

#[test]
fn passthrough_fd_exposes_backing_only_in_structure_only() {
    use crate::scan::scan_directory;
    use id3::TagLike;
    use std::collections::BTreeMap;
    use std::os::fd::AsFd;
    use std::os::unix::fs::MetadataExt;

    let dir = tempfile::tempdir().unwrap();
    {
        let mut tag = id3::Tag::new();
        tag.set_artist("Pix");
        tag.set_title("Song");
        let mut bytes = Vec::new();
        tag.write_to(&mut bytes, id3::Version::Id3v24).unwrap();
        bytes.extend_from_slice(&[0xFF, 0xFB, 1, 2, 3, 4]);
        std::fs::write(dir.path().join("a.mp3"), &bytes).unwrap();
    }
    let db_path = dir.path().join("m.db");
    {
        let db = musefs_db::Db::open(&db_path).unwrap();
        scan_directory(&db, dir.path()).unwrap();
    }
    let cfg = |mode| MountConfig {
        template: "$artist/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode,
        poll_interval: std::time::Duration::ZERO,
        case_insensitive: false,
        read_ahead_budget: 64 * 1024 * 1024,
        read_ahead_prefetch: false,
        skip_on_missing: false,
    };

    // StructureOnly: exposed, and the fd refers to the backing inode.
    let fs = Musefs::open(
        musefs_db::Db::open(&db_path).unwrap(),
        cfg(Mode::StructureOnly),
    )
    .unwrap();
    let artist = fs.lookup(VirtualTree::ROOT, "Pix").expect("artist dir");
    let (_, file_inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();
    let fh = fs.open_handle(file_inode).unwrap();
    let pfd = fs
        .passthrough_fd(fh)
        .expect("StructureOnly exposes the backing fd");
    let fd_meta = std::fs::File::from(pfd.as_fd().try_clone_to_owned().unwrap())
        .metadata()
        .unwrap();
    let backing_meta = std::fs::metadata(dir.path().join("a.mp3")).unwrap();
    assert_eq!(
        (fd_meta.dev(), fd_meta.ino()),
        (backing_meta.dev(), backing_meta.ino()),
        "passthrough fd must be the backing file"
    );

    // A released handle no longer resolves.
    fs.release_handle(fh);
    assert!(fs.passthrough_fd(fh).is_none());

    // Synthesis: never exposed, even for a live handle.
    let fs = Musefs::open(musefs_db::Db::open(&db_path).unwrap(), cfg(Mode::Synthesis)).unwrap();
    let artist = fs.lookup(VirtualTree::ROOT, "Pix").expect("artist dir");
    let (_, file_inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();
    let fh = fs.open_handle(file_inode).unwrap();
    assert!(fs.passthrough_fd(fh).is_none());
}

#[test]
fn order_entries_sorts_ascending_by_id() {
    // A real Db never hands render_entries id-unordered rows (list_tracks is
    // ORDER BY id), so this descending input is constructed directly to pin
    // the sort itself. Deleting/mutating order_entries' sort fails this test.
    let unordered = vec![
        (9_i64, "z.flac".to_string()),
        (2_i64, "a.flac".to_string()),
        (5_i64, "m.flac".to_string()),
    ];
    let ordered = Musefs::order_entries(unordered);
    let ids: Vec<i64> = ordered.iter().map(|(id, _)| *id).collect();
    assert_eq!(
        ids,
        vec![2, 5, 9],
        "order_entries must sort ascending by id"
    );
    // The pairing is preserved, not just the id column.
    assert_eq!(
        ordered,
        vec![
            (2_i64, "a.flac".to_string()),
            (5_i64, "m.flac".to_string()),
            (9_i64, "z.flac".to_string()),
        ]
    );
}

#[test]
fn full_rebuild_gives_bare_colliding_name_to_lower_id() {
    use musefs_db::{Format, NewTrack, Tag};
    use std::collections::BTreeMap;

    let db = musefs_db::Db::open_in_memory().unwrap();
    // Two tracks whose `$title` both render to "Same" -> colliding "Same.flac".
    // Insertion order fixes ascending ids: id_a < id_b.
    let id_a = db
        .upsert_track(&NewTrack {
            backing_path: "/a.flac".into(),
            format: Format::Flac,
            audio_offset: 0,
            audio_length: 1,
            backing_size: 1,
            backing_mtime_ns: 0,
            backing_ctime_ns: 0,
        })
        .unwrap();
    let id_b = db
        .upsert_track(&NewTrack {
            backing_path: "/b.flac".into(),
            format: Format::Flac,
            audio_offset: 0,
            audio_length: 1,
            backing_size: 1,
            backing_mtime_ns: 0,
            backing_ctime_ns: 0,
        })
        .unwrap();
    assert!(id_a < id_b, "insertion assigns ascending ids");
    db.replace_tags(id_a, &[Tag::new("title", "Same", 0)])
        .unwrap();
    db.replace_tags(id_b, &[Tag::new("title", "Same", 0)])
        .unwrap();

    let config = MountConfig {
        template: "$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode: Mode::Synthesis,
        poll_interval: std::time::Duration::ZERO,
        case_insensitive: false,
        read_ahead_budget: 64 * 1024 * 1024,
        read_ahead_prefetch: false,
        skip_on_missing: false,
    };
    let template = Template::parse(&config.template).expect("valid template");

    let mut alloc = InodeAllocator::new(false);
    let (tree, _snapshot) = Musefs::build_full(&db, &template, &config, &mut alloc).unwrap();

    let root = VirtualTree::ROOT;
    let bare = tree.lookup(root, "Same.flac").expect("bare name exists");
    let suffixed = tree
        .lookup(root, "Same (2).flac")
        .expect("suffixed name exists");
    // The LOWER id owns the bare name; the higher id is disambiguated. This
    // matches the incremental path's min-id rule (tree.rs introducing_id).
    assert_eq!(tree.inode_of_track(id_a), Some(bare));
    assert_eq!(tree.inode_of_track(id_b), Some(suffixed));
}

#[test]
fn getattr_size_cache_hit_detects_backing_change() {
    use crate::scan::scan_directory;
    use id3::TagLike;
    use std::collections::BTreeMap;

    let dir = tempfile::tempdir().unwrap();
    let backing = dir.path().join("a.mp3");
    {
        let mut tag = id3::Tag::new();
        tag.set_artist("Pix");
        tag.set_title("Song");
        let mut bytes = Vec::new();
        tag.write_to(&mut bytes, id3::Version::Id3v24).unwrap();
        bytes.extend_from_slice(&[0xFF, 0xFB, 1, 2, 3, 4]);
        std::fs::write(&backing, &bytes).unwrap();
    }

    let db_path = dir.path().join("m.db");
    {
        let db = musefs_db::Db::open(&db_path).unwrap();
        scan_directory(&db, dir.path()).unwrap();
    }
    let cfg = MountConfig {
        template: "$artist/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode: Mode::Synthesis,
        poll_interval: std::time::Duration::ZERO,
        case_insensitive: false,
        read_ahead_budget: 64 * 1024 * 1024,
        read_ahead_prefetch: false,
        skip_on_missing: false,
    };
    let fs = Musefs::open(musefs_db::Db::open(&db_path).unwrap(), cfg).unwrap();

    let artist = fs.lookup(VirtualTree::ROOT, "Pix").expect("artist dir");
    let (_, file_inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();

    // First getattr populates size_cache (miss path: full resolve).
    let attr1 = fs.getattr(file_inode).unwrap();
    assert!(attr1.size > 0, "baseline attr must be non-empty");

    // Second getattr with the file unchanged is a clean cache hit.
    let attr2 = fs.getattr(file_inode).unwrap();
    assert_eq!(attr1.size, attr2.size, "unchanged backing must stay a hit");

    // Change the backing file out-of-band, without any DB write — so
    // content_version is unchanged and the size_cache would otherwise hit.
    {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&backing)
            .unwrap();
        f.write_all(&[0u8; 64]).unwrap();
    }

    // getattr must now refuse to advertise stale attrs.
    assert!(
        matches!(fs.getattr(file_inode), Err(CoreError::BackingChanged(_))),
        "getattr must degrade to BackingChanged after an on-disk backing change"
    );
}

#[test]
fn open_rejects_template_with_control_byte() {
    let db = musefs_db::Db::open_in_memory().unwrap();
    let config = MountConfig {
        template: "a\0b/$title".to_string(),
        fallbacks: std::collections::BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode: Mode::Synthesis,
        poll_interval: std::time::Duration::ZERO,
        case_insensitive: false,
        read_ahead_budget: 64 * 1024 * 1024,
        read_ahead_prefetch: false,
        skip_on_missing: false,
    };
    assert!(matches!(
        Musefs::open(db, config),
        Err(crate::CoreError::InvalidTemplate(_))
    ));
}

#[test]
fn telemetry_counts_open_handles() {
    use crate::scan::scan_directory;
    use id3::TagLike;
    use std::collections::BTreeMap;

    let dir = tempfile::tempdir().unwrap();
    {
        let mut tag = id3::Tag::new();
        tag.set_artist("Pix");
        tag.set_title("Song");
        let mut bytes = Vec::new();
        tag.write_to(&mut bytes, id3::Version::Id3v24).unwrap();
        bytes.extend_from_slice(&[0xFF, 0xFB, 1, 2, 3, 4]);
        std::fs::write(dir.path().join("a.mp3"), &bytes).unwrap();
    }
    let db_path = dir.path().join("m.db");
    {
        let db = musefs_db::Db::open(&db_path).unwrap();
        scan_directory(&db, dir.path()).unwrap();
    }
    let cfg = MountConfig {
        template: "$artist/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode: Mode::Synthesis,
        poll_interval: std::time::Duration::ZERO,
        case_insensitive: false,
        read_ahead_budget: 64 * 1024 * 1024,
        read_ahead_prefetch: false,
        skip_on_missing: false,
    };
    let fs = Musefs::open(musefs_db::Db::open(&db_path).unwrap(), cfg).unwrap();
    let artist = fs.lookup(VirtualTree::ROOT, "Pix").expect("artist dir");
    let (_, file_inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();

    let base = fs.telemetry().handles_open;
    let fh = fs.open_handle(file_inode).unwrap();
    assert_eq!(fs.telemetry().handles_open, base + 1);
    fs.release_handle(fh);
    assert_eq!(fs.telemetry().handles_open, base);
}
