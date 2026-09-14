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
//   • `stsd` (Sample Description, FullBox, one bare `mp4a` entry) inside `stbl`
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
///   - `stsd` FullBox with one bare `mp4a` entry added before `stco` inside `stbl`
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

    // stsd FullBox with one bare `mp4a` AudioSampleEntry. mutagen opens a file
    // whose stsd is empty, but ffprobe refuses one ("invalid STSD entries 0"), and
    // the keyed-metadata test (#771) reads these fixtures with ffprobe too. The
    // entry is the 28 fixed AudioSampleEntry bytes and a `free` child: mutagen
    // reads one child atom there and ignores any that is not `esds`.
    let mut mp4a = vec![0u8; 6]; // reserved
    mp4a.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index
    mp4a.extend_from_slice(&[0u8; 8]); // version, revision, vendor
    mp4a.extend_from_slice(&2u16.to_be_bytes()); // channels
    mp4a.extend_from_slice(&16u16.to_be_bytes()); // sample size
    mp4a.extend_from_slice(&[0u8; 4]); // compression id, packet size
    mp4a.extend_from_slice(&(44_100u32 << 16).to_be_bytes()); // sample rate, 16.16
    mp4a.extend(bx(b"free", b""));
    let mut stsd_payload = vec![0u8; 4]; // version=0, flags=0
    stsd_payload.extend_from_slice(&1u32.to_be_bytes()); // entry_count = 1
    stsd_payload.extend(bx(b"mp4a", &mp4a));
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

/// A bare (QuickTime-style) keyed-metadata `meta`: an `mdta` `hdlr`, a `keys`
/// table naming each item's key, and an `ilst` whose items are typed by their
/// 1-based key index (#771).
fn keyed_meta(items: &[(&str, &[u8])]) -> Vec<u8> {
    let mut hdlr = vec![0u8; 8];
    hdlr.extend_from_slice(b"mdta");
    hdlr.extend_from_slice(&[0u8; 13]);
    let mut keys = vec![0u8; 4];
    keys.extend_from_slice(&u32::try_from(items.len()).unwrap().to_be_bytes());
    let mut ilst = Vec::new();
    for (i, (name, value)) in items.iter().enumerate() {
        keys.extend_from_slice(&u32::try_from(8 + name.len()).unwrap().to_be_bytes());
        keys.extend_from_slice(b"mdta");
        keys.extend_from_slice(name.as_bytes());
        let index = u32::try_from(i + 1).unwrap().to_be_bytes();
        ilst.extend(bx(&index, &m4a_data_atom(1, value)));
    }
    bx(
        b"meta",
        &[bx(b"hdlr", &hdlr), bx(b"keys", &keys), bx(b"ilst", &ilst)].concat(),
    )
}

/// `richer_m4a` plus Apple-style keyed metadata at the movie level and in the
/// audio track, whose values must not survive synthesis. Appended to the end of
/// `moov` and `trak`, which precede `mdat`, so the fixture's `stco` placeholder
/// is unaffected.
fn richer_m4a_with_keyed_metadata(mdat_payload: &[u8]) -> Vec<u8> {
    let plain = richer_m4a(mdat_payload);
    let movie = keyed_meta(&[
        ("com.apple.quicktime.artist", b"Old Keyed Artist"),
        ("com.apple.quicktime.title", b"Old Keyed Title"),
    ]);
    let track = keyed_meta(&[("com.apple.quicktime.comment", b"Old Keyed Comment")]);
    // Re-box: splice `track` at the end of the trak, `movie` at the end of moov.
    let moov_at = plain.windows(4).position(|w| w == b"moov").unwrap() - 4;
    let moov_len = usize::try_from(u32::from_be_bytes(
        plain[moov_at..moov_at + 4].try_into().unwrap(),
    ))
    .unwrap();
    let trak_at = plain.windows(4).position(|w| w == b"trak").unwrap() - 4;
    let trak_len = usize::try_from(u32::from_be_bytes(
        plain[trak_at..trak_at + 4].try_into().unwrap(),
    ))
    .unwrap();
    let trak = bx(
        b"trak",
        &[&plain[trak_at + 8..trak_at + trak_len], &track[..]].concat(),
    );
    let moov_body = [
        &plain[moov_at + 8..trak_at],
        &trak[..],
        &plain[trak_at + trak_len..moov_at + moov_len],
        &movie[..],
    ]
    .concat();
    let mut out = [
        &plain[..moov_at],
        &bx(b"moov", &moov_body)[..],
        &plain[moov_at + moov_len..],
    ]
    .concat();
    // Point the `stco` entry at the real payload: synthesis drops the keyed
    // metas, shrinking `moov`, so a placeholder 0 would relocate below zero.
    let payload_at = u32::try_from(out.len() - mdat_payload.len()).unwrap();
    let entry = out.windows(4).position(|w| w == b"stco").unwrap() + 12;
    out[entry..entry + 4].copy_from_slice(&payload_at.to_be_bytes());
    out
}
// ── end local M4A helper ─────────────────────────────────────────────────────

