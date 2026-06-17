use super::*;

#[test]
fn max_art_bytes_is_16_mib_minus_64_kib() {
    assert_eq!(MAX_ART_BYTES, 16_711_680);
}

#[test]
fn scan_caps_match_db_limits() {
    assert_eq!(
        i64::try_from(MAX_ART_BYTES).unwrap(),
        musefs_db::limits::MAX_ART_BYTES
    );
    assert_eq!(
        i64::try_from(MAX_BINARY_TAG_BYTES).unwrap(),
        musefs_db::limits::MAX_BINARY_TAG_BYTES
    );
}

#[test]
fn is_supported_audio_accepts_known_and_rejects_unknown() {
    for ok in [
        "a.flac", "a.mp3", "a.m4a", "a.m4b", "a.ogg", "a.oga", "a.opus", "a.wav",
    ] {
        assert!(
            is_supported_audio(std::path::Path::new(ok)),
            "{ok} should be supported"
        );
    }
    for bad in ["a.txt", "a.png", "a", "a.flacx"] {
        assert!(
            !is_supported_audio(std::path::Path::new(bad)),
            "{bad} must be rejected"
        );
    }
}

#[test]
fn collect_audio_skips_unsupported_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("keep.flac"), b"x").unwrap();
    std::fs::write(dir.path().join("skip.txt"), b"x").unwrap();
    let mut out = Vec::new();
    collect_audio(dir.path(), &mut out, false).unwrap();
    assert_eq!(out.len(), 1);
    assert!(out[0].ends_with("keep.flac"));
}

#[test]
fn scan_options_default_does_not_follow_symlinks() {
    assert!(!ScanOptions::default().follow_symlinks);
}

#[test]
fn collect_audio_follows_symlinked_file_when_enabled() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real.flac");
    std::fs::write(&real, b"x").unwrap();
    let lib = dir.path().join("lib");
    std::fs::create_dir(&lib).unwrap();
    std::os::unix::fs::symlink(&real, lib.join("link.flac")).unwrap();

    let mut on = Vec::new();
    collect_audio(&lib, &mut on, true).unwrap();
    assert_eq!(
        on.len(),
        1,
        "symlinked file should be collected when following"
    );

    let mut off = Vec::new();
    collect_audio(&lib, &mut off, false).unwrap();
    assert!(
        off.is_empty(),
        "symlinked file should be skipped by default"
    );
}

#[test]
fn collect_audio_follows_symlinked_dir_when_enabled() {
    let dir = tempfile::tempdir().unwrap();
    let real_dir = dir.path().join("music");
    std::fs::create_dir(&real_dir).unwrap();
    std::fs::write(real_dir.join("song.flac"), b"x").unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir(&root).unwrap();
    std::os::unix::fs::symlink(&real_dir, root.join("linkdir")).unwrap();

    let mut on = Vec::new();
    collect_audio(&root, &mut on, true).unwrap();
    assert_eq!(
        on.len(),
        1,
        "files under a symlinked dir should be collected"
    );

    let mut off = Vec::new();
    collect_audio(&root, &mut off, false).unwrap();
    assert!(off.is_empty(), "symlinked dir should be skipped by default");
}

#[test]
fn collect_audio_terminates_on_symlink_cycle() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    std::fs::create_dir(&a).unwrap();
    std::fs::write(a.join("song.flac"), b"x").unwrap();
    std::os::unix::fs::symlink(dir.path(), a.join("loop")).unwrap();

    let mut out = Vec::new();
    collect_audio(dir.path(), &mut out, true).unwrap();
    assert_eq!(
        out.iter().filter(|p| p.ends_with("song.flac")).count(),
        1,
        "each real file collected at most once despite the cycle"
    );
}

#[test]
fn collect_audio_skips_broken_symlink_when_following() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("real.flac"), b"x").unwrap();
    std::os::unix::fs::symlink(dir.path().join("nonexistent"), dir.path().join("dangling"))
        .unwrap();

    let mut out = Vec::new();
    let result = collect_audio(dir.path(), &mut out, true);
    assert!(
        result.is_ok(),
        "a dangling symlink must not abort collection"
    );
    assert_eq!(out.len(), 1);
    assert!(out[0].ends_with("real.flac"));
}

