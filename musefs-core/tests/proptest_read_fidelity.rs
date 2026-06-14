mod common;
use common::{write_flac, write_m4a, write_mp3, write_wav};
use musefs_core::{HeaderCache, Mode, ResolvedFile, read_at};
use musefs_db::{BinaryTag, Db, Format, NewArt, NewTrack, Tag, TrackArt};
use musefs_format::Segment;
use proptest::prelude::*;
use std::path::Path;

/// Build a single-track in-memory store backing `audio` with a `title` tag, using
/// `writer` to lay down the format-specific backing file at `song.<ext>`. When
/// `art` is `Some`, an 8×8 PNG blob is inserted and linked so the resolved layout
/// carries an `ArtImage` segment (mirrors the insert+link pattern in
/// `reader.rs::resolve_includes_art_image_segments`).
fn build_track(
    ext: &str,
    format: Format,
    audio: &[u8],
    title: &str,
    art: Option<&[u8]>,
    writer: impl Fn(&Path, &[u8]) -> (u64, u64),
) -> (tempfile::TempDir, Db, i64) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(format!("song.{ext}"));
    let (audio_offset, audio_length) = writer(&path, audio);
    let meta = std::fs::metadata(&path).unwrap();
    let db = Db::open_in_memory().unwrap();
    let id = db
        .upsert_track(&NewTrack {
            backing_path: path.to_string_lossy().into_owned(),
            format,
            audio_offset,
            audio_length,
            backing_size: meta.len(),
            backing_mtime_ns: common::real_mtime_ns(&path),
            backing_ctime_ns: common::real_ctime_ns(&path),
        })
        .unwrap();
    db.replace_tags(id, &[Tag::new("title", title, 0)]).unwrap();
    if let Some(art) = art {
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
    }
    (dir, db, id)
}

fn build_flac(audio: &[u8], title: &str) -> (tempfile::TempDir, Db, i64) {
    build_track("flac", Format::Flac, audio, title, None, |p, a| {
        write_flac(p, &["TITLE=Orig"], a)
    })
}
fn build_flac_with_art(audio: &[u8], title: &str, art: &[u8]) -> (tempfile::TempDir, Db, i64) {
    build_track("flac", Format::Flac, audio, title, Some(art), |p, a| {
        write_flac(p, &["TITLE=Orig"], a)
    })
}
fn build_wav(audio: &[u8], title: &str) -> (tempfile::TempDir, Db, i64) {
    build_track("wav", Format::Wav, audio, title, None, write_wav)
}
fn build_wav_with_art(audio: &[u8], title: &str, art: &[u8]) -> (tempfile::TempDir, Db, i64) {
    build_track("wav", Format::Wav, audio, title, Some(art), write_wav)
}
fn build_mp3(audio: &[u8], title: &str) -> (tempfile::TempDir, Db, i64) {
    build_track("mp3", Format::Mp3, audio, title, None, write_mp3)
}
fn build_mp3_with_art(audio: &[u8], title: &str, art: &[u8]) -> (tempfile::TempDir, Db, i64) {
    build_track("mp3", Format::Mp3, audio, title, Some(art), write_mp3)
}
fn build_m4a(audio: &[u8], title: &str) -> (tempfile::TempDir, Db, i64) {
    build_track("m4a", Format::M4a, audio, title, None, write_m4a)
}
fn build_m4a_with_art(audio: &[u8], title: &str, art: &[u8]) -> (tempfile::TempDir, Db, i64) {
    build_track("m4a", Format::M4a, audio, title, Some(art), write_m4a)
}
fn build_ogg(audio: &[u8], title: &str) -> (tempfile::TempDir, Db, i64) {
    build_track("opus", Format::Opus, audio, title, None, common::write_ogg)
}
fn build_ogg_with_art(audio: &[u8], title: &str, art: &[u8]) -> (tempfile::TempDir, Db, i64) {
    build_track(
        "opus",
        Format::Opus,
        audio,
        title,
        Some(art),
        common::write_ogg,
    )
}

