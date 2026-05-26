mod crc;
mod page;

pub use page::{parse_page, patch_page_header, PageHeader};

use crate::error::{FormatError, Result};

/// The codec carried inside an Ogg logical bitstream that we synthesize.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Opus,
    Vorbis,
    OggFlac,
}

fn detect_codec(first_packet: &[u8]) -> Result<Codec> {
    if first_packet.len() >= 8 && &first_packet[0..8] == b"OpusHead" {
        Ok(Codec::Opus)
    } else if first_packet.len() >= 7 && &first_packet[0..7] == b"\x01vorbis" {
        Ok(Codec::Vorbis)
    } else if first_packet.len() >= 5 && &first_packet[0..5] == b"\x7FFLAC" {
        Ok(Codec::OggFlac)
    } else {
        Err(FormatError::Malformed)
    }
}

/// For OggFLAC, packet 0 is `0x7F "FLAC" major minor count(2, BE) "fLaC" STREAMINFO`.
/// The 16-bit big-endian count is the number of metadata-block packets that follow
/// packet 0.
fn oggflac_following_packets(first_packet: &[u8]) -> Result<usize> {
    if first_packet.len() < 9 {
        return Err(FormatError::Malformed);
    }
    Ok(u16::from_be_bytes([first_packet[7], first_packet[8]]) as usize)
}

/// The parsed Ogg header region: codec, serial, the reassembled header packets,
/// the number of header pages, and where audio begins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OggHeader {
    pub codec: Codec,
    pub serial: u32,
    pub packets: Vec<Vec<u8>>,
    pub header_pages: u32,
    pub audio_offset: u64,
}

/// Parse the header region from the front of a logical bitstream. `data` may be the
/// whole file or just `[0, audio_offset)`; either way parsing stops once all header
/// packets are reassembled.
pub fn read_header(data: &[u8]) -> Result<OggHeader> {
    let first_page = page::parse_page(data, 0)?;
    let serial = first_page.serial;

    // Reassemble the first packet to detect the codec and (for OggFLAC) the count.
    let first = page::read_packets(data, 1)?;
    let first_pkt = first.first().ok_or(FormatError::Malformed)?;
    let codec = detect_codec(&first_pkt.data)?;

    let want = match codec {
        Codec::Opus => 2,
        Codec::Vorbis => 3,
        Codec::OggFlac => 1 + oggflac_following_packets(&first_pkt.data)?,
    };

    let pkts = page::read_packets(data, want)?;
    if pkts.len() != want {
        return Err(FormatError::Malformed);
    }
    let last = pkts.last().unwrap();
    Ok(OggHeader {
        codec,
        serial,
        packets: pkts.iter().map(|p| p.data.clone()).collect(),
        header_pages: last.pages_through_end,
        audio_offset: last.end_offset as u64,
    })
}

/// Strip a codec's comment-packet prefix, returning the VorbisComment body slice.
fn comment_body(codec: Codec, packet: &[u8]) -> Result<&[u8]> {
    let prefix = match codec {
        Codec::Opus => 8,    // "OpusTags"
        Codec::Vorbis => 7,  // 0x03 "vorbis"
        Codec::OggFlac => 4, // FLAC metadata block header (type + 24-bit length)
    };
    if packet.len() < prefix {
        return Err(FormatError::Malformed);
    }
    Ok(&packet[prefix..])
}

/// The index of the comment packet within the reassembled header packets.
fn comment_packet_index(header: &OggHeader) -> usize {
    match header.codec {
        Codec::Opus => 1,
        Codec::Vorbis => 1,
        // OggFLAC: packet 0 is the mapping header; the VORBIS_COMMENT block is
        // whichever following packet has block type 4.
        Codec::OggFlac => header
            .packets
            .iter()
            .enumerate()
            .skip(1)
            .find(|(_, p)| !p.is_empty() && (p[0] & 0x7F) == 4)
            .map(|(i, _)| i)
            .unwrap_or(0),
    }
}

/// Read existing `(FIELD, value)` tags from a complete file. Empty if none.
pub fn read_tags(data: &[u8]) -> Result<Vec<(String, String)>> {
    let header = read_header(data)?;
    let idx = comment_packet_index(&header);
    if idx == 0 {
        return Ok(Vec::new()); // no comment packet present
    }
    let body = comment_body(header.codec, &header.packets[idx])?;
    crate::vorbiscomment::parse(body)
}

use crate::input::EmbeddedPicture;