#[test]
fn collect_audio_skips_unreadable_subdir_and_continues() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("keep.flac"), b"x").unwrap();
    let locked = dir.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::write(locked.join("hidden.flac"), b"x").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

    // chmod-000 denial is meaningless under root (it bypasses permissions) — skip
    // rather than false-pass when the directory is still readable for us.
    if std::fs::read_dir(&locked).is_ok() {
        eprintln!(
            "skipping collect_audio_skips_unreadable_subdir_and_continues: directory permissions not enforced (running as root?)"
        );
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }

    let mut out = Vec::new();
    let result = collect_audio(dir.path(), &mut out, false);

    // Restore perms so the TempDir can be cleaned up.
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert!(
        result.is_ok(),
        "an unreadable subdirectory must not abort the whole scan"
    );
    assert_eq!(
        out.len(),
        1,
        "the readable sibling file must still be collected"
    );
    assert!(out[0].ends_with("keep.flac"));
}

#[test]
fn scan_stores_canonical_path_through_symlinked_root() {
    // Scanning through a directory symlink must still store the canonical,
    // symlink-resolved backing path, so a later revalidate — which keys on the
    // canonical path — matches it rather than re-probing or pruning (#440).
    let tmp = tempfile::tempdir().unwrap();
    let real = tmp.path().join("real");
    std::fs::create_dir(&real).unwrap();
    write_flac(&real.join("t.flac"), &["ARTIST=A", "TITLE=T"], None);
    let link = tmp.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let db = musefs_db::Db::open_in_memory().unwrap();
    crate::scan_directory(&db, &link).unwrap();

    let track = db.list_tracks().unwrap().into_iter().next().unwrap();
    let expected = std::fs::canonicalize(real.join("t.flac")).unwrap();
    assert_eq!(std::path::Path::new(&track.backing_path), expected);

    let stats = crate::revalidate(&db, &link).unwrap();
    assert_eq!(stats.unchanged, 1, "canonical key must match on revalidate");
    assert_eq!(stats.updated, 0);
    assert_eq!(stats.pruned, 0);
}

#[test]
fn collect_audio_does_not_follow_symlinks_by_default() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("real.flac"), b"x").unwrap();
    let other = dir.path().join("other.flac");
    std::fs::write(&other, b"x").unwrap();
    std::os::unix::fs::symlink(&other, dir.path().join("link.flac")).unwrap();

    let mut out = Vec::new();
    collect_audio(dir.path(), &mut out, false).unwrap();
    assert_eq!(out.len(), 2);
}

#[test]
fn collect_audio_ignores_symlink_to_non_file_target_when_following() {
    use std::os::unix::ffi::OsStrExt;

    let dir = tempfile::tempdir().unwrap();
    // A FIFO is neither a regular file nor a directory, and mkfifo works in
    // restricted sandboxes that deny Unix-socket bind (issue #277).
    let fifo = dir.path().join("fifo");
    let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    #[expect(unsafe_code, reason = "libc::mkfifo FFI; no std equivalent")]
    let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o644) };
    assert_eq!(rc, 0, "mkfifo failed: {}", std::io::Error::last_os_error());

    // Name the link with a supported audio extension so the only thing
    // keeping it out of `out` is the resolved target's is_file() check.
    std::os::unix::fs::symlink(&fifo, dir.path().join("link.flac")).unwrap();

    let mut out = Vec::new();
    collect_audio(dir.path(), &mut out, true).unwrap();
    assert!(
        out.is_empty(),
        "a symlink to a non-file, non-dir target must not be collected"
    );
}

#[test]
fn collect_audio_tallies_direct_special_file_with_audio_extension() {
    use std::os::unix::ffi::OsStrExt;

    let dir = tempfile::tempdir().unwrap();
    // A FIFO named like a track is a special file reached *directly* by the walk
    // (not behind a symlink): it is neither a regular file, dir, nor symlink, so
    // it must be tallied as a skip rather than vanishing without a trace (#544).
    let fifo = dir.path().join("track.flac");
    let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    #[expect(unsafe_code, reason = "libc::mkfifo FFI; no std equivalent")]
    let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o644) };
    assert_eq!(rc, 0, "mkfifo failed: {}", std::io::Error::last_os_error());

    let mut out = Vec::new();
    let tally = collect_audio(dir.path(), &mut out, false).unwrap();
    assert!(out.is_empty(), "a special file must never be collected");
    assert_eq!(
        tally.total, 1,
        "a direct special file must be tallied as skipped"
    );
}

