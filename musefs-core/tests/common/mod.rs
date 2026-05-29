#![allow(dead_code)]

use std::path::Path;

pub fn flac_block(block_type: u8, body: &[u8], is_last: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.push((if is_last { 0x80 } else { 0 }) | (block_type & 0x7F));
    let len = body.len();
    out.push(((len >> 16) & 0xFF) as u8);
    out.push(((len >> 8) & 0xFF) as u8);
    out.push((len & 0xFF) as u8);
    out.extend_from_slice(body);
    out
}

pub fn streaminfo_body() -> Vec<u8> {
    let mut b = vec![
        0x10, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0A, 0xC4, 0x42, 0xF0, 0x00,
        0x00, 0x00, 0x00,
    ];
    b.extend_from_slice(&[0u8; 16]);
    b
}

pub fn vorbis_comment_body(vendor: &str, comments: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    out.extend_from_slice(vendor.as_bytes());
    out.extend_from_slice(&(comments.len() as u32).to_le_bytes());
    for c in comments {
        out.extend_from_slice(&(c.len() as u32).to_le_bytes());
        out.extend_from_slice(c.as_bytes());
    }
    out
}

pub fn make_flac(blocks: &[(u8, Vec<u8>)], audio: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"fLaC");
    for (i, (bt, body)) in blocks.iter().enumerate() {
        out.extend_from_slice(&flac_block(*bt, body, i == blocks.len() - 1));
    }
    out.extend_from_slice(audio);
    out
}

/// Write a simple FLAC (STREAMINFO + comment + audio) to `path`,
/// returning (audio_offset, audio_length).
pub fn write_flac(path: &Path, comments: &[&str], audio: &[u8]) -> (i64, i64) {
    let si = streaminfo_body();
    let vc = vorbis_comment_body("orig", comments);
    let bytes = make_flac(&[(0, si), (4, vc)], audio);
    let audio_offset = (bytes.len() - audio.len()) as i64;
    std::fs::write(path, &bytes).unwrap();
    (audio_offset, audio.len() as i64)
}

/// Write a minimal MP3 (a 10-byte empty ID3v2.4 tag, then the given audio bytes)
/// to `path`, returning (audio_offset, audio_length). The leading tag is
/// arbitrary: MP3 synthesis regenerates the ID3v2 region entirely from the DB and
/// never reads the backing front, so only the audio offset/length matter.
pub fn write_mp3(path: &Path, audio: &[u8]) -> (i64, i64) {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"ID3");
    bytes.extend_from_slice(&[0x04, 0x00, 0x00]); // version 2.4.0, no flags
    bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // synchsafe size 0
    let audio_offset = bytes.len() as i64;
    bytes.extend_from_slice(audio);
    std::fs::write(path, &bytes).unwrap();
    (audio_offset, audio.len() as i64)
}

/// Write a minimal moov-first M4A (see `minimal_m4a`) to `path`, returning
/// (audio_offset, audio_length) for the verbatim trailing `mdat` payload. M4A
/// synthesis re-scans the file's structural boxes and serves the mdat payload
/// verbatim, so the stored bounds need only satisfy the reader's size guard.
pub fn write_m4a(path: &Path, audio: &[u8]) -> (i64, i64) {
    let bytes = minimal_m4a(audio);
    let audio_offset = (bytes.len() - audio.len()) as i64;
    std::fs::write(path, &bytes).unwrap();
    (audio_offset, audio.len() as i64)
}

/// Write a minimal valid PCM WAV (`fmt ` + `data`) to `path`, returning
/// (audio_offset, audio_length) of the `data` payload. Tags are applied via the DB
/// by the caller (mirrors how `write_flac` is paired with `replace_tags`).
pub fn write_wav(path: &Path, audio: &[u8]) -> (i64, i64) {
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

    let audio_offset = (bytes.len() - audio.len()) as i64;
    std::fs::write(path, &bytes).unwrap();
    (audio_offset, audio.len() as i64)
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