/// Extract embedded pictures from a complete file for scan-time ingestion.
///
/// Opus/Vorbis carry art as a base64 `METADATA_BLOCK_PICTURE` comment whose decoded
/// bytes are a FLAC PICTURE block body; OggFLAC carries native PICTURE block
/// packets (block type 6). Plan 1 only *reads* art (to seed the DB); synthesis does
/// not yet re-embed it.
pub fn read_pictures(data: &[u8]) -> Result<Vec<EmbeddedPicture>> {
    use base64::Engine;
    let header = read_header(data)?;
    let mut out = Vec::new();
    match header.codec {
        Codec::Opus | Codec::Vorbis => {
            let idx = comment_packet_index(&header);
            if idx == 0 {
                return Ok(out);
            }
            let body = comment_body(header.codec, &header.packets[idx])?;
            for (field, value) in crate::vorbiscomment::parse(body)? {
                if field.eq_ignore_ascii_case("METADATA_BLOCK_PICTURE") {
                    let raw = base64::engine::general_purpose::STANDARD
                        .decode(value.as_bytes())
                        .map_err(|_| FormatError::Malformed)?;
                    out.push(crate::flac::parse_picture_block(&raw)?);
                }
            }
        }
        Codec::OggFlac => {
            for pkt in header.packets.iter().skip(1) {
                if !pkt.is_empty() && (pkt[0] & 0x7F) == 6 {
                    // Strip the 4-byte FLAC metadata block header.
                    out.push(crate::flac::parse_picture_block(&pkt[4..])?);
                }
            }
        }
    }
    Ok(out)
}

/// Audio bounds + codec from a complete file, for the scanner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OggScan {
    pub codec: Codec,
    pub audio_offset: u64,
    pub audio_length: u64,
}

pub fn locate_audio(data: &[u8]) -> Result<OggScan> {
    let header = read_header(data)?;
    if header.audio_offset > data.len() as u64 {
        return Err(FormatError::Malformed);
    }
    Ok(OggScan {
        codec: header.codec,
        audio_offset: header.audio_offset,
        audio_length: data.len() as u64 - header.audio_offset,
    })
}

/// The header region parsed from the front of the file (`[0, audio_offset)`), for
/// synthesis. Identical to `read_header` but named to mirror `flac::read_metadata`.
pub fn read_metadata(front: &[u8]) -> Result<OggHeader> {
    read_header(front)
}

use crate::input::TagInput;
use crate::layout::{RegionLayout, Segment};

/// Build the regenerated header packets for a codec from the original header
/// packets and the new text tags. Plan 1 emits text VorbisComments only (no art).
fn rebuild_header_packets(header: &OggHeader, tags: &[TagInput]) -> Result<Vec<Vec<u8>>> {
    let vc = crate::vorbiscomment::build(tags);
    match header.codec {
        Codec::Opus => {
            let mut tags_pkt = b"OpusTags".to_vec();
            tags_pkt.extend_from_slice(&vc);
            Ok(vec![header.packets[0].clone(), tags_pkt])
        }
        Codec::Vorbis => {
            let mut comment = b"\x03vorbis".to_vec();
            comment.extend_from_slice(&vc);
            comment.push(0x01); // framing bit
            Ok(vec![
                header.packets[0].clone(),
                comment,
                header.packets[2].clone(),
            ])
        }
        Codec::OggFlac => rebuild_oggflac_packets(header, &vc),
    }
}

/// Assemble a synthesized layout: regenerated header pages (Inline) + one compact
/// `OggAudio` segment whose `seq_delta` renumbers the preserved audio pages.
pub fn synthesize_layout(
    header: &OggHeader,
    audio_offset: u64,
    audio_length: u64,
    tags: &[TagInput],
) -> Result<RegionLayout> {
    let new_packets = rebuild_header_packets(header, tags)?;
    let refs: Vec<&[u8]> = new_packets.iter().map(|p| p.as_slice()).collect();
    let (header_bytes, new_pages) = crate::ogg::page::build_header(header.serial, &refs);
    let seq_delta = new_pages as i64 - header.header_pages as i64;
    Ok(RegionLayout::new(vec![
        Segment::Inline(header_bytes),
        Segment::OggAudio {
            offset: audio_offset,
            len: audio_length,
            seq_delta,
        },
    ]))
}

