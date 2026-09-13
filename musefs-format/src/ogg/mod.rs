mod art_source;
mod b64;
mod crc;
mod page;

pub use art_source::{ArtSource, MapArtSource};

pub use b64::{B64Window, b64_len, b64_len_checked, b64_window, encode_b64_slice};
pub use page::{
    MAX_PAGE_BYTES, PageHeader, parse_page, patch_page_header, patch_page_header_algebraic,
    verify_page_crc,
};

use crate::error::{FormatError, Result};
use crate::probe::Extent;
use crate::size;

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
/// packet 0 — or, when it is zero, that the number is *unknown*. See
/// [`oggflac_header_packets`].
fn oggflac_following_packets(first_packet: &[u8]) -> Result<usize> {
    if first_packet.len() < 9 {
        return Err(FormatError::Malformed);
    }
    Ok(u16::from_be_bytes([first_packet[7], first_packet[8]]) as usize)
}

/// Byte offset of the STREAMINFO block header within the OggFLAC mapping packet:
/// `0x7F "FLAC"` (5) + major + minor (2) + count (2) + `"fLaC"` (4).
const OGGFLAC_STREAMINFO_POS: usize = 13;

/// The last-metadata-block flag, the high bit of a FLAC block header's first byte.
const FLAC_LAST_BLOCK: u8 = 0x80;

/// The reserved-invalid FLAC block type. A native FLAC audio packet opens with the
/// `0xFF` frame sync, whose low seven bits are exactly this value — which is what
/// makes the low bits the discriminator the Ogg mapping names for telling a
/// metadata packet from the first audio packet.
const FLAC_BLOCK_TYPE_INVALID: u8 = 127;

fn is_flac_metadata_block(packet: &[u8]) -> bool {
    packet
        .first()
        .is_some_and(|b| b & 0x7F != FLAC_BLOCK_TYPE_INVALID)
}

fn is_last_flac_metadata_block(packet: &[u8]) -> bool {
    packet.first().is_some_and(|b| b & FLAC_LAST_BLOCK != 0)
}

/// Reassemble exactly `want` packets, treating a short run as malformed.
fn read_exactly(data: &[u8], want: usize) -> Result<Vec<page::ReadPacket>> {
    let pkts = page::read_packets(data, want)?;
    if pkts.len() != want {
        return Err(FormatError::Malformed);
    }
    Ok(pkts)
}

/// Reassemble an OggFLAC header run: the mapping packet plus the metadata-block
/// packets that follow it.
///
/// A nonzero count is taken at its word: the mapping requires a count it gives to
/// be the true number of following packets, and zero is its escape hatch for
/// *unknown* — not a statement that there are none. Metadata packets still follow. Reading it as "none" put a file's real VORBIS_COMMENT (and any
/// PICTURE, SEEKTABLE or CUESHEET) past `audio_offset`, so it was never ingested
/// and was then replayed verbatim inside the synthesized stream, where a decoder
/// meets metadata blocks in place of audio frames (#723).
///
/// So an unknown count discovers the run by the rule the format defines: metadata
/// blocks run until one carries the last-block flag. STREAMINFO is itself a
/// metadata block, so a mapping packet that flags it as the last one ends the run
/// at packet 0.
fn oggflac_header_packets(data: &[u8], mapping: &[u8]) -> Result<Vec<page::ReadPacket>> {
    let declared = oggflac_following_packets(mapping)?;
    if declared > 0 {
        return read_exactly(data, 1 + declared);
    }
    if mapping
        .get(OGGFLAC_STREAMINFO_POS)
        .is_some_and(|b| b & FLAC_LAST_BLOCK != 0)
    {
        return read_exactly(data, 1);
    }
    page::read_packets_while(data, |out| {
        let last = out.last().expect("a packet was just completed");
        if out.len() == 1 {
            return Ok(true); // the mapping packet; its followers are what we seek
        }
        // A run that reaches the audio packet without ever flagging its last block
        // has no discoverable end: malformed, rather than something to guess at.
        // That is also what bounds the walk — along with the data itself, since
        // every page advances the cursor by at least its 27 header bytes and
        // running off the end is an error.
        if !is_flac_metadata_block(&last.data) {
            return Err(FormatError::Malformed);
        }
        Ok(!is_last_flac_metadata_block(&last.data))
    })
}

/// The parsed Ogg header region: codec, serial, the reassembled header packets,
/// the number of header pages, and where audio begins.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
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

    let pkts = match codec {
        Codec::Opus => read_exactly(data, 2)?,
        Codec::Vorbis => read_exactly(data, 3)?,
        Codec::OggFlac => oggflac_header_packets(data, &first_pkt.data)?,
    };
    let last = pkts.last().ok_or(FormatError::Malformed)?;
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
    let mut tags = crate::vorbiscomment::parse(body)?;
    // Cover art rides in the comment as a base64 METADATA_BLOCK_PICTURE entry, but
    // it has its own channel (read_pictures). Excluding it keeps read_tags
    // text-only and prevents the art being stored — and re-synthesized — twice.
    tags.retain(|(field, _)| !field.eq_ignore_ascii_case("METADATA_BLOCK_PICTURE"));
    Ok(tags)
}

use crate::input::EmbeddedPicture;

/// Extract embedded pictures from a complete file for scan-time ingestion.
///
/// Opus/Vorbis carry art as a base64 `METADATA_BLOCK_PICTURE` comment whose decoded
/// bytes are a FLAC PICTURE block body; OggFLAC carries native PICTURE block
/// packets (block type 6). Plan 1 only *reads* art (to seed the DB); synthesis does
/// not yet re-embed it.
pub fn read_pictures(data: &[u8]) -> Result<Vec<EmbeddedPicture>> {
    Ok(read_pictures_reporting(data)?.0)
}

/// An embedded picture the reader skipped because it could not be decoded, so
/// the caller can log the lossy drop (the format layer has no logging facade).
/// Carries only a reason and the skipped value's encoded size — never the bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PictureDrop {
    /// Why the picture was skipped.
    pub reason: &'static str,
    /// Size of the skipped value as it appeared in the file, in bytes.
    pub bytes: usize,
}

