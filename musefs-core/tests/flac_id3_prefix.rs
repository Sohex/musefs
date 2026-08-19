//! Scanning and serving FLAC files that carry an ID3v2 tag in front of the
//! `fLaC` marker (#602). Such a file used to be skipped with "no parseable audio
//! metadata"; it is now ingested, its ID3 contents are read as a fallback for
//! tags the FLAC itself does not carry, and the served file is a stock FLAC with
//! no ID3 anywhere.

mod common;
use common::{make_flac, streaminfo_body, vorbis_comment_body};
use musefs_core::{MountConfig, Musefs, VirtualTree, scan_directory};
use std::collections::BTreeMap;

/// Encode a 28-bit synchsafe size the way an ID3v2 header does.
fn syncsafe(n: u32) -> [u8; 4] {
    [
        ((n >> 21) & 0x7F) as u8,
        ((n >> 14) & 0x7F) as u8,
        ((n >> 7) & 0x7F) as u8,
        (n & 0x7F) as u8,
    ]
}

/// An ID3v2.4 tag carrying UTF-8 text frames, e.g. `("TIT2", "Song")`. With no
/// frames this is the blank 10-byte header the issue describes.
fn id3_tag(frames: &[(&str, &str)]) -> Vec<u8> {
    let mut body = Vec::new();
    for (id, value) in frames {
        let mut content = vec![0x03]; // UTF-8 encoding byte
        content.extend_from_slice(value.as_bytes());
        body.extend_from_slice(id.as_bytes());
        body.extend_from_slice(&syncsafe(u32::try_from(content.len()).unwrap()));
        body.extend_from_slice(&[0, 0]); // frame flags
        body.extend_from_slice(&content);
    }
    let mut tag = vec![b'I', b'D', b'3', 4, 0, 0];
    tag.extend_from_slice(&syncsafe(u32::try_from(body.len()).unwrap()));
    tag.extend(body);
    tag
}

fn config() -> MountConfig {
    MountConfig {
        template: "$artist/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode: musefs_core::Mode::Synthesis,
        poll_interval: std::time::Duration::ZERO,
        case_insensitive: false,
        read_ahead_budget: 64 * 1024 * 1024,
        read_ahead_prefetch: false,
        skip_on_missing: false,
    }
}

fn read_whole(fs: &Musefs, inode: u64) -> Vec<u8> {
    let size = fs.getattr(inode).unwrap().size;
    let fh = fs.open_handle(inode).unwrap();
    let mut out = Vec::new();
    let mut off = 0u64;
    while off < size {
        let got = fs.read(inode, Some(fh), off, 64 * 1024).unwrap();
        if got.is_empty() {
            break;
        }
        off += got.len() as u64;
        out.extend_from_slice(&got);
    }
    fs.release_handle(fh);
    out
}

#[test]
fn scans_a_flac_behind_a_leading_id3_tag() {
    let dir = tempfile::tempdir().unwrap();
    let audio = vec![0xCD; 4096];
    let mut bytes = id3_tag(&[]);
    bytes.extend(make_flac(
        &[
            (0, streaminfo_body()),
            (4, vorbis_comment_body("v", &["ARTIST=Alice", "TITLE=Song"])),
        ],
        &audio,
    ));
    std::fs::write(dir.path().join("a.flac"), &bytes).unwrap();

    let db = musefs_db::Db::open_in_memory().unwrap();
    let stats = scan_directory(&db, dir.path()).unwrap();

    assert_eq!(stats.scanned, 1, "the file is no longer skipped");
    let track = &db.list_tracks().unwrap()[0];
    // The audio bounds account for the tag, so the served audio is the real audio.
    assert_eq!(track.bounds.audio_length(), audio.len() as u64);
    assert_eq!(
        track.bounds.audio_offset(),
        (bytes.len() - audio.len()) as u64
    );
    let tags = db.get_tags(track.id).unwrap();
    assert!(tags.iter().any(|t| t.key == "title" && t.value == "Song"));
    assert!(tags.iter().any(|t| t.key == "artist" && t.value == "Alice"));
}

#[test]
fn ingests_id3_tags_when_the_flac_carries_none() {
    let dir = tempfile::tempdir().unwrap();
    let mut bytes = id3_tag(&[("TIT2", "From ID3"), ("TPE1", "ID3 Artist")]);
    // No VORBIS_COMMENT block at all — the ID3 header is the only tag source.
    bytes.extend(make_flac(&[(0, streaminfo_body())], &[0xAB; 64]));
    std::fs::write(dir.path().join("b.flac"), &bytes).unwrap();

    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();

    let track = &db.list_tracks().unwrap()[0];
    let tags = db.get_tags(track.id).unwrap();
    assert!(
        tags.iter()
            .any(|t| t.key == "title" && t.value == "From ID3"),
        "ID3 text frames fill in for absent Vorbis comments: {tags:?}"
    );
    assert!(
        tags.iter()
            .any(|t| t.key == "artist" && t.value == "ID3 Artist")
    );
}

