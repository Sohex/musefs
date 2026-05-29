mod common;
use common::write_flac;
use common::write_mp3;
use musefs_core::{read_at, HeaderCache, Mode};
use musefs_db::{Db, Format, NewArt, NewTrack, Tag, TrackArt};
use musefs_format::Segment;
use proptest::prelude::*;

fn build(audio: &[u8], title: &str) -> (tempfile::TempDir, Db, i64, Vec<u8>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("song.flac");
    let (audio_offset, audio_length) = write_flac(&path, &["TITLE=Orig"], audio);
    let meta = std::fs::metadata(&path).unwrap();
    let db = Db::open_in_memory().unwrap();
    let id = db
        .upsert_track(&NewTrack {
            backing_path: path.to_string_lossy().to_string(),
            format: Format::Flac,
            audio_offset,
            audio_length,
            backing_size: meta.len() as i64,
            backing_mtime: meta
                .modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64,
        })
        .unwrap();
    db.replace_tags(id, &[Tag::new("title", title, 0)]).unwrap();
    (dir, db, id, audio.to_vec())
}

/// Like `build`, but also inserts an art blob and links it to the track, so the
/// resolved layout contains an `ArtImage` segment. Mirrors the insert+link pattern
/// in `musefs-core/tests/reader.rs::resolve_includes_art_image_segments`.
fn build_with_art(audio: &[u8], title: &str, art: &[u8]) -> (tempfile::TempDir, Db, i64) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("song.flac");
    let (audio_offset, audio_length) = write_flac(&path, &["TITLE=Orig"], audio);
    let meta = std::fs::metadata(&path).unwrap();
    let db = Db::open_in_memory().unwrap();
    let id = db
        .upsert_track(&NewTrack {
            backing_path: path.to_string_lossy().to_string(),
            format: Format::Flac,
            audio_offset,
            audio_length,
            backing_size: meta.len() as i64,
            backing_mtime: meta
                .modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64,
        })
        .unwrap();
    db.replace_tags(id, &[Tag::new("title", title, 0)]).unwrap();
    let art_id = db
        .upsert_art(&NewArt {
            mime: "image/png".to_string(),
            width: Some(8),
            height: Some(8),
            data: art.to_vec(),
        })
        .unwrap();
    db.set_track_art(
        id,
        &[TrackArt {
            art_id,
            picture_type: 3,
            description: "front".to_string(),
            ordinal: 0,
        }],
    )
    .unwrap();
    (dir, db, id)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn read_at_preserves_backing_audio(
        audio in proptest::collection::vec(any::<u8>(), 1..512),
        title in "[ -~]{0,32}",
    ) {
        let (_dir, db, id, original) = build(&audio, &title);
        let resolved = HeaderCache::new(Mode::Synthesis).resolve(&db, id).unwrap();
        let whole = read_at(&resolved, &db, 0, resolved.total_len).unwrap();
        prop_assert_eq!(whole.len() as u64, resolved.total_len);
        let served_audio = &whole[resolved.layout.header_len() as usize..];
        prop_assert_eq!(served_audio, &original[..]);
    }

    #[test]
    fn read_at_partial_windows_match_whole(
        audio in proptest::collection::vec(any::<u8>(), 1..512),
        title in "[ -~]{0,32}",
        a in 0usize..4096,
        b in 0usize..4096,
    ) {
        let (_dir, db, id, _orig) = build(&audio, &title);
        let resolved = HeaderCache::new(Mode::Synthesis).resolve(&db, id).unwrap();
        let total = resolved.total_len;
        let whole = read_at(&resolved, &db, 0, total).unwrap();
        let offset = (a as u64) % (total + 1);
        let len = (b as u64) % (total - offset + 1);
        let got = read_at(&resolved, &db, offset, len).unwrap();
        prop_assert_eq!(got.len() as u64, len);
        prop_assert_eq!(&got[..], &whole[offset as usize..(offset + len) as usize]);
    }

    #[test]
    fn read_at_windows_spanning_header_seam(
        audio in proptest::collection::vec(any::<u8>(), 1..512),
        title in "[ -~]{0,32}",
        before in 0usize..4096,
        after in 0usize..4096,
    ) {
        let (_dir, db, id, _orig) = build(&audio, &title);
        let resolved = HeaderCache::new(Mode::Synthesis).resolve(&db, id).unwrap();
        let total = resolved.total_len;
        let hlen = resolved.layout.header_len();
        prop_assume!(hlen > 0 && hlen < total);
        let start = hlen - 1 - (before as u64 % hlen); // in [0, hlen)
        let end = hlen + 1 + (after as u64 % (total - hlen)); // in (hlen, total]
        let whole = read_at(&resolved, &db, 0, total).unwrap();
        let got = read_at(&resolved, &db, start, end - start).unwrap();
        prop_assert_eq!(&got[..], &whole[start as usize..end as usize]);
    }

    #[test]
    fn read_at_art_window_serves_blob(
        audio in proptest::collection::vec(any::<u8>(), 1..256),
        art in proptest::collection::vec(any::<u8>(), 1..256),
        a in 0usize..4096,
        b in 0usize..4096,
    ) {
        let (_dir, db, id) = build_with_art(&audio, "T", &art);
        let resolved = HeaderCache::new(Mode::Synthesis).resolve(&db, id).unwrap();
        let total = resolved.total_len;
        let whole = read_at(&resolved, &db, 0, total).unwrap();

        // Locate the ArtImage segment's exact byte offset in the assembled stream by
        // summing the serving lengths of the segments before it. (Asserting the blob
        // appears at this precise offset is robust; a `windows().any()` search would
        // false-positive when a tiny blob coincidentally matches audio bytes.)
        let mut art_off = 0u64;
        let mut art_len = None;
        for s in &resolved.layout.segments {
            match s {
                Segment::ArtImage { len, .. } => {
                    art_len = Some(*len);
                    break;
                }
                Segment::Inline(bytes) => art_off += bytes.len() as u64,
                Segment::BackingAudio { len, .. } => art_off += *len,
                other => panic!("unexpected FLAC segment: {other:?}"),
            }
        }
        let art_len = art_len.expect("layout has an ArtImage segment");
        prop_assert_eq!(art_len, art.len() as u64);
        // The art blob is served verbatim at its segment offset.
        prop_assert_eq!(
            &whole[art_off as usize..(art_off + art_len) as usize],
            &art[..]
        );
        // A partial window *within the art span* matches the independently-read
        // whole, so the assertion actually exercises art bytes (sampling the whole
        // stream here would be redundant with read_at_partial_windows_match_whole).
        let local_off = (a as u64) % (art_len + 1);
        let offset = art_off + local_off;
        let len = (b as u64) % (art_len - local_off + 1);
        let got = read_at(&resolved, &db, offset, len).unwrap();
        prop_assert_eq!(&got[..], &whole[offset as usize..(offset + len) as usize]);
    }
}

