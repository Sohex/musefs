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

/// #684: under `--follow-symlinks` the stored path and the probed bytes come from
/// one resolution. The hook retargets the symlink to a different file after the
/// worker resolves the walked name and before it probes; the row must still be
/// entirely the original target's. Were the probe to read the walked name again,
/// it would read the new target and store its geometry against the old path.
#[test]
fn a_symlink_retargeted_after_resolution_cannot_split_path_from_geometry() {
    let library = tempfile::tempdir().unwrap();
    let targets = tempfile::tempdir().unwrap();
    let first = targets.path().join("first.flac");
    let second = targets.path().join("second.flac");
    write_flac(&first, &["TITLE=First"], None);
    write_flac(
        &second,
        &[
            "TITLE=Second",
            "ARTIST=long enough to move the audio offset",
        ],
        None,
    );
    let link = library.path().join("link.flac");
    std::os::unix::fs::symlink(&first, &link).unwrap();

    let walked = std::fs::canonicalize(library.path())
        .unwrap()
        .join("link.flac");
    let (retarget_link, retarget_to) = (link.clone(), second.clone());
    set_after_resolve_hook(walked, move || {
        std::fs::remove_file(&retarget_link).unwrap();
        std::os::unix::fs::symlink(&retarget_to, &retarget_link).unwrap();
    });
    let db = musefs_db::Db::open_in_memory().unwrap();
    let options = ScanOptions {
        follow_symlinks: true,
        ..Default::default()
    };
    let scanned = crate::scan_directory_with(&db, library.path(), &options);
    clear_after_resolve_hook();
    assert_eq!(scanned.unwrap().scanned, 1);
    assert_eq!(
        std::fs::read_link(&link).unwrap(),
        second,
        "the hook must actually have retargeted the link"
    );

    let track = db.list_tracks().unwrap().into_iter().next().unwrap();
    let meta = std::fs::metadata(&first).unwrap();
    assert_eq!(track.backing_path, std::fs::canonicalize(&first).unwrap());
    assert_eq!(track.backing_size, meta.len());
    let expected = probe_full(&first, &std::fs::read(&first).unwrap()).unwrap();
    assert_eq!(track.bounds.audio_offset(), expected.audio_offset);
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
    // On the link, not the blob row: the dimensions describe this file's
    // picture block (#716).
    assert_eq!(ta[0].width, Some(10));
    assert_eq!(ta[0].height, Some(20));
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
    // On the link, not the blob row: the dimensions describe this file's
    // picture block (#716).
    assert_eq!(ta[0].width, Some(10));
    assert_eq!(ta[0].height, Some(20));
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
    let out = probe_file_caught(&path, WINDOW, ChecksumTier::Fingerprint);
    clear_after_s1_hook();
    match out {
        Ok(ProbeOutcome::Failed(f)) => assert_eq!(
            f.reason,
            SkipReason::Panicked,
            "a caught panic must have its own bucket, not merge into unparseable"
        ),
        other => panic!("got {other:?}"),
    }
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

/// `SkipReason` indexes `FailureTally`'s array by its discriminant, so `ALL`
/// must list the variants in declaration order — a mismatch would silently
/// credit one reason's skips to another.
#[test]
fn skip_reason_all_is_in_discriminant_order() {
    for (i, reason) in SkipReason::ALL.iter().enumerate() {
        assert_eq!(*reason as usize, i, "{reason:?} is out of order in ALL");
    }
}

/// Every reason belongs to exactly one summary group, or (for `Raced`) to none
/// deliberately: an accidentally ungrouped reason would be counted but never
/// reported.
#[test]
fn skip_reason_groups_partition_all_reasons_except_raced() {
    for reason in SkipReason::ALL {
        let in_failed = SkipReason::FAILED.contains(&reason);
        let in_walk = SkipReason::WALK.contains(&reason);
        if reason == SkipReason::Raced {
            assert!(
                !in_failed && !in_walk,
                "a race is reported whole as ScanStats::raced, not in a breakdown"
            );
        } else {
            assert!(
                in_failed ^ in_walk,
                "{reason:?} must be in exactly one summary group"
            );
        }
    }
}

/// The line the issue asked for: `failed N` explained by reason, ordered by
/// descending count with ties broken by name (matching the extension tally).
#[test]
fn failure_tally_breaks_down_failed_by_reason() {
    let tally = super::FailureTally::default();
    for _ in 0..30 {
        tally.record(SkipReason::Unparseable, format_args!("x"));
    }
    for _ in 0..5 {
        tally.record(SkipReason::Io, format_args!("x"));
    }
    for _ in 0..2 {
        tally.record(SkipReason::Oversize, format_args!("x"));
    }
    assert_eq!(
        tally.failed_summary().unwrap(),
        "failed 37: unparseable=30, io=5, oversize=2"
    );
    // Failure reasons are not walk errors, and neither breakdown may leak into
    // the other.
    assert!(tally.walk_summary().is_none());
}

#[test]
fn failure_tally_omits_empty_buckets_and_breaks_ties_by_reason_name() {
    let tally = super::FailureTally::default();
    tally.record(SkipReason::Panicked, format_args!("x"));
    tally.record(SkipReason::Io, format_args!("x"));
    assert_eq!(
        tally.failed_summary().unwrap(),
        "failed 2: io=1, panicked=1"
    );
}

/// A race is already reported whole as `ScanStats::raced`, and its breakdown
/// would only repeat its own name — but it is still counted, because that is
/// what caps its warns.
#[test]
fn failure_tally_counts_races_without_folding_them_into_failed() {
    let tally = super::FailureTally::default();
    for _ in 0..3 {
        tally.record(SkipReason::Raced, format_args!("x"));
    }
    assert_eq!(tally.count(SkipReason::Raced), 3);
    assert!(tally.failed_summary().is_none());
    assert!(tally.walk_summary().is_none());
}

/// Walk-time errors get their own line: they are counted in no `ScanStats`
/// field, so without it an unreadable subtree leaves no trace but its (capped)
/// per-entry warns.
#[test]
fn failure_tally_summarizes_walk_errors_separately() {
    let tally = super::FailureTally::default();
    for _ in 0..9 {
        tally.record(SkipReason::WalkUnreadable, format_args!("x"));
    }
    for _ in 0..3 {
        tally.record(SkipReason::WalkSymlink, format_args!("x"));
    }
    assert_eq!(
        tally.walk_summary().unwrap(),
        "walk errors 12: unreadable=9, symlink=3"
    );
    assert!(tally.failed_summary().is_none());
}

#[test]
fn failure_tally_empty_has_no_summaries() {
    let tally = super::FailureTally::default();
    assert!(tally.failed_summary().is_none());
    assert!(tally.walk_summary().is_none());
}

/// The cap: the first `SCAN_WARN_BURST` of a reason are logged at their own
/// level, one line announces the downgrade, and the rest are debug — so a
/// vanished share cannot emit one warn per file (#651).
#[test]
fn warn_budget_caps_each_reason_after_the_burst() {
    let tally = super::FailureTally::default();
    for i in 0..SCAN_WARN_BURST {
        assert_eq!(
            tally.decide(SkipReason::Unparseable),
            super::WarnBudget::Spend(log::Level::Warn),
            "skip {i} is still within the burst"
        );
    }
    assert_eq!(
        tally.decide(SkipReason::Unparseable),
        super::WarnBudget::Exhausted,
        "the first over-budget skip must announce the downgrade"
    );
    for _ in 0..100 {
        assert_eq!(
            tally.decide(SkipReason::Unparseable),
            super::WarnBudget::Over,
            "the announcement must be made once, not per file"
        );
    }
    assert_eq!(
        tally.count(SkipReason::Unparseable),
        SCAN_WARN_BURST + 101,
        "capped warns must still be counted in full"
    );
}

/// The budget is per reason: a flood of unparseable files must not hide the
/// first I/O error behind it.
#[test]
fn warn_budget_is_spent_per_reason() {
    let tally = super::FailureTally::default();
    for _ in 0..SCAN_WARN_BURST * 2 {
        tally.decide(SkipReason::Unparseable);
    }
    assert_eq!(
        tally.decide(SkipReason::Io),
        super::WarnBudget::Spend(log::Level::Warn)
    );
}

/// A caught parser panic is a musefs bug, not a property of the library, so its
/// in-budget lines keep `error` while everything else warns.
#[test]
fn warn_budget_keeps_caught_panics_at_error() {
    let tally = super::FailureTally::default();
    assert_eq!(
        tally.decide(SkipReason::Panicked),
        super::WarnBudget::Spend(log::Level::Error)
    );
    assert_eq!(
        tally.decide(SkipReason::WalkUnreadable),
        super::WarnBudget::Spend(log::Level::Warn)
    );
}

/// The walk's failure tally must see entries it could not read, so an
/// unreadable subtree is summarized rather than only warned about per entry.
#[test]
fn collect_audio_records_an_unreadable_subdir_in_the_failure_tally() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let locked = dir.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::write(locked.join("a.flac"), b"x").unwrap();
    let reachable = dir.path().join("b.flac");
    std::fs::write(&reachable, b"x").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read_dir(&locked).is_ok() {
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        eprintln!(
            "skipping collect_audio_records_an_unreadable_subdir_in_the_failure_tally: directory permissions not enforced (running as root?)"
        );
        return;
    }

    let failures = super::FailureTally::default();
    let mut out = Vec::new();
    let skips = collect_audio_with(dir.path(), &mut out, false, None, &failures).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(
        out,
        vec![reachable],
        "the readable file must still be found"
    );
    assert_eq!(
        failures.walk_summary().unwrap(),
        "walk errors 1: unreadable=1"
    );
    assert!(
        failures.failed_summary().is_none(),
        "a directory the walk could not read is not a file that failed to ingest"
    );
    assert_eq!(
        skips.total, 0,
        "walk errors must stay out of the extension tally that feeds ScanStats::skipped"
    );
}

