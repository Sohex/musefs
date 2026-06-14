use std::collections::BTreeMap;
use std::time::Duration;

use musefs_core::{MountConfig, Musefs, scan_directory};

// --- minimal proven FLAC fixture (mirrors musefs-fuse/tests/mount.rs) ---

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

fn vorbis_comment_body(vendor: &str, comments: &[&str]) -> Vec<u8> {
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
    out.extend_from_slice(&flac_block(4, &vorbis_comment_body("orig", comments), true));
    out.extend_from_slice(audio);
    out
}

/// Scan a backing dir into a Db with poll_interval=ZERO so metadata ops
/// trigger refresh immediately.
fn mount_config() -> MountConfig {
    MountConfig {
        template: "$artist/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode: musefs_core::Mode::Synthesis,
        poll_interval: Duration::ZERO,
        case_insensitive: false,
        read_ahead_budget: 64 * 1024 * 1024,
    }
}

#[test]
#[ignore = "requires /dev/fuse; run with: cargo test -p musefs-fuse -- --ignored"]
fn keep_cache_mount_reflects_retag_after_refresh() {
    // 1. Build a one-track FLAC library.
    let backing = tempfile::tempdir().unwrap();
    let flac = make_flac(&["ARTIST=Alice", "TITLE=Song"], &[0xAB; 64]);
    std::fs::write(backing.path().join("a.flac"), &flac).unwrap();

    // Use an on-disk DB so a second connection can retag the track.
    let db_dir = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("musefs.db");
    let db = musefs_db::Db::open(&db_path).unwrap();
    scan_directory(&db, backing.path()).unwrap();

    let fs = Musefs::open(db, mount_config()).unwrap();

    // 2. Mount with keep-cache=true and debounce=ZERO (already set via MountConfig).
    let cfg = musefs_fuse::FuseConfig {
        keep_cache: true,
        ..Default::default()
    };
    let mountpoint = tempfile::tempdir().unwrap();
    let session = musefs_fuse::spawn_with(fs, mountpoint.path(), "musefs-keepcache", cfg).unwrap();

    let song = mountpoint.path().join("Alice").join("Song.flac");

    // 3. Read the mounted file once to populate the kernel page cache.
    let original_bytes = std::fs::read(&song).unwrap();
    let original_size = original_bytes.len();

    // Verify initial tags read correctly.
    {
        let tag = metaflac::Tag::read_from(&mut std::io::Cursor::new(&original_bytes)).unwrap();
        assert_eq!(
            tag.vorbis_comments()
                .unwrap()
                .get("TITLE")
                .map(std::vec::Vec::as_slice),
            Some(["Song".to_string()].as_slice())
        );
    }

    // 4. Retag the track via a second DB connection: add album="Rev".
    {
        let db2 = musefs_db::Db::open(&db_path).unwrap();
        let tracks = db2.list_tracks().unwrap();
        assert_eq!(tracks.len(), 1, "expected exactly one track");
        let track_id = tracks[0].id;
        db2.replace_tags(
            track_id,
            &[
                musefs_db::Tag::new("artist", "Alice", 0),
                musefs_db::Tag::new("title", "Song", 0),
                musefs_db::Tag::new("album", "Rev", 0),
            ],
        )
        .unwrap();
    }

    // 5. Trigger metadata ops so the FUSE layer fires poll_refresh_notify +
    //    inval_inode. Poll_interval is ZERO so any metadata op triggers a check.
    //    invalidation is async (pool thread), so retry briefly. 1s (20×50ms) is
    //    ample: the pool fires within ~1ms and the kernel cache drop is synchronous.
    let new_bytes = {
        let mut result = None;
        for _ in 0..20 {
            // metadata op fires poll_refresh on the pool
            let _ = std::fs::metadata(&song);
            std::thread::sleep(Duration::from_millis(50));
            // re-read and check whether the size changed
            if let Ok(b) = std::fs::read(&song)
                && b.len() != original_size
            {
                result = Some(b);
                break;
            }
        }
        // Final read regardless — we'll assert on the content below
        result.unwrap_or_else(|| std::fs::read(&song).unwrap())
    };

    // 6. Assert the served bytes reflect the new tags (album=Rev enlarges the
    //    Vorbis comment block, so the total file size grows).
    assert_ne!(
        new_bytes.len(),
        original_size,
        "synthesized FLAC size should change after adding album tag"
    );

    // Also verify "Rev" appears in the parsed Vorbis comments.
    let tag = metaflac::Tag::read_from(&mut std::io::Cursor::new(&new_bytes)).unwrap();
    assert_eq!(
        tag.vorbis_comments()
            .unwrap()
            .get("ALBUM")
            .map(std::vec::Vec::as_slice),
        Some(["Rev".to_string()].as_slice()),
        "synthesized FLAC should contain the new album tag"
    );

    drop(session); // unmounts
    drop(backing);
}