#[test]
fn probe_returns_none_for_supported_ext_with_garbage_contents() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["bad.flac", "bad.mp3", "bad.m4a", "bad.wav", "bad.opus"] {
        let path = dir.path().join(name);
        std::fs::write(&path, b"not a real audio file").unwrap();
        assert!(
            probe_full(&path, b"not a real audio file").is_none(),
            "{name} must skip"
        );
    }
}

fn flac_block(bt: u8, body: &[u8], last: bool) -> Vec<u8> {
    let mut v = vec![(if last { 0x80 } else { 0 }) | (bt & 0x7F)];
    let n: u32 = u32::try_from(body.len()).unwrap();
    v.extend_from_slice(&[
        u8::try_from(n >> 16).unwrap(),
        u8::try_from(n >> 8).unwrap(),
        u8::try_from(n).unwrap(),
    ]);
    v.extend_from_slice(body);
    v
}
fn streaminfo() -> Vec<u8> {
    let mut si = vec![
        0x10, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0A, 0xC4, 0x42, 0xF0, 0x00,
        0x00, 0x00, 0x00,
    ];
    si.extend_from_slice(&[0u8; 16]);
    si
}
fn vorbis_comment(entries: &[&str]) -> Vec<u8> {
    let mut vc = Vec::new();
    let vendor = b"x";
    vc.extend_from_slice(&u32::try_from(vendor.len()).unwrap().to_le_bytes());
    vc.extend_from_slice(vendor);
    vc.extend_from_slice(&u32::try_from(entries.len()).unwrap().to_le_bytes());
    for e in entries {
        vc.extend_from_slice(&u32::try_from(e.len()).unwrap().to_le_bytes());
        vc.extend_from_slice(e.as_bytes());
    }
    vc
}
fn picture(width: u32, height: u32, data: &[u8]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&3u32.to_be_bytes());
    let mime = "image/png";
    b.extend_from_slice(&u32::try_from(mime.len()).unwrap().to_be_bytes());
    b.extend_from_slice(mime.as_bytes());
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(&width.to_be_bytes());
    b.extend_from_slice(&height.to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
    b.extend_from_slice(data);
    b
}
fn write_flac(path: &std::path::Path, entries: &[&str], pic: Option<(u32, u32)>) {
    let mut out = b"fLaC".to_vec();
    out.extend(flac_block(0, &streaminfo(), false));
    let last_is_vc = pic.is_none();
    out.extend(flac_block(4, &vorbis_comment(entries), last_is_vc));
    if let Some((w, h)) = pic {
        out.extend(flac_block(6, &picture(w, h, &[0xAB; 64]), true));
    }
    out.extend_from_slice(&[0xCD; 128]);
    std::fs::write(path, &out).unwrap();
}

#[test]
fn ingest_assigns_sequential_ordinals_per_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("multi.flac");
    write_flac(&path, &["ARTIST=A1", "ARTIST=A2"], None);
    let db = musefs_db::Db::open_in_memory().unwrap();
    crate::scan_directory(&db, &path).unwrap();
    let track = db.list_tracks().unwrap().into_iter().next().unwrap();
    let mut artists: Vec<(u64, String)> = db
        .get_tags(track.id)
        .unwrap()
        .into_iter()
        .filter(|t| t.key.eq_ignore_ascii_case("artist"))
        .map(|t| (t.ordinal, t.value))
        .collect();
    artists.sort();
    assert_eq!(artists, vec![(0, "A1".to_string()), (1, "A2".to_string())]);
}

#[test]
fn ingest_stores_nonzero_art_dimensions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("art.flac");
    write_flac(&path, &["ARTIST=A", "TITLE=T"], Some((10, 20)));
    let db = musefs_db::Db::open_in_memory().unwrap();
    crate::scan_directory(&db, &path).unwrap();
    let track = db.list_tracks().unwrap().into_iter().next().unwrap();
    let ta = db.get_track_art(track.id).unwrap();
    assert_eq!(ta.len(), 1);
    let meta = db.get_art_meta(ta[0].art_id).unwrap().unwrap();
    assert_eq!(meta.width, Some(10));
    assert_eq!(meta.height, Some(20));
}

