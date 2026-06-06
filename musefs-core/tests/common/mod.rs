#![allow(dead_code)]

use std::path::Path;

pub mod corpus;
pub mod report;

pub use musefs_format::fuzz_check::fixtures::{
    flac_block, make_flac, streaminfo_body, vorbis_comment_body,
};

/// Write a simple FLAC (STREAMINFO + comment + audio) to `path`,
/// returning (audio_offset, audio_length).
pub fn write_flac(path: &Path, comments: &[&str], audio: &[u8]) -> (u64, u64) {
    let si = streaminfo_body();
    let vc = vorbis_comment_body("orig", comments);
    let bytes = make_flac(&[(0, si), (4, vc)], audio);
    let audio_offset = (bytes.len() - audio.len()) as u64;
    std::fs::write(path, &bytes).unwrap();
    (audio_offset, audio.len() as u64)
}

/// Write a minimal MP3 (a 10-byte empty ID3v2.4 tag, then the given audio bytes)
/// to `path`, returning (audio_offset, audio_length). The leading tag is
/// arbitrary: MP3 synthesis regenerates the ID3v2 region entirely from the DB and
/// never reads the backing front, so only the audio offset/length matter.
pub fn write_mp3(path: &Path, audio: &[u8]) -> (u64, u64) {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"ID3");
    bytes.extend_from_slice(&[0x04, 0x00, 0x00]); // version 2.4.0, no flags
    bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // synchsafe size 0
    let audio_offset = bytes.len() as u64;
    bytes.extend_from_slice(audio);
    std::fs::write(path, &bytes).unwrap();
    (audio_offset, audio.len() as u64)
}

/// Write a minimal moov-first M4A (see `minimal_m4a`) to `path`, returning
/// (audio_offset, audio_length) for the verbatim trailing `mdat` payload. M4A
/// synthesis re-scans the file's structural boxes and serves the mdat payload
/// verbatim, so the stored bounds need only satisfy the reader's size guard.
pub fn write_m4a(path: &Path, audio: &[u8]) -> (u64, u64) {
    let bytes = minimal_m4a(audio);
    let audio_offset = (bytes.len() - audio.len()) as u64;
    std::fs::write(path, &bytes).unwrap();
    (audio_offset, audio.len() as u64)
}

/// Write a minimal valid PCM WAV (`fmt ` + `data`) to `path`, returning
/// (audio_offset, audio_length) of the `data` payload. Tags are applied via the DB
/// by the caller (mirrors how `write_flac` is paired with `replace_tags`).
pub fn write_wav(path: &Path, audio: &[u8]) -> (u64, u64) {
    let mut fmt = Vec::new();
    fmt.extend_from_slice(&1u16.to_le_bytes()); // PCM
    fmt.extend_from_slice(&1u16.to_le_bytes()); // mono
    fmt.extend_from_slice(&44_100u32.to_le_bytes()); // sample rate
    fmt.extend_from_slice(&88_200u32.to_le_bytes()); // byte rate
    fmt.extend_from_slice(&2u16.to_le_bytes()); // block align
    fmt.extend_from_slice(&16u16.to_le_bytes()); // bits per sample

    let mut body = Vec::new();
    for (id, payload) in [(&b"fmt "[..], &fmt[..]), (&b"data"[..], audio)] {
        body.extend_from_slice(id);
        body.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        body.extend_from_slice(payload);
    }
    let mut bytes = b"RIFF".to_vec();
    bytes.extend_from_slice(&((body.len() + 4) as u32).to_le_bytes());
    bytes.extend_from_slice(b"WAVE");
    bytes.extend_from_slice(&body);

    let audio_offset = (bytes.len() - audio.len()) as u64;
    std::fs::write(path, &bytes).unwrap();
    (audio_offset, audio.len() as u64)
}

/// Build a 32-bit-size box: [size][type][payload].
fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut v = ((8 + payload.len()) as u32).to_be_bytes().to_vec();
    v.extend_from_slice(kind);
    v.extend_from_slice(payload);
    v
}

/// An ilst `data` atom: [size]["data"][type 4][locale 4][value].
fn m4a_data_atom(type_code: u32, value: &[u8]) -> Vec<u8> {
    let mut p = type_code.to_be_bytes().to_vec();
    p.extend_from_slice(&0u32.to_be_bytes()); // locale
    p.extend_from_slice(value);
    bx(b"data", &p)
}

