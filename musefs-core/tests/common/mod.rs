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
    [ftyp, moov, mdat].concat()
}
