#![allow(dead_code)]

use std::path::Path;

pub mod corpus;
pub mod report;

pub use musefs_format::fuzz_check::fixtures::{
    flac_block, make_flac, streaminfo_body, vorbis_comment_body,
};

/// Return the mtime of `p` as seconds since the Unix epoch (truncated, not
/// rounded), matching what `scan_directory` stores in the DB.
pub fn real_mtime(p: &std::path::Path) -> i64 {
    std::fs::metadata(p)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .cast_signed()
}

/// Return the mtime of `p` as nanoseconds since the Unix epoch.
pub fn real_mtime_ns(p: &std::path::Path) -> i64 {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(p).unwrap();
    meta.mtime() * 1_000_000_000 + meta.mtime_nsec()
}

/// Return the ctime of `p` as nanoseconds since the Unix epoch.
pub fn real_ctime_ns(p: &std::path::Path) -> i64 {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(p).unwrap();
    meta.ctime() * 1_000_000_000 + meta.ctime_nsec()
}

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
        body.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_le_bytes());
        body.extend_from_slice(payload);
    }
    let mut bytes = b"RIFF".to_vec();
    bytes.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_le_bytes());
    bytes.extend_from_slice(b"WAVE");
    bytes.extend_from_slice(&body);

    let audio_offset = (bytes.len() - audio.len()) as u64;
    std::fs::write(path, &bytes).unwrap();
    (audio_offset, audio.len() as u64)
}

/// Build a 32-bit-size box: [size][type][payload].
fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut v = u32::try_from(8 + payload.len())
        .unwrap()
        .to_be_bytes()
        .to_vec();
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
    let mdat_payload_offset = u32::try_from(out.len() - mdat_payload.len()).unwrap();
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
    let mdat_payload_offset = u32::try_from(8 + b"M4A isom".len() + 8).unwrap();
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

/// The payload size of one audio page in [`write_ogg_vorbis`]. Real libvorbis
/// output averages ~1 890 B per page; the number is load-bearing because the
/// algebraic CRC patch advances across `5 + seg_count + payload_len` zero bytes
/// per served page, so page size *is* the per-page serve cost (#666).
pub const VORBIS_PAGE_PAYLOAD: usize = 1890;

