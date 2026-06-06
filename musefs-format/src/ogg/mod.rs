mod b64;
mod crc;
mod page;

pub use b64::{b64_len, b64_window, encode_b64_slice, B64Window};
pub use page::{
    parse_page, patch_page_header, patch_page_header_algebraic, verify_page_crc, PageHeader,
};

use crate::error::{FormatError, Result};
use crate::probe::Extent;

/// The codec carried inside an Ogg logical bitstream that we synthesize.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Opus,
    Vorbis,
    OggFlac,
}

const METADATA_BLOCK_PICTURE_KEY: &[u8] = b"METADATA_BLOCK_PICTURE=";

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

/// Bounded twin of [`read_metadata`]. OGG header packets (and all OGG embedded
/// art) are front-anchored, so a prefix covering the header region is sufficient.
/// `read_header` does not expose an exact byte need, so on a short/truncated
/// prefix this geometrically grows the window (doubling, capped at `file_len`):
/// header regions are tiny, so the first 1 MiB window almost always completes,
/// and the cap guarantees the worst case equals reading the whole file.
pub fn read_metadata_bounded(prefix: &[u8], file_len: u64) -> Result<Extent<OggHeader>> {
    match read_header(prefix) {
        Ok(header) => Ok(Extent::Complete(header)),
        // `read_header` cannot distinguish a truncated front from genuine
        // corruption, so we widen optimistically; a real error resurfaces via the
        // `Err(e)` arm once `prefix` reaches `file_len` (and the caller's retry
        // limit + full-read fallback bound the cost).
        Err(_) if (prefix.len() as u64) < file_len => {
            let grown = ((prefix.len() as u64).saturating_mul(2)).max(64 * 1024);
            Ok(Extent::NeedMore {
                up_to: grown.min(file_len),
            })
        }
        Err(e) => Err(e),
    }
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
    // Exclude zero-byte art: an empty image yields a meaningless
    // METADATA_BLOCK_PICTURE comment (and an empty OggArtSlice run). Mirrors the
    // FLAC/MP3/MP4/WAV synthesis skip so every format drops degenerate art.
    let arts: Vec<OggArt> = arts
        .iter()
        .filter(|a| a.meta.data_len > 0)
        .copied()
        .collect();
    let packet_chunks = build_packets_with_art(header, tags, &arts)?;
    let mut segments: Vec<Segment> = Vec::new();
    let mut seq = 0u32;
    for (i, chunks) in packet_chunks.iter().enumerate() {
        let (segs, used) =
            crate::ogg::page::lace_chunks_to_segments(header.serial, seq, i == 0, chunks);
        segments.extend(segs);
        seq += used;
    }
    let seq_delta = i64::from(seq) - i64::from(header.header_pages);
    segments.push(Segment::OggAudio {
        offset: audio_offset,
        len: audio_length,
        seq_delta,
    });
    Ok(RegionLayout::validated(segments)?)
}

/// Build the FLAC PICTURE block *body prefix* (everything before the image data:
/// type, mime, description, dimensions, depth, colors, data-length) for `art`,
/// padding the description with spaces so the prefix length is a multiple of 3.
/// This makes `base64(prefix ++ image) == base64(prefix) ++ base64(image)`, so the
/// image's base64 is an independent substring that can be served incrementally.
/// The declared data-length field is the true image length (`art.data_len`).
fn picture_prefix(art: &crate::input::ArtInput) -> Result<Vec<u8>> {
    // Unpadded prefix length = 4(type)+4(mimelen)+mime +4(desclen)+desc
    //   +4(w)+4(h)+4(depth)+4(colors)+4(datalen) = 32 + mime + desc.
    let base = 32 + art.mime.len() + art.description.len();
    let pad = (3 - base % 3) % 3;
    let description = format!("{}{}", art.description, " ".repeat(pad));

    let mut out = Vec::new();
    out.extend_from_slice(&art.picture_type.to_be_bytes());
    out.extend_from_slice(
        &u32::try_from(art.mime.len())
            .map_err(|_| FormatError::TooLarge)?
            .to_be_bytes(),
    );
    out.extend_from_slice(art.mime.as_bytes());
    out.extend_from_slice(
        &u32::try_from(description.len())
            .map_err(|_| FormatError::TooLarge)?
            .to_be_bytes(),
    );
    out.extend_from_slice(description.as_bytes());
    out.extend_from_slice(&art.width.to_be_bytes());
    out.extend_from_slice(&art.height.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes()); // depth
    out.extend_from_slice(&0u32.to_be_bytes()); // colors
    out.extend_from_slice(
        &u32::try_from(art.data_len)
            .map_err(|_| FormatError::TooLarge)?
            .to_be_bytes(),
    ); // image data length
    Ok(out)
}

