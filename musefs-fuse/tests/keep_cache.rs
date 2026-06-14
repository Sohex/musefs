use std::time::Duration;

use musefs_core::{Musefs, scan_directory};

mod common;
use common::{config, make_flac};

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

    let fs = Musefs::open(db, config()).unwrap();

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
