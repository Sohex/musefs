use musefs_cli::{ChecksumMode, run_scan};

fn flac_block(block_type: u8, body: &[u8], is_last: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.push((if is_last { 0x80 } else { 0 }) | (block_type & 0x7F));
    let len = body.len();
    out.push(u8::try_from((len >> 16) & 0xFF).unwrap());
    out.push(u8::try_from((len >> 8) & 0xFF).unwrap());
    out.push(u8::try_from(len & 0xFF).unwrap());
    out.extend_from_slice(body);
    out
}

fn streaminfo_body() -> Vec<u8> {
    let mut b = vec![
        0x10, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0A, 0xC4, 0x42, 0xF0, 0x00,
        0x00, 0x00, 0x00,
    ];
    b.extend_from_slice(&[0u8; 16]);
    b
}

fn vorbis_comment_body(comments: &[&str]) -> Vec<u8> {
    let vendor = "orig";
    let mut out = Vec::new();
    out.extend_from_slice(&u32::try_from(vendor.len()).unwrap().to_le_bytes());
    out.extend_from_slice(vendor.as_bytes());
    out.extend_from_slice(&u32::try_from(comments.len()).unwrap().to_le_bytes());
    for c in comments {
        out.extend_from_slice(&u32::try_from(c.len()).unwrap().to_le_bytes());
        out.extend_from_slice(c.as_bytes());
    }
    out
}

fn make_flac(comments: &[&str], audio: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"fLaC");
    out.extend_from_slice(&flac_block(0, &streaminfo_body(), false));
    out.extend_from_slice(&flac_block(4, &vorbis_comment_body(comments), true));
    out.extend_from_slice(audio);
    out
}

#[test]
fn scan_ingests_flacs_into_a_fresh_db() {
    let backing = tempfile::tempdir().unwrap();
    std::fs::write(
        backing.path().join("a.flac"),
        make_flac(&["ARTIST=Alice", "TITLE=Song"], &[0xAB; 32]),
    )
    .unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("musefs.db");

    run_scan(
        &db_path,
        &[backing.path().to_path_buf()],
        false,
        false,
        0,
        false,
        false,
        ChecksumMode::Fingerprint,
        false,
        false,
    )
    .unwrap();

    // The DB file was created and persists the track.
    let db = musefs_db::Db::open(&db_path).unwrap();
    let tracks = db.list_tracks().unwrap();
    assert_eq!(tracks.len(), 1);
    assert!(tracks[0].backing_path.ends_with("a.flac"));
}

#[test]
fn scan_ingests_multiple_targets_under_one_db() {
    let backing_a = tempfile::tempdir().unwrap();
    std::fs::write(
        backing_a.path().join("a.flac"),
        make_flac(&["TITLE=A"], &[0xAB; 32]),
    )
    .unwrap();
    let backing_b = tempfile::tempdir().unwrap();
    std::fs::write(
        backing_b.path().join("b.flac"),
        make_flac(&["TITLE=B"], &[0xCD; 32]),
    )
    .unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("musefs.db");

    run_scan(
        &db_path,
        &[
            backing_a.path().to_path_buf(),
            backing_b.path().to_path_buf(),
        ],
        false,
        false,
        0,
        false,
        false,
        ChecksumMode::Fingerprint,
        false,
        false,
    )
    .unwrap();

    let db = musefs_db::Db::open(&db_path).unwrap();
    assert_eq!(db.list_tracks().unwrap().len(), 2);
}

#[test]
fn scan_fails_fast_on_a_bad_target() {
    let backing = tempfile::tempdir().unwrap();
    std::fs::write(
        backing.path().join("a.flac"),
        make_flac(&["TITLE=A"], &[0xAB; 32]),
    )
    .unwrap();
    let db_dir = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("musefs.db");

    // Second target does not exist; collect_audio's read_dir errors, so the
    // batch aborts with Err.
    let missing = backing.path().join("does-not-exist");
    let result = run_scan(
        &db_path,
        &[backing.path().to_path_buf(), missing],
        false,
        false,
        0,
        false,
        false,
        ChecksumMode::Fingerprint,
        false,
        false,
    );
    assert!(result.is_err());
}

#[test]
fn scan_returns_per_file_failed_count() {
    // A per-file parse failure does not abort the batch (that stays an Ok return);
    // instead it is tallied and surfaced via the returned `failed` count so the CLI
    // can signal partial/total ingest failure with a non-zero exit (#554).
    let backing = tempfile::tempdir().unwrap();
    std::fs::write(
        backing.path().join("good.flac"),
        make_flac(&["TITLE=Good"], &[0xAB; 32]),
    )
    .unwrap();
    std::fs::write(backing.path().join("bad.flac"), b"not a flac at all").unwrap();
    let db_dir = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("musefs.db");

    let failed = run_scan(
        &db_path,
        &[backing.path().to_path_buf()],
        false,
        false,
        0,
        false,
        false,
        ChecksumMode::Fingerprint,
        false,
        false,
    )
    .unwrap();

    assert_eq!(failed, 1, "the one unparseable file must be tallied");
    let db = musefs_db::Db::open(&db_path).unwrap();
    assert_eq!(db.list_tracks().unwrap().len(), 1, "the good file ingested");
}

fn write_n_flacs(dir: &std::path::Path, n: usize) {
    for i in 0..n {
        let title = format!("TITLE=T{i}");
        std::fs::write(
            dir.join(format!("t{i:02}.flac")),
            make_flac(&[title.as_str()], &[0xAB; 32]),
        )
        .unwrap();
    }
}

#[test]
fn scan_with_progress_ingests_all_files() {
    let backing = tempfile::tempdir().unwrap();
    write_n_flacs(backing.path(), 20);
    let db_dir = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("musefs.db");

    run_scan(
        &db_path,
        &[backing.path().to_path_buf()],
        false,
        false,
        0,
        false,
        false,
        ChecksumMode::Fingerprint,
        false,
        false,
    )
    .unwrap();

    let db = musefs_db::Db::open(&db_path).unwrap();
    assert_eq!(db.list_tracks().unwrap().len(), 20);
}

#[test]
fn quiet_scan_still_ingests_all_files() {
    let backing = tempfile::tempdir().unwrap();
    write_n_flacs(backing.path(), 20);
    let db_dir = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("musefs.db");

    run_scan(
        &db_path,
        &[backing.path().to_path_buf()],
        false,
        false,
        0,
        false,
        true,
        ChecksumMode::Fingerprint,
        false,
        false,
    )
    .unwrap();

    let db = musefs_db::Db::open(&db_path).unwrap();
    assert_eq!(db.list_tracks().unwrap().len(), 20);
}

#[test]
fn revalidate_with_progress_reports_unchanged() {
    let backing = tempfile::tempdir().unwrap();
    write_n_flacs(backing.path(), 20);
    let db_dir = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("musefs.db");

    run_scan(
        &db_path,
        &[backing.path().to_path_buf()],
        false,
        false,
        0,
        false,
        false,
        ChecksumMode::Fingerprint,
        false,
        false,
    )
    .unwrap();
    run_scan(
        &db_path,
        &[backing.path().to_path_buf()],
        true,
        false,
        0,
        false,
        false,
        ChecksumMode::Fingerprint,
        false,
        false,
    )
    .unwrap();

    let db = musefs_db::Db::open(&db_path).unwrap();
    assert_eq!(db.list_tracks().unwrap().len(), 20);
}