/// An unparseable backing file must arrive at the caller as a tallyable
/// `Failure`, not as a warn the probe already spent.
#[test]
fn probe_reports_unparseable_with_its_reason() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bad.flac");
    std::fs::write(&path, b"not a real audio file").unwrap();
    match probe_file(&path, WINDOW, ChecksumTier::Fingerprint).unwrap() {
        ProbeOutcome::Failed(f) => {
            assert_eq!(f.reason, SkipReason::Unparseable);
            assert!(
                f.message.contains("no parseable audio metadata"),
                "got {:?}",
                f.message
            );
        }
        other => panic!("got {other:?}"),
    }
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
    assert_eq!(s2.pruned, 0);
    assert_eq!(db.list_tracks().unwrap().len(), 1);

    let s3 = crate::revalidate_with(
        &db,
        dir.path(),
        &ScanOptions {
            prune: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(s3.pruned, 1);
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
        backing_path: ghost.clone(),
        format: Format::Flac,
        audio_offset: 0,
        audio_length: 0,
        backing_size: 0,
        backing_mtime_ns: 0,
        backing_ctime_ns: 0,
        backing_ino: None,
    })
    .unwrap();

    let stats = crate::revalidate(&db, dir.path()).unwrap();
    assert_eq!(stats.pruned, 0, "ENOTDIR is not NotFound → must not prune");
    assert!(
        db.list_tracks()
            .unwrap()
            .iter()
            .any(|t| t.backing_path == ghost),
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

/// Probed carrying a valid and an empty binary tag. Only the valid one is
/// stored: an empty payload carries nothing to serve (and `EmptySegment` would
/// fail layout validation), so dropping it costs the user nothing. Oversize
/// payloads are a different matter and no longer appear here — they fail the
/// whole file (#644), asserted separately below.
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
        ],
        structural_blocks: Vec::new(),
    }
}