#[test]
fn ingest_oracle_path_stores_nonzero_art_dimensions() {
    // Drives the single-file `ingest` (not `ingest_bulk`) so the
    // `(pic.width != 0).then_some(..)` dimension guards there are pinned.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("art.flac");
    write_flac(&path, &["ARTIST=A", "TITLE=T"], Some((10, 20)));
    let db = musefs_db::Db::open_in_memory().unwrap();
    crate::scan_directory_full_oracle(&db, &path).unwrap();
    let track = db.list_tracks().unwrap().into_iter().next().unwrap();
    let ta = db.get_track_art(track.id).unwrap();
    assert_eq!(ta.len(), 1);
    let meta = db.get_art_meta(ta[0].art_id).unwrap().unwrap();
    assert_eq!(meta.width, Some(10));
    assert_eq!(meta.height, Some(20));
}

#[test]
fn scan_directory_counts_scanned_failed_and_skipped() {
    let dir = tempfile::tempdir().unwrap();
    write_flac(
        &dir.path().join("ok1.flac"),
        &["ARTIST=A", "TITLE=T1"],
        None,
    );
    write_flac(
        &dir.path().join("ok2.flac"),
        &["ARTIST=A", "TITLE=T2"],
        None,
    );
    // Supported extension, unparseable bytes → a scan failure.
    std::fs::write(dir.path().join("bad.flac"), b"garbage").unwrap();
    // Unsupported extension → skipped at collection, never probed.
    std::fs::write(dir.path().join("notes.txt"), b"hello").unwrap();
    let db = musefs_db::Db::open_in_memory().unwrap();
    let stats = crate::scan_directory(&db, dir.path()).unwrap();
    assert_eq!(stats.scanned, 2);
    assert_eq!(stats.failed, 1);
    assert_eq!(stats.skipped, 1);
}

#[test]
fn probe_file_caught_isolates_parser_panic_as_failed() {
    // A residual parser panic — one the format-layer alloc guards don't catch —
    // must drop just that file (counted as failed), not unwind the scan worker
    // thread and silently truncate the rest of the library (#425). Mirrors the
    // read path's `read_outcome` panic boundary (#359). The after-S1 hook stands
    // in for a parser that panics partway through the probe.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("boom.flac");
    write_flac(&path, &["ARTIST=A", "TITLE=T"], None);
    set_after_s1_hook(|| panic!("parser exploded"));
    let out = probe_file_caught(&path, WINDOW);
    clear_after_s1_hook();
    assert!(matches!(out, Ok(ProbeOutcome::Unparseable)), "got {out:?}");
}

#[test]
fn skip_tally_summary_orders_by_descending_count() {
    let mut tally = super::SkipTally::default();
    for _ in 0..20 {
        tally.record(std::path::Path::new("art/cover.jpg"));
    }
    for _ in 0..10 {
        tally.record(std::path::Path::new("disc.cue"));
    }
    for _ in 0..8 {
        tally.record(std::path::Path::new("rip.log"));
    }
    for _ in 0..4 {
        tally.record(std::path::Path::new("README"));
    }
    assert_eq!(tally.total, 42);
    assert_eq!(
        tally.summary().unwrap(),
        "skipped 42: jpg=20, cue=10, log=8, <none>=4"
    );
}

#[test]
fn skip_tally_lowercases_extension_and_buckets_extensionless() {
    let mut tally = super::SkipTally::default();
    tally.record(std::path::Path::new("a.JPG"));
    tally.record(std::path::Path::new("b.jpg"));
    tally.record(std::path::Path::new("noext"));
    assert_eq!(tally.summary().unwrap(), "skipped 3: jpg=2, <none>=1");
}

#[test]
fn skip_tally_ties_break_by_extension_name() {
    let mut tally = super::SkipTally::default();
    tally.record(std::path::Path::new("a.nfo"));
    tally.record(std::path::Path::new("b.cue"));
    assert_eq!(tally.summary().unwrap(), "skipped 2: cue=1, nfo=1");
}

#[test]
fn skip_tally_empty_has_no_summary() {
    assert!(super::SkipTally::default().summary().is_none());
}