/// Build a minimal valid, moov-first M4A that musefs-format accepts:
/// `ftyp`, a `moov` with `mvhd` + one `soun` `trak` (whose `stbl` has a 1-entry
/// `stco`) + `udta/meta/ilst` carrying `©nam` = "Orig M4A" and `©ART` =
/// "Orig Artist", followed by an `mdat` with the given verbatim payload.
/// Mirrors the box layout of `musefs-format/src/mp4.rs`'s `mp4_with_ilst` test
/// helper (meta is a FullBox; the soun hdlr payload is `soun`).
pub fn minimal_m4a(mdat_payload: &[u8]) -> Vec<u8> {
    let ilst_atoms = [
        bx(b"\xa9nam", &m4a_data_atom(1, b"Orig M4A")),
        bx(b"\xa9ART", &m4a_data_atom(1, b"Orig Artist")),
    ]
    .concat();
    let ilst = bx(b"ilst", &ilst_atoms);

    let mut meta_hdlr = vec![0u8; 8];
    meta_hdlr.extend_from_slice(b"mdir");
    meta_hdlr.extend_from_slice(b"appl");
    meta_hdlr.extend_from_slice(&[0u8; 9]);
    let mut meta = vec![0u8; 4]; // FullBox version/flags
    meta.extend(bx(b"hdlr", &meta_hdlr));
    meta.extend(ilst);
    let udta = bx(b"udta", &bx(b"meta", &meta));

    let mut soun_hdlr = vec![0u8; 8];
    soun_hdlr.extend_from_slice(b"soun");
    soun_hdlr.extend_from_slice(&[0u8; 12]);
    let mut stco = vec![0u8; 4];
    stco.extend_from_slice(&1u32.to_be_bytes());
    stco.extend_from_slice(&0u32.to_be_bytes());
    let minf = bx(b"minf", &bx(b"stbl", &bx(b"stco", &stco)));
    let trak = bx(
        b"trak",
        &bx(b"mdia", &[bx(b"hdlr", &soun_hdlr), minf].concat()),
    );
    let moov = bx(b"moov", &[bx(b"mvhd", &[0u8; 8]), trak, udta].concat());
    let ftyp = bx(b"ftyp", b"M4A isom");
    let mdat = bx(b"mdat", mdat_payload);
    let mut out = [ftyp, moov, mdat].concat();
    // Point the single `stco` chunk offset at the real `mdat` payload start. A real
    // M4A's chunk offsets are absolute file positions; leaving it 0 means a retag
    // that shrinks the `moov` patches the offset below zero and synthesis fails
    // (TooLarge). With the true offset, the patched value lands at the new payload
    // position. The first `stco` occurrence is the box type (it precedes `mdat`).
    let mdat_payload_offset = (out.len() - mdat_payload.len()) as u32;
    let stco = out
        .windows(4)
        .position(|w| w == b"stco")
        .expect("stco present");
    let entry = stco + 4 + 4 + 4; // past "stco" type + version/flags + entry count
    out[entry..entry + 4].copy_from_slice(&mdat_payload_offset.to_be_bytes());
    out
}

