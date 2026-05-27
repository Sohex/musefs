mod b64;
mod crc;
mod page;

pub use b64::{b64_len, b64_window, encode_b64_slice, B64Window};
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

/// Reject multiplexed/chained Ogg: within the header region every page must share
/// the first page's serial and only the first page may carry BOS.
fn validate_single_bitstream(data: &[u8], audio_offset: u64, serial: u32) -> Result<()> {
    let mut pos = 0usize;
    let mut first = true;
    while (pos as u64) < audio_offset {
        let h = crate::ogg::page::parse_page(data, pos)?;
        if h.serial != serial {
            return Err(FormatError::Malformed);
        }
        if !first && (h.header_type & crate::ogg::page::FLAG_BOS) != 0 {
            return Err(FormatError::Malformed);
        }
        first = false;
        pos += h.total_len();
    }
    Ok(())
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
    let audio_offset = last.end_offset as u64;
    validate_single_bitstream(data, audio_offset, serial)?;
    Ok(OggHeader {
        codec,
        serial,
        packets: pkts.iter().map(|p| p.data.clone()).collect(),
        header_pages: last.pages_through_end,
        audio_offset,
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
        Codec::Opus | Codec::Vorbis => 1,
        // OggFLAC: packet 0 is the mapping header; the VORBIS_COMMENT block is
        // whichever following packet has block type 4.
        Codec::OggFlac => header
            .packets
            .iter()
            .enumerate()
            .skip(1)
            .find(|(_, p)| !p.is_empty() && (p[0] & 0x7F) == 4)
            .map_or(0, |(i, _)| i),
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

/// Assemble a synthesized layout: regenerated header pages (with embedded art as
/// `OggArtSlice` runs) + one compact `OggAudio` segment renumbering the preserved
/// audio pages. `arts` carries each embedded image's metadata + raw bytes (used
/// transiently to compute page CRCs; not retained in the layout).
pub fn synthesize_layout(
    header: &OggHeader,
    audio_offset: u64,
    audio_length: u64,
    tags: &[TagInput],
    arts: &[OggArt],
) -> Result<RegionLayout> {
    let packet_chunks = build_packets_with_art(header, tags, arts)?;
    let mut segments: Vec<Segment> = Vec::new();
    let mut seq = 0u32;
    for (i, chunks) in packet_chunks.iter().enumerate() {
        let (segs, used) =
            crate::ogg::page::lace_chunks_to_segments(header.serial, seq, i == 0, chunks);
        segments.extend(segs);
        seq += used;
    }
    let seq_delta = seq as i64 - header.header_pages as i64;
    segments.push(Segment::OggAudio {
        offset: audio_offset,
        len: audio_length,
        seq_delta,
    });
    Ok(RegionLayout::new(segments))
}

/// Build the FLAC PICTURE block *body prefix* (everything before the image data:
/// type, mime, description, dimensions, depth, colors, data-length) for `art`,
/// padding the description with spaces so the prefix length is a multiple of 3.
/// This makes `base64(prefix ++ image) == base64(prefix) ++ base64(image)`, so the
/// image's base64 is an independent substring that can be served incrementally.
/// The declared data-length field is the true image length (`art.data_len`).
fn picture_prefix(art: &crate::input::ArtInput) -> Vec<u8> {
    // Unpadded prefix length = 4(type)+4(mimelen)+mime +4(desclen)+desc
    //   +4(w)+4(h)+4(depth)+4(colors)+4(datalen) = 32 + mime + desc.
    let base = 32 + art.mime.len() + art.description.len();
    let pad = (3 - base % 3) % 3;
    let description = format!("{}{}", art.description, " ".repeat(pad));

    let mut out = Vec::new();
    out.extend_from_slice(&art.picture_type.to_be_bytes());
    out.extend_from_slice(&(art.mime.len() as u32).to_be_bytes());
    out.extend_from_slice(art.mime.as_bytes());
    out.extend_from_slice(&(description.len() as u32).to_be_bytes());
    out.extend_from_slice(description.as_bytes());
    out.extend_from_slice(&art.width.to_be_bytes());
    out.extend_from_slice(&art.height.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes()); // depth
    out.extend_from_slice(&0u32.to_be_bytes()); // colors
    out.extend_from_slice(&(art.data_len as u32).to_be_bytes()); // image data length
    out
}

use crate::ogg::page::PayloadChunk;
use base64::Engine;

/// One image to embed: its metadata and raw bytes (read transiently at resolve).
pub struct OggArt<'a> {
    pub meta: &'a crate::input::ArtInput,
    pub image: &'a [u8],
}

fn b64_encode(bytes: &[u8]) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .encode(bytes)
        .into_bytes()
}

/// Build the regenerated header packets as chunk lists, embedding `arts`.
/// Opus/Vorbis: art goes into the comment packet as `METADATA_BLOCK_PICTURE`
/// comments (last). OggFLAC: each art is a native PICTURE block packet.
fn build_packets_with_art(
    header: &OggHeader,
    tags: &[TagInput],
    arts: &[OggArt],
) -> Result<Vec<Vec<PayloadChunk>>> {
    match header.codec {
        Codec::Opus | Codec::Vorbis => {
            // VorbisComment value length is a 32-bit field; guard against overflow
            // for absurdly large images (cover art is far below this).
            for a in arts {
                if b64_len(a.meta.data_len) > u32::MAX as u64 {
                    return Err(FormatError::TooLarge);
                }
            }
            if header.codec == Codec::Opus {
                Ok(vec![
                    vec![PayloadChunk::Bytes(header.packets[0].clone())],
                    comment_packet_chunks(b"OpusTags", tags, arts, false),
                ])
            } else {
                Ok(vec![
                    vec![PayloadChunk::Bytes(header.packets[0].clone())],
                    comment_packet_chunks(b"\x03vorbis", tags, arts, true),
                    vec![PayloadChunk::Bytes(header.packets[2].clone())],
                ])
            }
        }
        Codec::OggFlac => oggflac_packets_with_art(header, tags, arts),
    }
}

/// Build a VorbisComment-style comment packet (Opus `OpusTags` / Vorbis
/// `0x03vorbis`) as chunks: a leading `Bytes` chunk (magic + vendor + count + text
/// comments + each art comment's framing and base64(prefix)), an `Art` chunk per
/// image (base64 of the image), and — for Vorbis — a trailing framing-bit `Bytes`
/// chunk.
fn comment_packet_chunks(
    magic: &[u8],
    tags: &[TagInput],
    arts: &[OggArt],
    framing_bit: bool,
) -> Vec<PayloadChunk> {
    let text_body = crate::vorbiscomment::build(tags); // vendor + count(text) + text comments
    let vendor_len = u32::from_le_bytes(text_body[0..4].try_into().unwrap()) as usize;
    let count_pos = 4 + vendor_len;
    let text_count = u32::from_le_bytes(text_body[count_pos..count_pos + 4].try_into().unwrap());
    let mut leading = text_body.clone();
    let new_count = text_count + arts.len() as u32;
    leading[count_pos..count_pos + 4].copy_from_slice(&new_count.to_le_bytes());

    let mut chunks: Vec<PayloadChunk> = Vec::new();
    let mut head = magic.to_vec();
    head.extend_from_slice(&leading);

    const KEY: &[u8] = b"METADATA_BLOCK_PICTURE=";
    for art in arts {
        let prefix = picture_prefix(art.meta);
        let b64_prefix = b64_encode(&prefix);
        let value_len = KEY.len() + b64_prefix.len() + b64_len(art.meta.data_len) as usize;
        head.extend_from_slice(&(value_len as u32).to_le_bytes());
        head.extend_from_slice(KEY);
        head.extend_from_slice(&b64_prefix);
        chunks.push(PayloadChunk::Bytes(std::mem::take(&mut head)));
        chunks.push(PayloadChunk::Art {
            art_id: art.meta.art_id,
            out: b64_encode(art.image),
            base64: true,
            art_total: art.meta.data_len,
        });
    }
    if framing_bit {
        head.push(0x01);
    }
    if !head.is_empty() {
        chunks.push(PayloadChunk::Bytes(head));
    }
    chunks
}

/// OggFLAC header packets with art: the text comment packet (no art) plus one
/// native PICTURE block packet per image. The last metadata-block packet carries
/// the last-block flag, and packet 0's 16-bit following-packet count is recomputed.
fn oggflac_packets_with_art(
    header: &OggHeader,
    tags: &[TagInput],
    arts: &[OggArt],
) -> Result<Vec<Vec<PayloadChunk>>> {
    if header.packets.is_empty() {
        return Err(FormatError::Malformed);
    }
    let mut structural: Vec<Vec<u8>> = Vec::new();
    for pkt in header.packets.iter().skip(1) {
        if !pkt.is_empty() && matches!(pkt[0] & 0x7F, 2 | 3 | 5) {
            structural.push(pkt.clone());
        }
    }

    let vc = crate::vorbiscomment::build(tags);
    let mut comment = Vec::new();
    crate::flac::push_block_header(&mut comment, 4, vc.len(), false);
    comment.extend_from_slice(&vc);

    let following_count = structural.len() + 1 + arts.len();
    let count = u16::try_from(following_count).map_err(|_| FormatError::TooLarge)?;

    let mut block_packets: Vec<Vec<PayloadChunk>> = Vec::new();
    for s in &structural {
        block_packets.push(vec![PayloadChunk::Bytes(s.clone())]);
    }
    block_packets.push(vec![PayloadChunk::Bytes(comment)]);
    for art in arts {
        let prefix = picture_prefix(art.meta);
        let body_len = prefix.len() as u64 + art.meta.data_len;
        if body_len > 0x00FF_FFFF {
            return Err(FormatError::TooLarge);
        }
        let mut blk = Vec::new();
        crate::flac::push_block_header(&mut blk, 6, body_len as usize, false);
        blk.extend_from_slice(&prefix);
        block_packets.push(vec![
            PayloadChunk::Bytes(blk),
            PayloadChunk::Art {
                art_id: art.meta.art_id,
                out: art.image.to_vec(),
                base64: false,
                art_total: art.meta.data_len,
            },
        ]);
    }

    let n = block_packets.len();
    for (i, bp) in block_packets.iter_mut().enumerate() {
        if let Some(PayloadChunk::Bytes(b)) = bp.first_mut() {
            if i + 1 == n {
                b[0] |= 0x80;
            } else {
                b[0] &= 0x7F;
            }
        }
    }

    let mut mapping = header.packets[0].clone();
    if mapping.len() < 9 {
        return Err(FormatError::Malformed);
    }
    mapping[7..9].copy_from_slice(&count.to_be_bytes());

    let mut out = vec![vec![PayloadChunk::Bytes(mapping)]];
    out.extend(block_packets);
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
            &[],
        )
        .unwrap();

        // Collect all Inline header bytes (one per page) until OggAudio.
        let mut header_bytes: Vec<u8> = Vec::new();
        let mut audio_seg = None;
        for seg in layout.segments() {
            match seg {
                Segment::Inline(b) => header_bytes.extend_from_slice(b),
                Segment::OggAudio { offset, len, .. } => {
                    audio_seg = Some((*offset, *len));
                    break;
                }
                other => panic!("unexpected segment {other:?}"),
            }
        }
        let h = read_header(&header_bytes).unwrap();
        assert_eq!(h.codec, Codec::Opus);
        let body = comment_body(Codec::Opus, &h.packets[1]).unwrap();
        let tags = crate::vorbiscomment::parse(body).unwrap();
        assert_eq!(tags, vec![("ALBUM".to_string(), "Geogaddi".to_string())]);
        let (offset, len) = audio_seg.expect("expected OggAudio segment");
        assert_eq!(offset, scan.audio_offset);
        assert_eq!(len, scan.audio_length);
    }

    fn vorbis_headers_with(setup: &[u8]) -> Vec<u8> {
        // Minimal-but-shaped Vorbis ID header (30 bytes from 0x01"vorbis").
        let mut id = b"\x01vorbis".to_vec();
        id.extend_from_slice(&0u32.to_le_bytes()); // version
        id.push(2); // channels
        id.extend_from_slice(&44100u32.to_le_bytes()); // sample rate
        id.extend_from_slice(&0u32.to_le_bytes()); // bitrate max
        id.extend_from_slice(&128_000u32.to_le_bytes()); // nominal
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
            &[],
        )
        .unwrap();

        let mut header_bytes: Vec<u8> = Vec::new();
        for seg in layout.segments() {
            match seg {
                Segment::Inline(b) => header_bytes.extend_from_slice(b),
                Segment::OggAudio { .. } => break,
                other => panic!("unexpected segment {other:?}"),
            }
        }
        let h = read_header(&header_bytes).unwrap();
        assert_eq!(h.codec, Codec::Vorbis);
        assert_eq!(h.packets[2], setup); // setup preserved byte-for-byte
        let body = comment_body(Codec::Vorbis, &h.packets[1]).unwrap();
        let tags = crate::vorbiscomment::parse(body).unwrap();
        assert_eq!(tags, vec![("ARTIST".to_string(), "Autechre".to_string())]);
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
        let comment = format!("METADATA_BLOCK_PICTURE={b64}");
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
    fn rejects_multiplexed_second_bitstream() {
        // Two BOS pages with DIFFERENT serials at the start => multiplexed; must reject.
        let head = b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".to_vec();
        let (mut data, _) = crate::ogg::page::lace_packet(0x1111, 0, true, 0, &head);
        // A second logical stream's BOS page (different serial).
        let (other, _) = crate::ogg::page::lace_packet(
            0x2222,
            0,
            true,
            0,
            b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".as_ref(),
        );
        data.extend_from_slice(&other);
        // Some audio after, so audio_offset (if it were accepted) is past these pages.
        let (audio, _) = crate::ogg::page::lace_packet(0x1111, 1, false, 960, &[0u8; 50]);
        data.extend_from_slice(&audio);
        assert!(read_header(&data).is_err());
        assert!(locate_audio(&data).is_err());
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
            &[],
        )
        .unwrap();

        let mut header_bytes: Vec<u8> = Vec::new();
        for seg in layout.segments() {
            match seg {
                Segment::Inline(b) => header_bytes.extend_from_slice(b),
                Segment::OggAudio { .. } => break,
                other => panic!("unexpected segment {other:?}"),
            }
        }
        let h = read_header(&header_bytes).unwrap();
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
    }

    #[test]
    fn synthesize_opus_embeds_art_that_round_trips() {
        let mut data = opus_headers();
        let (audio, _) = crate::ogg::page::lace_packet(0x1234, 2, false, 960, &[0u8; 80]);
        data.extend_from_slice(&audio);
        let scan = locate_audio(&data).unwrap();
        let header = read_metadata(&data[..scan.audio_offset as usize]).unwrap();

        let image: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        let meta = crate::input::ArtInput {
            art_id: 7,
            mime: "image/jpeg".to_string(),
            description: String::new(),
            picture_type: 3,
            width: 64,
            height: 64,
            data_len: image.len() as u64,
        };
        let layout = synthesize_layout(
            &header,
            scan.audio_offset,
            scan.audio_length,
            &[TagInput::new("title", "Cover")],
            &[OggArt {
                meta: &meta,
                image: &image,
            }],
        )
        .unwrap();

        // Materialize the header region from the layout, expanding OggArtSlice by
        // re-deriving its bytes from `image` (mirrors what read_at does).
        let mut bytes = Vec::new();
        for s in layout.segments() {
            match s {
                Segment::Inline(b) => bytes.extend_from_slice(b),
                Segment::OggArtSlice {
                    offset,
                    len,
                    base64,
                    art_total,
                    ..
                } => {
                    assert!(*base64);
                    let w = b64_window(*offset, *len, *art_total);
                    let raw = &image[w.in_start as usize..(w.in_start + w.in_len) as usize];
                    bytes.extend_from_slice(&encode_b64_slice(raw, w.skip, *len as usize));
                }
                Segment::OggAudio { .. } => break, // header region ends here
                other => panic!("unexpected {other:?}"),
            }
        }

        let pics = read_pictures(&bytes).unwrap();
        assert_eq!(pics.len(), 1);
        assert_eq!(pics[0].mime, "image/jpeg");
        assert_eq!(pics[0].data, image);
        let h = read_header(&bytes).unwrap();
        assert_eq!(h.codec, Codec::Opus);
    }

    // Materialize the header region of a synthesized layout into bytes, expanding
    // each OggArtSlice from `images` (art_id -> raw image), mirroring read_at.
    fn materialize_header(layout: &RegionLayout, images: &[(i64, &[u8])]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for s in layout.segments() {
            match s {
                Segment::Inline(b) => bytes.extend_from_slice(b),
                Segment::OggArtSlice {
                    art_id,
                    offset,
                    len,
                    base64,
                    art_total,
                } => {
                    let img = images.iter().find(|(id, _)| id == art_id).expect("image").1;
                    if *base64 {
                        let w = b64_window(*offset, *len, *art_total);
                        let raw = &img[w.in_start as usize..(w.in_start + w.in_len) as usize];
                        bytes.extend_from_slice(&encode_b64_slice(raw, w.skip, *len as usize));
                    } else {
                        bytes.extend_from_slice(&img[*offset as usize..(*offset + *len) as usize]);
                    }
                }
                Segment::OggAudio { .. } => break,
                other => panic!("unexpected {other:?}"),
            }
        }
        bytes
    }

    fn art_input(art_id: i64, mime: &str, len: usize) -> crate::input::ArtInput {
        crate::input::ArtInput {
            art_id,
            mime: mime.to_string(),
            description: String::new(),
            picture_type: 3,
            width: 10,
            height: 10,
            data_len: len as u64,
        }
    }

    #[test]
    fn synthesize_vorbis_embeds_art_that_round_trips() {
        let setup = b"\x05vorbis-SETUP".to_vec();
        let mut data = vorbis_headers_with(&setup);
        let (audio, _) = crate::ogg::page::lace_packet(55, 99, false, 1024, &[0u8; 64]);
        data.extend_from_slice(&audio);
        let scan = locate_audio(&data).unwrap();
        let header = read_metadata(&data[..scan.audio_offset as usize]).unwrap();

        let image: Vec<u8> = (0..4000u32).map(|i| (i % 251) as u8).collect();
        let meta = art_input(11, "image/png", image.len());
        let layout = synthesize_layout(
            &header,
            scan.audio_offset,
            scan.audio_length,
            &[TagInput::new("artist", "X")],
            &[OggArt {
                meta: &meta,
                image: &image,
            }],
        )
        .unwrap();

        let bytes = materialize_header(&layout, &[(11, &image)]);
        let h = read_header(&bytes).unwrap();
        assert_eq!(h.codec, Codec::Vorbis);
        assert_eq!(h.packets[2], setup); // setup preserved
        let pics = read_pictures(&bytes).unwrap();
        assert_eq!(pics.len(), 1);
        assert_eq!(pics[0].data, image);
    }

    #[test]
    fn synthesize_oggflac_embeds_art_that_round_trips() {
        let mut data = oggflac_headers();
        let (audio, _) = crate::ogg::page::lace_packet(77, 3, false, 4096, &[0u8; 64]);
        data.extend_from_slice(&audio);
        let scan = locate_audio(&data).unwrap();
        let header = read_metadata(&data[..scan.audio_offset as usize]).unwrap();

        let image: Vec<u8> = (0..4000u32).map(|i| (i % 251) as u8).collect();
        let meta = art_input(22, "image/png", image.len());
        let layout = synthesize_layout(
            &header,
            scan.audio_offset,
            scan.audio_length,
            &[TagInput::new("title", "Y")],
            &[OggArt {
                meta: &meta,
                image: &image,
            }],
        )
        .unwrap();

        let bytes = materialize_header(&layout, &[(22, &image)]);
        let h = read_header(&bytes).unwrap();
        assert_eq!(h.codec, Codec::OggFlac);
        let pics = read_pictures(&bytes).unwrap();
        assert_eq!(pics.len(), 1);
        assert_eq!(pics[0].data, image);
    }

    #[test]
    fn synthesize_opus_embeds_multiple_images() {
        let mut data = opus_headers();
        let (audio, _) = crate::ogg::page::lace_packet(0x1234, 2, false, 960, &[0u8; 64]);
        data.extend_from_slice(&audio);
        let scan = locate_audio(&data).unwrap();
        let header = read_metadata(&data[..scan.audio_offset as usize]).unwrap();

        let img_a: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
        let img_b: Vec<u8> = (0..1500u32).map(|i| ((i * 3) % 251) as u8).collect();
        let meta_a = art_input(1, "image/png", img_a.len());
        let meta_b = art_input(2, "image/jpeg", img_b.len());
        let layout = synthesize_layout(
            &header,
            scan.audio_offset,
            scan.audio_length,
            &[TagInput::new("title", "Multi")],
            &[
                OggArt {
                    meta: &meta_a,
                    image: &img_a,
                },
                OggArt {
                    meta: &meta_b,
                    image: &img_b,
                },
            ],
        )
        .unwrap();

        let bytes = materialize_header(&layout, &[(1, &img_a), (2, &img_b)]);
        let h = read_header(&bytes).unwrap();
        assert_eq!(h.codec, Codec::Opus);
        let pics = read_pictures(&bytes).unwrap();
        assert_eq!(pics.len(), 2);
        assert_eq!(pics[0].data, img_a);
        assert_eq!(pics[1].data, img_b);
    }

    #[test]
    fn picture_prefix_is_3_aligned_and_declares_image_len() {
        let art = crate::input::ArtInput {
            art_id: 1,
            mime: "image/png".to_string(), // 9 -> base = 32+9+0 = 41 -> pad 1
            description: String::new(),
            picture_type: 3,
            width: 1,
            height: 1,
            data_len: 12345,
        };
        let p = picture_prefix(&art);
        assert_eq!(p.len() % 3, 0);
        // datalen is the last 4 bytes (big-endian) and equals the true image length.
        let dl = u32::from_be_bytes(p[p.len() - 4..].try_into().unwrap());
        assert_eq!(dl, 12345);
        // Reusing the existing FLAC picture parser proves the framing is valid:
        // parse_picture_block expects the body (prefix + image); append dummy image.
        let mut body = p.clone();
        body.extend(std::iter::repeat_n(0u8, 12345));
        let pic = crate::flac::parse_picture_block(&body).unwrap();
        assert_eq!(pic.mime, "image/png");
        assert_eq!(pic.picture_type, 3);
    }
}