#[test]
fn revalidate_buckets_unchanged_and_prunes_missing() {
    let dir = tempfile::tempdir().unwrap();
    let keep = dir.path().join("keep.flac");
    write_flac(&keep, &["ARTIST=A", "TITLE=T"], None);
    let db = musefs_db::Db::open_in_memory().unwrap();
    crate::scan_directory(&db, dir.path()).unwrap();

    let s1 = crate::revalidate(&db, dir.path()).unwrap();
    assert_eq!(s1.unchanged, 1);
    assert_eq!(s1.updated, 0);
    assert_eq!(s1.pruned, 0);

    std::fs::remove_file(&keep).unwrap();
    let s2 = crate::revalidate(&db, dir.path()).unwrap();
    assert_eq!(s2.pruned, 1);
    assert!(db.list_tracks().unwrap().is_empty());
}

#[test]
fn revalidate_does_not_prune_on_non_notfound_error() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("real.flac");
    write_flac(&file, &["ARTIST=A", "TITLE=T"], None);
    let db = musefs_db::Db::open_in_memory().unwrap();
    crate::scan_directory(&db, dir.path()).unwrap();

    use musefs_db::{Format, NewTrack};
    let track = db.list_tracks().unwrap().into_iter().next().unwrap();
    db.delete_track(track.id).unwrap();
    let canon = std::fs::canonicalize(dir.path()).unwrap();
    let ghost = canon.join("real.flac").join("ghost.flac");
    db.upsert_track(&NewTrack {
        backing_path: ghost.to_string_lossy().into_owned(),
        format: Format::Flac,
        audio_offset: 0,
        audio_length: 0,
        backing_size: 0,
        backing_mtime_ns: 0,
        backing_ctime_ns: 0,
    })
    .unwrap();

    let stats = crate::revalidate(&db, dir.path()).unwrap();
    assert_eq!(stats.pruned, 0, "ENOTDIR is not NotFound → must not prune");
    assert!(
        db.list_tracks()
            .unwrap()
            .iter()
            .any(|t| t.backing_path == ghost.to_string_lossy()),
        "ghost track must still exist"
    );
}

#[test]
fn scan_ingests_binary_tags_and_promotes() {
    use id3::frame::{Content, Popularimeter, Unknown};
    use id3::{Encoder, Frame, Tag, TagLike, Version};

    let dir = tempfile::tempdir().unwrap();

    // Build an MP3 with a PRIV (opaque) + POPM (promoted) tag.
    let mut tag = Tag::new();
    tag.add_frame(Popularimeter {
        user: "u".into(),
        rating: 128,
        counter: 3,
    });
    tag.add_frame(Frame::with_content(
        "PRIV",
        Content::Unknown(Unknown {
            data: vec![1, 1, 2, 3, 5],
            version: Version::Id3v24,
        }),
    ));
    let mut bytes = Vec::new();
    Encoder::new()
        .version(Version::Id3v24)
        .encode(&tag, &mut bytes)
        .unwrap();
    // A real MP3 frame header is enough for locate_audio_bounded to find audio.
    bytes.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00, 0x00, 0x00, 0x00, 0x00]);
    std::fs::write(dir.path().join("a.mp3"), &bytes).unwrap();

    let db = musefs_db::Db::open_in_memory().unwrap();
    crate::scan::scan_directory(&db, dir.path()).unwrap();
    let track = db.list_tracks().unwrap().into_iter().next().unwrap();
    let tid = track.id;

    // Opaque PRIV survives as a binary row.
    let bin = db.get_binary_tags(tid).unwrap();
    assert!(
        bin.iter().any(|r| r.key == "PRIV" && r.byte_len == 5),
        "PRIV not ingested as binary row; got: {bin:?}"
    );

    // POPM promoted into editable text tags.
    let texts = db.get_tags(tid).unwrap();
    assert!(
        texts.iter().any(|t| t.key == "rating" && t.value == "128"),
        "rating not promoted; got: {texts:?}"
    );
    assert!(
        texts.iter().any(|t| t.key == "playcount" && t.value == "3"),
        "playcount not promoted; got: {texts:?}"
    );
}

