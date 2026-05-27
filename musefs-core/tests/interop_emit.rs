mod common;
use musefs_core::{read_at, HeaderCache, Mode};
use musefs_db::{Db, Format, NewTrack, Tag};
use musefs_format::fuzz_check::fixtures;
use std::io::Write;
use std::path::Path;

// ── local helper: richer M4A for the interop fixture ────────────────────────
//
// `fuzz_check::fixtures::m4a` is intentionally minimal (no `mdhd`, no `stsd`)
// and is used by many other tests — do NOT touch it.  This local builder adds
// the two boxes that mutagen's MP4 stream-info parser requires so that
// `mutagen.mp4.MP4(path)` opens the synthesized output without error:
//
//   • `mdhd` (Media Header, FullBox v0) inside `trak/mdia`
//   • `stsd` (Sample Description, FullBox, entry_count=0) inside `stbl`
//
// musefs's own `read_structure` / `validate_moov` is unaffected: it only
// requires `ftyp`, `mvhd`, exactly one `soun` trak whose `stbl` has `stco`,
// and `udta/meta/ilst`.
fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut v = ((8 + payload.len()) as u32).to_be_bytes().to_vec();
    v.extend_from_slice(kind);
    v.extend_from_slice(payload);
    v
}

fn m4a_data_atom(type_code: u32, value: &[u8]) -> Vec<u8> {
    let mut p = type_code.to_be_bytes().to_vec();
    p.extend_from_slice(&0u32.to_be_bytes()); // locale
    p.extend_from_slice(value);
    bx(b"data", &p)
}

/// Richer M4A fixture accepted by both musefs's `read_structure` and
/// `mutagen.mp4.MP4`.  Differences from `fuzz_check::fixtures::m4a`:
///   - `mdhd` v0 FullBox added before `hdlr` inside `trak/mdia`
///   - empty `stsd` FullBox added before `stco` inside `stbl`
fn richer_m4a(mdat_payload: &[u8]) -> Vec<u8> {
    // ilst tag atoms
    let ilst_atoms = [
        bx(b"\xa9nam", &m4a_data_atom(1, b"Orig M4A")),
        bx(b"\xa9ART", &m4a_data_atom(1, b"Orig Artist")),
    ]
    .concat();
    let ilst = bx(b"ilst", &ilst_atoms);

    // meta FullBox (4-byte version/flags prefix, then hdlr + ilst)
    let mut meta_hdlr_payload = vec![0u8; 8];
    meta_hdlr_payload.extend_from_slice(b"mdir");
    meta_hdlr_payload.extend_from_slice(b"appl");
    meta_hdlr_payload.extend_from_slice(&[0u8; 9]);
    let mut meta_payload = vec![0u8; 4]; // version=0, flags=0
    meta_payload.extend(bx(b"hdlr", &meta_hdlr_payload));
    meta_payload.extend(ilst);
    let udta = bx(b"udta", &bx(b"meta", &meta_payload));

    // soun handler
    let mut soun_hdlr_payload = vec![0u8; 8];
    soun_hdlr_payload.extend_from_slice(b"soun");
    soun_hdlr_payload.extend_from_slice(&[0u8; 12]);
    let soun_hdlr = bx(b"hdlr", &soun_hdlr_payload);

    // stco FullBox: version/flags(4) + entry_count(4) + one placeholder offset(4)
    let mut stco_payload = vec![0u8; 4];
    stco_payload.extend_from_slice(&1u32.to_be_bytes());
    stco_payload.extend_from_slice(&0u32.to_be_bytes());

    // stsd FullBox (empty — entry_count=0); mutagen reads it but does not
    // require actual codec entries to open the file.
    let mut stsd_payload = vec![0u8; 4]; // version=0, flags=0
    stsd_payload.extend_from_slice(&0u32.to_be_bytes()); // entry_count = 0
    let stbl = bx(
        b"stbl",
        &[bx(b"stsd", &stsd_payload), bx(b"stco", &stco_payload)].concat(),
    );

    // mdhd v0 FullBox:
    //   version(1)+flags(3) | creation_time(4) | modification_time(4) |
    //   timescale(4) | duration(4) | language(2) | pre_defined(2)
    let mut mdhd_payload = vec![0u8; 4]; // version=0, flags=0
    mdhd_payload.extend_from_slice(&0u32.to_be_bytes()); // creation_time
    mdhd_payload.extend_from_slice(&0u32.to_be_bytes()); // modification_time
    mdhd_payload.extend_from_slice(&1000u32.to_be_bytes()); // timescale
    mdhd_payload.extend_from_slice(&1000u32.to_be_bytes()); // duration
    mdhd_payload.extend_from_slice(&[0x55, 0xc4]); // language (und)
    mdhd_payload.extend_from_slice(&[0x00, 0x00]); // pre_defined
    let mdhd = bx(b"mdhd", &mdhd_payload);

    let mdia = bx(b"mdia", &[mdhd, soun_hdlr, bx(b"minf", &stbl)].concat());
    let trak = bx(b"trak", &mdia);
    let moov = bx(b"moov", &[bx(b"mvhd", &[0u8; 8]), trak, udta].concat());

    [bx(b"ftyp", b"M4A isom"), moov, bx(b"mdat", mdat_payload)].concat()
}
// ── end local M4A helper ─────────────────────────────────────────────────────

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

    // MP4 (audio = mdat payload) — use the richer local fixture so that
    // mutagen.mp4.MP4 can open the synthesized output (requires mdhd + stsd).
    {
        let bytes = richer_m4a(&[7u8; 64]);
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