// Mirrored byte-for-byte in tests/interop/test_mutagen_roundtrip.py
// (COVR_JPEG / COVR_PNG): mutagen must read these exact images back.
const COVR_JPEG: &[u8] = b"\xFF\xD8\xFF\xE0interop-jpeg-cover";
const COVR_PNG: &[u8] = b"\x89PNG\r\n\x1a\ninterop-png-cover";

/// One art link as a fixture declares it.
struct ArtLink {
    data: &'static [u8],
    mime: &'static str,
    picture_type: u32,
    description: &'static str,
    width: Option<u32>,
    height: Option<u32>,
    depth: u32,
    colors: u32,
}

impl ArtLink {
    /// A link that states nothing beyond its mime: MP4 `covr` has no field for
    /// the rest.
    const fn plain(data: &'static [u8], mime: &'static str) -> ArtLink {
        ArtLink {
            data,
            mime,
            picture_type: 3,
            description: "",
            width: None,
            height: None,
            depth: 0,
            colors: 0,
        }
    }
}

/// The FLAC and MP3 fixtures' pictures, every field away from its default so a
/// serializer that drops one is caught rather than matched by the default. The
/// PNG comes first so the order is not the M4A fixture's either. Mirrored in
/// tests/interop/test_mutagen_roundtrip.py (PICTURES); ID3 `APIC` carries no
/// geometry, so the MP3 side asserts all but width/height/depth/colors.
const PICTURES: [ArtLink; 2] = [
    ArtLink {
        data: COVR_PNG,
        mime: "image/png",
        picture_type: 4,
        description: "Back Cover",
        width: Some(300),
        height: Some(200),
        depth: 8,
        colors: 256,
    },
    ArtLink {
        data: COVR_JPEG,
        mime: "image/jpeg",
        picture_type: 6,
        description: "Disc",
        width: Some(640),
        height: Some(480),
        depth: 24,
        colors: 0,
    },
];

fn real_mtime_ns(p: &Path) -> i64 {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(p).unwrap();
    meta.mtime() * 1_000_000_000 + meta.mtime_nsec()
}