/// Probed carrying a valid, an empty, and an oversize binary tag. Only the
/// valid one is stored: the filter drops empty (`EmptySegment` would fail
/// layout validation) and oversize (`> MAX_BINARY_TAG_BYTES`) payloads, with
/// gap-free ordinals.
fn probed_with_mixed_binary_tags() -> Probed {
    Probed {
        format: musefs_db::Format::Mp3,
        audio_offset: 0,
        audio_length: 0,
        tags: Vec::new(),
        pictures: Vec::new(),
        binary_tags: vec![
            EmbeddedBinaryTag {
                key: "PRIV".into(),
                payload: vec![1, 2, 3],
            },
            EmbeddedBinaryTag {
                key: "GEOB".into(),
                payload: Vec::new(),
            },
            EmbeddedBinaryTag {
                key: "SYLT".into(),
                payload: vec![0u8; MAX_BINARY_TAG_BYTES + 1],
            },
        ],
        structural_blocks: Vec::new(),
    }
}

#[test]
fn ingest_filters_empty_and_oversize_binary_tags() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.mp3");
    std::fs::write(&path, b"x").unwrap();
    let meta = std::fs::metadata(&path).unwrap();
    let db = Db::open_in_memory().unwrap();

    ingest(
        &db,
        &path.to_string_lossy(),
        &meta,
        probed_with_mixed_binary_tags(),
    )
    .unwrap();

    let tid = db.list_tracks().unwrap()[0].id;
    let rows = db.get_binary_tags(tid).unwrap();
    assert_eq!(
        rows.len(),
        1,
        "only the valid binary tag survives: {rows:?}"
    );
    assert_eq!(rows[0].key, "PRIV");
    assert_eq!(rows[0].byte_len, 3);
}

#[test]
fn ingest_bulk_filters_empty_and_oversize_binary_tags() {
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
            probed_with_mixed_binary_tags(),
        )
        .unwrap();
        bw.commit().unwrap();
    }
    let tid = db.list_tracks().unwrap()[0].id;
    let rows = db.get_binary_tags(tid).unwrap();
    assert_eq!(
        rows.len(),
        1,
        "only the valid binary tag survives: {rows:?}"
    );
    assert_eq!(rows[0].key, "PRIV");
    assert_eq!(rows[0].byte_len, 3);
}

#[test]
fn accept_pictures_keeps_at_cap_and_drops_over_cap() {
    let mk = |len: usize| EmbeddedPicture {
        mime: "image/jpeg".to_string(),
        picture_type: musefs_format::PictureType::new(3).unwrap(),
        description: String::new(),
        width: 0,
        height: 0,
        data: vec![0u8; len],
    };
    // A picture exactly at the cap is kept; one byte over is dropped. The
    // boundary pins `>` against `>=` (an at-cap drop would be silent loss).
    let kept = accept_pictures("/x.flac", vec![mk(MAX_ART_BYTES), mk(MAX_ART_BYTES + 1)]);
    assert_eq!(kept.len(), 1, "exactly the at-cap picture survives");
    assert_eq!(kept[0].data.len(), MAX_ART_BYTES);
}

#[test]
fn accept_binary_tags_keeps_at_cap_and_drops_over_cap() {
    let mk = |len: usize| EmbeddedBinaryTag {
        key: "PRIV".to_string(),
        payload: vec![0u8; len],
    };
    let kept = accept_binary_tags(
        "/x.mp3",
        vec![mk(MAX_BINARY_TAG_BYTES), mk(MAX_BINARY_TAG_BYTES + 1)],
    );
    assert_eq!(kept.len(), 1, "exactly the at-cap binary tag survives");
    assert_eq!(kept[0].payload.len(), MAX_BINARY_TAG_BYTES);
}

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
            ("", "dropped"),        // empty key
            ("a\u{7}b", "dropped"), // control char
            ("a\u{0}b", "dropped"), // embedded NUL — DB CHECK can't see it, the floor can
            ("a=b", "kept"),        // '=' is NOT a floor violation
        ]),
    )
    .unwrap();

    let tid = db.list_tracks().unwrap()[0].id;
    let keys: Vec<String> = db
        .get_tags(tid)
        .unwrap()
        .into_iter()
        .map(|t| t.key)
        .collect();
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
                ("a\u{0}b", "dropped"), // embedded NUL — floor drops it
                ("a=b", "kept"),
            ]),
        )
        .unwrap();
        bw.commit().unwrap();
    }
    let tid = db.list_tracks().unwrap()[0].id;
    let keys: Vec<String> = db
        .get_tags(tid)
        .unwrap()
        .into_iter()
        .map(|t| t.key)
        .collect();
    assert_eq!(keys, vec!["a=b".to_string(), "artist".to_string()]);
}

