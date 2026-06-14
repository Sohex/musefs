mod common;
use common::write_flac;
use musefs_core::freshness::BackingStamp;
use musefs_core::{
    BackingReader, HeaderCache, Mode, ReadAhead, ReadAheadPool, read_at, read_at_with_file,
};
use musefs_db::{Db, Format, NewTrack, Tag};
use std::sync::{Arc, Mutex};

fn setup() -> (tempfile::TempDir, Db, i64) {
    let dir = tempfile::tempdir().unwrap();
    let flac = dir.path().join("song.flac");
    let audio: Vec<u8> = (0..200u32).map(|i| (i % 251) as u8).collect();
    let (audio_offset, audio_length) = write_flac(&flac, &["TITLE=Orig"], &audio);
    let meta = std::fs::metadata(&flac).unwrap();
    let db = Db::open_in_memory().unwrap();
    let id = db
        .upsert_track(&NewTrack {
            backing_path: flac.to_string_lossy().into_owned(),
            format: Format::Flac,
            audio_offset,
            audio_length,
            backing_size: meta.len(),
            backing_mtime_ns: common::real_mtime_ns(&flac),
            backing_ctime_ns: common::real_ctime_ns(&flac),
        })
        .unwrap();
    db.replace_tags(id, &[Tag::new("title", "Real", 0)])
        .unwrap();
    (dir, db, id)
}

#[test]
fn reading_whole_file_matches_total_len_and_splices_audio() {
    let (_dir, db, id) = setup();
    let cache = HeaderCache::new(Mode::Synthesis);
    let resolved = cache.resolve(&db, id).unwrap();

    let whole = read_at(&resolved, &db, 0, resolved.total_len).unwrap();
    assert_eq!(whole.len() as u64, resolved.total_len);

    let audio_part = &whole[usize::try_from(resolved.layout.header_len()).unwrap()..];
    let expected: Vec<u8> = (0..200u32).map(|i| (i % 251) as u8).collect();
    assert_eq!(audio_part, &expected[..]);

    let tag = metaflac::Tag::read_from(&mut std::io::Cursor::new(&whole)).unwrap();
    assert_eq!(
        tag.vorbis_comments()
            .unwrap()
            .get("TITLE")
            .map(std::vec::Vec::as_slice),
        Some(["Real".to_string()].as_slice())
    );
}

#[test]
fn random_offset_and_size_match_the_whole_read() {
    let (_dir, db, id) = setup();
    let cache = HeaderCache::new(Mode::Synthesis);
    let resolved = cache.resolve(&db, id).unwrap();
    let whole = read_at(&resolved, &db, 0, resolved.total_len).unwrap();

    for (off, size) in [
        (0u64, 10u64),
        (resolved.layout.header_len() - 5, 20),
        (resolved.total_len - 7, 50),
        (50, 0),
    ] {
        let got = read_at(&resolved, &db, off, size).unwrap();
        let end = usize::try_from((off + size).min(resolved.total_len)).unwrap();
        assert_eq!(
            got,
            &whole[usize::try_from(off).unwrap()..end],
            "off={off} size={size}"
        );
    }
}

#[test]
fn reading_past_eof_returns_empty() {
    let (_dir, db, id) = setup();
    let cache = HeaderCache::new(Mode::Synthesis);
    let resolved = cache.resolve(&db, id).unwrap();
    assert!(
        read_at(&resolved, &db, resolved.total_len, 100)
            .unwrap()
            .is_empty()
    );
    assert!(
        read_at(&resolved, &db, resolved.total_len + 5, 100)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn read_at_streams_art_image_segments() {
    use musefs_core::{ResolvedFile, read_at};
    use musefs_format::{BlobLen, RegionLayout, Segment};

    let db = Db::open_in_memory().unwrap();
    let art = vec![1u8, 2, 3, 4, 5, 6, 7, 8];
    let art_id = db
        .upsert_art(&musefs_db::NewArt {
            mime: "image/png".to_string(),
            width: None,
            height: None,
            data: art.clone(),
        })
        .unwrap();

    let layout = RegionLayout::validated(vec![
        Segment::Inline(vec![0xAA, 0xBB]),
        Segment::ArtImage {
            art_id,
            len: BlobLen::new(art.len() as u64).unwrap(),
        },
    ])
    .unwrap();
    let total_len = layout.total_len();
    let resolved = ResolvedFile {
        layout,
        total_len,
        content_version: 0,
        backing_path: std::path::PathBuf::from("/unused"),
        stamp: BackingStamp {
            size: 0,
            mtime_ns: 0,
            ctime_ns: 0,
        },
        mtime_secs: 0,
        last_page: std::sync::Mutex::new(None),
        cache_bytes: 0,
        has_binary_tag: false,
    };

    // Whole read: inline framing then the streamed art bytes.
    let whole = read_at(&resolved, &db, 0, total_len).unwrap();
    assert_eq!(whole, vec![0xAA, 0xBB, 1, 2, 3, 4, 5, 6, 7, 8]);

    // A window that lands entirely inside the art segment (offset 4 -> art[2..5]).
    assert_eq!(read_at(&resolved, &db, 4, 3).unwrap(), vec![3, 4, 5]);
}

#[test]
fn out_of_range_read_short_circuits_before_opening_the_backing_file() {
    let (_dir, db, id) = setup();
    let cache = HeaderCache::new(Mode::Synthesis);
    let resolved = cache.resolve(&db, id).unwrap();
    // Past-EOF and zero-size reads must return empty without ever opening the
    // backing file; deleting it makes any open attempt an error.
    std::fs::remove_file(&resolved.backing_path).unwrap();

    assert!(
        read_at(&resolved, &db, resolved.total_len, 100)
            .unwrap()
            .is_empty()
    );
    assert!(read_at(&resolved, &db, 10, 0).unwrap().is_empty());
}

#[test]
fn read_at_with_file_out_of_range_and_zero_size_return_empty() {
    let (_dir, db, id) = setup();
    let cache = HeaderCache::new(Mode::Synthesis);
    let resolved = cache.resolve(&db, id).unwrap();
    let file = std::fs::File::open(&resolved.backing_path).unwrap();
    let pool = ReadAheadPool::new(0);
    let buf = Arc::new(Mutex::new(ReadAhead::new(0)));
    let br = BackingReader::new(&file, &buf, &pool, 0, resolved.total_len);

    // The per-handle path has no outer guard: read_segments_into itself must
    // bail on offset > total_len (its window arithmetic would underflow).
    for (off, size) in [
        (resolved.total_len + 5, 100u64),
        (resolved.total_len, 1),
        (10, 0),
    ] {
        assert!(
            read_at_with_file(&resolved, &db, &br, off, size)
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn read_at_reserves_only_the_served_window() {
    let (_dir, db, id) = setup();
    let cache = HeaderCache::new(Mode::Synthesis);
    let resolved = cache.resolve(&db, id).unwrap();

    // A short tail read must reserve ~window-size capacity, not anything
    // scaled by the offset: FUSE workers retain this buffer across reads, so
    // an over-reserve would inflate every worker's resident memory.
    let got = read_at(&resolved, &db, resolved.total_len - 7, 50).unwrap();
    assert_eq!(got.len(), 7);
    assert!(got.capacity() < usize::try_from(resolved.total_len).unwrap());
}