/// Like [`read_pictures`], but also returns the pictures skipped as undecodable.
///
/// A bad picture skips *that* picture and nothing else: returning early would let
/// one unparseable `METADATA_BLOCK_PICTURE` discard every valid picture in the
/// same file, and the scan path swallows the error, so the loss would be silent
/// (#673). Only a malformed *container* (a bad header or comment packet) still
/// errors, since then there is no list of pictures to salvage.
pub fn read_pictures_reporting(data: &[u8]) -> Result<(Vec<EmbeddedPicture>, Vec<PictureDrop>)> {
    let header = read_header(data)?;
    let mut out = Vec::new();
    let mut dropped = Vec::new();
    match header.codec {
        Codec::Opus | Codec::Vorbis => {
            let idx = comment_packet_index(&header);
            if idx == 0 {
                return Ok((out, dropped));
            }
            let body = comment_body(header.codec, &header.packets[idx])?;
            for (field, value) in crate::vorbiscomment::parse(body)? {
                if !field.eq_ignore_ascii_case("METADATA_BLOCK_PICTURE") {
                    continue;
                }
                let Some(raw) = decode_picture_base64(&value) else {
                    dropped.push(PictureDrop {
                        reason: "undecodable base64",
                        bytes: value.len(),
                    });
                    continue;
                };
                match crate::flac::parse_picture_block(&raw) {
                    Ok(pic) => out.push(pic),
                    Err(_) => dropped.push(PictureDrop {
                        reason: "malformed PICTURE block",
                        bytes: raw.len(),
                    }),
                }
            }
        }
        Codec::OggFlac => {
            for pkt in header.packets.iter().skip(1) {
                // An empty packet carries no block type, so it is not a picture at
                // all; `is_empty` also guards the `pkt[0]` index below.
                if pkt.is_empty() || (pkt[0] & 0x7F) != 6 {
                    continue;
                }
                // The packet length is attacker-controlled, so a 1-3 byte type-6
                // packet must not reach the `&pkt[4..]` slice (#365). It is a
                // truncated block header rather than a packet to ignore, so it is
                // reported like any other undecodable picture.
                let Some(body) = pkt.get(4..) else {
                    dropped.push(PictureDrop {
                        reason: "truncated PICTURE block header",
                        bytes: pkt.len(),
                    });
                    continue;
                };
                match crate::flac::parse_picture_block(body) {
                    Ok(pic) => out.push(pic),
                    Err(_) => dropped.push(PictureDrop {
                        reason: "malformed PICTURE block",
                        bytes: body.len(),
                    }),
                }
            }
        }
    }
    Ok((out, dropped))
}

/// Base64-decode a `METADATA_BLOCK_PICTURE` value, tolerating ASCII whitespace.
///
/// Vorbis comment values are length-prefixed, so the 76-column wrapping of the
/// older MIME style is unnecessary and is not what the Xiph recommendation
/// describes — but the strict engine rejects a wrapped value outright at the
/// first line break, costing the whole picture. Filtering is leniency, not
/// conformance, so it allocates a stripped copy only when whitespace is actually
/// present; the overwhelmingly common unwrapped value decodes in place (#673).
fn decode_picture_base64(value: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    let engine = base64::engine::general_purpose::STANDARD;
    if value.bytes().any(|b| b.is_ascii_whitespace()) {
        let stripped: Vec<u8> = value.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
        engine.decode(&stripped).ok()
    } else {
        engine.decode(value.as_bytes()).ok()
    }
}

/// Audio bounds + codec from a complete file, for the scanner.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct OggScan {
    pub codec: Codec,
    pub audio_offset: u64,
    pub audio_length: u64,
}

/// What a file's final page says about how many logical bitstreams it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chaining {
    /// The final page belongs to the header's bitstream: one stream, start to end.
    Single,
    /// The final page belongs to a different bitstream: the file is chained.
    Chained,
    /// No page ended at the file's last byte within the window given — a truncated
    /// file, or trailing bytes that are not a page. Nothing is proven either way.
    Unknown,
}

/// Classify a file by the serial of its final page, where `tail` is its last
/// `tail.len()` bytes starting at absolute offset `tail_start`.
///
/// `validate_single_bitstream` can only prove things about the *header* region,
/// which is where multiplexing shows up. A chain is complete logical bitstreams
/// concatenated end to end (RFC 3533), so the second stream's pages necessarily
/// begin after the first stream's audio does — inside what the scanner otherwise
/// declares to be one audio region, where the serve path would renumber them as
/// though they belonged to the first stream (#722).
///
/// The final page is the cheap discriminator: a chain's last page belongs to its
/// last stream, so a serial other than the header's proves chaining for the whole
/// well-formed-chain class at the cost of one page-sized read. A *missing* page
/// end at EOF proves nothing — that is what a truncated file looks like, and those
/// still serve — so it reports [`Chaining::Unknown`] rather than a rejection.
///
/// A `tail` at least [`MAX_PAGE_BYTES`] long (or covering the whole file) always
/// contains the final page whole, so the CRC below is always checkable.
pub fn classify_tail(tail: &[u8], tail_start: u64, serial: u32) -> Chaining {
    let file_end = tail_start + tail.len() as u64;
    // Backwards, like the serve path's page location: the final page is the one
    // whose declared length lands exactly on the last byte, and its CRC is what
    // separates it from a coincidental `OggS` in audio payload.
    let mut i = tail.len().saturating_sub(4);
    let mut crc_checks = 0usize;
    loop {
        if tail[i..].starts_with(page::CAPTURE)
            && let Ok(h) = page::parse_page(tail, i)
            && tail_start + (i + h.total_len()) as u64 == file_end
        {
            // Spend from the budget before the CRC, which walks the whole page: a
            // crafted tail can plant a length-to-EOF candidate at every offset,
            // and without a bound one file would cost ~65,000 whole-page CRCs
            // (the amplification #619 bounded on the serve path's own scan). A
            // real file resolves on its first candidate. Past the budget nothing
            // is proven, which is what `Unknown` already means.
            if crc_checks == MAX_TAIL_CRC_CHECKS {
                return Chaining::Unknown;
            }
            crc_checks += 1;
            if page::verify_page_crc(&tail[i..]).unwrap_or(false) {
                return if h.serial == serial {
                    Chaining::Single
                } else {
                    Chaining::Chained
                };
            }
        }
        if i == 0 {
            return Chaining::Unknown;
        }
        i -= 1;
    }
}

/// Whole-page CRC validations one [`classify_tail`] scan may pay. Mirrors the
/// serve path's `MAX_CRC_CHECKS`: orders of magnitude more headroom than a real
/// file needs, and a fixed small multiple of the legitimate cost.
const MAX_TAIL_CRC_CHECKS: usize = 64;

/// [`classify_tail`] over a whole-file buffer, taking its own tail window.
fn classify_whole(data: &[u8], serial: u32) -> Chaining {
    let want = crate::convert::usize_from((data.len() as u64).min(MAX_PAGE_BYTES));
    let start = data.len() - want;
    classify_tail(&data[start..], start as u64, serial)
}