/// Probed with two structural blocks of the SAME kind, to make the per-kind
/// ordinal increment (`*ord += 1`) observable. A real FLAC carries only one
/// STREAMINFO/SEEKTABLE, so a duplicate kind is the only input under which the
/// second block's ordinal differs from the first; without it the increment's
/// mutants survive.
fn probed_with_duplicate_structural_kind() -> Probed {
    Probed {
        format: musefs_db::Format::Flac,
        audio_offset: 0,
        audio_length: 0,
        tags: Vec::new(),
        pictures: Vec::new(),
        binary_tags: Vec::new(),
        structural_blocks: vec![
            ("SEEKTABLE".to_string(), vec![0xA1]),
            ("SEEKTABLE".to_string(), vec![0xB2]),
        ],
    }
}

#[test]
fn ingest_assigns_sequential_structural_ordinals_per_kind() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.flac");
    std::fs::write(&path, b"x").unwrap();
    let meta = std::fs::metadata(&path).unwrap();
    let db = Db::open_in_memory().unwrap();

    ingest(
        &db,
        &path.to_string_lossy(),
        &meta,
        probed_with_duplicate_structural_kind(),
    )
    .unwrap();

    let tid = db.list_tracks().unwrap()[0].id;
    let got = db.get_structural_blocks(tid).unwrap();
    // Rows come back ORDER BY kind, ordinal: the two same-kind blocks must hold
    // ordinals 0 then 1 (the `-=`/`*=` mutants collapse or invert this).
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].ordinal, 0);
    assert_eq!(got[0].body, vec![0xA1]);
    assert_eq!(got[1].ordinal, 1);
    assert_eq!(got[1].body, vec![0xB2]);
}

/// Probed with two tags of the SAME key, to make the per-key ordinal
/// increment (`*ord += 1` in the tag loop) observable. The production
/// `ingest_bulk` path is exercised with a multi-value tag elsewhere, but the
/// oracle-only `ingest` is not, so without this its tag-ordinal mutants
/// survive. Distinct values under one key: a collapsed ordinal (the `-=`/`*=`
/// mutants) either underflows or duplicates the `(track_id, key, ordinal)`
/// primary key — both observable.
fn probed_with_duplicate_tag_key() -> Probed {
    Probed {
        format: musefs_db::Format::Flac,
        audio_offset: 0,
        audio_length: 0,
        tags: vec![
            ("ARTIST".to_string(), "A".to_string()),
            ("ARTIST".to_string(), "B".to_string()),
        ],
        pictures: Vec::new(),
        binary_tags: Vec::new(),
        structural_blocks: Vec::new(),
    }
}

#[test]
fn ingest_assigns_sequential_tag_ordinals_per_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.flac");
    std::fs::write(&path, b"x").unwrap();
    let meta = std::fs::metadata(&path).unwrap();
    let db = Db::open_in_memory().unwrap();

    ingest(
        &db,
        &path.to_string_lossy(),
        &meta,
        probed_with_duplicate_tag_key(),
    )
    .unwrap();

    let tid = db.list_tracks().unwrap()[0].id;
    let got = db.get_tags(tid).unwrap();
    // get_tags is ORDER BY key, ordinal: the two same-key tags must hold
    // ordinals 0 then 1 (the `-=`/`*=` mutants collapse or invert this).
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].ordinal, 0);
    assert_eq!(got[0].value, "A");
    assert_eq!(got[1].ordinal, 1);
    assert_eq!(got[1].value, "B");
}

#[test]
fn ingest_bulk_assigns_sequential_structural_ordinals_per_kind() {
    let db = Db::open_in_memory().unwrap();
    {
        let mut bw = db.bulk_writer().unwrap();
        ingest_bulk(
            &mut bw,
            "/a.flac",
            BackingStamp {
                size: 1,
                mtime_ns: 0,
                ctime_ns: 0,
            },
            probed_with_duplicate_structural_kind(),
        )
        .unwrap();
        bw.commit().unwrap();
    }
    let tid = db.list_tracks().unwrap()[0].id;
    let got = db.get_structural_blocks(tid).unwrap();
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].ordinal, 0);
    assert_eq!(got[0].body, vec![0xA1]);
    assert_eq!(got[1].ordinal, 1);
    assert_eq!(got[1].body, vec![0xB2]);
}