/// Lace `packets` into one run of Ogg pages, packing as many packets per page as
/// the 255-value segment table holds. Returns (bytes, pages_used).
///
/// `lace_packet` gives every packet a page of its own. A real Vorbis encoder
/// instead packs the comment and setup headers together, and that difference is
/// the whole point of this helper: `musefs` re-lays each header packet onto its
/// own page, so a file packed this way has a *shorter* header than the file
/// musefs serves and every audio page is renumbered. Opus cannot show this —
/// RFC 7845 requires one header packet per page, so an Opus fixture's sequence
/// numbers survive synthesis untouched and its `crc32(DELTA)` is always zero.
fn pack_packets(serial: u32, seq_start: u32, bos: bool, packets: &[&[u8]]) -> (Vec<u8>, u32) {
    // One lacing table across the whole run, plus a flag per value marking where
    // a packet begins — that is what decides a continuation page's FLAG_CONTINUED.
    let mut table: Vec<u8> = Vec::new();
    let mut starts: Vec<bool> = Vec::new();
    let mut payload: Vec<u8> = Vec::new();
    for pkt in packets {
        starts.push(true);
        let full = pkt.len() / 255;
        table.resize(table.len() + full, 255u8);
        starts.resize(starts.len() + full, false);
        table.push(u8::try_from(pkt.len() % 255).expect("x % 255 < 256"));
        payload.extend_from_slice(pkt);
    }

    let mut out = Vec::new();
    let mut seq = seq_start;
    let (mut lace_pos, mut payload_pos) = (0usize, 0usize);
    let mut first = true;
    while first || lace_pos < table.len() {
        let chunk = (table.len() - lace_pos).min(255);
        let laces = &table[lace_pos..lace_pos + chunk];
        let page_payload: usize = laces.iter().map(|&b| b as usize).sum();

        let mut header_type = 0u8;
        if bos && first {
            header_type |= 0x02; // BOS
        }
        if !starts.get(lace_pos).copied().unwrap_or(true) {
            header_type |= 0x01; // continued packet
        }

        let page_start = out.len();
        out.extend_from_slice(b"OggS");
        out.push(0); // stream structure version
        out.push(header_type);
        out.extend_from_slice(&0u64.to_le_bytes()); // granule: header pages carry 0
        out.extend_from_slice(&serial.to_le_bytes());
        out.extend_from_slice(&seq.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // CRC, stamped below
        out.push(u8::try_from(chunk).expect("chunk is .min(255) so fits in u8"));
        out.extend_from_slice(laces);
        out.extend_from_slice(&payload[payload_pos..payload_pos + page_payload]);

        // Re-stamping the sequence number the page already carries leaves the page
        // unchanged apart from the CRC, which `patch_page_header` recomputes.
        let patched = musefs_format::ogg::patch_page_header(&out[page_start..], seq).unwrap();
        out[page_start..page_start + patched.len()].copy_from_slice(&patched);

        lace_pos += chunk;
        payload_pos += page_payload;
        seq = seq.wrapping_add(1);
        first = false;
    }
    (out, seq.wrapping_sub(seq_start))
}

/// Write an Ogg **Vorbis** file with the page geometry a real encoder produces:
/// the identification header alone on the BOS page, the comment and setup
/// headers packed onto the page after it, then audio laced into
/// [`VORBIS_PAGE_PAYLOAD`]-sized pages. Returns (audio_offset, audio_length),
/// where audio_length is the page span.
///
/// Neither property is reachable through [`write_ogg`], and both decide what the
/// Ogg serve path actually costs:
///
/// * **Renumbering.** Synthesis gives each of the three header packets its own
///   page, so the served header is longer than the original one and every audio
///   page's sequence number shifts. That non-zero delta is what makes
///   `patch_page_header_algebraic` do work at all.
/// * **Page size.** `write_ogg` laces the whole track as a single packet, so
///   every page is max-size — a regime where the per-page CRC advance is
///   amortized over 65 025 payload bytes instead of ~1 890.
///
/// `picture`, when given, is a FLAC PICTURE block body (see
/// [`picture_block_body`]) carried as a base64 `METADATA_BLOCK_PICTURE` comment.
pub fn write_ogg_vorbis(
    path: &Path,
    comments: &[&str],
    picture: Option<&[u8]>,
    audio: &[u8],
) -> (u64, u64) {
    use base64::Engine as _;
    use musefs_format::ogg::page_test_support::lace_packet_pub;
    let serial = 0x7662_7273; // "vbrs"

    // Identification header (Vorbis I §4.2.1): 2 channels at 44 100 Hz, 192 kbps
    // nominal, 256/2048 block sizes, framing bit set. musefs carries the packet
    // through verbatim, so only its length reaches the page geometry — but a
    // fixture that claims to be encoder-realistic should be a valid packet, and
    // the spec's field list is what fixes the length at 30 bytes.
    let mut id = b"\x01vorbis".to_vec();
    id.extend_from_slice(&0u32.to_le_bytes()); // vorbis_version
    id.push(2); // audio_channels
    id.extend_from_slice(&44_100u32.to_le_bytes()); // audio_sample_rate
    id.extend_from_slice(&0u32.to_le_bytes()); // bitrate_maximum
    id.extend_from_slice(&192_000u32.to_le_bytes()); // bitrate_nominal
    id.extend_from_slice(&0u32.to_le_bytes()); // bitrate_minimum
    id.push(0xb8); // blocksize_0 = 2^8, blocksize_1 = 2^11
    id.push(1); // framing flag
    assert_eq!(id.len(), 30, "Vorbis identification header is 30 bytes");
    let mbp = picture.map(|p| {
        format!(
            "METADATA_BLOCK_PICTURE={}",
            base64::engine::general_purpose::STANDARD.encode(p)
        )
    });
    let mut all: Vec<&str> = comments.to_vec();
    if let Some(m) = &mbp {
        all.push(m);
    }
    let mut comment = b"\x03vorbis".to_vec();
    comment.extend_from_slice(&vorbis_comment_body("Xiph.Org libVorbis I 20200704", &all));
    comment.push(1); // Vorbis framing bit
    // Stand-in for the codebook setup header: opaque to musefs (carried verbatim),
    // sized like a real one so it shares the comment's page the way libvorbis packs it.
    let mut setup = b"\x05vorbis".to_vec();
    setup.extend((0..4096u32).map(|i| u8::try_from(i % 251).unwrap()));

    // The identification header must own the BOS page (Vorbis I §4.2.1); the
    // comment and setup headers share the pages after it, which is the packing
    // musefs does not reproduce.
    let (mut bytes, id_pages) = pack_packets(serial, 0, true, &[&id]);
    let (rest, rest_pages) = pack_packets(serial, id_pages, false, &[&comment, &setup]);
    bytes.extend_from_slice(&rest);
    let header_pages = id_pages + rest_pages;
    let header_len = bytes.len();

    // `chunks` yields nothing for empty audio; emit one empty page instead so the
    // file still has an audio region (the rule `lace_packet` follows internally).
    let pages: Vec<&[u8]> = if audio.is_empty() {
        vec![&[]]
    } else {
        audio.chunks(VORBIS_PAGE_PAYLOAD).collect()
    };
    let mut seq = header_pages;
    for (i, chunk) in pages.iter().enumerate() {
        let granule = (i as u64 + 1) * 1024;
        let (page, used) = lace_packet_pub(serial, seq, false, granule, chunk);
        bytes.extend_from_slice(&page);
        seq += used;
    }
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
    v.extend_from_slice(&u32::try_from(b"image/png".len()).unwrap().to_be_bytes());
    v.extend_from_slice(b"image/png");
    v.extend_from_slice(&0u32.to_be_bytes()); // empty description
    v.extend_from_slice(&1u32.to_be_bytes()); // width
    v.extend_from_slice(&1u32.to_be_bytes()); // height
    v.extend_from_slice(&0u32.to_be_bytes()); // depth
    v.extend_from_slice(&0u32.to_be_bytes()); // colors
    v.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
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
