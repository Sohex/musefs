use musefs_cli::run_scan;

fn flac_block(block_type: u8, body: &[u8], is_last: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.push((if is_last { 0x80 } else { 0 }) | (block_type & 0x7F));
    let len = body.len();
    out.push(((len >> 16) & 0xFF) as u8);
    out.push(((len >> 8) & 0xFF) as u8);
    out.push((len & 0xFF) as u8);
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
    out.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    out.extend_from_slice(vendor.as_bytes());
    out.extend_from_slice(&(comments.len() as u32).to_le_bytes());
    for c in comments {
        out.extend_from_slice(&(c.len() as u32).to_le_bytes());
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

    run_scan(&db_path, backing.path(), false).unwrap();

    // The DB file was created and persists the track.
    let db = musefs_db::Db::open(&db_path).unwrap();
    let tracks = db.list_tracks().unwrap();
    assert_eq!(tracks.len(), 1);
    assert!(tracks[0].backing_path.ends_with("a.flac"));
}