/// `probed_with_mixed_binary_tags` plus one payload a single byte over the cap.
fn probed_with_oversize_binary_tag() -> Probed {
    let mut p = probed_with_mixed_binary_tags();
    p.binary_tags.push(EmbeddedBinaryTag {
        key: "SYLT".into(),
        payload: vec![0u8; MAX_BINARY_TAG_BYTES + 1],
    });
    p
}

#[test]
fn ingest_filters_empty_binary_tags() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.mp3");
    std::fs::write(&path, b"x").unwrap();
    let meta = std::fs::metadata(&path).unwrap();
    let db = Db::open_in_memory().unwrap();

    ingest(&db, &path, &meta, probed_with_mixed_binary_tags()).unwrap();

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

/// #644: an oversize payload is not quietly omitted from an otherwise-stored
/// track. The direct `ingest` entry point rejects the whole file, so a caller
/// cannot end up with a track that is silently missing data.
#[test]
fn ingest_rejects_a_file_with_an_oversize_binary_tag() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.mp3");
    std::fs::write(&path, b"x").unwrap();
    let meta = std::fs::metadata(&path).unwrap();
    let db = Db::open_in_memory().unwrap();

    let err = ingest(&db, &path, &meta, probed_with_oversize_binary_tag())
        .expect_err("an oversize binary tag must fail the file");

    let msg = err.to_string();
    assert!(msg.contains("SYLT"), "names the offending tag: {msg}");
    assert!(
        msg.contains(&MAX_BINARY_TAG_BYTES.to_string()),
        "quotes the limit: {msg}"
    );
    // Nothing partial was written.
    assert!(
        db.list_tracks().unwrap().is_empty(),
        "no track row survives"
    );
}