/// Build a minimal valid M4A with `moov` AFTER `mdat`. Same box contents as
/// `minimal_m4a`; only top-level order differs — `moov` trails `mdat`, so a
/// bounded-read implementation must seek backward over the payload to reach the
/// metadata (the SP1 hard case). The MP4 reader locates boxes by scanning, so
/// order does not affect parsing.
pub fn minimal_m4a_moov_last(mdat_payload: &[u8]) -> Vec<u8> {
    let ilst_atoms = [
        bx(b"\xa9nam", &m4a_data_atom(1, b"Orig M4A")),
        bx(b"\xa9ART", &m4a_data_atom(1, b"Orig Artist")),
    ]
    .concat();
    let ilst = bx(b"ilst", &ilst_atoms);

    let mut meta_hdlr = vec![0u8; 8];
    meta_hdlr.extend_from_slice(b"mdir");
    meta_hdlr.extend_from_slice(b"appl");
    meta_hdlr.extend_from_slice(&[0u8; 9]);
    let mut meta = vec![0u8; 4]; // FullBox version/flags
    meta.extend(bx(b"hdlr", &meta_hdlr));
    meta.extend(ilst);
    let udta = bx(b"udta", &bx(b"meta", &meta));

    let mut soun_hdlr = vec![0u8; 8];
    soun_hdlr.extend_from_slice(b"soun");
    soun_hdlr.extend_from_slice(&[0u8; 12]);
    let mut stco = vec![0u8; 4];
    stco.extend_from_slice(&1u32.to_be_bytes());
    stco.extend_from_slice(&0u32.to_be_bytes());
    let minf = bx(b"minf", &bx(b"stbl", &bx(b"stco", &stco)));
    let trak = bx(
        b"trak",
        &bx(b"mdia", &[bx(b"hdlr", &soun_hdlr), minf].concat()),
    );
    let moov = bx(b"moov", &[bx(b"mvhd", &[0u8; 8]), trak, udta].concat());
    let ftyp = bx(b"ftyp", b"M4A isom");
    let mdat = bx(b"mdat", mdat_payload);

    // Order: ftyp, mdat, moov. The mdat payload starts right after ftyp + mdat header.
    // ftyp box: 8-byte header + 8-byte payload "M4A isom" = 16 bytes total.
    // mdat header: 8 bytes. So payload offset = 16 + 8 = 24.
    // Search for `stco` only within `moov`: the mdat payload precedes it here
    // and could otherwise contain a false `stco` byte match.
    let moov_start = ftyp.len() + mdat.len();
    let mut out = [ftyp, mdat, moov].concat();
    let mdat_payload_offset = (8 + b"M4A isom".len() + 8) as u32;
    let stco_pos = moov_start
        + out[moov_start..]
            .windows(4)
            .position(|w| w == b"stco")
            .expect("stco present");
    let entry = stco_pos + 4 + 4 + 4; // past "stco" type + version/flags + entry count
    out[entry..entry + 4].copy_from_slice(&mdat_payload_offset.to_be_bytes());
    out
}

/// Write a moov-at-end M4A to `path`, returning (audio_offset, audio_length) of
/// the verbatim `mdat` payload.
pub fn write_m4a_moov_last(path: &Path, audio: &[u8]) -> (u64, u64) {
    let bytes = minimal_m4a_moov_last(audio);
    // ftyp: 8 header + 8 payload = 16; mdat header: 8 → payload at offset 24.
    let audio_offset = (8 + b"M4A isom".len() + 8) as u64;
    std::fs::write(path, &bytes).unwrap();
    (audio_offset, audio.len() as u64)
}

/// Write a minimal valid Ogg **Opus** file (two header pages + one audio page
/// whose packet body is `audio`) to `path`, returning (audio_offset,
/// audio_length) where audio_length is the Ogg page span (raw audio bytes plus
/// page-framing overhead, not `audio.len()`). Mirrors the recipe in
/// `musefs-core/src/scan.rs`'s `ogg_probe_tests`: the `OpusTags` body must be a
/// parseable VorbisComment (here empty) because the scanner runs `read_tags`.
/// The synthesizer treats the audio packet body as opaque (renumbers pages,
/// recomputes CRCs, never decodes), so arbitrary `audio` bytes are valid. The
/// return is informational — `scan_directory` re-probes the file.
pub fn write_ogg(path: &Path, audio: &[u8]) -> (u64, u64) {
    use musefs_format::ogg::page_test_support::{
        build_header_pub, lace_packet_pub, vorbis_body_empty,
    };
    let head = b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".to_vec();
    let mut tags = b"OpusTags".to_vec();
    tags.extend_from_slice(&vorbis_body_empty());
    let serial = 0x6d75_7366; // "musf"
                              // build_header returns (bytes, header_page_count); the audio page continues
                              // the sequence at that count.
    let (mut bytes, header_pages) = build_header_pub(serial, &[&head, &tags]);
    let header_len = bytes.len();
    let (page, _) = lace_packet_pub(serial, header_pages, false, 960, audio);
    bytes.extend_from_slice(&page);
    std::fs::write(path, &bytes).unwrap();
    (header_len as u64, (bytes.len() - header_len) as u64)
}

