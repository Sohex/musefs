use crate::error::{FormatError, Result};

pub(crate) const FLAC_MARKER: &[u8; 4] = b"fLaC";

pub(crate) const BLOCK_STREAMINFO: u8 = 0;
pub(crate) const BLOCK_APPLICATION: u8 = 2;
pub(crate) const BLOCK_SEEKTABLE: u8 = 3;
pub(crate) const BLOCK_VORBIS_COMMENT: u8 = 4;
pub(crate) const BLOCK_CUESHEET: u8 = 5;
pub(crate) const BLOCK_PICTURE: u8 = 6;

/// A preserved FLAC metadata block: its type and its body (excluding the 4-byte header).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataBlock {
    pub block_type: u8,
    pub body: Vec<u8>,
}

/// Result of scanning a FLAC file: where audio begins/ends and the structural blocks to preserve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlacScan {
    pub audio_offset: u64,
    pub audio_length: u64,
    pub preserved: Vec<MetadataBlock>,
}

/// The metadata region of a FLAC file: where audio begins and the structural
/// blocks to carry over. Unlike `FlacScan`, this does not include `audio_length`
/// (which requires the full file size), so it can be computed from the front alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlacMeta {
    pub audio_offset: u64,
    pub preserved: Vec<MetadataBlock>,
}

fn parse_blocks(data: &[u8]) -> Result<FlacMeta> {
    if data.len() < 4 || &data[0..4] != FLAC_MARKER {
        return Err(FormatError::NotFlac);
    }
    let mut pos = 4usize;
    let mut preserved = Vec::new();
    loop {
        if pos + 4 > data.len() {
            return Err(FormatError::Malformed);
        }
        let header = data[pos];
        let is_last = (header & 0x80) != 0;
        let block_type = header & 0x7F;
        let len = ((data[pos + 1] as usize) << 16)
            | ((data[pos + 2] as usize) << 8)
            | (data[pos + 3] as usize);
        let body_start = pos + 4;
        let body_end = body_start + len;
        if body_end > data.len() {
            return Err(FormatError::Malformed);
        }
        match block_type {
            BLOCK_STREAMINFO | BLOCK_APPLICATION | BLOCK_SEEKTABLE | BLOCK_CUESHEET => {
                preserved.push(MetadataBlock {
                    block_type,
                    body: data[body_start..body_end].to_vec(),
                });
            }
            _ => {}
        }
        pos = body_end;
        if is_last {
            break;
        }
    }
    Ok(FlacMeta {
        audio_offset: pos as u64,
        preserved,
    })
}

/// Parse just the FLAC metadata region (the front of the file), recovering the
/// audio boundary and structural blocks. Use when the audio length is already
/// known (e.g. stored in a database) and the full file should not be read.
pub fn read_metadata(data: &[u8]) -> Result<FlacMeta> {
    parse_blocks(data)
}

/// Parse the FLAC metadata section of a complete file, returning the audio
/// boundary, audio length, and the structural blocks to carry over.
pub fn locate_audio(data: &[u8]) -> Result<FlacScan> {
    let meta = parse_blocks(data)?;
    Ok(FlacScan {
        audio_offset: meta.audio_offset,
        audio_length: data.len() as u64 - meta.audio_offset,
        preserved: meta.preserved,
    })
}

use crate::input::{ArtInput, EmbeddedPicture, TagInput};
use crate::layout::{RegionLayout, Segment};

pub(crate) const VENDOR: &str = "musefs";

fn push_block_header(out: &mut Vec<u8>, block_type: u8, body_len: usize, is_last: bool) {
    let first = (if is_last { 0x80 } else { 0 }) | (block_type & 0x7F);
    out.push(first);
    out.push(((body_len >> 16) & 0xFF) as u8);
    out.push(((body_len >> 8) & 0xFF) as u8);
    out.push((body_len & 0xFF) as u8);
}

fn vorbis_comment_body(tags: &[TagInput]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(VENDOR.len() as u32).to_le_bytes());
    out.extend_from_slice(VENDOR.as_bytes());
    out.extend_from_slice(&(tags.len() as u32).to_le_bytes());
    for t in tags {
        let comment = format!("{}={}", t.key.to_ascii_uppercase(), t.value);
        out.extend_from_slice(&(comment.len() as u32).to_le_bytes());
        out.extend_from_slice(comment.as_bytes());
    }
    out
}