fn resolve(db: &Db, id: i64) -> std::sync::Arc<ResolvedFile> {
    HeaderCache::new(Mode::Synthesis).resolve(db, id).unwrap()
}

// --- the four read-fidelity invariants, one body each, shared across formats ---

/// The served audio (everything after the synthesized header) is byte-identical to
/// the original backing audio. Used by formats with a clean header/audio split.
fn check_preserves_backing_audio(
    resolved: &ResolvedFile,
    db: &Db,
    original: &[u8],
) -> Result<(), TestCaseError> {
    let whole = read_at(resolved, db, 0, resolved.total_len).unwrap();
    prop_assert_eq!(whole.len() as u64, resolved.total_len);
    let served_audio = &whole[usize::try_from(resolved.layout.header_len()).unwrap()..];
    prop_assert_eq!(served_audio, original);
    Ok(())
}

/// WAV variant: the `data` payload is byte-identical to the original audio, but it
/// is not the trailing bytes (a word-align pad may follow), so locate it.
fn check_wav_preserves_backing_audio(
    resolved: &ResolvedFile,
    db: &Db,
    original: &[u8],
) -> Result<(), TestCaseError> {
    let whole = read_at(resolved, db, 0, resolved.total_len).unwrap();
    prop_assert_eq!(whole.len() as u64, resolved.total_len);
    let bounds = musefs_format::wav::locate_audio(&whole).unwrap();
    prop_assert_eq!(
        &whole[usize::try_from(bounds.audio_offset).unwrap()
            ..usize::try_from(bounds.audio_offset + bounds.audio_length).unwrap()],
        original
    );
    Ok(())
}

/// An arbitrary `[offset, offset+len)` window matches the same slice of the
/// independently-read whole stream.
fn check_partial_window(
    resolved: &ResolvedFile,
    db: &Db,
    a: usize,
    b: usize,
) -> Result<(), TestCaseError> {
    let total = resolved.total_len;
    let whole = read_at(resolved, db, 0, total).unwrap();
    let offset = (a as u64) % (total + 1);
    let len = (b as u64) % (total - offset + 1);
    let got = read_at(resolved, db, offset, len).unwrap();
    prop_assert_eq!(got.len() as u64, len);
    prop_assert_eq!(
        &got[..],
        &whole[usize::try_from(offset).unwrap()..usize::try_from(offset + len).unwrap()]
    );
    Ok(())
}

/// A window straddling the header/audio seam matches the whole stream.
fn check_header_seam(
    resolved: &ResolvedFile,
    db: &Db,
    before: usize,
    after: usize,
) -> Result<(), TestCaseError> {
    let total = resolved.total_len;
    let hlen = resolved.layout.header_len();
    prop_assume!(hlen > 0 && hlen < total);
    let start = hlen - 1 - (before as u64 % hlen); // in [0, hlen)
    let end = hlen + 1 + (after as u64 % (total - hlen)); // in (hlen, total]
    let whole = read_at(resolved, db, 0, total).unwrap();
    let got = read_at(resolved, db, start, end - start).unwrap();
    prop_assert_eq!(
        &got[..],
        &whole[usize::try_from(start).unwrap()..usize::try_from(end).unwrap()]
    );
    Ok(())
}

