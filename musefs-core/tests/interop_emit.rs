mod common;
use musefs_core::{read_at, HeaderCache, Mode};
use musefs_db::{Db, Format, NewTrack, Tag};
use musefs_format::fuzz_check::fixtures;
use std::io::Write;
use std::path::Path;

fn real_mtime(p: &Path) -> i64 {
    std::fs::metadata(p)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Write `bytes` to `src`, store a track with the given bounds + known tags,
/// assemble the synthesized file via read_at, write it to `dst`, and return the
/// (title, artist) we expect a reader to see back.
fn emit(
    src: &Path,
    dst: &Path,
    bytes: &[u8],
    format: Format,
    audio_offset: i64,
    audio_length: i64,
) {
    std::fs::write(src, bytes).unwrap();
    let db = Db::open_in_memory().unwrap();
    let id = db
        .upsert_track(&NewTrack {
            backing_path: src.to_string_lossy().to_string(),
            format,
            audio_offset,
            audio_length,
            backing_size: std::fs::metadata(src).unwrap().len() as i64,
            backing_mtime: real_mtime(src),
        })
        .unwrap();
    db.replace_tags(
        id,
        &[
            Tag::new("title", "Interop Title", 0),
            Tag::new("artist", "Interop Artist", 0),
        ],
    )
    .unwrap();
    let resolved = HeaderCache::new(Mode::Synthesis).resolve(&db, id).unwrap();
    let out = read_at(&resolved, &db, 0, resolved.total_len).unwrap();
    std::fs::write(dst, &out).unwrap();
}

#[test]
#[ignore = "interop fixture emitter; run explicitly with MUSEFS_INTEROP_DIR set"]
fn emit_interop_fixtures() {
    let dir = std::env::var("MUSEFS_INTEROP_DIR").expect("set MUSEFS_INTEROP_DIR");
    let dir = Path::new(&dir);
    std::fs::create_dir_all(dir).unwrap();
    let mut manifest: Vec<(&str, &str, &str)> = Vec::new(); // (file, title, artist)

    // FLAC
    {
        let bytes = fixtures::flac(&(0..400u32).map(|i| (i % 251) as u8).collect::<Vec<u8>>());
        let scan = musefs_format::flac::locate_audio(&bytes).unwrap();
        emit(
            &dir.join("src.flac"),
            &dir.join("out.flac"),
            &bytes,
            Format::Flac,
            scan.audio_offset as i64,
            scan.audio_length as i64,
        );
        manifest.push(("out.flac", "Interop Title", "Interop Artist"));
    }

    // MP3
    {
        let bytes = fixtures::mp3();
        let b = musefs_format::mp3::locate_audio(&bytes).unwrap();
        emit(
            &dir.join("src.mp3"),
            &dir.join("out.mp3"),
            &bytes,
            Format::Mp3,
            b.audio_offset as i64,
            b.audio_length as i64,
        );
        manifest.push(("out.mp3", "Interop Title", "Interop Artist"));
    }

    // MP4 (audio = mdat payload)
    {
        let bytes = fixtures::m4a(&[7u8; 64]);
        let scan = musefs_format::mp4::read_structure(&bytes).unwrap();
        emit(
            &dir.join("src.m4a"),
            &dir.join("out.m4a"),
            &bytes,
            Format::M4a,
            scan.mdat_payload_offset as i64,
            scan.mdat_payload_len as i64,
        );
        manifest.push(("out.m4a", "Interop Title", "Interop Artist"));
    }

    // Ogg
    {
        let bytes = fixtures::ogg_opus();
        let scan = musefs_format::ogg::locate_audio(&bytes).unwrap();
        emit(
            &dir.join("src.ogg"),
            &dir.join("out.ogg"),
            &bytes,
            Format::Opus,
            scan.audio_offset as i64,
            scan.audio_length as i64,
        );
        manifest.push(("out.ogg", "Interop Title", "Interop Artist"));
    }

    // WAV
    {
        let bytes = fixtures::wav(&[0i16, 1, -1, 100, -100, 32767, -32768, 5, 6, 7]);
        let b = musefs_format::wav::locate_audio(&bytes).unwrap();
        emit(
            &dir.join("src.wav"),
            &dir.join("out.wav"),
            &bytes,
            Format::Wav,
            b.audio_offset as i64,
            b.audio_length as i64,
        );
        manifest.push(("out.wav", "Interop Title", "Interop Artist"));
    }

    let json: Vec<String> = manifest
        .iter()
        .map(|(f, t, a)| format!("{{\"file\":{f:?},\"title\":{t:?},\"artist\":{a:?}}}"))
        .collect();
    let mut f = std::fs::File::create(dir.join("manifest.json")).unwrap();
    write!(f, "[{}]", json.join(",")).unwrap();
}