#[test]
fn ingest_bulk_filters_empty_binary_tags() {
    let db = Db::open_in_memory().unwrap();
    {
        let mut bw = db.bulk_writer().unwrap();
        ingest_bulk(
            &mut bw,
            Path::new("/a.mp3"),
            BackingStamp {
                size: 1,
                mtime_ns: 0,
                ctime_ns: 0,
                ino: None,
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
fn ingest_bulk_rejects_a_file_with_an_oversize_binary_tag() {
    let db = Db::open_in_memory().unwrap();
    let mut bw = db.bulk_writer().unwrap();
    let err = ingest_bulk(
        &mut bw,
        Path::new("/a.mp3"),
        BackingStamp {
            size: 1,
            mtime_ns: 0,
            ctime_ns: 0,
            ino: None,
        },
        probed_with_oversize_binary_tag(),
    )
    .expect_err("an oversize binary tag must fail the file");
    assert!(err.to_string().contains("SYLT"), "{err}");
}

fn picture_of_len(len: usize) -> EmbeddedPicture {
    EmbeddedPicture {
        mime: "image/jpeg".to_string(),
        picture_type: musefs_format::PictureType::new(3).unwrap(),
        description: String::new(),
        width: 0,
        height: 0,
        depth: 0,
        colors: 0,
        data: vec![0u8; len],
    }
}

fn probed_with_pictures(pictures: Vec<EmbeddedPicture>) -> Probed {
    Probed {
        format: musefs_db::Format::Mp3,
        audio_offset: 0,
        audio_length: 0,
        tags: Vec::new(),
        pictures,
        binary_tags: Vec::new(),
        structural_blocks: Vec::new(),
    }
}

/// The cap boundary is inclusive on both sides of every field, pinning each
/// `>` against a `>=`/`==` mutant. An at-cap rejection would fail a file that
/// stores fine; an over-cap acceptance is the #644 crash.
#[test]
fn check_storable_accepts_art_at_cap_and_rejects_one_over() {
    assert!(
        check_storable(
            Path::new("/x.flac"),
            &probed_with_pictures(vec![picture_of_len(MAX_ART_BYTES)])
        )
        .is_ok()
    );
    let err = check_storable(
        Path::new("/x.flac"),
        &probed_with_pictures(vec![picture_of_len(MAX_ART_BYTES + 1)]),
    )
    .expect_err("one byte over the art cap fails the file");
    assert!(err.to_string().contains("/x.flac"), "names the file: {err}");
}

#[test]
fn check_storable_accepts_binary_tag_at_cap_and_rejects_one_over() {
    let mk = |len: usize| {
        let mut p = probed_with_pictures(Vec::new());
        p.binary_tags = vec![EmbeddedBinaryTag {
            key: "PRIV".to_string(),
            payload: vec![0u8; len],
        }];
        p
    };
    assert!(check_storable(Path::new("/x.mp3"), &mk(MAX_BINARY_TAG_BYTES)).is_ok());
    assert!(check_storable(Path::new("/x.mp3"), &mk(MAX_BINARY_TAG_BYTES + 1)).is_err());
}

#[test]
fn check_storable_accepts_tag_value_at_cap_and_rejects_one_over() {
    let cap = usize::try_from(musefs_db::limits::MAX_TAG_VALUE_LEN).unwrap();
    // MP3, so the FLAC block-total check does not confound the per-value one.
    let mk = |len: usize| probed_with_text_tags(&[("LYRICS", &"v".repeat(len))]);
    assert!(check_storable(Path::new("/x.mp3"), &mk(cap)).is_ok());
    let err = check_storable(Path::new("/x.mp3"), &mk(cap + 1))
        .expect_err("one byte over the tag-value cap fails the file");
    let msg = err.to_string();
    assert!(msg.contains("LYRICS"), "names the tag: {msg}");
    assert!(msg.contains(&cap.to_string()), "quotes the limit: {msg}");
}

#[test]
fn check_storable_accepts_tag_key_at_cap_and_rejects_one_over() {
    let cap = usize::try_from(musefs_db::limits::MAX_TAG_KEY_LEN).unwrap();
    let at = "k".repeat(cap);
    let over = "k".repeat(cap + 1);
    assert!(check_storable(Path::new("/x.mp3"), &probed_with_text_tags(&[(&at, "v")])).is_ok());
    assert!(check_storable(Path::new("/x.mp3"), &probed_with_text_tags(&[(&over, "v")])).is_err());
}

#[test]
fn check_storable_accepts_art_description_at_cap_and_rejects_one_over() {
    let cap = usize::try_from(musefs_db::limits::MAX_ART_DESCRIPTION_LEN).unwrap();
    let mk = |len: usize| {
        let mut pic = picture_of_len(1);
        pic.description = "d".repeat(len);
        probed_with_pictures(vec![pic])
    };
    assert!(check_storable(Path::new("/x.flac"), &mk(cap)).is_ok());
    assert!(check_storable(Path::new("/x.flac"), &mk(cap + 1)).is_err());
}

/// The TEXT caps are compared against SQLite `length()`, which counts
/// characters, not bytes. Counting bytes here would reject a legal multibyte
/// key/description the `CHECK` would have accepted — failing a file for a limit
/// it does not exceed, the mirror image of the bug #644 reported.
#[test]
fn check_storable_counts_characters_not_bytes_for_text_caps() {
    let cap = usize::try_from(musefs_db::limits::MAX_TAG_KEY_LEN).unwrap();
    let key = "é".repeat(cap); // `cap` chars, 2 * cap bytes
    assert!(key.len() > cap, "fixture must be multibyte");
    assert!(
        check_storable(Path::new("/x.mp3"), &probed_with_text_tags(&[(&key, "v")])).is_ok(),
        "a key at the character cap must pass even when its byte length exceeds it"
    );
}

#[test]
fn check_storable_accepts_art_mime_at_cap_and_rejects_one_over() {
    let cap = usize::try_from(musefs_db::limits::MAX_ART_MIME_LEN).unwrap();
    let mk = |len: usize| {
        let mut pic = picture_of_len(1);
        pic.mime = "m".repeat(len);
        probed_with_pictures(vec![pic])
    };
    assert!(check_storable(Path::new("/x.flac"), &mk(cap)).is_ok());
    let err = check_storable(Path::new("/x.flac"), &mk(cap + 1))
        .expect_err("one character over the MIME cap fails the file");
    assert!(err.to_string().contains("MIME"), "{err}");
}

/// One tag whose `VORBIS_COMMENT` framing lands the running total on *exactly*
/// the block ceiling, and the same tag one byte longer.
///
/// The pair pins the whole size computation, not just its verdict: the framing
/// is `4-byte comment length + KEY + '=' + VALUE` on top of an 8-byte header,
/// so getting any of those terms wrong shifts the total off the boundary and
/// flips one of these two assertions. A test using values far over the ceiling
/// (as the FLAC case below does) cannot see that — it is over either way.
fn probed_with_comment_block_total(format: musefs_db::Format, total: usize) -> Probed {
    // total = 8 (vendor len + comment count) + 4 (comment len) + "A" + '=' + value
    let value = "v".repeat(total - 14);
    let mut p = probed_with_text_tags(&[("A", &value)]);
    p.format = format;
    p
}

#[test]
fn check_storable_comment_block_boundary_is_inclusive_and_exactly_framed() {
    let cap = usize::try_from(musefs_format::flac::MAX_BLOCK_BODY).unwrap();
    assert!(
        check_storable(
            Path::new("/x.flac"),
            &probed_with_comment_block_total(musefs_db::Format::Flac, cap)
        )
        .is_ok(),
        "a comment block landing exactly on the ceiling still fits"
    );
    assert!(
        check_storable(
            Path::new("/x.flac"),
            &probed_with_comment_block_total(musefs_db::Format::Flac, cap + 1)
        )
        .is_err(),
        "one byte past the ceiling cannot be synthesized"
    );
}

/// Ogg FLAC carries the same FLAC metadata blocks as native FLAC, so it is
/// bound by the same 24-bit ceiling and must not be waved through.
#[test]
fn check_storable_applies_the_comment_block_ceiling_to_ogg_flac() {
    let cap = usize::try_from(musefs_format::flac::MAX_BLOCK_BODY).unwrap();
    assert!(
        check_storable(
            Path::new("/x.oga"),
            &probed_with_comment_block_total(musefs_db::Format::OggFlac, cap)
        )
        .is_ok()
    );
    let err = check_storable(
        Path::new("/x.oga"),
        &probed_with_comment_block_total(musefs_db::Format::OggFlac, cap + 1),
    )
    .expect_err("Ogg FLAC is bound by FLAC's block ceiling too");
    assert!(err.to_string().contains("Ogg FLAC"), "{err}");
}

/// A tag value is capped at exactly FLAC's block ceiling, so one always fits
/// alone — but two need not. Without this check the file would scan clean and
/// then `EIO` on every read, which is the state #644's fail-the-file policy
/// exists to prevent.
#[test]
fn check_storable_rejects_flac_tags_that_cannot_fit_a_comment_block() {
    let cap = usize::try_from(musefs_db::limits::MAX_TAG_VALUE_LEN).unwrap();
    let big = "v".repeat(cap * 2 / 3);
    let mut probed = probed_with_text_tags(&[("A", &big), ("B", &big)]);
    probed.format = musefs_db::Format::Flac;
    let err = check_storable(Path::new("/x.flac"), &probed)
        .expect_err("two two-thirds-cap tags overflow the comment block");
    assert!(err.to_string().contains("FLAC"), "{err}");

    // The same tags in an MP3 are fine: ID3v2's ceiling is 256 MiB.
    let mut mp3 = probed_with_text_tags(&[("A", &big), ("B", &big)]);
    mp3.format = musefs_db::Format::Mp3;
    assert!(check_storable(Path::new("/x.mp3"), &mp3).is_ok());
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
        &path,
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
            Path::new("/a.mp3"),
            BackingStamp {
                size: 1,
                mtime_ns: 0,
                ctime_ns: 0,
                ino: None,
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

    ingest(&db, &path, &meta, probed_with_duplicate_structural_kind()).unwrap();

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

    ingest(&db, &path, &meta, probed_with_duplicate_tag_key()).unwrap();

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
            Path::new("/a.flac"),
            BackingStamp {
                size: 1,
                mtime_ns: 0,
                ctime_ns: 0,
                ino: None,
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

// --- #659: text and binary tag rows share one ordinal space per key ---

/// A track whose backing file carries the same key as both a text tag and a
/// binary payload. Real shapes: a FLAC `CUESHEET` Vorbis comment beside a
/// CUESHEET metadata block, or an ID3 `TXXX` frame whose description names a
/// binary frame (`PRIV`, `GEOB`, `MCDI`) the tag also carries.
fn probed_with_key_in_both_tag_classes() -> Probed {
    Probed {
        format: musefs_db::Format::Flac,
        audio_offset: 0,
        audio_length: 0,
        tags: vec![("CUESHEET".to_string(), "text value".to_string())],
        pictures: Vec::new(),
        binary_tags: vec![EmbeddedBinaryTag {
            key: "CUESHEET".to_string(),
            payload: vec![0xC0, 0xDE],
        }],
        structural_blocks: Vec::new(),
    }
}

/// `tags`' primary key is `(track_id, key, ordinal)` and does not discriminate
/// on `value_blob`, so numbering the text and binary rows from 0 independently
/// wrote two rows at the same key and ordinal. That surfaced as
/// `UNIQUE constraint failed: tags.track_id, tags.key, tags.ordinal` from
/// inside the ingest transaction, which aborts the whole scan rather than
/// failing the one file (#659).
#[test]
fn ingest_keeps_text_and_binary_rows_of_one_key_apart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.flac");
    std::fs::write(&path, b"x").unwrap();
    let meta = std::fs::metadata(&path).unwrap();
    let db = Db::open_in_memory().unwrap();

    ingest(&db, &path, &meta, probed_with_key_in_both_tag_classes()).unwrap();

    let tid = db.list_tracks().unwrap()[0].id;
    let text = db.get_tags(tid).unwrap();
    assert_eq!(text.len(), 1);
    assert_eq!(text[0].ordinal, 0);
    assert_eq!(text[0].value, "text value");
    // The binary row continues the same key's numbering rather than restarting.
    let binary = db.get_binary_tags(tid).unwrap();
    assert_eq!(binary.len(), 1);
    assert_eq!(binary[0].key, "CUESHEET");
    assert_eq!(
        db.read_binary_tag_chunk(binary[0].rowid, 0, 2).unwrap(),
        vec![0xC0, 0xDE]
    );
}

/// Same collision through the production batch writer, whose transaction is
/// the one the scan aborts on.
#[test]
fn ingest_bulk_keeps_text_and_binary_rows_of_one_key_apart() {
    let db = Db::open_in_memory().unwrap();
    {
        let mut bw = db.bulk_writer().unwrap();
        ingest_bulk(
            &mut bw,
            Path::new("/a.flac"),
            BackingStamp {
                size: 1,
                mtime_ns: 0,
                ctime_ns: 0,
                ino: None,
            },
            probed_with_key_in_both_tag_classes(),
        )
        .unwrap();
        bw.commit().unwrap();
    }
    let tid = db.list_tracks().unwrap()[0].id;
    assert_eq!(db.get_tags(tid).unwrap().len(), 1);
    assert_eq!(db.get_binary_tags(tid).unwrap().len(), 1);
}

/// Binary ordinals are per key, not one running index across the whole track:
/// two payloads under one key must number 0 then 1, and a second key restarts.
#[test]
fn ingest_numbers_binary_tags_per_key() {
    let db = Db::open_in_memory().unwrap();
    {
        let mut bw = db.bulk_writer().unwrap();
        ingest_bulk(
            &mut bw,
            Path::new("/a.mp3"),
            BackingStamp {
                size: 1,
                mtime_ns: 0,
                ctime_ns: 0,
                ino: None,
            },
            Probed {
                format: musefs_db::Format::Mp3,
                audio_offset: 0,
                audio_length: 0,
                tags: Vec::new(),
                pictures: Vec::new(),
                binary_tags: vec![
                    EmbeddedBinaryTag {
                        key: "PRIV".to_string(),
                        payload: vec![0xA1],
                    },
                    EmbeddedBinaryTag {
                        key: "GEOB".to_string(),
                        payload: vec![0xB2],
                    },
                    EmbeddedBinaryTag {
                        key: "PRIV".to_string(),
                        payload: vec![0xC3],
                    },
                ],
                structural_blocks: Vec::new(),
            },
        )
        .unwrap();
        bw.commit().unwrap();
    }
    let tid = db.list_tracks().unwrap()[0].id;
    // ORDER BY key, ordinal: GEOB(0), then PRIV(0), PRIV(1).
    let rows = db.get_binary_tags(tid).unwrap();
    let keys: Vec<&str> = rows.iter().map(|r| r.key.as_str()).collect();
    assert_eq!(keys, vec!["GEOB", "PRIV", "PRIV"]);
    assert_eq!(
        db.read_binary_tag_chunk(rows[1].rowid, 0, 1).unwrap(),
        vec![0xA1],
        "the first PRIV payload sorts first"
    );
    assert_eq!(
        db.read_binary_tag_chunk(rows[2].rowid, 0, 1).unwrap(),
        vec![0xC3]
    );
}

/// An empty payload is dropped before numbering, so it must not consume an
/// ordinal the next payload under that key then skips.
#[test]
fn storable_binary_tags_skips_empty_payloads_without_burning_ordinals() {
    let mut ordinals = HashMap::new();
    let got = super::storable_binary_tags(
        vec![
            EmbeddedBinaryTag {
                key: "PRIV".to_string(),
                payload: Vec::new(),
            },
            EmbeddedBinaryTag {
                key: "PRIV".to_string(),
                payload: vec![0xA1],
            },
        ],
        &mut ordinals,
    );
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].ordinal, 0);
}

// --- #655 / #651: mutation-gate survivors from PR #656 ---

/// `uncommitted_total` sums the two ways a dispatched file can finish without a
/// commit. A difference would pass every test that can drive the pipeline
/// (`raced` is always 0 there — the probe race hook is thread-local and the
/// probes run on worker threads), so the sum is pinned directly.
#[test]
fn uncommitted_total_sums_failures_and_races() {
    assert_eq!(super::uncommitted_total(3, 2), 5, "failures plus races");
    // The mutation this exists to kill: a difference agrees on every input
    // where one side is zero, which is every case a pipeline test can build.
    assert_ne!(super::uncommitted_total(3, 2), 3 - 2);
    assert_eq!(super::uncommitted_total(0, 4), 4, "races alone still count");
    assert_eq!(
        super::uncommitted_total(4, 0),
        4,
        "failures alone still count"
    );
    assert_eq!(super::uncommitted_total(0, 0), 0);
}

/// The end-of-scan failure breakdown has to actually reach the log. The
/// `failed_summary`/`walk_summary` formatters are unit-tested above, but
/// nothing asserted that `run_pipeline` emits them — so stubbing the emission
/// out entirely went unnoticed.
#[test]
fn scan_emits_the_failure_breakdown_to_the_log() {
    crate::warn_limit::log_capture::install();
    let dir = tempfile::tempdir().unwrap();
    // Supported extension, unparseable content: counted `failed`, so the
    // breakdown must name the `unparseable` bucket.
    for i in 0..3 {
        std::fs::write(dir.path().join(format!("broken{i}.flac")), b"not a flac").unwrap();
    }
    let db = Db::open_in_memory().unwrap();
    let stats = scan_directory_with(&db, dir.path(), &ScanOptions::default()).unwrap();
    assert_eq!(stats.failed, 3);

    // Asserted by substring rather than by count: the capture buffer is shared
    // across this binary's parallel tests. Presence is enough to kill the
    // stub-the-emission mutant, which would leave the buffer without any
    // breakdown line at all.
    let breakdowns = crate::warn_limit::log_capture::messages_containing("unparseable=");
    assert!(
        breakdowns.iter().any(|m| m.starts_with("failed ")),
        "expected a `failed N: unparseable=N` breakdown; captured: {breakdowns:?}"
    );
}

/// Minimal FLAC: marker, STREAMINFO, a VORBIS_COMMENT carrying one `TITLE`,
/// then audio. The title matters — it is the row the poison trigger below fires
/// on, which is what puts the violation *after* this file's `tracks` row has
/// already been written.
fn flac_titled(title: &str) -> Vec<u8> {
    let mut comment = Vec::new();
    comment.extend_from_slice(&0u32.to_le_bytes()); // empty vendor string
    comment.extend_from_slice(&1u32.to_le_bytes()); // one user comment
    let field = format!("TITLE={title}");
    comment.extend_from_slice(&u32::try_from(field.len()).unwrap().to_le_bytes());
    comment.extend_from_slice(field.as_bytes());

    let mut bytes = b"fLaC".to_vec();
    bytes.push(0x00); // type 0 (STREAMINFO), more blocks follow
    bytes.extend_from_slice(&[0, 0, 34]); // 24-bit length = 34
    bytes.extend(std::iter::repeat_n(0u8, 34));
    bytes.push(0x84); // last-block flag set, type 4 (VORBIS_COMMENT)
    let len = u32::try_from(comment.len()).unwrap().to_be_bytes();
    bytes.extend_from_slice(&len[1..]); // 24-bit length
    bytes.extend_from_slice(&comment);
    bytes.extend_from_slice(b"AUDIOPAYLOAD");
    bytes
}

/// Make the store refuse one file's rows, the way a constraint the scanner does
/// not pre-check would. Installed over a second connection to the store file,
/// so the scan's own connection is untouched, and `RAISE(ABORT)` raises
/// `SQLITE_CONSTRAINT_TRIGGER` — the same primary code as the primary-key
/// collision that aborted the scan in #659, which is what the classifier keys
/// on.
/// Make the store refuse one file's rows, the way a constraint the scanner does
/// not pre-check would. Installed over a second connection to the store file,
/// so the scan's own connection is untouched, and `RAISE(ABORT)` raises
/// `SQLITE_CONSTRAINT_TRIGGER` — the same primary code as the primary-key
/// collision that aborted the scan in #659, which is what the classifier keys
/// on.
///
/// It fires on the `tags` insert, not the `tracks` insert, so the file's
/// `tracks` row is already in the transaction when the violation lands. That is
/// the case the savepoint exists for: a statement-level `ABORT` undoes only its
/// own statement, so without one the batch would commit a half-ingested track.
fn poison_one_title(store: &std::path::Path, needle: &str) {
    let conn = rusqlite::Connection::open(store).unwrap();
    conn.execute_batch(&format!(
        "CREATE TRIGGER musefs_test_poison BEFORE INSERT ON tags \
         WHEN NEW.value LIKE '%{needle}%' \
         BEGIN SELECT RAISE(ABORT, 'synthetic constraint failed: tags.value'); END;"
    ))
    .unwrap();
}

/// The #662 acceptance case, driven through the production batch path: one
/// file's rows are refused inside the ingest transaction that holds the whole
/// batch, and the scan has to fail that one file and ingest the rest.
#[test]
fn scan_fails_only_the_file_whose_rows_the_store_rejects() {
    crate::warn_limit::log_capture::install();
    let dir = tempfile::tempdir().unwrap();
    for name in ["a.flac", "poison.flac", "b.flac"] {
        std::fs::write(dir.path().join(name), flac_titled(name)).unwrap();
    }
    let store = dir.path().join("musefs.db");
    let db = Db::open(&store).unwrap();
    poison_one_title(&store, "poison");

    let stats = scan_directory_with(&db, dir.path(), &ScanOptions::default())
        .expect("a rejected file must not abort the scan");

    assert_eq!(stats.failed, 1, "the rejected file is one `failed` file");
    assert_eq!(stats.scanned, 2, "the other two files are still ingested");
    let paths: Vec<std::path::PathBuf> = db
        .list_tracks()
        .unwrap()
        .into_iter()
        .map(|t| t.backing_path)
        .collect();
    assert_eq!(paths.len(), 2, "{paths:?}");
    assert!(
        !paths.iter().any(|p| p.to_string_lossy().contains("poison")),
        "the rejected file must leave no rows behind, not a track with no tags: {paths:?}"
    );

    // Not silence: the file is named and the constraint text preserved, so a
    // file musefs refuses to store is visible rather than quietly missing from
    // the mount (#284). Substring assertions — the capture buffer is shared
    // across this binary's parallel tests.
    let named = crate::warn_limit::log_capture::messages_containing("poison.flac");
    assert!(
        named
            .iter()
            .any(|m| m.contains("synthetic constraint failed")),
        "expected the rejected file named with its constraint text; captured: {named:?}"
    );
    let breakdowns = crate::warn_limit::log_capture::messages_containing("rejected=");
    assert!(
        breakdowns.iter().any(|m| m.starts_with("failed ")),
        "expected a `failed N: rejected=N` breakdown; captured: {breakdowns:?}"
    );
}

/// The other half of the classification, at the boundary the writer keys on: a
/// constraint violation fails one file, and everything that says the run itself
/// cannot proceed still aborts it (#662).
#[test]
fn only_constraint_violations_are_per_file_rejections() {
    let rejection =
        crate::error::CoreError::Db(musefs_db::DbError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE),
            Some("UNIQUE constraint failed: tags.track_id, tags.key, tags.ordinal".into()),
        )));
    assert!(super::is_store_rejection(&rejection), "{rejection}");

    for code in [
        rusqlite::ffi::SQLITE_CORRUPT,
        rusqlite::ffi::SQLITE_FULL,
        rusqlite::ffi::SQLITE_IOERR,
        rusqlite::ffi::SQLITE_READONLY,
        rusqlite::ffi::SQLITE_NOTADB,
        rusqlite::ffi::SQLITE_BUSY,
    ] {
        let fatal = crate::error::CoreError::Db(musefs_db::DbError::Sqlite(
            rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), None),
        ));
        assert!(
            !super::is_store_rejection(&fatal),
            "SQLite code {code} must still abort the scan"
        );
    }

    // Not every ingest error is a DB error, and none of the others may be
    // mistaken for one the scan can carry on past.
    let io = crate::error::CoreError::BackingChanged("/a.flac".into());
    assert!(!super::is_store_rejection(&io), "{io}");
}