use crate::ogg::page::PayloadChunk;
use base64::Engine;

/// One image to embed: its metadata and raw bytes (read transiently at resolve).
#[derive(Clone, Copy)]
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
            // for absurdly large images (cover art is far below this). The full
            // value includes the key, base64 of the picture prefix, and base64 of
            // the image; any one of these alone may fit in u32 but the sum may not.
            for a in arts {
                let prefix = picture_prefix(a.meta)?;
                let b64_prefix_len = b64_len(prefix.len() as u64);
                let value_len = METADATA_BLOCK_PICTURE_KEY.len() as u64
                    + b64_prefix_len
                    + b64_len(a.meta.data_len);
                if value_len > u64::from(u32::MAX) {
                    return Err(FormatError::TooLarge);
                }
            }
            if header.codec == Codec::Opus {
                Ok(vec![
                    vec![PayloadChunk::Bytes(header.packets[0].clone())],
                    comment_packet_chunks(b"OpusTags", tags, arts, false)?,
                ])
            } else {
                Ok(vec![
                    vec![PayloadChunk::Bytes(header.packets[0].clone())],
                    comment_packet_chunks(b"\x03vorbis", tags, arts, true)?,
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
) -> Result<Vec<PayloadChunk>> {
    let text_body = crate::vorbiscomment::build(tags)?; // vendor + count(text) + text comments
    let vendor_len = u32::from_le_bytes(text_body[0..4].try_into().unwrap()) as usize;
    let count_pos = 4 + vendor_len;
    let text_count = u32::from_le_bytes(text_body[count_pos..count_pos + 4].try_into().unwrap());
    let mut leading = text_body.clone();
    let new_count = text_count + u32::try_from(arts.len()).map_err(|_| FormatError::TooLarge)?;
    leading[count_pos..count_pos + 4].copy_from_slice(&new_count.to_le_bytes());

    let mut chunks: Vec<PayloadChunk> = Vec::new();
    let mut head = magic.to_vec();
    head.extend_from_slice(&leading);

    for art in arts {
        let prefix = picture_prefix(art.meta)?;
        let b64_prefix = b64_encode(&prefix);
        let value_len = METADATA_BLOCK_PICTURE_KEY.len()
            + b64_prefix.len()
            + crate::convert::usize_from(b64_len(art.meta.data_len));
        head.extend_from_slice(
            &u32::try_from(value_len)
                .map_err(|_| FormatError::TooLarge)?
                .to_le_bytes(),
        );
        head.extend_from_slice(METADATA_BLOCK_PICTURE_KEY);
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
    Ok(chunks)
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

    let vc = crate::vorbiscomment::build(tags)?;
    if vc.len() as u64 > crate::flac::MAX_BLOCK_BODY {
        return Err(FormatError::TooLarge);
    }
    let mut comment = Vec::new();
    crate::flac::push_block_header(&mut comment, 4, vc.len(), false)?;
    comment.extend_from_slice(&vc);

    let following_count = structural.len() + 1 + arts.len();
    let count = u16::try_from(following_count).map_err(|_| FormatError::TooLarge)?;

    let mut block_packets: Vec<Vec<PayloadChunk>> = Vec::new();
    for s in &structural {
        block_packets.push(vec![PayloadChunk::Bytes(s.clone())]);
    }
    block_packets.push(vec![PayloadChunk::Bytes(comment)]);
    for art in arts {
        let prefix = picture_prefix(art.meta)?;
        let body_len = prefix.len() as u64 + art.meta.data_len;
        if body_len > crate::flac::MAX_BLOCK_BODY {
            return Err(FormatError::TooLarge);
        }
        let mut blk = Vec::new();
        crate::flac::push_block_header(&mut blk, 6, crate::convert::usize_from(body_len), false)?;
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
        crate::vorbiscomment::build(&[]).unwrap()
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
        let body =
            crate::vorbiscomment::build(&[crate::input::TagInput::new("title", "Sun")]).unwrap();
        let mut tags_pkt = b"OpusTags".to_vec();
        tags_pkt.extend_from_slice(&body);
        let head = b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".to_vec();
        let (mut data, _) = crate::ogg::page::build_header(7, &[&head, &tags_pkt]);
        let (audio, _) = crate::ogg::page::lace_packet(7, 2, false, 960, &[0u8; 50]);
        data.extend_from_slice(&audio);

        let tags = read_tags(&data).unwrap();
        assert_eq!(tags, vec![("title".to_string(), "Sun".to_string())]);
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
        let header = read_metadata(&data[..crate::convert::usize_from(scan.audio_offset)]).unwrap();

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
        assert_eq!(tags, vec![("album".to_string(), "Geogaddi".to_string())]);
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
        comment.extend_from_slice(&crate::vorbiscomment::build(&[]).unwrap());
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
        let header = read_metadata(&data[..crate::convert::usize_from(scan.audio_offset)]).unwrap();
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
        assert_eq!(tags, vec![("artist".to_string(), "Autechre".to_string())]);
    }

    #[test]
    fn read_pictures_opus_decodes_metadata_block_picture() {
        use base64::Engine;
        // A minimal FLAC PICTURE block body: type=3, mime="image/png", empty desc,
        // 1x1, depth 0, colors 0, data="PNG".
        let mut pic = Vec::new();
        pic.extend_from_slice(&3u32.to_be_bytes());
        let mime = b"image/png";
        pic.extend_from_slice(&u32::try_from(mime.len()).unwrap().to_be_bytes());
        pic.extend_from_slice(mime);
        pic.extend_from_slice(&0u32.to_be_bytes()); // desc len
        pic.extend_from_slice(&1u32.to_be_bytes()); // width
        pic.extend_from_slice(&1u32.to_be_bytes()); // height
        pic.extend_from_slice(&0u32.to_be_bytes()); // depth
        pic.extend_from_slice(&0u32.to_be_bytes()); // colors
        let img = b"PNG";
        pic.extend_from_slice(&u32::try_from(img.len()).unwrap().to_be_bytes());
        pic.extend_from_slice(img);
        let b64 = base64::engine::general_purpose::STANDARD.encode(&pic);

        let mut body = Vec::new();
        body.extend_from_slice(
            &u32::try_from(crate::vorbiscomment::VENDOR.len())
                .unwrap()
                .to_le_bytes(),
        );
        body.extend_from_slice(crate::vorbiscomment::VENDOR.as_bytes());
        body.extend_from_slice(&1u32.to_le_bytes()); // one comment
        let comment = format!("METADATA_BLOCK_PICTURE={b64}");
        body.extend_from_slice(&u32::try_from(comment.len()).unwrap().to_le_bytes());
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
        crate::flac::push_block_header(&mut streaminfo, 0, 34, false).unwrap();
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
        crate::flac::push_block_header(&mut seektable, 3, 18, false).unwrap();
        seektable.extend(std::iter::repeat_n(0xEEu8, 18));

        // An existing VORBIS_COMMENT (type 4, last) to be replaced.
        let mut old_vc = Vec::new();
        let body = crate::vorbiscomment::build(&[crate::input::TagInput::new("x", "old")]).unwrap();
        crate::flac::push_block_header(&mut old_vc, 4, body.len(), true).unwrap();
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
        let header = read_metadata(&data[..crate::convert::usize_from(scan.audio_offset)]).unwrap();

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
            vec![("title".to_string(), "Kaini Industries".to_string())]
        );
    }

    #[test]
    fn synthesize_opus_embeds_art_that_round_trips() {
        let mut data = opus_headers();
        let (audio, _) = crate::ogg::page::lace_packet(0x1234, 2, false, 960, &[0u8; 80]);
        data.extend_from_slice(&audio);
        let scan = locate_audio(&data).unwrap();
        let header = read_metadata(&data[..crate::convert::usize_from(scan.audio_offset)]).unwrap();

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
                    let raw = &image[crate::convert::usize_from(w.in_start)
                        ..crate::convert::usize_from(w.in_start + w.in_len)];
                    bytes.extend_from_slice(&encode_b64_slice(
                        raw,
                        w.skip,
                        crate::convert::usize_from(*len),
                    ));
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
                        let raw = &img[crate::convert::usize_from(w.in_start)
                            ..crate::convert::usize_from(w.in_start + w.in_len)];
                        bytes.extend_from_slice(&encode_b64_slice(
                            raw,
                            w.skip,
                            crate::convert::usize_from(*len),
                        ));
                    } else {
                        bytes.extend_from_slice(
                            &img[crate::convert::usize_from(*offset)
                                ..crate::convert::usize_from(*offset + *len)],
                        );
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
        let header = read_metadata(&data[..crate::convert::usize_from(scan.audio_offset)]).unwrap();

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
        let header = read_metadata(&data[..crate::convert::usize_from(scan.audio_offset)]).unwrap();

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
        let header = read_metadata(&data[..crate::convert::usize_from(scan.audio_offset)]).unwrap();

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
    fn synthesize_opus_skips_zero_byte_art() {
        let mut data = opus_headers();
        let (audio, _) = crate::ogg::page::lace_packet(0x1234, 2, false, 960, &[0u8; 64]);
        data.extend_from_slice(&audio);
        let scan = locate_audio(&data).unwrap();
        let header = read_metadata(&data[..crate::convert::usize_from(scan.audio_offset)]).unwrap();

        let img: Vec<u8> = (0..2000u32).map(|i| (i % 251) as u8).collect();
        let empty: Vec<u8> = Vec::new();
        let meta_empty = art_input(1, "image/jpeg", 0);
        let meta_real = art_input(2, "image/png", img.len());
        // Empty art listed first: it must be dropped without disturbing the real one.
        let layout = synthesize_layout(
            &header,
            scan.audio_offset,
            scan.audio_length,
            &[TagInput::new("title", "Skip")],
            &[
                OggArt {
                    meta: &meta_empty,
                    image: &empty,
                },
                OggArt {
                    meta: &meta_real,
                    image: &img,
                },
            ],
        )
        .unwrap();

        let bytes = materialize_header(&layout, &[(2, &img)]);
        let h = read_header(&bytes).unwrap();
        assert_eq!(h.codec, Codec::Opus);
        let pics = read_pictures(&bytes).unwrap();
        assert_eq!(pics.len(), 1, "zero-byte art must be skipped at synthesis");
        assert_eq!(pics[0].data, img);
    }

    #[test]
    fn oversized_full_art_value_rejected_by_build_packets() {
        let meta = crate::input::ArtInput {
            art_id: 0,
            mime: "image/jpeg".to_string(),
            description: String::new(),
            data_len: u64::from(u32::MAX),
            picture_type: 3,
            width: 0,
            height: 0,
        };
        let art = OggArt {
            meta: &meta,
            image: &[],
        };
        let header = OggHeader {
            codec: Codec::Vorbis,
            serial: 0,
            packets: vec![vec![], vec![], vec![]],
            header_pages: 1,
            audio_offset: 0,
        };
        let result = build_packets_with_art(&header, &[], &[art]);
        assert!(result.is_err(), "expected Err for oversized art");
    }

    #[test]
    fn sum_overflow_art_value_rejected_by_build_packets() {
        // data_len and prefix individually fit in u32, but the full value
        // (key + b64(prefix) + b64(data)) exceeds u32::MAX.
        let meta = crate::input::ArtInput {
            art_id: 0,
            mime: "image/png".to_string(),
            description: "x".repeat(256),
            data_len: 3_221_225_470, // b64_len = 4_294_967_294 < u32::MAX
            picture_type: 3,
            width: 0,
            height: 0,
        };
        let art = OggArt {
            meta: &meta,
            image: &[],
        };
        let header = OggHeader {
            codec: Codec::Vorbis,
            serial: 0,
            packets: vec![vec![], vec![], vec![]],
            header_pages: 1,
            audio_offset: 0,
        };
        let result = build_packets_with_art(&header, &[], &[art]);
        assert!(
            result.is_err(),
            "expected Err when key + b64(prefix) + b64(data) overflows u32"
        );
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
        let p = picture_prefix(&art).unwrap();
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

    #[test]
    fn detect_codec_matches_each_magic_and_rejects_others() {
        assert_eq!(detect_codec(b"OpusHead........").unwrap(), Codec::Opus);
        assert_eq!(detect_codec(b"\x01vorbis...").unwrap(), Codec::Vorbis);
        assert_eq!(detect_codec(b"\x7FFLAC...").unwrap(), Codec::OggFlac);
        // Too-short and non-matching inputs must error (kills the :25 && -> || and
        // the length-guard mutations).
        assert!(detect_codec(b"OpusHea").is_err()); // 7 bytes, len guard
        assert!(detect_codec(b"XXXXXXXX").is_err()); // right length, wrong magic
        assert!(detect_codec(b"\x01vorbi").is_err()); // 6 bytes
    }

    #[test]
    fn comment_body_strips_each_codec_prefix_and_guards_length() {
        assert_eq!(comment_body(Codec::Opus, b"OpusTagsBODY").unwrap(), b"BODY");
        assert_eq!(
            comment_body(Codec::Vorbis, b"\x03vorbisBODY").unwrap(),
            b"BODY"
        );
        assert_eq!(
            comment_body(Codec::OggFlac, b"\x04\x00\x00\x00BODY").unwrap(),
            b"BODY"
        );
        // packet shorter than the prefix errors (kills :113 < -> ==/<=).
        assert!(comment_body(Codec::Opus, b"OpusTa").is_err());
        assert!(comment_body(Codec::OggFlac, b"\x04\x00\x00").is_err());
    }

    #[test]
    fn oggflac_following_packets_reads_be_count_and_guards_length() {
        // 0x7F"FLAC" major minor count(BE) ... ; count bytes at [7],[8].
        let pkt = b"\x7FFLAC\x01\x00\x00\x05rest";
        assert_eq!(oggflac_following_packets(pkt).unwrap(), 5);
        assert!(oggflac_following_packets(b"\x7FFLAC\x01\x00").is_err()); // 7 bytes (<9)
    }

    #[test]
    fn oggflac_comment_block_size_boundary_is_inclusive() {
        // The regenerated OggFLAC VORBIS_COMMENT block shares FLAC's 24-bit
        // block length. Derive the non-value overhead from production, then
        // size the value so the body lands exactly on the limit; one more byte
        // errors. The `>` accepts the inclusive limit; the `>=` mutant rejects.
        let header = OggHeader {
            codec: Codec::OggFlac,
            serial: 1,
            packets: vec![vec![0x7F; 9]],
            header_pages: 1,
            audio_offset: 0,
        };
        let overhead = crate::vorbiscomment::build(&[crate::input::TagInput::new("title", "")])
            .unwrap()
            .len() as u64;
        let at_limit = "x".repeat(crate::convert::usize_from(
            crate::flac::MAX_BLOCK_BODY - overhead,
        ));
        let tags = [crate::input::TagInput::new("title", at_limit.as_str())];
        assert!(oggflac_packets_with_art(&header, &tags, &[]).is_ok());
        // one byte over must still error, pinning the high side of the boundary.
        let over = format!("{at_limit}x");
        let tags = [crate::input::TagInput::new("title", over.as_str())];
        assert!(matches!(
            oggflac_packets_with_art(&header, &tags, &[]),
            Err(FormatError::TooLarge)
        ));
    }

    #[test]
    fn oggflac_picture_block_size_boundary_is_inclusive() {
        // body_len = picture_prefix(meta).len() + data_len; the guard shares
        // FLAC's 24-bit block limit. data_len is only a count (image bytes are
        // streamed), so the exact boundary is cheap to pin. The `>` accepts the
        // inclusive limit — which also pins the `+` assembly, since a product
        // of the two terms overshoots it — while the `>=` mutant rejects it.
        let header = OggHeader {
            codec: Codec::OggFlac,
            serial: 1,
            packets: vec![vec![0x7F; 9]],
            header_pages: 1,
            audio_offset: 0,
        };
        let mk = |data_len: u64| crate::input::ArtInput {
            art_id: 1,
            mime: "image/png".to_string(),
            description: String::new(),
            picture_type: 3,
            width: 0,
            height: 0,
            data_len,
        };
        let framing_len = picture_prefix(&mk(0)).unwrap().len() as u64;
        let at_limit = mk(crate::flac::MAX_BLOCK_BODY - framing_len);
        let arts = [OggArt {
            meta: &at_limit,
            image: &[],
        }];
        assert!(oggflac_packets_with_art(&header, &[], &arts).is_ok());
        // one byte over must still error, pinning the high side of the boundary.
        let over = mk(crate::flac::MAX_BLOCK_BODY - framing_len + 1);
        let arts = [OggArt {
            meta: &over,
            image: &[],
        }];
        assert!(matches!(
            oggflac_packets_with_art(&header, &[], &arts),
            Err(FormatError::TooLarge)
        ));
    }

    #[test]
    fn comment_packet_index_locates_the_comment_block() {
        // Opus/Vorbis: always packet index 1 (kills :121 -> 1 only if a non-1 case
        // exists; assert OggFLAC search to pin the skip(1)+find logic at :130).
        let opus = OggHeader {
            codec: Codec::Opus,
            serial: 1,
            packets: vec![vec![], vec![]],
            header_pages: 1,
            audio_offset: 0,
        };
        assert_eq!(comment_packet_index(&opus), 1);

        // OggFLAC: packet 0 mapping, packet 1 type 1 (non-comment), packet 2 type 4.
        let oggflac = OggHeader {
            codec: Codec::OggFlac,
            serial: 1,
            packets: vec![vec![0x7F], vec![0x01], vec![0x84]], // 0x84 & 0x7F == 4
            header_pages: 1,
            audio_offset: 0,
        };
        assert_eq!(comment_packet_index(&oggflac), 2);
        // No type-4 block -> 0 (kills the bitmask / == mutations at :130).
        let none = OggHeader {
            codec: Codec::OggFlac,
            serial: 1,
            packets: vec![vec![0x7F], vec![0x01], vec![0x05]],
            header_pages: 1,
            audio_offset: 0,
        };
        assert_eq!(comment_packet_index(&none), 0);
    }

    #[test]
    fn locate_audio_accepts_empty_audio_region() {
        // opus_headers() is header pages only: audio_offset == data.len(). The
        // original `>` yields Ok (audio_length 0); the :196 `==`/`>=` mutants reject.
        let file = opus_headers();
        let scan = locate_audio(&file).unwrap();
        assert_eq!(scan.codec, Codec::Opus);
        assert_eq!(scan.audio_offset, file.len() as u64);
        assert_eq!(scan.audio_length, 0);
    }

    #[test]
    fn picture_prefix_declared_desc_len_pins_padding() {
        let art = crate::input::ArtInput {
            art_id: 1,
            mime: "image/png".into(), // 9
            description: "x".into(),  // 1 -> base = 42, 42 % 3 == 0 -> pad 0
            picture_type: 3,
            width: 1,
            height: 1,
            data_len: 100,
        };
        let prefix = picture_prefix(&art).unwrap();
        assert_eq!(prefix.len() % 3, 0);
        // Declared description length lives at offset 8 + mime.len() (after
        // type[4] + mimelen[4] + mime). pad = declared - desc.len() must be 0..=2.
        let off = 8 + art.mime.len();
        let declared = u32::from_be_bytes(prefix[off..off + 4].try_into().unwrap());
        let pad = declared - u32::try_from(art.description.len()).unwrap();
        assert!(pad <= 2, "pad must be 0..=2, got {pad}");
        assert_eq!(pad, 0, "base % 3 == 0 implies pad 0");
    }
}

#[cfg(test)]
mod bounded_tests {
    use super::*;
    use crate::ogg::page_test_support::{build_header_pub, lace_packet_pub, vorbis_body_empty};

    /// A minimal Opus stream: OpusHead + OpusTags header packets, then a trailing
    /// audio page. Returns (full, audio_offset). Mirrors the proven fixture in
    /// `musefs-core/src/scan.rs::ogg_probe_tests::probe_detects_opus_and_seeds_tags`.
    /// `build_header_pub(serial, &[&[u8]])` laces *all* header packets across
    /// pages (BOS set once) and returns `(Vec<u8>, u32)`; `lace_packet_pub` takes
    /// `(serial, seq_start, bos, granule, packet)` and returns `(Vec<u8>, u32)`.
    fn opus_stream() -> (Vec<u8>, u64) {
        let head = b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".to_vec();
        let mut tags = b"OpusTags".to_vec();
        tags.extend_from_slice(&vorbis_body_empty());
        let serial = 0x1234;
        let (mut v, _) = build_header_pub(serial, &[&head, &tags]);
        let audio_offset = v.len() as u64;
        let (audio, _) = lace_packet_pub(serial, 2, false, 960, &[0u8; 100]);
        v.extend_from_slice(&audio);
        (v, audio_offset)
    }

    #[test]
    fn read_metadata_bounded_complete_when_prefix_covers_header() {
        let (full, audio_offset) = opus_stream();
        let file_len = full.len() as u64;
        let prefix = &full[..crate::convert::usize_from(audio_offset)]; // exactly the header region
        match read_metadata_bounded(prefix, file_len).unwrap() {
            Extent::Complete(h) => assert_eq!(h.audio_offset, audio_offset),
            other @ Extent::NeedMore { .. } => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn read_metadata_bounded_needmore_when_header_truncated() {
        let (full, _audio_offset) = opus_stream();
        let file_len = full.len() as u64;
        let prefix = &full[..20]; // mid first page
        match read_metadata_bounded(prefix, file_len).unwrap() {
            Extent::NeedMore { up_to } => assert!(up_to > 20 && up_to <= file_len),
            other @ Extent::Complete(_) => panic!("expected NeedMore, got {other:?}"),
        }
    }

    #[test]
    fn read_metadata_bounded_errors_when_whole_file_is_unparseable() {
        // A short garbage buffer that IS the whole file: prefix.len() == file_len and
        // read_header errors. The guard `(prefix.len() as u64) < file_len` is FALSE,
        // so the function must fall to the `Err(e)` arm and return Err — never grow.
        let bad: &[u8] = b"not an ogg stream at all"; // capture pattern != "OggS"
                                                      // Confirm the premise: read_header genuinely errors on this buffer.
        assert!(read_header(bad).is_err());
        let len = bad.len() as u64;
        // kills ogg L226 guard `< file_len` -> `true`: under `true` this returns
        // NeedMore; correct is Err.
        // kills ogg L226 `<` -> `<=`: `len <= len` is true -> NeedMore; correct is Err.
        match read_metadata_bounded(bad, len) {
            Err(_) => {}
            Ok(other) => panic!("expected Err when whole file unparseable, got {other:?}"),
        }
    }

    #[test]
    fn read_metadata_bounded_doubles_window_exactly() {
        // L = 100_000 bytes of garbage (read_header errors): L > 64*1024 so `.max`
        // does not mask, and file_len = 10_000_000 > L*2 so `.min` does not clamp.
        // Correct up_to = L*2 = 200_000. `+`->100_002, `/`->50_000 all differ.
        let buf = vec![0u8; 100_000]; // all zeros: capture pattern != "OggS" -> errors
                                      // Confirm the premise: read_header genuinely errors on this buffer.
        assert!(read_header(&buf).is_err());
        let file_len = 10_000_000u64;
        match read_metadata_bounded(&buf, file_len).unwrap() {
            Extent::NeedMore { up_to } => assert_eq!(up_to, 200_000),
            other @ Extent::Complete(_) => panic!("expected NeedMore, got {other:?}"),
        }
    }

    #[test]
    fn read_metadata_bounded_floor_is_64kib_for_small_prefix() {
        // The `*` at L227 col 74 is the `64 * 1024` FLOOR in `.max(64 * 1024)`,
        // not the doubling. To exercise it the floor must bind: a tiny prefix whose
        // doubled length (200) is below 64 KiB, with file_len well above 64 KiB so
        // `.min(file_len)` doesn't clamp. Correct floor = 65_536.
        let buf = vec![0u8; 100]; // garbage: read_header errors
        assert!(read_header(&buf).is_err());
        let file_len = 10_000_000u64;
        // kills ogg L227 `64 * 1024` -> `64 + 1024` (=1088) and `64 / 1024` (=0):
        // only `*` yields the 65_536 floor when the doubled length is smaller.
        match read_metadata_bounded(&buf, file_len).unwrap() {
            Extent::NeedMore { up_to } => assert_eq!(up_to, 65_536),
            other @ Extent::Complete(_) => panic!("expected NeedMore, got {other:?}"),
        }
    }

    #[test]
    fn read_metadata_bounded_grows_when_truncated_prefix_shorter_than_file() {
        // Pins the TRUE side of the guard: a truncated valid-prefix where
        // prefix.len() < file_len must return NeedMore (kills `<`->`<=` from the
        // other direction by requiring growth here while requiring Err when equal).
        let (full, _audio_offset) = opus_stream();
        let file_len = full.len() as u64;
        let prefix = &full[..10]; // far short of the header region
        assert!(read_header(prefix).is_err());
        match read_metadata_bounded(prefix, file_len).unwrap() {
            Extent::NeedMore { up_to } => assert!(up_to > prefix.len() as u64),
            other @ Extent::Complete(_) => panic!("expected NeedMore, got {other:?}"),
        }
    }
}
