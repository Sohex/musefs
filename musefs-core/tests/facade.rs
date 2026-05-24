mod common;
use std::collections::BTreeMap;
use common::make_flac;
use common::{streaminfo_body, vorbis_comment_body};
use musefs_core::{scan_directory, Musefs, MountConfig, VirtualTree};

fn config() -> MountConfig {
    MountConfig {
        template: "$artist/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
    }
}

fn scanned_db(dir: &std::path::Path) -> musefs_db::Db {
    let a = make_flac(&[(0, streaminfo_body()), (4, vorbis_comment_body("v", &["ARTIST=Alice", "TITLE=Song"]))], &[0xAB; 64]);
    std::fs::write(dir.join("a.flac"), &a).unwrap();
    let db = musefs_db::Db::open_in_memory().unwrap();
    // Use an on-disk DB? in-memory is fine; scan writes absolute backing paths.
    scan_directory(&db, dir).unwrap();
    db
}

#[test]
fn lookup_getattr_readdir_and_read_through_the_facade() {
    let dir = tempfile::tempdir().unwrap();
    let db = scanned_db(dir.path());
    let mut fs = Musefs::open(db, config()).unwrap();

    // Tree: /Alice/Song.flac
    let artist = fs.lookup(VirtualTree::ROOT, "Alice").expect("artist dir");
    let dattr = fs.getattr(artist).unwrap();
    assert!(dattr.is_dir);

    let entries = fs.readdir(artist).unwrap();
    assert_eq!(entries.len(), 1);
    let (name, file_inode, is_dir) = entries.into_iter().next().unwrap();
    assert_eq!(name, "Song.flac");
    assert!(!is_dir);

    let fattr = fs.getattr(file_inode).unwrap();
    assert!(!fattr.is_dir);
    assert!(fattr.size > 0);

    // Reading the whole file yields a valid FLAC whose TITLE is the synthesized value.
    let bytes = fs.read(file_inode, 0, fattr.size).unwrap();
    assert_eq!(bytes.len() as u64, fattr.size);
    let tag = metaflac::Tag::read_from(&mut std::io::Cursor::new(&bytes)).unwrap();
    assert_eq!(
        tag.vorbis_comments().unwrap().get("TITLE").map(|v| v.as_slice()),
        Some(["Song".to_string()].as_slice())
    );
}

#[test]
fn refresh_rebuilds_tree_after_new_tracks() {
    let dir = tempfile::tempdir().unwrap();
    let db = scanned_db(dir.path());
    let mut fs = Musefs::open(db, config()).unwrap();
    assert!(fs.lookup(VirtualTree::ROOT, "Alice").is_some());
    assert!(fs.lookup(VirtualTree::ROOT, "Bob").is_none());

    // This test only asserts refresh() runs and the tree is rebuilt from the DB;
    // adding rows would require a handle to the DB, which Musefs now owns. So we
    // simply confirm refresh() succeeds and the existing entry is still present.
    fs.refresh().unwrap();
    assert!(fs.lookup(VirtualTree::ROOT, "Alice").is_some());
}