fn build_mp3(audio: &[u8], title: &str) -> (tempfile::TempDir, Db, i64, Vec<u8>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("song.mp3");
    let (audio_offset, audio_length) = write_mp3(&path, audio);
    let meta = std::fs::metadata(&path).unwrap();
    let db = Db::open_in_memory().unwrap();
    let id = db
        .upsert_track(&NewTrack {
            backing_path: path.to_string_lossy().to_string(),
            format: Format::Mp3,
            audio_offset,
            audio_length,
            backing_size: meta.len() as i64,
            backing_mtime: meta
                .modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64,
        })
        .unwrap();
    db.replace_tags(id, &[Tag::new("title", title, 0)]).unwrap();
    (dir, db, id, audio.to_vec())
}

fn build_mp3_with_art(audio: &[u8], title: &str, art: &[u8]) -> (tempfile::TempDir, Db, i64) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("song.mp3");
    let (audio_offset, audio_length) = write_mp3(&path, audio);
    let meta = std::fs::metadata(&path).unwrap();
    let db = Db::open_in_memory().unwrap();
    let id = db
        .upsert_track(&NewTrack {
            backing_path: path.to_string_lossy().to_string(),
            format: Format::Mp3,
            audio_offset,
            audio_length,
            backing_size: meta.len() as i64,
            backing_mtime: meta
                .modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64,
        })
        .unwrap();
    db.replace_tags(id, &[Tag::new("title", title, 0)]).unwrap();
    let art_id = db
        .upsert_art(&NewArt {
            mime: "image/png".to_string(),
            width: Some(8),
            height: Some(8),
            data: art.to_vec(),
        })
        .unwrap();
    db.set_track_art(
        id,
        &[TrackArt {
            art_id,
            picture_type: 3,
            description: "front".to_string(),
            ordinal: 0,
        }],
    )
    .unwrap();
    (dir, db, id)
}