/// The `ArtImage` blob is served verbatim at its segment offset, and a partial
/// window *within the art span* matches the whole stream. The offset is located by
/// summing the serving lengths of the segments before it (asserting the blob
/// appears at this precise offset is robust; a `windows().any()` search would
/// false-positive when a tiny blob coincidentally matches audio bytes).
fn check_art_window_serves_blob(
    resolved: &ResolvedFile,
    db: &Db,
    art: &[u8],
    a: usize,
    b: usize,
    label: &str,
) -> Result<(), TestCaseError> {
    let total = resolved.total_len;
    let whole = read_at(resolved, db, 0, total).unwrap();

    let mut art_off = 0u64;
    let mut art_len = None;
    for s in resolved.layout.segments() {
        match s {
            Segment::ArtImage { len, .. } => {
                art_len = Some(len.get());
                break;
            }
            Segment::Inline(bytes) => art_off += bytes.len() as u64,
            Segment::BackingAudio { len, .. } => art_off += *len,
            other => panic!("unexpected {label} segment: {other:?}"),
        }
    }
    let art_len = art_len.expect("layout has an ArtImage segment");
    prop_assert_eq!(art_len, art.len() as u64);
    prop_assert_eq!(
        &whole[usize::try_from(art_off).unwrap()..usize::try_from(art_off + art_len).unwrap()],
        art
    );
    // A partial window within the art span (sampling the whole stream here would be
    // redundant with check_partial_window).
    let local_off = (a as u64) % (art_len + 1);
    let offset = art_off + local_off;
    let len = (b as u64) % (art_len - local_off + 1);
    let got = read_at(resolved, db, offset, len).unwrap();
    prop_assert_eq!(
        &got[..],
        &whole[usize::try_from(offset).unwrap()..usize::try_from(offset + len).unwrap()]
    );
    Ok(())
}

// --- per-property test stampers: each emits one proptest! block in the caller's
// module, so a format module composes only the invariants it actually has ---

macro_rules! prop_preserves_backing_audio {
    ($build:path) => {
        proptest! {
            #![proptest_config(ProptestConfig::with_cases(64))]
            #[test]
            fn preserves_backing_audio(
                audio in proptest::collection::vec(any::<u8>(), 1..512),
                title in "[ -~]{0,32}",
            ) {
                let (_dir, db, id) = $build(&audio, &title);
                let resolved = resolve(&db, id);
                check_preserves_backing_audio(&resolved, &db, &audio)?;
            }
        }
    };
}

macro_rules! prop_wav_preserves_backing_audio {
    ($build:path) => {
        proptest! {
            #![proptest_config(ProptestConfig::with_cases(64))]
            #[test]
            fn preserves_backing_audio(
                audio in proptest::collection::vec(any::<u8>(), 1..512),
                title in "[ -~]{0,32}",
            ) {
                let (_dir, db, id) = $build(&audio, &title);
                let resolved = resolve(&db, id);
                check_wav_preserves_backing_audio(&resolved, &db, &audio)?;
            }
        }
    };
}

macro_rules! prop_partial_windows {
    ($build:path) => {
        proptest! {
            #![proptest_config(ProptestConfig::with_cases(64))]
            #[test]
            fn partial_windows_match_whole(
                audio in proptest::collection::vec(any::<u8>(), 1..512),
                title in "[ -~]{0,32}",
                a in 0usize..4096,
                b in 0usize..4096,
            ) {
                let (_dir, db, id) = $build(&audio, &title);
                let resolved = resolve(&db, id);
                check_partial_window(&resolved, &db, a, b)?;
            }
        }
    };
}

macro_rules! prop_header_seam {
    ($build:path) => {
        proptest! {
            #![proptest_config(ProptestConfig::with_cases(64))]
            #[test]
            fn windows_spanning_header_seam(
                audio in proptest::collection::vec(any::<u8>(), 1..512),
                title in "[ -~]{0,32}",
                before in 0usize..4096,
                after in 0usize..4096,
            ) {
                let (_dir, db, id) = $build(&audio, &title);
                let resolved = resolve(&db, id);
                check_header_seam(&resolved, &db, before, after)?;
            }
        }
    };
}