/// Rebuild OggFLAC header packets: keep packet 0 (mapping header `0x7F FLAC` +
/// version + count + `fLaC` + STREAMINFO) but recompute its 16-bit following-packet
/// count; carry over structural metadata-block packets (APPLICATION=2, SEEKTABLE=3,
/// CUESHEET=5); drop existing VORBIS_COMMENT/PICTURE/PADDING; append one fresh
/// VORBIS_COMMENT block. Set the last-metadata-block flag on the final block.
fn rebuild_oggflac_packets(header: &OggHeader, vc: &[u8]) -> Result<Vec<Vec<u8>>> {
    if header.packets.is_empty() {
        return Err(FormatError::Malformed);
    }
    // Structural blocks to keep (each block packet starts with the 4-byte FLAC
    // metadata block header; type is the low 7 bits of byte 0).
    let mut blocks: Vec<Vec<u8>> = Vec::new();
    for pkt in header.packets.iter().skip(1) {
        if pkt.is_empty() {
            continue;
        }
        match pkt[0] & 0x7F {
            2 | 3 | 5 => blocks.push(pkt.clone()), // APPLICATION, SEEKTABLE, CUESHEET
            _ => {}
        }
    }

    // Fresh VORBIS_COMMENT block (type 4): 4-byte header + body.
    let mut comment = Vec::new();
    crate::flac::push_block_header(&mut comment, 4, vc.len(), false);
    comment.extend_from_slice(vc);
    blocks.push(comment);

    // Normalize the last-metadata-block flag: clear on all but the last, set on the
    // last. Byte 0 high bit (0x80) is the flag.
    let n = blocks.len();
    for (i, b) in blocks.iter_mut().enumerate() {
        if i + 1 == n {
            b[0] |= 0x80;
        } else {
            b[0] &= 0x7F;
        }
    }

    // Rebuild the mapping header (packet 0) with the new following-packet count.
    let mut mapping = header.packets[0].clone();
    if mapping.len() < 9 {
        return Err(FormatError::Malformed);
    }
    let count = u16::try_from(blocks.len()).map_err(|_| FormatError::TooLarge)?;
    mapping[7..9].copy_from_slice(&count.to_be_bytes());

    let mut out = vec![mapping];
    out.extend(blocks);
    Ok(out)
}

#[doc(hidden)]
pub mod page_test_support {
    pub use crate::ogg::page::{build_header as build_header_pub, lace_packet as lace_packet_pub};