// Finding #5, non-FLAC dimension: the same read-fidelity invariants over the MP3
// synthesis path (regenerated ID3v2 header + backing audio, plus an APIC art
// window). Mirrors the FLAC block above; the WAV dimension lands in its own phase.
proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn read_at_preserves_backing_audio_mp3(
        audio in proptest::collection::vec(any::<u8>(), 1..512),
        title in "[ -~]{0,32}",
    ) {
        let (_dir, db, id, original) = build_mp3(&audio, &title);
        let resolved = HeaderCache::new(Mode::Synthesis).resolve(&db, id).unwrap();
        let whole = read_at(&resolved, &db, 0, resolved.total_len).unwrap();
        prop_assert_eq!(whole.len() as u64, resolved.total_len);
        let served_audio = &whole[resolved.layout.header_len() as usize..];
        prop_assert_eq!(served_audio, &original[..]);
    }

    #[test]
    fn read_at_partial_windows_match_whole_mp3(
        audio in proptest::collection::vec(any::<u8>(), 1..512),
        title in "[ -~]{0,32}",
        a in 0usize..4096,
        b in 0usize..4096,
    ) {
        let (_dir, db, id, _orig) = build_mp3(&audio, &title);
        let resolved = HeaderCache::new(Mode::Synthesis).resolve(&db, id).unwrap();
        let total = resolved.total_len;
        let whole = read_at(&resolved, &db, 0, total).unwrap();
        let offset = (a as u64) % (total + 1);
        let len = (b as u64) % (total - offset + 1);
        let got = read_at(&resolved, &db, offset, len).unwrap();
        prop_assert_eq!(got.len() as u64, len);
        prop_assert_eq!(&got[..], &whole[offset as usize..(offset + len) as usize]);
    }

    #[test]
    fn read_at_windows_spanning_header_seam_mp3(
        audio in proptest::collection::vec(any::<u8>(), 1..512),
        title in "[ -~]{0,32}",
        before in 0usize..4096,
        after in 0usize..4096,
    ) {
        let (_dir, db, id, _orig) = build_mp3(&audio, &title);
        let resolved = HeaderCache::new(Mode::Synthesis).resolve(&db, id).unwrap();
        let total = resolved.total_len;
        let hlen = resolved.layout.header_len();
        prop_assume!(hlen > 0 && hlen < total);
        let start = hlen - 1 - (before as u64 % hlen); // in [0, hlen)
        let end = hlen + 1 + (after as u64 % (total - hlen)); // in (hlen, total]
        let whole = read_at(&resolved, &db, 0, total).unwrap();
        let got = read_at(&resolved, &db, start, end - start).unwrap();
        prop_assert_eq!(&got[..], &whole[start as usize..end as usize]);
    }

    #[test]
    fn read_at_art_window_serves_blob_mp3(
        audio in proptest::collection::vec(any::<u8>(), 1..256),
        art in proptest::collection::vec(any::<u8>(), 1..256),
        a in 0usize..4096,
        b in 0usize..4096,
    ) {
        let (_dir, db, id) = build_mp3_with_art(&audio, "T", &art);
        let resolved = HeaderCache::new(Mode::Synthesis).resolve(&db, id).unwrap();
        let total = resolved.total_len;
        let whole = read_at(&resolved, &db, 0, total).unwrap();

        // Locate the ArtImage segment's exact byte offset by summing the serving
        // lengths of the segments before it (see the FLAC variant for why a
        // byte-search would false-positive).
        let mut art_off = 0u64;
        let mut art_len = None;
        for s in &resolved.layout.segments {
            match s {
                Segment::ArtImage { len, .. } => {
                    art_len = Some(*len);
                    break;
                }
                Segment::Inline(bytes) => art_off += bytes.len() as u64,
                Segment::BackingAudio { len, .. } => art_off += *len,
                other => panic!("unexpected MP3 segment: {other:?}"),
            }
        }
        let art_len = art_len.expect("layout has an ArtImage segment");
        prop_assert_eq!(art_len, art.len() as u64);
        prop_assert_eq!(
            &whole[art_off as usize..(art_off + art_len) as usize],
            &art[..]
        );
        let local_off = (a as u64) % (art_len + 1);
        let offset = art_off + local_off;
        let len = (b as u64) % (art_len - local_off + 1);
        let got = read_at(&resolved, &db, offset, len).unwrap();
        prop_assert_eq!(&got[..], &whole[offset as usize..(offset + len) as usize]);
    }
}