pub fn locate_audio(data: &[u8]) -> Result<OggScan> {
    let header = read_header(data)?;
    if header.audio_offset > data.len() as u64 {
        return Err(FormatError::Malformed);
    }
    if classify_whole(data, header.serial) == Chaining::Chained {
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

pub fn synthesize_layout(
    header: &OggHeader,
    audio_offset: u64,
    audio_length: u64,
    tags: &[TagInput],
    arts: &[OggArt],
    src: &dyn ArtSource,
) -> Result<RegionLayout> {
    let arts: Vec<OggArt> = arts.to_vec();
    let packet_chunks = build_packets_with_art(header, tags, &arts)?;
    let mut segments: Vec<Segment> = Vec::new();
    let mut seq = 0u32;
    for (i, chunks) in packet_chunks.iter().enumerate() {
        let (segs, used) =
            crate::ogg::page::lace_chunks_to_segments(header.serial, seq, i == 0, chunks, src)?;
        segments.extend(segs);
        seq += used;
    }
    let seq_delta = i64::from(seq) - i64::from(header.header_pages);
    segments.push(Segment::OggAudio {
        offset: audio_offset,
        len: audio_length,
        seq_delta,
        serial: header.serial,
    });
    Ok(RegionLayout::validated(segments)?)
}

/// Build the FLAC PICTURE block *body prefix* (everything before the image data:
/// type, mime, description, dimensions, depth, colors, data-length) for `art`,
/// padding the description with spaces so the prefix length is a multiple of 3.
/// This makes `base64(prefix ++ image) == base64(prefix) ++ base64(image)`, so the
/// image's base64 is an independent substring that can be served incrementally.
/// The declared data-length field is the true image length (`art.data_len`).
///
/// The actual byte layout is shared with the plain FLAC write path via
/// [`crate::flac::picture_body_framing`]; only the description padding is unique here.
fn picture_prefix(art: &crate::input::ArtInput) -> Result<Vec<u8>> {
    // Unpadded prefix length = 4(type)+4(mimelen)+mime +4(desclen)+desc
    //   +4(w)+4(h)+4(depth)+4(colors)+4(datalen) = 32 + mime + desc.
    let base = 32 + art.mime.len() + art.description.len();
    let pad = (3 - base % 3) % 3;
    let description = format!("{}{}", art.description, " ".repeat(pad));
    crate::flac::picture_body_framing(art, &description)
}

use crate::ogg::page::PayloadChunk;
use base64::Engine;

/// One image to embed: its metadata. Bytes are read from an `ArtSource` only to
/// compute page CRCs at synthesis time; they are never retained in the layout.
#[derive(Clone, Copy)]
pub struct OggArt<'a> {
    pub meta: &'a crate::input::ArtInput,
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
                let b64_prefix_len =
                    b64_len_checked(prefix.len() as u64).ok_or(FormatError::TooLarge)?;
                let b64_image_len =
                    b64_len_checked(a.meta.data_len.get()).ok_or(FormatError::TooLarge)?;
                let value_len = size::checked_sum([
                    METADATA_BLOCK_PICTURE_KEY.len() as u64,
                    b64_prefix_len,
                    b64_image_len,
                ])?;
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
        let b64_image_len =
            b64_len_checked(art.meta.data_len.get()).ok_or(FormatError::TooLarge)?;
        let value_len = size::checked_sum([
            METADATA_BLOCK_PICTURE_KEY.len() as u64,
            b64_prefix.len() as u64,
            b64_image_len,
        ])?;
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
            base64: true,
            art_total: art.meta.data_len.get(),
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
        let body_len = size::checked_add(prefix.len() as u64, art.meta.data_len.get())?;
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
                base64: false,
                art_total: art.meta.data_len.get(),
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
    // Metadata blocks always follow this packet — the regenerated comment block at
    // minimum — so STREAMINFO is never the last one. A source whose count was
    // unknown may have flagged it as last; leaving that flag set would terminate
    // the metadata run before the comment block a decoder is about to meet.
    if let Some(b) = mapping.get_mut(OGGFLAC_STREAMINFO_POS) {
        *b &= !FLAC_LAST_BLOCK;
    }

    let mut out = vec![vec![PayloadChunk::Bytes(mapping)]];
    out.extend(block_packets);
    Ok(out)
}

/// Page and comment-body builders for fixtures, used by `fuzz_check` and by
/// musefs-core's tests. Behind `fuzzing` with the rest of the test surface, so
/// none of it is published API.
#[cfg(any(test, feature = "fuzzing"))]
pub mod page_test_support {
    pub use crate::ogg::page::{build_header as build_header_pub, lace_packet as lace_packet_pub};

    /// An empty VorbisComment body (vendor + zero comments), for fixtures.
    pub fn vorbis_body_empty() -> Vec<u8> {
        crate::vorbiscomment::build(&[]).unwrap()
    }

    /// A VorbisComment body carrying `comments` in order, for fixtures that need
    /// a specific field — an embedded `METADATA_BLOCK_PICTURE` above all.
    /// Keys are normalized to lowercase on the way in, as they are for any tag
    /// musefs writes; Vorbis field names are case-insensitive by spec and the
    /// picture reader matches accordingly. Panics on a key the format rejects,
    /// which in a fixture is a test bug.
    pub fn vorbis_body_with(comments: &[(&str, &str)]) -> Vec<u8> {
        let inputs: Vec<crate::input::TagInput> = comments
            .iter()
            .map(|(k, v)| crate::input::TagInput::new(k, v))
            .collect();
        crate::vorbiscomment::build(&inputs).unwrap()
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
    fn oggflac_following_packets_accepts_minimal_9_byte_packet() {
        // The 16-bit count lives in bytes [7],[8], so a 9-byte first packet is the
        // minimum valid input (`len < 9` rejects anything shorter). `<=` would
        // wrongly reject this exact-length packet.
        let mut pkt = [0u8; 9];
        pkt[7] = 0x00;
        pkt[8] = 0x03; // 3 following metadata-block packets
        assert_eq!(oggflac_following_packets(&pkt).unwrap(), 3);
    }

    #[test]
    fn comment_body_accepts_packet_with_empty_body() {
        // A packet exactly `prefix` bytes long has an empty (but valid) comment
        // body: `&packet[prefix..]` is the empty slice. `len < prefix` rejects only
        // shorter packets; `<=` would wrongly reject this one. Opus prefix = 8.
        assert!(comment_body(Codec::Opus, b"OpusTags").unwrap().is_empty());
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
    fn read_tags_excludes_metadata_block_picture() {
        // A METADATA_BLOCK_PICTURE comment whose value is a base64 FLAC picture
        // block carrying a 1-byte image, plus one ordinary text tag.
        let mut block = Vec::new();
        block.extend_from_slice(&3u32.to_be_bytes()); // picture type: front cover
        block.extend_from_slice(&9u32.to_be_bytes());
        block.extend_from_slice(b"image/png");
        block.extend_from_slice(&0u32.to_be_bytes()); // description length
        block.extend_from_slice(&1u32.to_be_bytes()); // width
        block.extend_from_slice(&1u32.to_be_bytes()); // height
        block.extend_from_slice(&8u32.to_be_bytes()); // depth
        block.extend_from_slice(&0u32.to_be_bytes()); // colors used
        block.extend_from_slice(&1u32.to_be_bytes()); // image length
        block.push(0xAB);
        let pic_value = base64::engine::general_purpose::STANDARD.encode(&block);

        let body = crate::vorbiscomment::build(&[
            crate::input::TagInput::new("title", "Sun"),
            crate::input::TagInput::new("METADATA_BLOCK_PICTURE", &pic_value),
        ])
        .unwrap();
        let mut tags_pkt = b"OpusTags".to_vec();
        tags_pkt.extend_from_slice(&body);
        let head = b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".to_vec();
        let (mut data, _) = crate::ogg::page::build_header(7, &[&head, &tags_pkt]);
        let (audio, _) = crate::ogg::page::lace_packet(7, 2, false, 960, &[0u8; 50]);
        data.extend_from_slice(&audio);

        // read_tags returns only the text tag — the picture comment is excluded...
        let tags = read_tags(&data).unwrap();
        assert_eq!(tags, vec![("title".to_string(), "Sun".to_string())]);
        // ...while read_pictures still finds the embedded art.
        let pics = read_pictures(&data).unwrap();
        assert_eq!(pics.len(), 1);
        assert_eq!(pics[0].data, vec![0xAB]);
    }

    /// A FLAC PICTURE block body (the payload a `METADATA_BLOCK_PICTURE` value
    /// base64-encodes) carrying a PNG of `len` bytes, every one of them `marker`.
    fn picture_block_n(marker: u8, len: usize) -> Vec<u8> {
        let mut block = Vec::new();
        block.extend_from_slice(&3u32.to_be_bytes()); // picture type: front cover
        block.extend_from_slice(&9u32.to_be_bytes());
        block.extend_from_slice(b"image/png");
        block.extend_from_slice(&0u32.to_be_bytes()); // description length
        block.extend_from_slice(&1u32.to_be_bytes()); // width
        block.extend_from_slice(&1u32.to_be_bytes()); // height
        block.extend_from_slice(&8u32.to_be_bytes()); // depth
        block.extend_from_slice(&0u32.to_be_bytes()); // colors used
        block.extend_from_slice(&u32::try_from(len).unwrap().to_be_bytes());
        block.extend(std::iter::repeat_n(marker, len));
        block
    }

    /// [`picture_block_n`] with a single image byte.
    fn picture_block(marker: u8) -> Vec<u8> {
        picture_block_n(marker, 1)
    }

    /// A complete Opus file whose comment packet carries `comments`, plus one
    /// audio page so `audio_offset` lands before EOF.
    fn opus_with_comments(comments: &[(&str, &str)]) -> Vec<u8> {
        let inputs: Vec<crate::input::TagInput> = comments
            .iter()
            .map(|(k, v)| crate::input::TagInput::new(k, v))
            .collect();
        let mut tags_pkt = b"OpusTags".to_vec();
        tags_pkt.extend_from_slice(&crate::vorbiscomment::build(&inputs).unwrap());
        let head = b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".to_vec();
        let (mut data, _) = build_header(7, &[&head, &tags_pkt]);
        let (audio, _) = lace_packet(7, 2, false, 960, &[0u8; 50]);
        data.extend_from_slice(&audio);
        data
    }

    /// Wrap `s` at 76 columns with CRLF, the older MIME base64 style.
    fn wrap76(s: &str) -> String {
        s.as_bytes()
            .chunks(76)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect::<Vec<_>>()
            .join("\r\n")
    }

    #[test]
    fn read_pictures_accepts_whitespace_wrapped_base64() {
        // Vorbis comment values are length-prefixed so wrapping is unnecessary,
        // but the strict engine used to fail at the first line break and discard
        // the picture (#673). A wrapped value must decode to the same bytes.
        // 200 image bytes so the encoded value is comfortably past 76 columns.
        let value = base64::engine::general_purpose::STANDARD.encode(picture_block_n(0xAB, 200));
        let wrapped = wrap76(&value);
        assert!(wrapped.contains("\r\n"), "test value must actually wrap");

        let data = opus_with_comments(&[("METADATA_BLOCK_PICTURE", &wrapped)]);
        let (pics, dropped) = read_pictures_reporting(&data).unwrap();
        assert_eq!(dropped, vec![]);
        assert_eq!(pics.len(), 1);
        assert_eq!(pics[0].data, vec![0xAB; 200]);
    }

    #[test]
    fn read_pictures_skips_one_bad_value_and_keeps_the_others() {
        // One unparseable picture used to abandon the loop, discarding every
        // valid picture in the same file — and the scan path swallowed the
        // error, so the loss was silent (#673).
        let good_a = base64::engine::general_purpose::STANDARD.encode(picture_block(0xAA));
        let good_b = base64::engine::general_purpose::STANDARD.encode(picture_block(0xBB));
        let data = opus_with_comments(&[
            ("METADATA_BLOCK_PICTURE", "not!valid!base64"),
            ("METADATA_BLOCK_PICTURE", &good_a),
            ("METADATA_BLOCK_PICTURE", &good_b),
        ]);

        let (pics, dropped) = read_pictures_reporting(&data).unwrap();
        assert_eq!(pics.len(), 2);
        assert_eq!(pics[0].data, vec![0xAA]);
        assert_eq!(pics[1].data, vec![0xBB]);
        assert_eq!(
            dropped,
            vec![PictureDrop {
                reason: "undecodable base64",
                bytes: "not!valid!base64".len(),
            }]
        );
    }

    #[test]
    fn read_pictures_reports_a_decodable_but_malformed_picture_block() {
        // Valid base64 whose decoded bytes are a truncated PICTURE block: the
        // drop is attributed to the block, not to the base64.
        let truncated = base64::engine::general_purpose::STANDARD.encode([0u8; 3]);
        let good = base64::engine::general_purpose::STANDARD.encode(picture_block(0xCD));
        let data = opus_with_comments(&[
            ("METADATA_BLOCK_PICTURE", &truncated),
            ("METADATA_BLOCK_PICTURE", &good),
        ]);

        let (pics, dropped) = read_pictures_reporting(&data).unwrap();
        assert_eq!(pics.len(), 1);
        assert_eq!(pics[0].data, vec![0xCD]);
        assert_eq!(
            dropped,
            vec![PictureDrop {
                reason: "malformed PICTURE block",
                bytes: 3,
            }]
        );
    }

    #[test]
    fn read_pictures_ignores_oggflac_packets_that_are_not_pictures() {
        // OggFLAC's following packets are metadata blocks of every type, not just
        // PICTURE. A non-type-6 block must be passed over silently — neither
        // parsed as art nor reported as a drop.
        let mut mapping = vec![0x7F];
        mapping.extend_from_slice(b"FLAC");
        mapping.push(1);
        mapping.push(0);
        mapping.extend_from_slice(&1u16.to_be_bytes()); // one following packet
        mapping.extend_from_slice(b"fLaC");
        let mut streaminfo = Vec::new();
        crate::flac::push_block_header(&mut streaminfo, 0, 34, false).unwrap();
        streaminfo.extend(std::iter::repeat_n(0u8, 34));
        mapping.extend_from_slice(&streaminfo);

        // A VORBIS_COMMENT block (type 4), long enough to survive the 4-byte
        // header slice if the type check were to let it through.
        let mut comment = Vec::new();
        crate::flac::push_block_header(&mut comment, 4, 8, true).unwrap();
        comment.extend(std::iter::repeat_n(0u8, 8));

        let (data, _) = build_header(78, &[&mapping, &comment]);
        assert_eq!(read_header(&data).unwrap().codec, Codec::OggFlac);

        let (pics, dropped) = read_pictures_reporting(&data).unwrap();
        assert!(pics.is_empty());
        assert_eq!(
            dropped,
            vec![],
            "a non-picture block is not a dropped picture"
        );
    }

    #[test]
    fn read_pictures_still_errors_on_a_malformed_container() {
        // Per-picture leniency must not extend to the container: with no parsable
        // header there is no list of pictures to salvage.
        assert!(read_pictures_reporting(b"not an ogg stream").is_err());
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
            &MapArtSource::default(),
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

    #[test]
    fn synthesize_emits_nonzero_seq_delta_when_header_page_count_changes() {
        // seq_delta = synthesized_page_count - original_header_pages. When the
        // original header spanned a different number of pages than the regenerated
        // one, the delta is non-zero — pins the subtraction at the OggAudio segment.
        let mut data = opus_headers();
        let (audio, _) = crate::ogg::page::lace_packet(0x1234, 2, false, 960, &[0u8; 80]);
        data.extend_from_slice(&audio);
        let scan = locate_audio(&data).unwrap();
        let mut header =
            read_metadata(&data[..crate::convert::usize_from(scan.audio_offset)]).unwrap();

        // Synthesis re-lays the Opus header into a known page count; record it.
        let baseline = synthesize_layout(
            &header,
            scan.audio_offset,
            scan.audio_length,
            &[],
            &[],
            &MapArtSource::default(),
        )
        .unwrap();
        let synth_pages = baseline
            .segments()
            .iter()
            .filter(|s| matches!(s, Segment::Inline(_)))
            .count();
        assert!(synth_pages >= 1);

        // Pretend the ORIGINAL header spanned three extra pages, so the served audio
        // pages must be renumbered downward by exactly three.
        let original_pages = u32::try_from(synth_pages).unwrap() + 3;
        header.header_pages = original_pages;
        let layout = synthesize_layout(
            &header,
            scan.audio_offset,
            scan.audio_length,
            &[],
            &[],
            &MapArtSource::default(),
        )
        .unwrap();
        let delta = layout
            .segments()
            .iter()
            .find_map(|s| match s {
                Segment::OggAudio { seq_delta, .. } => Some(*seq_delta),
                _ => None,
            })
            .expect("expected an OggAudio segment");
        assert_eq!(
            delta,
            i64::try_from(synth_pages).unwrap() - i64::from(original_pages),
            "seq_delta must be synthesized pages minus original header pages"
        );
        assert_eq!(delta, -3);
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
            &MapArtSource::default(),
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

    #[test]
    fn read_pictures_oggflac_short_picture_packet_does_not_panic() {
        // Crafted OggFLAC: a structurally-valid mapping header declaring one
        // following packet, where that packet is a 1-byte type-6 (PICTURE) block —
        // too short for the 4-byte FLAC metadata block header. The `&pkt[4..]`
        // slice must not panic (issue #365).
        let mut mapping = vec![0x7F];
        mapping.extend_from_slice(b"FLAC");
        mapping.push(1);
        mapping.push(0);
        mapping.extend_from_slice(&1u16.to_be_bytes()); // one following packet
        mapping.extend_from_slice(b"fLaC");
        let mut streaminfo = Vec::new();
        crate::flac::push_block_header(&mut streaminfo, 0, 34, false).unwrap();
        streaminfo.extend(std::iter::repeat_n(0u8, 34));
        mapping.extend_from_slice(&streaminfo);

        // One lacing byte yields this 1-byte packet: block type 6, no body.
        let short_picture = vec![0x06u8];

        let (data, _) = crate::ogg::page::build_header(77, &[&mapping, &short_picture]);

        // Sanity: the header parses, so we actually reach the picture loop.
        assert_eq!(read_header(&data).unwrap().codec, Codec::OggFlac);
        let (pics, dropped) = read_pictures_reporting(&data).unwrap();
        assert!(pics.is_empty());
        // Too short to be a picture, but it claimed to be one: report the drop
        // rather than passing over it silently (#673).
        assert_eq!(
            dropped,
            vec![PictureDrop {
                reason: "truncated PICTURE block header",
                bytes: 1,
            }]
        );
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

    /// Stream A (the `opus_headers` bitstream, serial 0x1234) complete, then a
    /// whole second logical bitstream appended under its own serial — a chain, in
    /// the shape RFC 3533 defines. Returns the file and stream A's length.
    fn chained_opus() -> (Vec<u8>, usize) {
        let mut data = opus_headers();
        let (audio, _) = lace_packet(0x1234, 2, false, 960, &[0u8; 120]);
        data.extend_from_slice(&audio);
        let stream_a_len = data.len();

        let head = b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".to_vec();
        let tags = b"OpusTags\x06\x00\x00\x00musefs\x00\x00\x00\x00".to_vec();
        let (b_header, b_pages) = build_header(0x5678, &[&head, &tags]);
        data.extend_from_slice(&b_header);
        let (b_audio, _) = lace_packet(0x5678, b_pages, false, 960, &[1u8; 120]);
        data.extend_from_slice(&b_audio);
        (data, stream_a_len)
    }

    #[test]
    fn rejects_chained_second_bitstream() {
        let (data, stream_a_len) = chained_opus();
        // The header region alone still looks like one clean bitstream — which is
        // exactly the gap the final-page check closes: a chain's second stream
        // begins after the first one's audio does (#722).
        assert!(read_header(&data).is_ok());
        assert_eq!(classify_whole(&data, 0x1234), Chaining::Chained);
        assert!(locate_audio(&data).is_err());
        // Stream A on its own is unaffected.
        assert!(locate_audio(&data[..stream_a_len]).is_ok());
    }

    #[test]
    fn classify_tail_works_from_a_window_anchored_mid_file() {
        // The scanner passes only the file's last page-sized window, not the file.
        let (data, _) = chained_opus();
        let start = data.len() - 200;
        assert_eq!(
            classify_tail(&data[start..], start as u64, 0x1234),
            Chaining::Chained
        );
        // A window that does not reach the final page's start proves nothing —
        // which is why the scanner reads a whole page's worth.
        let short = data.len() - 64;
        assert_eq!(
            classify_tail(&data[short..], short as u64, 0x1234),
            Chaining::Unknown
        );
    }

    #[test]
    fn classify_tail_rejects_ogg_captures_that_are_not_the_final_page() {
        // A tail densely packed with `OggS` must not be read as a page end. Every
        // candidate here parses as a zero-segment page, so only the "declared
        // length lands exactly on the last byte" filter and the CRC separate them
        // from the real thing — and neither passes.
        let mut tail = vec![0u8; 4096];
        for i in 0..tail.len() - 27 {
            tail[i..i + 4].copy_from_slice(page::CAPTURE);
        }
        assert_eq!(classify_tail(&tail, 0, 0x1234), Chaining::Unknown);
    }

    #[test]
    fn classify_whole_covers_a_file_that_is_a_single_page() {
        // The window is the file's LAST `MAX_PAGE_BYTES` bytes, so for a file
        // shorter than that it has to start at zero. A window anchored one byte
        // in would miss a page that begins at the very start of the file.
        let (page, _) = lace_packet(0x1234, 0, true, 0, &[7u8; 40]);
        assert_eq!(classify_whole(&page, 0x1234), Chaining::Single);
        assert_eq!(classify_whole(&page, 0x5678), Chaining::Chained);
    }

    /// One valid page of serial 0x5678 whose payload hides `decoys` fake page
    /// headers, each declaring a length that lands exactly on EOF so only the CRC
    /// can reject it. The backward scan meets every decoy before the real page.
    fn tail_with_decoys(decoys: usize) -> Vec<u8> {
        let stride = 300usize; // > one decoy's 282-byte maximum header extent
        let payload = vec![0u8; stride * (decoys + 1)];
        let (mut page, _) = lace_packet(0x5678, 9, false, 0, &payload);
        let h = parse_page(&page, 0).unwrap();
        let len = page.len();

        for k in 1..=decoys {
            let i = len - k * stride;
            assert!(
                i >= h.header_len,
                "decoys must not overwrite the real header"
            );
            page[i..i + 4].copy_from_slice(page::CAPTURE);
            page[i + 4] = 0; // version
            page[i + 5] = 0; // header_type
            // total_len = 27 + seg_count + sum(lacing) must land on EOF.
            page[i + 26] = 255;
            let mut left = len - i - 27 - 255;
            for seg in 0..255usize {
                let v = left.min(255);
                page[i + 27 + seg] = u8::try_from(v).expect("v <= 255");
                left -= v;
            }
            assert_eq!(left, 0, "decoy payload must fit one page's lacing table");
            assert_eq!(page::parse_page(&page, i).unwrap().total_len(), len - i);
        }
        // The real page's CRC covers the decoys, so recompute it over them.
        let patched = patch_page_header(&page, h.seq).unwrap();
        page[..h.header_len].copy_from_slice(&patched);
        assert!(page::verify_page_crc(&page).unwrap());
        page
    }

    #[test]
    fn classify_tail_stops_after_a_bounded_number_of_crc_checks() {
        // Decoys are what a crafted tail costs: without a bound, one file would
        // pay a whole-page CRC per candidate — the amplification #619 bounded on
        // the serve path's own backward scan. Just inside the budget the real
        // page is still found; one decoy more and detection is given up rather
        // than amplified. The serve path's serial check is what still fails such
        // a file closed, which is why giving up here is `Unknown`, not an error.
        let within = tail_with_decoys(MAX_TAIL_CRC_CHECKS - 1);
        assert_eq!(classify_tail(&within, 0, 0x1234), Chaining::Chained);
        assert_eq!(classify_tail(&within, 0, 0x5678), Chaining::Single);

        let past = tail_with_decoys(MAX_TAIL_CRC_CHECKS);
        assert_eq!(classify_tail(&past, 0, 0x1234), Chaining::Unknown);
    }

    #[test]
    fn truncated_stream_is_not_mistaken_for_a_chain() {
        // A torn final page proves nothing about chaining, and a truncated file
        // still serves — so it must stay accepted.
        let mut data = opus_headers();
        let (audio, _) = lace_packet(0x1234, 2, false, 960, &[0u8; 120]);
        data.extend_from_slice(&audio);
        data.truncate(data.len() - 7);
        assert_eq!(classify_whole(&data, 0x1234), Chaining::Unknown);
        assert!(locate_audio(&data).is_ok());
    }

    #[test]
    fn a_whole_single_stream_classifies_as_single() {
        let mut data = opus_headers();
        let (audio, _) = lace_packet(0x1234, 2, false, 960, &[0u8; 120]);
        data.extend_from_slice(&audio);
        assert_eq!(classify_whole(&data, 0x1234), Chaining::Single);
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
            &MapArtSource::default(),
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

    /// Build an OggFLAC file: a mapping packet declaring `declared` following
    /// packets with STREAMINFO flagged `streaminfo_last`, then `blocks` as one
    /// metadata packet each, then one audio page. Returns the file and the byte
    /// length of its header region.
    fn oggflac_file(declared: u16, streaminfo_last: bool, blocks: &[Vec<u8>]) -> (Vec<u8>, usize) {
        let mut streaminfo = Vec::new();
        crate::flac::push_block_header(&mut streaminfo, 0, 34, streaminfo_last).unwrap();
        streaminfo.extend(std::iter::repeat_n(0u8, 34));

        let mut mapping = vec![0x7F];
        mapping.extend_from_slice(b"FLAC");
        mapping.push(1);
        mapping.push(0);
        mapping.extend_from_slice(&declared.to_be_bytes());
        mapping.extend_from_slice(b"fLaC");
        mapping.extend_from_slice(&streaminfo);

        let mut packets: Vec<&[u8]> = vec![&mapping];
        packets.extend(blocks.iter().map(Vec::as_slice));
        let (header, pages) = crate::ogg::page::build_header(77, &packets);
        let header_len = header.len();

        let mut data = header;
        // A native FLAC audio frame opens with the 0xFF sync, which is what tells
        // the discovery walk it has left the metadata run.
        let (audio, _) =
            crate::ogg::page::lace_packet(77, pages, false, 4096, &[0xFFu8, 0xF8, 0x69, 0x18]);
        data.extend_from_slice(&audio);
        (data, header_len)
    }

    fn vorbis_comment_block(last: bool, title: &str) -> Vec<u8> {
        let body =
            crate::vorbiscomment::build(&[crate::input::TagInput::new("title", title)]).unwrap();
        let mut blk = Vec::new();
        crate::flac::push_block_header(&mut blk, 4, body.len(), last).unwrap();
        blk.extend_from_slice(&body);
        blk
    }

    #[test]
    fn oggflac_unknown_count_discovers_the_header_run() {
        // A count of 0 means "unknown", not "none": the VORBIS_COMMENT still
        // follows, and reading the count literally left it inside the audio
        // region, un-ingested and replayed by synthesis (#723).
        let blocks = vec![vorbis_comment_block(true, "RealTitle")];
        let (unknown, header_len) = oggflac_file(0, false, &blocks);
        let (declared, _) = oggflac_file(1, false, &blocks);

        let h = read_header(&unknown).unwrap();
        assert_eq!(h.codec, Codec::OggFlac);
        assert_eq!(h.packets.len(), 2, "mapping packet + VORBIS_COMMENT");
        assert_eq!(h.audio_offset, header_len as u64);
        assert_eq!(
            read_tags(&unknown).unwrap(),
            vec![("title".to_string(), "RealTitle".to_string())]
        );
        // An honest count and an unknown one describe the same file — the mapping
        // packet's count byte is the only difference between the two.
        let d = read_header(&declared).unwrap();
        assert_eq!(h.packets[1..], d.packets[1..]);
        assert_eq!(
            (h.header_pages, h.audio_offset),
            (d.header_pages, d.audio_offset)
        );
        assert_eq!(
            locate_audio(&unknown).unwrap(),
            locate_audio(&declared).unwrap()
        );
    }

    #[test]
    fn oggflac_unknown_count_walks_past_blocks_that_are_not_last() {
        // Discovery must continue through every block whose last-block flag is
        // clear and stop at the one that sets it — a run of exactly one block
        // would not tell the two conditions apart.
        let seektable = {
            let mut b = Vec::new();
            crate::flac::push_block_header(&mut b, 3, 18, false).unwrap();
            b.extend(std::iter::repeat_n(0xEEu8, 18));
            b
        };
        let blocks = vec![seektable, vorbis_comment_block(true, "RealTitle")];
        let (data, header_len) = oggflac_file(0, false, &blocks);

        let h = read_header(&data).unwrap();
        assert_eq!(h.packets.len(), 3, "mapping + SEEKTABLE + VORBIS_COMMENT");
        assert_eq!(h.audio_offset, header_len as u64);
        assert_eq!(
            read_tags(&data).unwrap(),
            vec![("title".to_string(), "RealTitle".to_string())]
        );
    }

    #[test]
    fn oggflac_unknown_count_ends_at_a_streaminfo_flagged_last() {
        // STREAMINFO is itself a metadata block: flagged last, the run is packet 0.
        let (data, header_len) = oggflac_file(0, true, &[]);
        let h = read_header(&data).unwrap();
        assert_eq!(h.packets.len(), 1);
        assert_eq!(h.audio_offset, header_len as u64);
        assert_eq!(read_tags(&data).unwrap(), Vec::new());
    }

    #[test]
    fn oggflac_run_that_never_flags_its_last_block_is_malformed() {
        // No last-block flag anywhere: the walk reaches the audio packet, which is
        // not a metadata block, and there is nothing left to guess from.
        let (data, _) = oggflac_file(0, false, &[vorbis_comment_block(false, "T")]);
        assert!(read_header(&data).is_err());
        assert!(locate_audio(&data).is_err());
    }

    #[test]
    fn synthesize_oggflac_from_unknown_count_clears_the_streaminfo_last_flag() {
        // Source: STREAMINFO flagged last, no following blocks. Synthesis appends a
        // comment block, so STREAMINFO must stop claiming to be the last one.
        let (data, _) = oggflac_file(0, true, &[]);
        let scan = locate_audio(&data).unwrap();
        let header = read_metadata(&data[..crate::convert::usize_from(scan.audio_offset)]).unwrap();
        let layout = synthesize_layout(
            &header,
            scan.audio_offset,
            scan.audio_length,
            &[TagInput::new("title", "Kaini Industries")],
            &[],
            &MapArtSource::default(),
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
        assert_eq!(
            h.packets[0][OGGFLAC_STREAMINFO_POS] & FLAC_LAST_BLOCK,
            0,
            "STREAMINFO is no longer the last metadata block"
        );
        assert_eq!(u16::from_be_bytes([h.packets[0][7], h.packets[0][8]]), 1);
        assert_eq!(
            read_tags(&header_bytes).unwrap(),
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
            picture_type: crate::input::PictureType::new(3).unwrap(),
            width: 64,
            height: 64,
            depth: 0,
            colors: 0,
            data_len: crate::input::BlobLen::new(image.len() as u64).unwrap(),
        };
        let src = MapArtSource::new([(meta.art_id, image.clone())]);
        let layout = synthesize_layout(
            &header,
            scan.audio_offset,
            scan.audio_length,
            &[TagInput::new("title", "Cover")],
            &[OggArt { meta: &meta }],
            &src,
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
                    let w = b64_window(*offset, len.get(), *art_total);
                    let raw = &image[crate::convert::usize_from(w.in_start)
                        ..crate::convert::usize_from(w.in_start + w.in_len)];
                    bytes.extend_from_slice(
                        &encode_b64_slice(raw, w.skip, crate::convert::usize_from(len.get()))
                            .expect("window lies within the encoded output"),
                    );
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
                        let w = b64_window(*offset, len.get(), *art_total);
                        let raw = &img[crate::convert::usize_from(w.in_start)
                            ..crate::convert::usize_from(w.in_start + w.in_len)];
                        bytes.extend_from_slice(
                            &encode_b64_slice(raw, w.skip, crate::convert::usize_from(len.get()))
                                .expect("window lies within the encoded output"),
                        );
                    } else {
                        bytes.extend_from_slice(
                            &img[crate::convert::usize_from(*offset)
                                ..crate::convert::usize_from(*offset + len.get())],
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
            picture_type: crate::input::PictureType::new(3).unwrap(),
            width: 10,
            height: 10,
            depth: 0,
            colors: 0,
            data_len: crate::input::BlobLen::new(len as u64).unwrap(),
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
        let src = MapArtSource::new([(meta.art_id, image.clone())]);
        let layout = synthesize_layout(
            &header,
            scan.audio_offset,
            scan.audio_length,
            &[TagInput::new("artist", "X")],
            &[OggArt { meta: &meta }],
            &src,
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
        let src = MapArtSource::new([(meta.art_id, image.clone())]);
        let layout = synthesize_layout(
            &header,
            scan.audio_offset,
            scan.audio_length,
            &[TagInput::new("title", "Y")],
            &[OggArt { meta: &meta }],
            &src,
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
    fn synthesize_oggflac_embeds_large_art_spanning_pages_round_trips() {
        // A >64 KiB raw PICTURE block forces the art run across multiple pages,
        // exercising the non-base64 streaming-CRC path at page boundaries (the
        // base64 path is covered by the lacer test; this pins the raw path).
        let mut data = oggflac_headers();
        let (audio, _) = crate::ogg::page::lace_packet(77, 3, false, 4096, &[0u8; 64]);
        data.extend_from_slice(&audio);
        let scan = locate_audio(&data).unwrap();
        let header = read_metadata(&data[..crate::convert::usize_from(scan.audio_offset)]).unwrap();

        let image: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let meta = art_input(31, "image/png", image.len());
        let src = MapArtSource::new([(meta.art_id, image.clone())]);
        let layout = synthesize_layout(
            &header,
            scan.audio_offset,
            scan.audio_length,
            &[TagInput::new("title", "Big")],
            &[OggArt { meta: &meta }],
            &src,
        )
        .unwrap();

        // The art run must split across pages: more than one raw OggArtSlice.
        let art_slices = layout
            .segments()
            .iter()
            .filter(|s| matches!(s, Segment::OggArtSlice { base64: false, .. }))
            .count();
        assert!(
            art_slices >= 2,
            "expected the raw art to span multiple pages, got {art_slices} slice(s)"
        );

        let bytes = materialize_header(&layout, &[(31, &image)]);
        let h = read_header(&bytes).unwrap();
        assert_eq!(h.codec, Codec::OggFlac);
        let pics = read_pictures(&bytes).unwrap();
        assert_eq!(pics.len(), 1);
        assert_eq!(
            pics[0].data, image,
            "large art must round-trip byte-for-byte"
        );
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
        let src = MapArtSource::new([
            (meta_a.art_id, img_a.clone()),
            (meta_b.art_id, img_b.clone()),
        ]);
        let layout = synthesize_layout(
            &header,
            scan.audio_offset,
            scan.audio_length,
            &[TagInput::new("title", "Multi")],
            &[OggArt { meta: &meta_a }, OggArt { meta: &meta_b }],
            &src,
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
    fn oversized_full_art_value_rejected_by_build_packets() {
        let meta = crate::input::ArtInput {
            art_id: 0,
            mime: "image/jpeg".to_string(),
            description: String::new(),
            depth: 0,
            colors: 0,
            data_len: crate::input::BlobLen::new(u64::from(u32::MAX)).unwrap(),
            picture_type: crate::input::PictureType::new(3).unwrap(),
            width: 0,
            height: 0,
        };
        let art = OggArt { meta: &meta };
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
            depth: 0,
            colors: 0,
            data_len: crate::input::BlobLen::new(3_221_225_470).unwrap(),
            picture_type: crate::input::PictureType::new(3).unwrap(),
            width: 0,
            height: 0,
        };
        let art = OggArt { meta: &meta };
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
    fn art_value_at_u32_max_boundary_is_accepted_by_build_packets() {
        // Pin the exact `value_len > u32::MAX` boundary: with mime "image/png" and
        // an empty description, key(23) + b64(prefix=42)=56 + b64(data_len) lands on
        // u32::MAX EXACTLY when data_len == 3_221_225_412. A correct `>` admits it
        // (the downstream u32 length field still fits); `>=`/`==` or a `*`-mutated
        // sum would wrongly reject it. The declared 3 GiB image is never
        // materialized (image bytes are empty), so the build is cheap.
        let meta = crate::input::ArtInput {
            art_id: 0,
            mime: "image/png".to_string(),
            description: String::new(),
            depth: 0,
            colors: 0,
            data_len: crate::input::BlobLen::new(3_221_225_412).unwrap(),
            picture_type: crate::input::PictureType::new(3).unwrap(),
            width: 0,
            height: 0,
        };
        let art = OggArt { meta: &meta };
        let header = OggHeader {
            codec: Codec::Vorbis,
            serial: 0,
            packets: vec![vec![], vec![], vec![]],
            header_pages: 1,
            audio_offset: 0,
        };
        let accepted = build_packets_with_art(&header, &[], &[art]).is_ok();
        assert!(
            accepted,
            "value_len exactly u32::MAX must be accepted by build_packets_with_art"
        );
    }

    #[test]
    fn near_u64_max_art_value_rejected_by_build_packets() {
        // data_len near u64::MAX makes b64_len(data_len) overflow u64; the builder
        // must fail closed with TooLarge at the checked b64 length, not panic
        // (debug) inside the pre-flight value_len computation.
        let meta = crate::input::ArtInput {
            art_id: 0,
            mime: "image/jpeg".to_string(),
            description: String::new(),
            depth: 0,
            colors: 0,
            data_len: crate::input::BlobLen::new(u64::MAX).unwrap(),
            picture_type: crate::input::PictureType::new(3).unwrap(),
            width: 0,
            height: 0,
        };
        let art = OggArt { meta: &meta };
        let header = OggHeader {
            codec: Codec::Vorbis,
            serial: 0,
            packets: vec![vec![], vec![], vec![]],
            header_pages: 1,
            audio_offset: 0,
        };
        let result = build_packets_with_art(&header, &[], &[art]);
        let is_too_large = matches!(&result, Err(FormatError::TooLarge));
        assert!(is_too_large, "expected Err(TooLarge) for near-u64::MAX art");
    }

    #[test]
    fn near_u64_max_art_value_rejected_by_oggflac_build_packets() {
        // The Ogg-FLAC art path builds a METADATA_BLOCK_PICTURE body as
        // prefix + raw image. A hostile data_len near u64::MAX must fail closed
        // with TooLarge, not panic (debug) / wrap (release). picture_prefix's u32
        // length field already rejects it; the checked add keeps the body-length
        // site self-defending regardless of that ordering.
        let meta = crate::input::ArtInput {
            art_id: 0,
            mime: "image/jpeg".to_string(),
            description: String::new(),
            depth: 0,
            colors: 0,
            data_len: crate::input::BlobLen::new(u64::MAX).unwrap(),
            picture_type: crate::input::PictureType::new(3).unwrap(),
            width: 0,
            height: 0,
        };
        let art = OggArt { meta: &meta };
        let header = OggHeader {
            codec: Codec::OggFlac,
            serial: 0,
            packets: vec![vec![0x7F]],
            header_pages: 1,
            audio_offset: 0,
        };
        let result = build_packets_with_art(&header, &[], &[art]);
        let is_too_large = matches!(&result, Err(FormatError::TooLarge));
        assert!(is_too_large, "expected Err(TooLarge) for near-u64::MAX art");
    }

    #[test]
    fn picture_prefix_is_3_aligned_and_declares_image_len() {
        let art = crate::input::ArtInput {
            art_id: 1,
            mime: "image/png".to_string(), // 9 -> base = 32+9+0 = 41 -> pad 1
            description: String::new(),
            picture_type: crate::input::PictureType::new(3).unwrap(),
            width: 1,
            height: 1,
            depth: 0,
            colors: 0,
            data_len: crate::input::BlobLen::new(12345).unwrap(),
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
        assert_eq!(pic.picture_type.get(), 3);
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
            picture_type: crate::input::PictureType::new(3).unwrap(),
            width: 0,
            height: 0,
            depth: 0,
            colors: 0,
            data_len: crate::input::BlobLen::new(data_len).unwrap(),
        };
        let framing_len = picture_prefix(&mk(1)).unwrap().len() as u64;
        let at_limit = mk(crate::flac::MAX_BLOCK_BODY - framing_len);
        let arts = [OggArt { meta: &at_limit }];
        assert!(oggflac_packets_with_art(&header, &[], &arts).is_ok());
        // one byte over must still error, pinning the high side of the boundary.
        let over = mk(crate::flac::MAX_BLOCK_BODY - framing_len + 1);
        let arts = [OggArt { meta: &over }];
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
            picture_type: crate::input::PictureType::new(3).unwrap(),
            width: 1,
            height: 1,
            depth: 0,
            colors: 0,
            data_len: crate::input::BlobLen::new(100).unwrap(),
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

    #[test]
    fn synthesis_reads_art_in_page_bounded_windows() {
        use std::cell::Cell;
        struct Counting<'a> {
            inner: MapArtSource,
            max: &'a Cell<usize>,
        }
        impl ArtSource for Counting<'_> {
            fn read_window(&self, art_id: i64, offset: u64, buf: &mut [u8]) -> crate::Result<()> {
                self.max.set(self.max.get().max(buf.len()));
                self.inner.read_window(art_id, offset, buf)
            }
        }

        // Inline Opus fixture, mirroring synthesize_opus_emits_valid_header_and_audio_segment.
        let mut data = opus_headers();
        let scan = locate_audio({
            let (audio, _) = crate::ogg::page::lace_packet(0x1234, 2, false, 960, &[0u8; 80]);
            data.extend_from_slice(&audio);
            &data
        })
        .unwrap();
        let header = read_metadata(&data[..crate::convert::usize_from(scan.audio_offset)]).unwrap();

        let image: Vec<u8> = (0..500_000u32).map(|i| (i % 251) as u8).collect();
        let meta = crate::input::ArtInput {
            art_id: 7,
            mime: "image/jpeg".to_string(),
            description: String::new(),
            picture_type: crate::input::PictureType::new(3).unwrap(),
            width: 0,
            height: 0,
            depth: 0,
            colors: 0,
            data_len: crate::input::BlobLen::new(image.len() as u64).unwrap(),
        };
        let max = Cell::new(0usize);
        let src = Counting {
            inner: MapArtSource::new([(7i64, image.clone())]),
            max: &max,
        };
        synthesize_layout(
            &header,
            scan.audio_offset,
            scan.audio_length,
            &[],
            &[OggArt { meta: &meta }],
            &src,
        )
        .unwrap();
        // One Ogg page's payload is at most 255*255 = 65025 bytes; raw windows are
        // <= that. The 500 KB image is never read in a single call.
        assert!(
            max.get() > 0 && max.get() <= 65_025,
            "max single read was {}",
            max.get()
        );
    }
}

#[cfg(test)]
mod page_test_support_tests {
    /// Pins the fixture's contract: a parseable VorbisComment with zero
    /// comments. Consumers (musefs-core's ogg tests) splice it into OpusTags
    /// packets, but only format-local tests can kill mutations of it — the
    /// mutation gate runs each crate's own suite.
    #[test]
    fn vorbis_body_empty_is_a_parseable_empty_comment() {
        let body = super::page_test_support::vorbis_body_empty();
        let parsed = crate::vorbiscomment::parse(&body).unwrap();
        assert!(parsed.is_empty());
    }

    /// Same contract, for the fixture that carries fields: the comments come back
    /// in order, with their keys normalized to lowercase the way every tag
    /// musefs writes is.
    #[test]
    fn vorbis_body_with_round_trips_its_comments() {
        let body =
            super::page_test_support::vorbis_body_with(&[("TITLE", "Sun"), ("ARTIST", "Boc")]);
        let parsed = crate::vorbiscomment::parse(&body).unwrap();
        assert_eq!(
            parsed,
            vec![
                ("title".to_string(), "Sun".to_string()),
                ("artist".to_string(), "Boc".to_string()),
            ]
        );
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