    /// An empty VorbisComment body (vendor + zero comments), for fixtures.
    pub fn vorbis_body_empty() -> Vec<u8> {
        crate::vorbiscomment::build(&[])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ogg::page::{build_header, lace_packet};

    fn opus_headers() -> Vec<u8> {
        let head = b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".to_vec();
        let tags = b"OpusTags\x06\x00\x00\x00musefs\x00\x00\x00\x00".to_vec();
        let (bytes, _) = build_header(0x1234, &[&head, &tags]);
        bytes
    }

    #[test]
    fn locate_audio_reports_bounds() {
        let mut data = opus_headers();
        let header_len = data.len();
        let (audio, _) = crate::ogg::page::lace_packet(0x1234, 2, false, 960, &[0u8; 120]);
        data.extend_from_slice(&audio);

        let scan = locate_audio(&data).unwrap();
        assert_eq!(scan.codec, Codec::Opus);
        assert_eq!(scan.audio_offset, header_len as u64);
        assert_eq!(scan.audio_length, (data.len() - header_len) as u64);
    }

    #[test]
    fn reads_opus_header() {
        let mut data = opus_headers();
        // Append one audio page so audio_offset lands before EOF.
        let (audio, _) = lace_packet(0x1234, 2, false, 960, &[0u8; 100]);
        let header_len = data.len();
        data.extend_from_slice(&audio);

        let h = read_header(&data).unwrap();
        assert_eq!(h.codec, Codec::Opus);
        assert_eq!(h.serial, 0x1234);
        assert_eq!(h.packets.len(), 2);
        assert_eq!(h.audio_offset, header_len as u64);
        assert_eq!(h.header_pages, 2);
    }

    #[test]
    fn read_tags_opus() {
        // Build an OpusTags packet with one real comment via the shared builder.
        let body = crate::vorbiscomment::build(&[crate::input::TagInput::new("title", "Sun")]);
        let mut tags_pkt = b"OpusTags".to_vec();
        tags_pkt.extend_from_slice(&body);
        let head = b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".to_vec();
        let (mut data, _) = crate::ogg::page::build_header(7, &[&head, &tags_pkt]);
        let (audio, _) = crate::ogg::page::lace_packet(7, 2, false, 960, &[0u8; 50]);
        data.extend_from_slice(&audio);

        let tags = read_tags(&data).unwrap();
        assert_eq!(tags, vec![("TITLE".to_string(), "Sun".to_string())]);
    }

    #[test]
    fn synthesize_opus_emits_valid_header_and_audio_segment() {
        let mut data = opus_headers();
        let scan = locate_audio({
            let (audio, _) = crate::ogg::page::lace_packet(0x1234, 2, false, 960, &[0u8; 80]);
            data.extend_from_slice(&audio);
            &data
        })
        .unwrap();
        let header = read_metadata(&data[..scan.audio_offset as usize]).unwrap();

        let layout = synthesize_layout(
            &header,
            scan.audio_offset,
            scan.audio_length,
            &[TagInput::new("album", "Geogaddi")],
        )
        .unwrap();

        // Header segment is valid Ogg with regenerated tags; audio segment carries
        // the original bounds.
        match &layout.segments()[0] {
            Segment::Inline(bytes) => {
                let h = read_header(bytes).unwrap();
                assert_eq!(h.codec, Codec::Opus);
                let body = comment_body(Codec::Opus, &h.packets[1]).unwrap();
                let tags = crate::vorbiscomment::parse(body).unwrap();
                assert_eq!(tags, vec![("ALBUM".to_string(), "Geogaddi".to_string())]);
            }
            other => panic!("expected Inline header, got {other:?}"),
        }
        match &layout.segments()[1] {
            Segment::OggAudio { offset, len, .. } => {
                assert_eq!(*offset, scan.audio_offset);
                assert_eq!(*len, scan.audio_length);
            }
            other => panic!("expected OggAudio, got {other:?}"),
        }
    }

    fn vorbis_headers_with(setup: &[u8]) -> Vec<u8> {
        // Minimal-but-shaped Vorbis ID header (30 bytes from 0x01"vorbis").
        let mut id = b"\x01vorbis".to_vec();
        id.extend_from_slice(&0u32.to_le_bytes()); // version
        id.push(2); // channels
        id.extend_from_slice(&44100u32.to_le_bytes()); // sample rate
        id.extend_from_slice(&0u32.to_le_bytes()); // bitrate max
        id.extend_from_slice(&128000u32.to_le_bytes()); // nominal
        id.extend_from_slice(&0u32.to_le_bytes()); // min
        id.push(0xB8); // blocksizes
        id.push(0x01); // framing bit
        let mut comment = b"\x03vorbis".to_vec();
        comment.extend_from_slice(&crate::vorbiscomment::build(&[]));
        comment.push(0x01);
        let (bytes, _) = crate::ogg::page::build_header(55, &[&id, &comment, setup]);
        bytes
    }

    #[test]
    fn synthesize_vorbis_preserves_setup_and_rewrites_comment() {
        let setup = b"\x05vorbis-SETUP-CODEBOOKS-PLACEHOLDER".to_vec();
        let mut data = vorbis_headers_with(&setup);
        let (audio, _) = crate::ogg::page::lace_packet(55, 99, false, 1024, &[0u8; 64]);
        data.extend_from_slice(&audio);

        let scan = locate_audio(&data).unwrap();
        assert_eq!(scan.codec, Codec::Vorbis);
        let header = read_metadata(&data[..scan.audio_offset as usize]).unwrap();
        // The original setup packet (3rd header packet) must be carried through.
        assert_eq!(header.packets[2], setup);

        let layout = synthesize_layout(
            &header,
            scan.audio_offset,
            scan.audio_length,
            &[TagInput::new("artist", "Autechre")],
        )
        .unwrap();

        if let Segment::Inline(bytes) = &layout.segments()[0] {
            let h = read_header(bytes).unwrap();
            assert_eq!(h.codec, Codec::Vorbis);
            assert_eq!(h.packets[2], setup); // setup preserved byte-for-byte
            let body = comment_body(Codec::Vorbis, &h.packets[1]).unwrap();
            let tags = crate::vorbiscomment::parse(body).unwrap();
            assert_eq!(tags, vec![("ARTIST".to_string(), "Autechre".to_string())]);
        } else {
            panic!("expected Inline header");
        }
    }

    #[test]
    fn read_pictures_opus_decodes_metadata_block_picture() {
        use base64::Engine;
        // A minimal FLAC PICTURE block body: type=3, mime="image/png", empty desc,
        // 1x1, depth 0, colors 0, data="PNG".
        let mut pic = Vec::new();
        pic.extend_from_slice(&3u32.to_be_bytes());
        let mime = b"image/png";
        pic.extend_from_slice(&(mime.len() as u32).to_be_bytes());
        pic.extend_from_slice(mime);
        pic.extend_from_slice(&0u32.to_be_bytes()); // desc len
        pic.extend_from_slice(&1u32.to_be_bytes()); // width
        pic.extend_from_slice(&1u32.to_be_bytes()); // height
        pic.extend_from_slice(&0u32.to_be_bytes()); // depth
        pic.extend_from_slice(&0u32.to_be_bytes()); // colors
        let img = b"PNG";
        pic.extend_from_slice(&(img.len() as u32).to_be_bytes());
        pic.extend_from_slice(img);
        let b64 = base64::engine::general_purpose::STANDARD.encode(&pic);

        let mut body = Vec::new();
        body.extend_from_slice(&(crate::vorbiscomment::VENDOR.len() as u32).to_le_bytes());
        body.extend_from_slice(crate::vorbiscomment::VENDOR.as_bytes());
        body.extend_from_slice(&1u32.to_le_bytes()); // one comment
        let comment = format!("METADATA_BLOCK_PICTURE={}", b64);
        body.extend_from_slice(&(comment.len() as u32).to_le_bytes());
        body.extend_from_slice(comment.as_bytes());

        let mut tags_pkt = b"OpusTags".to_vec();
        tags_pkt.extend_from_slice(&body);
        let head = b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".to_vec();
        let (mut data, _) = crate::ogg::page::build_header(7, &[&head, &tags_pkt]);
        let (audio, _) = crate::ogg::page::lace_packet(7, 2, false, 960, &[0u8; 50]);
        data.extend_from_slice(&audio);

        let pics = read_pictures(&data).unwrap();
        assert_eq!(pics.len(), 1);
        assert_eq!(pics[0].mime, "image/png");
        assert_eq!(pics[0].data, b"PNG");
    }

    fn oggflac_headers() -> Vec<u8> {
        // STREAMINFO block (type 0): 4-byte header + 34-byte body (zeros are fine
        // for our framing test).
        let mut streaminfo = Vec::new();
        crate::flac::push_block_header(&mut streaminfo, 0, 34, false);
        streaminfo.extend(std::iter::repeat_n(0u8, 34));

        // Mapping header packet: 0x7F "FLAC" v1.0 count "fLaC" STREAMINFO.
        let mut mapping = vec![0x7F];
        mapping.extend_from_slice(b"FLAC");
        mapping.push(1);
        mapping.push(0);
        mapping.extend_from_slice(&2u16.to_be_bytes()); // count: SEEKTABLE + VORBIS_COMMENT
        mapping.extend_from_slice(b"fLaC");
        mapping.extend_from_slice(&streaminfo);

        // A SEEKTABLE block (type 3, structural — must be preserved).
        let mut seektable = Vec::new();
        crate::flac::push_block_header(&mut seektable, 3, 18, false);
        seektable.extend(std::iter::repeat_n(0xEEu8, 18));

        // An existing VORBIS_COMMENT (type 4, last) to be replaced.
        let mut old_vc = Vec::new();
        let body = crate::vorbiscomment::build(&[crate::input::TagInput::new("x", "old")]);
        crate::flac::push_block_header(&mut old_vc, 4, body.len(), true);
        old_vc.extend_from_slice(&body);

        let (bytes, _) = crate::ogg::page::build_header(77, &[&mapping, &seektable, &old_vc]);
        bytes
    }

    #[test]
    fn synthesize_oggflac_keeps_seektable_replaces_comment_and_count() {
        let mut data = oggflac_headers();
        let (audio, _) = crate::ogg::page::lace_packet(77, 3, false, 4096, &[0u8; 64]);
        data.extend_from_slice(&audio);

        let scan = locate_audio(&data).unwrap();
        assert_eq!(scan.codec, Codec::OggFlac);
        let header = read_metadata(&data[..scan.audio_offset as usize]).unwrap();

        let layout = synthesize_layout(
            &header,
            scan.audio_offset,
            scan.audio_length,
            &[TagInput::new("title", "Kaini Industries")],
        )
        .unwrap();

        if let Segment::Inline(bytes) = &layout.segments()[0] {
            let h = read_header(bytes).unwrap();
            assert_eq!(h.codec, Codec::OggFlac);
            // packet 0 mapping count == number of following blocks (SEEKTABLE + VC == 2)
            assert_eq!(u16::from_be_bytes([h.packets[0][7], h.packets[0][8]]), 2);
            // SEEKTABLE preserved
            assert!(h.packets.iter().skip(1).any(|p| (p[0] & 0x7F) == 3));
            // exactly one VORBIS_COMMENT, with the new tag, flagged last
            let vc = h
                .packets
                .iter()
                .skip(1)
                .find(|p| (p[0] & 0x7F) == 4)
                .unwrap();
            assert_eq!(vc[0] & 0x80, 0x80);
            let tags = crate::vorbiscomment::parse(&vc[4..]).unwrap();
            assert_eq!(
                tags,
                vec![("TITLE".to_string(), "Kaini Industries".to_string())]
            );
        } else {
            panic!("expected Inline header");
        }
    }
}
