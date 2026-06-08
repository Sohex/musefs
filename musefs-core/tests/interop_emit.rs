mod common;
use musefs_core::{HeaderCache, Mode, read_at};
use musefs_db::{BinaryTag, Db, Format, NewArt, NewTrack, Tag, TrackArt};
use musefs_format::Segment;
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
    let mut v = u32::try_from(8 + payload.len())
        .unwrap()
        .to_be_bytes()
        .to_vec();
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

// Mirrored byte-for-byte in tests/interop/test_mutagen_roundtrip.py
// (COVR_JPEG / COVR_PNG): mutagen must read these exact images back.
const COVR_JPEG: &[u8] = b"\xFF\xD8\xFF\xE0interop-jpeg-cover";
const COVR_PNG: &[u8] = b"\x89PNG\r\n\x1a\ninterop-png-cover";

fn real_mtime(p: &Path) -> i64 {
    i64::try_from(
        std::fs::metadata(p)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
}

#[derive(Debug)]
struct ManifestRow {
    file: &'static str,
    source_file: &'static str,
    title: &'static str,
    artist: &'static str,
    source_audio_offset: u64,
    source_audio_length: u64,
    synth_audio_offset: u64,
    synth_audio_length: u64,
    ogg_payload_only: bool,
    covr_count: usize,
}

fn synthesized_audio_range(layout: &musefs_format::RegionLayout) -> (u64, u64) {
    let mut output_offset = 0u64;
    for segment in layout.segments() {
        let len = segment.len();
        if matches!(
            segment,
            Segment::BackingAudio { .. } | Segment::OggAudio { .. }
        ) {
            return (output_offset, len);
        }
        output_offset += len;
    }
    panic!("synthesized layout has no audio segment");
}

/// Write `bytes` to `src`, store a track with the given bounds + known tags,
/// assemble the synthesized file via read_at, write it to `dst`, and return the
/// output byte range of the audio payload in the synthesized file.
fn emit(
    src: &Path,
    dst: &Path,
    bytes: &[u8],
    format: Format,
    audio_offset: u64,
    audio_length: u64,
    arts: &[(&[u8], &str)],
) -> (u64, u64) {
    std::fs::write(src, bytes).unwrap();
    let db = Db::open_in_memory().unwrap();
    let id = db
        .upsert_track(&NewTrack {
            backing_path: src.to_string_lossy().into_owned(),
            format,
            audio_offset,
            audio_length,
            backing_size: std::fs::metadata(src).unwrap().len(),
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
    let links: Vec<TrackArt> = arts
        .iter()
        .enumerate()
        .map(|(i, (data, mime))| {
            let art_id = db
                .upsert_art(&NewArt {
                    mime: (*mime).to_string(),
                    width: None,
                    height: None,
                    data: data.to_vec(),
                })
                .unwrap();
            TrackArt {
                art_id,
                picture_type: 3,
                description: String::new(),
                ordinal: i as u64,
            }
        })
        .collect();
    if !links.is_empty() {
        db.set_track_art(id, &links).unwrap();
    }
    let resolved = HeaderCache::new(Mode::Synthesis).resolve(&db, id).unwrap();
    let synth_audio = synthesized_audio_range(&resolved.layout);
    let out = read_at(&resolved, &db, 0, resolved.total_len).unwrap();
    std::fs::write(dst, &out).unwrap();
    synth_audio
}

/// Like `emit`, but also writes promoted text tags and opaque binary tags to the
/// DB before synthesis — mirroring how a media manager populates the store.
#[allow(clippy::too_many_arguments)]
fn emit_binary(
    src: &Path,
    dst: &Path,
    bytes: &[u8],
    format: Format,
    audio_offset: u64,
    audio_length: u64,
    text: &[Tag],
    binary: &[BinaryTag],
) {
    std::fs::write(src, bytes).unwrap();
    let db = Db::open_in_memory().unwrap();
    let id = db
        .upsert_track(&NewTrack {
            backing_path: src.to_string_lossy().into_owned(),
            format,
            audio_offset,
            audio_length,
            backing_size: std::fs::metadata(src).unwrap().len(),
            backing_mtime: real_mtime(src),
        })
        .unwrap();
    db.replace_tags(id, text).unwrap();
    db.set_binary_tags(id, binary).unwrap();
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
    let mut manifest: Vec<ManifestRow> = Vec::new();

    // FLAC
    {
        let bytes = fixtures::flac(&(0..400u32).map(|i| (i % 251) as u8).collect::<Vec<u8>>());
        let scan = musefs_format::flac::locate_audio(&bytes).unwrap();
        let (ao, al) = emit(
            &dir.join("src.flac"),
            &dir.join("out.flac"),
            &bytes,
            Format::Flac,
            scan.audio_offset,
            scan.audio_length,
            &[],
        );
        manifest.push(ManifestRow {
            file: "out.flac",
            source_file: "src.flac",
            title: "Interop Title",
            artist: "Interop Artist",
            source_audio_offset: scan.audio_offset,
            source_audio_length: scan.audio_length,
            synth_audio_offset: ao,
            synth_audio_length: al,
            ogg_payload_only: false,
            covr_count: 0,
        });
    }

    // MP3
    {
        let bytes = fixtures::mp3();
        let b = musefs_format::mp3::locate_audio(&bytes).unwrap();
        let (ao, al) = emit(
            &dir.join("src.mp3"),
            &dir.join("out.mp3"),
            &bytes,
            Format::Mp3,
            b.audio_offset,
            b.audio_length,
            &[],
        );
        manifest.push(ManifestRow {
            file: "out.mp3",
            source_file: "src.mp3",
            title: "Interop Title",
            artist: "Interop Artist",
            source_audio_offset: b.audio_offset,
            source_audio_length: b.audio_length,
            synth_audio_offset: ao,
            synth_audio_length: al,
            ogg_payload_only: false,
            covr_count: 0,
        });
    }

    // MP4 (audio = mdat payload) — use the richer local fixture so that
    // mutagen.mp4.MP4 can open the synthesized output (requires mdhd + stsd).
    {
        let bytes = richer_m4a(&[7u8; 64]);
        let scan = musefs_format::mp4::read_structure(&bytes).unwrap();
        let (ao, al) = emit(
            &dir.join("src.m4a"),
            &dir.join("out.m4a"),
            &bytes,
            Format::M4a,
            scan.mdat_payload_offset,
            scan.mdat_payload_len,
            &[(COVR_JPEG, "image/jpeg"), (COVR_PNG, "image/png")],
        );
        manifest.push(ManifestRow {
            file: "out.m4a",
            source_file: "src.m4a",
            title: "Interop Title",
            artist: "Interop Artist",
            source_audio_offset: scan.mdat_payload_offset,
            source_audio_length: scan.mdat_payload_len,
            synth_audio_offset: ao,
            synth_audio_length: al,
            ogg_payload_only: false,
            covr_count: 2,
        });
    }

    // Ogg
    {
        let bytes = fixtures::ogg_opus();
        let scan = musefs_format::ogg::locate_audio(&bytes).unwrap();
        let (ao, al) = emit(
            &dir.join("src.ogg"),
            &dir.join("out.ogg"),
            &bytes,
            Format::Opus,
            scan.audio_offset,
            scan.audio_length,
            &[],
        );
        manifest.push(ManifestRow {
            file: "out.ogg",
            source_file: "src.ogg",
            title: "Interop Title",
            artist: "Interop Artist",
            source_audio_offset: scan.audio_offset,
            source_audio_length: scan.audio_length,
            synth_audio_offset: ao,
            synth_audio_length: al,
            ogg_payload_only: true,
            covr_count: 0,
        });
    }

    // WAV
    {
        let bytes = fixtures::wav(&[0i16, 1, -1, 100, -100, 32767, -32768, 5, 6, 7]);
        let b = musefs_format::wav::locate_audio(&bytes).unwrap();
        let (ao, al) = emit(
            &dir.join("src.wav"),
            &dir.join("out.wav"),
            &bytes,
            Format::Wav,
            b.audio_offset,
            b.audio_length,
            &[],
        );
        manifest.push(ManifestRow {
            file: "out.wav",
            source_file: "src.wav",
            title: "Interop Title",
            artist: "Interop Artist",
            source_audio_offset: b.audio_offset,
            source_audio_length: b.audio_length,
            synth_audio_offset: ao,
            synth_audio_length: al,
            ogg_payload_only: false,
            covr_count: 0,
        });
    }

    // ── Binary-frame fixtures (spec §Testing: POPM/UFID/PRIV/GEOB + MP4 ----) ──
    // Known ASCII payloads so the Python side compares without hex.
    let priv_owner = "musefs";
    let priv_data = "PRIV-ANALYSIS-001";
    let geob_data = "GEOB-OBJECT-XYZ";
    let mb_trackid = "11111111-2222-3333-4444-555555555555";
    let rating = "200";
    let playcount = "42";
    let freeform_name = "MUSEFSTEST";
    let freeform_data = "FREEFORM-DATA-001";

    // MP3: PRIV + GEOB opaque; POPM/UFID via promoted text tags.
    {
        let bytes = fixtures::mp3();
        let b = musefs_format::mp3::locate_audio(&bytes).unwrap();
        let mut priv_body = priv_owner.as_bytes().to_vec();
        priv_body.push(0);
        priv_body.extend_from_slice(priv_data.as_bytes());
        let mut geob_body = vec![0x00u8]; // latin-1 text encoding
        geob_body.extend_from_slice(b"application/octet-stream\0");
        geob_body.push(0); // empty filename
        geob_body.push(0); // empty description
        geob_body.extend_from_slice(geob_data.as_bytes());
        emit_binary(
            &dir.join("src_bin.mp3"),
            &dir.join("out_bin.mp3"),
            &bytes,
            Format::Mp3,
            b.audio_offset,
            b.audio_length,
            &[
                Tag::new("title", "Bin Title", 0),
                Tag::new("artist", "Bin Artist", 0),
                Tag::new("rating", rating, 0),
                Tag::new("playcount", playcount, 0),
                Tag::new("musicbrainz_trackid", mb_trackid, 0),
            ],
            &[
                BinaryTag {
                    key: "PRIV".into(),
                    payload: priv_body,
                    ordinal: 0,
                },
                BinaryTag {
                    key: "GEOB".into(),
                    payload: geob_body,
                    ordinal: 0,
                },
            ],
        );
    }

    // MP4: one `----` freeform atom.
    {
        let bytes = richer_m4a(&[7u8; 64]);
        let scan = musefs_format::mp4::read_structure(&bytes).unwrap();
        emit_binary(
            &dir.join("src_bin.m4a"),
            &dir.join("out_bin.m4a"),
            &bytes,
            Format::M4a,
            scan.mdat_payload_offset,
            scan.mdat_payload_len,
            &[
                Tag::new("title", "Bin Title", 0),
                Tag::new("artist", "Bin Artist", 0),
            ],
            &[BinaryTag {
                key: format!("----:com.apple.iTunes:{freeform_name}"),
                payload: freeform_data.as_bytes().to_vec(),
                ordinal: 0,
            }],
        );
    }

    // Emit the binary manifest the Python test consumes.
    let binary_manifest = format!(
        "{{\"mp3\":{{\"file\":\"out_bin.mp3\",\"priv_owner\":{priv_owner:?},\"priv_data\":{priv_data:?},\
         \"geob_data\":{geob_data:?},\"rating\":{rating},\"playcount\":{playcount},\
         \"mb_trackid\":{mb_trackid:?}}},\
         \"mp4\":{{\"file\":\"out_bin.m4a\",\"freeform_key\":\"----:com.apple.iTunes:{freeform_name}\",\
         \"freeform_data\":{freeform_data:?}}}}}",
    );
    std::fs::write(dir.join("binary_manifest.json"), binary_manifest).unwrap();

    let json: Vec<String> = manifest
        .iter()
        .map(|row| {
            format!(
                "{{\"file\":{file:?},\"source_file\":{source_file:?},\"title\":{title:?},\"artist\":{artist:?},\"source_audio_offset\":{source_audio_offset},\"source_audio_length\":{source_audio_length},\"synth_audio_offset\":{synth_audio_offset},\"synth_audio_length\":{synth_audio_length},\"ogg_payload_only\":{ogg_payload_only},\"covr_count\":{covr_count}}}",
                file = row.file,
                source_file = row.source_file,
                title = row.title,
                artist = row.artist,
                source_audio_offset = row.source_audio_offset,
                source_audio_length = row.source_audio_length,
                synth_audio_offset = row.synth_audio_offset,
                synth_audio_length = row.synth_audio_length,
                ogg_payload_only = row.ogg_payload_only,
                covr_count = row.covr_count,
            )
        })
        .collect();
    let mut f = std::fs::File::create(dir.join("manifest.json")).unwrap();
    write!(f, "[{}]", json.join(",")).unwrap();
}