fn picture_body_framing(art: &ArtInput) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&art.picture_type.to_be_bytes());
    out.extend_from_slice(&(art.mime.len() as u32).to_be_bytes());
    out.extend_from_slice(art.mime.as_bytes());
    out.extend_from_slice(&(art.description.len() as u32).to_be_bytes());
    out.extend_from_slice(art.description.as_bytes());
    out.extend_from_slice(&art.width.to_be_bytes());
    out.extend_from_slice(&art.height.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes()); // color depth (unknown)
    out.extend_from_slice(&0u32.to_be_bytes()); // number of colors (non-indexed)
    out.extend_from_slice(&(art.data_len as u32).to_be_bytes()); // picture data length
    out
}

/// Build the ordered segment layout for a synthesized FLAC file:
/// `fLaC` + preserved structural blocks + a regenerated VORBIS_COMMENT + PICTURE
/// blocks (one `ArtImage` segment each) + the backing audio.
pub fn synthesize_layout(scan: &FlacScan, tags: &[TagInput], arts: &[ArtInput]) -> RegionLayout {
    let num_blocks = scan.preserved.len() + 1 + arts.len(); // preserved + VORBIS_COMMENT + pictures
    let last_index = num_blocks - 1;

    let mut segments: Vec<Segment> = Vec::new();
    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(FLAC_MARKER);

    let mut idx = 0usize;

    for blk in &scan.preserved {
        push_block_header(&mut buf, blk.block_type, blk.body.len(), idx == last_index);
        buf.extend_from_slice(&blk.body);
        idx += 1;
    }

    let vc = vorbis_comment_body(tags);
    push_block_header(&mut buf, BLOCK_VORBIS_COMMENT, vc.len(), idx == last_index);
    buf.extend_from_slice(&vc);
    idx += 1;

    for art in arts {
        let framing = picture_body_framing(art);
        let body_len = framing.len() as u64 + art.data_len;
        // FLAC metadata block lengths are 24-bit (max ~16 MiB). Real cover art is far
        // smaller; enforcing a hard limit at art ingestion is deferred to a later milestone.
        debug_assert!(
            body_len <= 0x00FF_FFFF,
            "FLAC PICTURE block body ({body_len} bytes) exceeds the 24-bit length limit"
        );
        push_block_header(
            &mut buf,
            BLOCK_PICTURE,
            body_len as usize,
            idx == last_index,
        );
        buf.extend_from_slice(&framing);
        segments.push(Segment::Inline(std::mem::take(&mut buf)));
        segments.push(Segment::ArtImage {
            art_id: art.art_id,
            len: art.data_len,
        });
        idx += 1;
    }

    if !buf.is_empty() {
        segments.push(Segment::Inline(buf));
    }
    segments.push(Segment::BackingAudio {
        offset: scan.audio_offset,
        len: scan.audio_length,
    });

    RegionLayout::new(segments)
}

/// Read the existing VORBIS_COMMENT block from a complete FLAC file, returning
/// `(FIELD, value)` pairs in order. Comments without a `=` are skipped. Returns
/// an empty vec if there is no comment block. Used by the scanner to seed tags.
pub fn read_vorbis_comments(data: &[u8]) -> Result<Vec<(String, String)>> {
    if data.len() < 4 || &data[0..4] != FLAC_MARKER {
        return Err(FormatError::NotFlac);
    }
    let mut pos = 4usize;
    loop {
        if pos + 4 > data.len() {
            return Err(FormatError::Malformed);
        }
        let header = data[pos];
        let is_last = (header & 0x80) != 0;
        let block_type = header & 0x7F;
        let len = ((data[pos + 1] as usize) << 16)
            | ((data[pos + 2] as usize) << 8)
            | (data[pos + 3] as usize);
        let body_start = pos + 4;
        let body_end = body_start + len;
        if body_end > data.len() {
            return Err(FormatError::Malformed);
        }
        if block_type == BLOCK_VORBIS_COMMENT {
            return parse_vorbis_comment_body(&data[body_start..body_end]);
        }
        pos = body_end;
        if is_last {
            break;
        }
    }
    Ok(Vec::new())
}