fn real_ctime_ns(p: &Path) -> i64 {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(p).unwrap();
    meta.ctime() * 1_000_000_000 + meta.ctime_nsec()
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
    arts: &[ArtLink],
) -> (u64, u64) {
    std::fs::write(src, bytes).unwrap();
    let db = Db::open_in_memory().unwrap();
    let id = db
        .upsert_track(&NewTrack {
            backing_path: src.to_path_buf(),
            format,
            audio_offset,
            audio_length,
            backing_size: std::fs::metadata(src).unwrap().len(),
            backing_mtime_ns: real_mtime_ns(src),
            backing_ctime_ns: real_ctime_ns(src),
            backing_ino: None,
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
        .map(|(i, art)| {
            let art_id = db
                .upsert_art(&NewArt {
                    data: art.data.to_vec(),
                })
                .unwrap();
            // Every field is the fixture's own: mutagen reads each one back off
            // the synthesized block, which is the round trip this suite exists
            // to prove.
            TrackArt {
                art_id,
                picture_type: art.picture_type,
                description: art.description.to_string(),
                mime: art.mime.to_string(),
                width: art.width,
                height: art.height,
                depth: art.depth,
                colors: art.colors,
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
            backing_path: src.to_path_buf(),
            format,
            audio_offset,
            audio_length,
            backing_size: std::fs::metadata(src).unwrap().len(),
            backing_mtime_ns: real_mtime_ns(src),
            backing_ctime_ns: real_ctime_ns(src),
            backing_ino: None,
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
            &PICTURES,
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
            &PICTURES,
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

    // MP3 whose backing carries a prepended tag, an appended ID3v2.4 tag with
    // its footer, and an ID3v1 trailer (#768). The served file must carry the
    // synthesized front tag and none of the three.
    {
        let bytes = fixtures::mp3_with_front_and_back_tags();
        let b = musefs_format::mp3::locate_audio(&bytes).unwrap();
        let (ao, al) = emit(
            &dir.join("src_multi.mp3"),
            &dir.join("out_multi.mp3"),
            &bytes,
            Format::Mp3,
            b.audio_offset,
            b.audio_length,
            &PICTURES,
        );
        manifest.push(ManifestRow {
            file: "out_multi.mp3",
            source_file: "src_multi.mp3",
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
            &[
                ArtLink::plain(COVR_JPEG, "image/jpeg"),
                ArtLink::plain(COVR_PNG, "image/png"),
            ],
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

    // MP4 whose source carries QuickTime keyed metadata (#771): the served file
    // must carry only the store's tags, so no reader sees the old keyed values.
    {
        let bytes = richer_m4a_with_keyed_metadata(&[7u8; 64]);
        let scan = musefs_format::mp4::read_structure(&bytes).unwrap();
        let (ao, al) = emit(
            &dir.join("src_keyed.m4a"),
            &dir.join("out_keyed.m4a"),
            &bytes,
            Format::M4a,
            scan.mdat_payload_offset,
            scan.mdat_payload_len,
            &[],
        );
        manifest.push(ManifestRow {
            file: "out_keyed.m4a",
            source_file: "src_keyed.m4a",
            title: "Interop Title",
            artist: "Interop Artist",
            source_audio_offset: scan.mdat_payload_offset,
            source_audio_length: scan.mdat_payload_len,
            synth_audio_offset: ao,
            synth_audio_length: al,
            ogg_payload_only: false,
            covr_count: 0,
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

    // WAV, big-endian (RIFX, #770). mutagen reads only RIFF, so the Python side
    // reads this one through libsndfile instead.
    {
        let bytes = fixtures::wav_in(
            &[0x0102i16, -2, 300, -32768, 32767, 5, 6, 7],
            musefs_format::wav::ByteOrder::Big,
        );
        let b = musefs_format::wav::locate_audio(&bytes).unwrap();
        let (ao, al) = emit(
            &dir.join("src_rifx.wav"),
            &dir.join("out_rifx.wav"),
            &bytes,
            Format::Wav,
            b.audio_offset,
            b.audio_length,
            &[],
        );
        manifest.push(ManifestRow {
            file: "out_rifx.wav",
            source_file: "src_rifx.wav",
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
    let geob_filename = "analysis.bin";
    let geob_desc = "musefs interop";
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
        geob_body.extend_from_slice(geob_filename.as_bytes());
        geob_body.push(0);
        geob_body.extend_from_slice(geob_desc.as_bytes());
        geob_body.push(0);
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
         \"geob_data\":{geob_data:?},\"geob_mime\":\"application/octet-stream\",\
         \"geob_filename\":{geob_filename:?},\"geob_desc\":{geob_desc:?},\"rating\":{rating},\"playcount\":{playcount},\
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