/// A FLAC PICTURE block body (type 3 = front cover, image/png) carrying `data`.
/// The identical bytes serve three fixtures: a native FLAC PICTURE block, the
/// base64 payload of an Opus/Vorbis `METADATA_BLOCK_PICTURE` comment, and an
/// OggFLAC native PICTURE packet body. `data` must be non-empty: FLAC synthesis
/// only emits an `ArtImage` segment for `data_len > 0`.
pub fn picture_block_body(data: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&3u32.to_be_bytes()); // picture type: front cover
    v.extend_from_slice(&(b"image/png".len() as u32).to_be_bytes());
    v.extend_from_slice(b"image/png");
    v.extend_from_slice(&0u32.to_be_bytes()); // empty description
    v.extend_from_slice(&1u32.to_be_bytes()); // width
    v.extend_from_slice(&1u32.to_be_bytes()); // height
    v.extend_from_slice(&0u32.to_be_bytes()); // depth
    v.extend_from_slice(&0u32.to_be_bytes()); // colors
    v.extend_from_slice(&(data.len() as u32).to_be_bytes());
    v.extend_from_slice(data);
    v
}

/// Write an Opus file whose `OpusTags` packet carries `comments` plus a base64
/// `METADATA_BLOCK_PICTURE` of `picture` (a PICTURE block body, e.g. from
/// `picture_block_body`), returning (audio_offset, audio_length). Same page
/// recipe as `write_ogg`.
pub fn write_opus_with_art(
    path: &Path,
    comments: &[&str],
    picture: &[u8],
    audio: &[u8],
) -> (u64, u64) {
    use base64::Engine as _;
    use musefs_format::ogg::page_test_support::{build_header_pub, lace_packet_pub};
    let head = b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".to_vec();
    let mbp = format!(
        "METADATA_BLOCK_PICTURE={}",
        base64::engine::general_purpose::STANDARD.encode(picture)
    );
    let mut all: Vec<&str> = comments.to_vec();
    all.push(&mbp);
    let mut tags = b"OpusTags".to_vec();
    tags.extend_from_slice(&vorbis_comment_body("v", &all));
    let serial = 0x6d75_7366; // "musf"
    let (mut bytes, header_pages) = build_header_pub(serial, &[&head, &tags]);
    let header_len = bytes.len();
    let (page, _) = lace_packet_pub(serial, header_pages, false, 960, audio);
    bytes.extend_from_slice(&page);
    std::fs::write(path, &bytes).unwrap();
    (header_len as u64, (bytes.len() - header_len) as u64)
}

/// Write an OggFLAC file (`0x7F "FLAC"` 1.0 mapping) whose header packets carry
/// a VORBIS_COMMENT block with `comments` and a native PICTURE block with
/// `picture` (a PICTURE block body), returning (audio_offset, audio_length).
/// Packet 0 is `0x7F "FLAC" major minor count(u16 BE) "fLaC" STREAMINFO`; the
/// count is the number of metadata-block packets that follow.
pub fn write_oggflac_with_art(
    path: &Path,
    comments: &[&str],
    picture: &[u8],
    audio: &[u8],
) -> (u64, u64) {
    use musefs_format::ogg::page_test_support::{build_header_pub, lace_packet_pub};
    let mut pkt0 = vec![0x7F];
    pkt0.extend_from_slice(b"FLAC");
    pkt0.extend_from_slice(&[1, 0]); // mapping version 1.0
    pkt0.extend_from_slice(&2u16.to_be_bytes()); // two metadata packets follow
    pkt0.extend_from_slice(b"fLaC");
    pkt0.extend_from_slice(&flac_block(0, &streaminfo_body(), false));
    let vc_pkt = flac_block(4, &vorbis_comment_body("v", comments), false);
    let pic_pkt = flac_block(6, picture, true);
    let serial = 0x6f67_666c;
    let (mut bytes, header_pages) = build_header_pub(serial, &[&pkt0, &vc_pkt, &pic_pkt]);
    let header_len = bytes.len();
    let (page, _) = lace_packet_pub(serial, header_pages, false, 960, audio);
    bytes.extend_from_slice(&page);
    std::fs::write(path, &bytes).unwrap();
    (header_len as u64, (bytes.len() - header_len) as u64)
}