fn read_u32_le(data: &[u8], pos: usize) -> Result<u32> {
    if pos + 4 > data.len() {
        return Err(FormatError::Malformed);
    }
    Ok(u32::from_le_bytes([
        data[pos],
        data[pos + 1],
        data[pos + 2],
        data[pos + 3],
    ]))
}

fn read_u32_be(data: &[u8], pos: usize) -> Result<u32> {
    if pos + 4 > data.len() {
        return Err(FormatError::Malformed);
    }
    Ok(u32::from_be_bytes([
        data[pos],
        data[pos + 1],
        data[pos + 2],
        data[pos + 3],
    ]))
}

fn parse_picture_block(body: &[u8]) -> Result<EmbeddedPicture> {
    let mut pos = 0usize;
    let picture_type = read_u32_be(body, pos)?;
    pos += 4;
    let mime_len = read_u32_be(body, pos)? as usize;
    pos += 4;
    let mime_end = pos + mime_len;
    if mime_end > body.len() {
        return Err(FormatError::Malformed);
    }
    let mime = String::from_utf8_lossy(&body[pos..mime_end]).into_owned();
    pos = mime_end;
    let desc_len = read_u32_be(body, pos)? as usize;
    pos += 4;
    let desc_end = pos + desc_len;
    if desc_end > body.len() {
        return Err(FormatError::Malformed);
    }
    let description = String::from_utf8_lossy(&body[pos..desc_end]).into_owned();
    pos = desc_end;
    let width = read_u32_be(body, pos)?;
    pos += 4;
    let height = read_u32_be(body, pos)?;
    pos += 4;
    let _depth = read_u32_be(body, pos)?;
    pos += 4;
    let _colors = read_u32_be(body, pos)?;
    pos += 4;
    let data_len = read_u32_be(body, pos)? as usize;
    pos += 4;
    let data_end = pos + data_len;
    if data_end > body.len() {
        return Err(FormatError::Malformed);
    }
    Ok(EmbeddedPicture {
        mime,
        picture_type,
        description,
        width,
        height,
        data: body[pos..data_end].to_vec(),
    })
}

/// Extract all PICTURE blocks from a complete FLAC file as embedded pictures, for
/// scan-time art ingestion. Returns an empty vec if there are none.
pub fn read_pictures(data: &[u8]) -> Result<Vec<EmbeddedPicture>> {
    if data.len() < 4 || &data[0..4] != FLAC_MARKER {
        return Err(FormatError::NotFlac);
    }
    let mut pos = 4usize;
    let mut out = Vec::new();
    loop {
        if pos + 4 > data.len() {
            return Err(FormatError::Malformed);
        }
        let header = data[pos];
        let is_last = (header & 0x80) != 0;
        let block_type = header & 0x7F;
        let len = ((data[pos + 1] as usize) << 16)
            | ((data[pos + 2] as usize) << 8)
            | (data[pos + 3] as usize);
        let body_start = pos + 4;
        let body_end = body_start + len;
        if body_end > data.len() {
            return Err(FormatError::Malformed);
        }
        if block_type == BLOCK_PICTURE {
            out.push(parse_picture_block(&data[body_start..body_end])?);
        }
        pos = body_end;
        if is_last {
            break;
        }
    }
    Ok(out)
}

fn parse_vorbis_comment_body(body: &[u8]) -> Result<Vec<(String, String)>> {
    let vendor_len = read_u32_le(body, 0)? as usize;
    let mut pos = 4 + vendor_len;
    let count = read_u32_le(body, pos)? as usize;
    pos += 4;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let clen = read_u32_le(body, pos)? as usize;
        pos += 4;
        let end = pos + clen;
        if end > body.len() {
            return Err(FormatError::Malformed);
        }
        let comment = std::str::from_utf8(&body[pos..end]).map_err(|_| FormatError::Malformed)?;
        if let Some((field, value)) = comment.split_once('=') {
            out.push((field.to_string(), value.to_string()));
        }
        pos = end;
    }
    Ok(out)
}