#[test]
fn vorbis_comments_win_over_the_id3_tag() {
    let dir = tempfile::tempdir().unwrap();
    let mut bytes = id3_tag(&[("TIT2", "Stale ID3 Title"), ("TPE1", "Only In ID3")]);
    bytes.extend(make_flac(
        &[
            (0, streaminfo_body()),
            (4, vorbis_comment_body("v", &["TITLE=Authoritative"])),
        ],
        &[0xAB; 64],
    ));
    std::fs::write(dir.path().join("c.flac"), &bytes).unwrap();

    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();

    let track = &db.list_tracks().unwrap()[0];
    let tags = db.get_tags(track.id).unwrap();
    let titles: Vec<&str> = tags
        .iter()
        .filter(|t| t.key == "title")
        .map(|t| t.value.as_str())
        .collect();
    assert_eq!(
        titles,
        vec!["Authoritative"],
        "the FLAC's own tag wins outright, with no ID3 value alongside it"
    );
    // A key the Vorbis block does not define is still taken from the ID3 tag.
    assert!(
        tags.iter()
            .any(|t| t.key == "artist" && t.value == "Only In ID3")
    );
}

#[test]
fn serves_a_stock_flac_from_an_id3_prefixed_backing() {
    let dir = tempfile::tempdir().unwrap();
    let audio = vec![0xCD; 4096];
    let mut bytes = id3_tag(&[("TIT2", "Stale ID3 Title")]);
    bytes.extend(make_flac(
        &[
            (0, streaminfo_body()),
            (3, vec![0xEE; 36]), // SEEKTABLE
            (4, vorbis_comment_body("v", &["ARTIST=Alice", "TITLE=Song"])),
        ],
        &audio,
    ));
    std::fs::write(dir.path().join("a.flac"), &bytes).unwrap();

    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();
    let fs = Musefs::open(db, config()).unwrap();

    let artist = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let (_, inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();
    let served = read_whole(&fs, inode);

    // The synthesized file is a plain FLAC: the backing file's ID3 tag is
    // metadata, so it is regenerated away rather than carried through.
    assert_eq!(&served[0..4], b"fLaC", "no ID3 prefix on the served file");
    let tag = metaflac::Tag::read_from(&mut std::io::Cursor::new(&served)).expect("valid FLAC");
    let comments = tag.vorbis_comments().expect("vorbis comments");
    assert_eq!(comments.title().unwrap()[0], "Song");

    // The audio rides through untouched, positioned past the ID3 tag.
    let scan = musefs_format::flac::locate_audio(&served).unwrap();
    let start = usize::try_from(scan.audio_offset).unwrap();
    assert_eq!(&served[start..], &audio[..]);
}

#[test]
fn bounded_widening_matches_the_full_probe_over_a_large_tag() {
    // A tag far larger than the scan window forces the widen loop to run against
    // the ID3 header's declared length before the `fLaC` marker is ever visible.
    let dir = tempfile::tempdir().unwrap();
    let mut bytes = id3_tag(&[("TIT2", "Widened"), ("TPE1", "Alice")]);
    bytes.extend(std::iter::repeat_n(0u8, 8192)); // padding inside the ID3 body
    let body_len = u32::try_from(bytes.len() - 10).unwrap();
    bytes[6..10].copy_from_slice(&syncsafe(body_len));
    bytes.extend(make_flac(
        &[
            (0, streaminfo_body()),
            (4, vorbis_comment_body("v", &["TITLE=Widened"])),
        ],
        &[0xCD; 512],
    ));
    std::fs::write(dir.path().join("wide.flac"), &bytes).unwrap();

    let oracle_db = musefs_db::Db::open_in_memory().unwrap();
    musefs_core::scan_directory_full_oracle(&oracle_db, dir.path()).unwrap();

    let bounded_db = musefs_db::Db::open_in_memory().unwrap();
    musefs_core::scan_directory_with(
        &bounded_db,
        dir.path(),
        &musefs_core::ScanOptions {
            window: 64,
            ..Default::default()
        },
    )
    .unwrap();

    let rows = |db: &musefs_db::Db| {
        let t = &db.list_tracks().unwrap()[0];
        let mut tags: Vec<(String, String)> = db
            .get_tags(t.id)
            .unwrap()
            .into_iter()
            .map(|tg| (tg.key, tg.value))
            .collect();
        tags.sort();
        (t.bounds.audio_offset(), t.bounds.audio_length(), tags)
    };
    let oracle = rows(&oracle_db);
    assert_eq!(oracle.1, 512, "the oracle located the audio");
    assert_eq!(oracle, rows(&bounded_db));
}