macro_rules! prop_art_window {
    ($build:path, $label:literal) => {
        proptest! {
            #![proptest_config(ProptestConfig::with_cases(64))]
            #[test]
            fn art_window_serves_blob(
                audio in proptest::collection::vec(any::<u8>(), 1..256),
                art in proptest::collection::vec(any::<u8>(), 1..256),
                a in 0usize..4096,
                b in 0usize..4096,
            ) {
                let (_dir, db, id) = $build(&audio, "T", &art);
                let resolved = resolve(&db, id);
                check_art_window_serves_blob(&resolved, &db, &art, a, b, $label)?;
            }
        }
    };
}

// FLAC, MP3, M4A share the full four-invariant set (clean header/audio split,
// exact art-blob location). WAV diverges only in `preserves_backing_audio`
// (`locate_audio` word-align padding). Ogg lacks a clean header/audio split (no
// `preserves_backing_audio` variant) and patches art in-place rather than serving a
// standalone blob (only the partial-window invariant applies, via `ogg_art`).

mod flac {
    use super::*;
    prop_preserves_backing_audio!(build_flac);
    prop_partial_windows!(build_flac);
    prop_header_seam!(build_flac);
    prop_art_window!(build_flac_with_art, "FLAC");
}

mod wav {
    use super::*;
    prop_wav_preserves_backing_audio!(build_wav);
    prop_partial_windows!(build_wav);
    prop_header_seam!(build_wav);
    prop_art_window!(build_wav_with_art, "WAV");
}

mod mp3 {
    use super::*;
    prop_preserves_backing_audio!(build_mp3);
    prop_partial_windows!(build_mp3);
    prop_header_seam!(build_mp3);
    prop_art_window!(build_mp3_with_art, "MP3");

    // MP3-only: a PRIV/rating-bearing header must still leave the backing audio
    // untouched — `Segment::BinaryTag` emission never disturbs the `BackingAudio`
    // run. Hand-written (unique fixture) rather than stamped from a macro.
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        #[test]
        fn preserves_backing_audio_with_binary_frames(
            audio in proptest::collection::vec(any::<u8>(), 1..512),
            priv_payload in proptest::collection::vec(any::<u8>(), 1..120),
            rating in 0u8..=255,
        ) {
            let (_dir, db, id) = build_mp3(&audio, "Bin Title");
            db.set_binary_tags(
                id,
                &[BinaryTag {
                    key: "PRIV".into(),
                    payload: {
                        let mut p = b"musefs\0".to_vec();
                        p.extend_from_slice(&priv_payload);
                        p
                    },
                    ordinal: 0,
                }],
            )
            .unwrap();
            db.replace_tags(
                id,
                &[Tag::new("title", "Bin Title", 0), Tag::new("rating", &rating.to_string(), 0)],
            )
            .unwrap();

            let resolved = resolve(&db, id);
            prop_assert!(
                resolved.layout.segments().iter().any(|s| matches!(s, Segment::BinaryTag { .. })),
                "resolve did not emit a BinaryTag segment"
            );
            check_preserves_backing_audio(&resolved, &db, &audio)?;
        }
    }
}

mod m4a {
    use super::*;
    prop_preserves_backing_audio!(build_m4a);
    prop_partial_windows!(build_m4a);
    prop_header_seam!(build_m4a);
    prop_art_window!(build_m4a_with_art, "M4A");
}

mod ogg {
    use super::*;
    prop_partial_windows!(build_ogg);
    prop_header_seam!(build_ogg);
}

mod ogg_art {
    use super::*;
    // Ogg patches art in-place (no standalone blob), so only the partial-window
    // invariant applies; the art builder takes an extra `art` argument, so this is
    // stamped by hand rather than via `prop_partial_windows!`.
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        #[test]
        fn partial_windows_match_whole(
            audio in proptest::collection::vec(any::<u8>(), 1..256),
            art in proptest::collection::vec(any::<u8>(), 1..256),
            a in 0usize..4096,
            b in 0usize..4096,
        ) {
            let (_dir, db, id) = build_ogg_with_art(&audio, "T", &art);
            let resolved = resolve(&db, id);
            check_partial_window(&resolved, &db, a, b)?;
        }
    }
}
