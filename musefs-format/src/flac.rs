use crate::error::{FormatError, Result};
use crate::probe::Extent;

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
        let len = u24_be(data[pos + 1], data[pos + 2], data[pos + 3]);
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

/// Bounded twin of [`read_metadata`]: walk the metadata blocks present in
/// `prefix` (which may be a front-only window of the file). If a block's declared
/// body runs past the prefix, return `NeedMore { up_to }` with the exact end of
/// that block — the caller widens the window and retries. Otherwise `Complete`.
pub fn read_metadata_bounded(prefix: &[u8]) -> Result<Extent<FlacMeta>> {
    if prefix.len() < 4 || &prefix[0..4] != FLAC_MARKER {
        return Err(FormatError::NotFlac);
    }
    let mut pos = 4usize;
    let mut preserved = Vec::new();
    loop {
        if pos + 4 > prefix.len() {
            // Need at least the 4-byte block header.
            return Ok(Extent::NeedMore {
                up_to: (pos + 4) as u64,
            });
        }
        let header = prefix[pos];
        let is_last = (header & 0x80) != 0;
        let block_type = header & 0x7F;
        let len = u24_be(prefix[pos + 1], prefix[pos + 2], prefix[pos + 3]);
        let body_start = pos + 4;
        let body_end = body_start + len;
        if body_end > prefix.len() {
            return Ok(Extent::NeedMore {
                up_to: body_end as u64,
            });
        }
        match block_type {
            BLOCK_STREAMINFO | BLOCK_APPLICATION | BLOCK_SEEKTABLE | BLOCK_CUESHEET => {
                preserved.push(MetadataBlock {
                    block_type,
                    body: prefix[body_start..body_end].to_vec(),
                });
            }
            _ => {}
        }
        pos = body_end;
        if is_last {
            break;
        }
    }
    Ok(Extent::Complete(FlacMeta {
        audio_offset: pos as u64,
        preserved,
    }))
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

use crate::input::{ArtInput, BinaryTagInput, EmbeddedBinaryTag, EmbeddedPicture, TagInput};
use crate::layout::{RegionLayout, Segment};

/// Inclusive maximum body length of a FLAC metadata block (24-bit length field).
pub(crate) const MAX_BLOCK_BODY: u64 = 0x00FF_FFFF;

pub(crate) fn push_block_header(
    out: &mut Vec<u8>,
    block_type: u8,
    body_len: usize,
    is_last: bool,
) -> Result<()> {
    // A FLAC block length is a 24-bit field; refuse anything larger rather
    // than emit a truncated length.
    let len = u32::try_from(body_len)
        .ok()
        .filter(|&v| u64::from(v) <= MAX_BLOCK_BODY)
        .ok_or(FormatError::TooLarge)?;
    let first = (if is_last { 0x80 } else { 0 }) | (block_type & 0x7F);
    out.push(first);
    out.extend_from_slice(&len.to_be_bytes()[1..]);
    Ok(())
}

/// Map a stored structural-block `kind` string back to its FLAC block type.
/// Only STREAMINFO/SEEKTABLE live in the structural store; everything else
/// returns `None` (APPLICATION/CUESHEET are binary tags, not structural).
pub fn structural_block_type(kind: &str) -> Option<u8> {
    match kind {
        "STREAMINFO" => Some(BLOCK_STREAMINFO),
        "SEEKTABLE" => Some(BLOCK_SEEKTABLE),
        _ => None,
    }
}

/// Split a FLAC file's preserved metadata blocks into the read-only structural
/// store (STREAMINFO/SEEKTABLE, as `(kind, body)` pairs in file order) and the
/// editable binary tags (APPLICATION/CUESHEET, as `EmbeddedBinaryTag`s keyed by
/// block name; `payload` is the full block body, including APPLICATION's 4-byte
/// app id). Blocks of any other type are ignored (PICTURE/VORBIS_COMMENT are
/// handled by their own paths and are never in `preserved`).
pub fn split_preserved(
    blocks: &[MetadataBlock],
) -> (Vec<(String, Vec<u8>)>, Vec<EmbeddedBinaryTag>) {
    let mut structural = Vec::new();
    let mut binary = Vec::new();
    for blk in blocks {
        match blk.block_type {
            BLOCK_STREAMINFO => structural.push(("STREAMINFO".to_string(), blk.body.clone())),
            BLOCK_SEEKTABLE => structural.push(("SEEKTABLE".to_string(), blk.body.clone())),
            BLOCK_APPLICATION => binary.push(EmbeddedBinaryTag {
                key: "APPLICATION".to_string(),
                payload: blk.body.clone(),
            }),
            BLOCK_CUESHEET => binary.push(EmbeddedBinaryTag {
                key: "CUESHEET".to_string(),
                payload: blk.body.clone(),
            }),
            _ => {}
        }
    }
    (structural, binary)
}

fn picture_body_framing(art: &ArtInput) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    out.extend_from_slice(&art.picture_type.to_be_bytes());
    out.extend_from_slice(
        &u32::try_from(art.mime.len())
            .map_err(|_| FormatError::TooLarge)?
            .to_be_bytes(),
    );
    out.extend_from_slice(art.mime.as_bytes());
    out.extend_from_slice(
        &u32::try_from(art.description.len())
            .map_err(|_| FormatError::TooLarge)?
            .to_be_bytes(),
    );
    out.extend_from_slice(art.description.as_bytes());
    out.extend_from_slice(&art.width.to_be_bytes());
    out.extend_from_slice(&art.height.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes()); // color depth (unknown)
    out.extend_from_slice(&0u32.to_be_bytes()); // number of colors (non-indexed)
    out.extend_from_slice(
        &u32::try_from(art.data_len)
            .map_err(|_| FormatError::TooLarge)?
            .to_be_bytes(),
    ); // picture data length
    Ok(out)
}

/// Build the ordered segment layout for a synthesized FLAC file:
/// `fLaC` + structural blocks (sorted by type) + a regenerated VORBIS_COMMENT +
/// streamed APPLICATION/CUESHEET binary tags + PICTURE blocks (one `ArtImage`
/// segment each) + the backing audio.  Structural blocks must be only
/// STREAMINFO/SEEKTABLE; APPLICATION/CUESHEET ride through `binary_tags`.
pub fn synthesize_layout(
    structural: &[MetadataBlock],
    audio_offset: u64,
    audio_length: u64,
    tags: &[TagInput],
    binary_tags: &[BinaryTagInput],
    arts: &[ArtInput],
) -> Result<RegionLayout> {
    let mut ordered: Vec<&MetadataBlock> = structural.iter().collect();
    ordered.sort_by_key(|b| b.block_type);

    let valid_binary: Vec<&BinaryTagInput> = binary_tags
        .iter()
        .filter(|bt| matches!(bt.key.as_str(), "APPLICATION" | "CUESHEET"))
        .collect();

    let nonempty_art = arts.iter().filter(|a| a.data_len > 0).count();
    let num_blocks = ordered.len() + 1 + valid_binary.len() + nonempty_art;
    let last_index = num_blocks - 1;

    let mut segments: Vec<Segment> = Vec::new();
    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(FLAC_MARKER);
    let mut idx = 0usize;

    for blk in &ordered {
        push_block_header(&mut buf, blk.block_type, blk.body.len(), idx == last_index)?;
        buf.extend_from_slice(&blk.body);
        idx += 1;
    }

    let vc = crate::vorbiscomment::build(tags)?;
    if vc.len() as u64 > MAX_BLOCK_BODY {
        return Err(FormatError::TooLarge);
    }
    push_block_header(&mut buf, BLOCK_VORBIS_COMMENT, vc.len(), idx == last_index)?;
    buf.extend_from_slice(&vc);
    idx += 1;

    for bt in valid_binary {
        let block_type = match bt.key.as_str() {
            "APPLICATION" => BLOCK_APPLICATION,
            "CUESHEET" => BLOCK_CUESHEET,
            _ => continue,
        };
        if bt.len > MAX_BLOCK_BODY {
            return Err(FormatError::TooLarge);
        }
        push_block_header(
            &mut buf,
            block_type,
            crate::convert::usize_from(bt.len),
            idx == last_index,
        )?;
        segments.push(Segment::Inline(std::mem::take(&mut buf)));
        segments.push(Segment::BinaryTag {
            payload_id: bt.payload_id,
            len: bt.len,
        });
        idx += 1;
    }

    for art in arts {
        if art.data_len == 0 {
            continue;
        }
        let framing = picture_body_framing(art)?;
        let body_len = framing.len() as u64 + art.data_len;
        if body_len > MAX_BLOCK_BODY {
            return Err(FormatError::TooLarge);
        }
        push_block_header(
            &mut buf,
            BLOCK_PICTURE,
            crate::convert::usize_from(body_len),
            idx == last_index,
        )?;
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
        offset: audio_offset,
        len: audio_length,
    });

    Ok(RegionLayout::validated(segments)?)
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
        let len = u24_be(data[pos + 1], data[pos + 2], data[pos + 3]);
        let body_start = pos + 4;
        let body_end = body_start + len;
        if body_end > data.len() {
            return Err(FormatError::Malformed);
        }
        if block_type == BLOCK_VORBIS_COMMENT {
            return crate::vorbiscomment::parse(&data[body_start..body_end]);
        }
        pos = body_end;
        if is_last {
            break;
        }
    }
    Ok(Vec::new())
}

/// Assemble a 24-bit big-endian block length from its three raw bytes.
fn u24_be(b0: u8, b1: u8, b2: u8) -> usize {
    u32::from_be_bytes([0, b0, b1, b2]) as usize
}

pub(crate) fn read_u32_be(data: &[u8], pos: usize) -> Result<u32> {
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

pub(crate) fn parse_picture_block(body: &[u8]) -> Result<EmbeddedPicture> {
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
        let len = u24_be(data[pos + 1], data[pos + 2], data[pos + 3]);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::Extent;

    /// Build a minimal FLAC: marker + a single last STREAMINFO (type 0, 34-byte
    /// body) + `audio` bytes. Returns (full_bytes, audio_offset).
    fn flac_with_streaminfo(audio: &[u8]) -> (Vec<u8>, u64) {
        let mut v = b"fLaC".to_vec();
        push_block_header(&mut v, BLOCK_STREAMINFO, 34, true).unwrap();
        v.extend(std::iter::repeat_n(0u8, 34));
        let audio_offset = v.len() as u64;
        v.extend_from_slice(audio);
        (v, audio_offset)
    }

    #[test]
    fn read_metadata_bounded_complete_when_prefix_covers_blocks() {
        let (full, audio_offset) = flac_with_streaminfo(b"AUDIOAUDIO");
        // Prefix that includes all metadata but not all audio.
        let prefix = &full[..crate::convert::usize_from(audio_offset) + 2];
        match read_metadata_bounded(prefix).unwrap() {
            Extent::Complete(meta) => assert_eq!(meta.audio_offset, audio_offset),
            other @ Extent::NeedMore { .. } => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn read_metadata_bounded_needmore_when_block_body_truncated() {
        let (full, audio_offset) = flac_with_streaminfo(b"AUDIO");
        // Cut inside the STREAMINFO body (header is 4 bytes after the marker).
        let prefix = &full[..8];
        match read_metadata_bounded(prefix).unwrap() {
            Extent::NeedMore { up_to } => assert_eq!(up_to, audio_offset),
            other @ Extent::Complete(_) => panic!("expected NeedMore, got {other:?}"),
        }
    }

    #[test]
    fn read_u32_be_assembles_big_endian_and_guards_length() {
        let data = [0x11u8, 0x22, 0x33, 0x44, 0x55];
        assert_eq!(read_u32_be(&data, 0).unwrap(), 0x1122_3344);
        // pins :224 (`+` -> `*`): at pos=1 the second byte is data[2]=0x33, not data[1].
        // pins :219 (`>` -> `==`/`>=`): pos+4 == len (5) is valid, so this unwrap must
        // succeed — a mutated bound returns Err here and the unwrap panics.
        assert_eq!(read_u32_be(&data, 1).unwrap(), 0x2233_4455);
        assert_eq!(read_u32_be(&data, 2), Err(FormatError::Malformed));
    }

    #[test]
    fn push_block_header_emits_24bit_length_big_endian() {
        // pins :101 (`>>16` -> `<<16`): high byte 0x12 must land in out[1].
        let mut out = Vec::new();
        push_block_header(&mut out, BLOCK_PICTURE, 0x12_3456, false).unwrap();
        assert_eq!(out, vec![BLOCK_PICTURE, 0x12, 0x34, 0x56]);
        // :99 is equivalent, but exercise the is_last/0x80 path anyway.
        let mut last = Vec::new();
        push_block_header(&mut last, BLOCK_VORBIS_COMMENT, 0, true).unwrap();
        assert_eq!(last, vec![0x80 | BLOCK_VORBIS_COMMENT, 0x00, 0x00, 0x00]);
    }

    /// One FLAC metadata block: 4-byte header (last-flag, type, 24-bit BE length)
    /// + body, built independently of production framing so a mutation in
    ///   `push_block_header` cannot mask a fixture. `len_override` lets a test claim a
    ///   length different from `body.len()`.
    fn raw_block(block_type: u8, body: &[u8], last: bool, len_override: Option<usize>) -> Vec<u8> {
        let n = len_override.unwrap_or(body.len());
        let mut v = vec![(if last { 0x80 } else { 0 }) | (block_type & 0x7F)];
        v.extend_from_slice(&u32::try_from(n).unwrap().to_be_bytes()[1..]);
        v.extend_from_slice(body);
        v
    }

    /// `fLaC` + the given blocks (no audio).
    fn flac_with(blocks: &[Vec<u8>]) -> Vec<u8> {
        let mut f = b"fLaC".to_vec();
        for b in blocks {
            f.extend_from_slice(b);
        }
        f
    }

    #[test]
    fn parse_blocks_rejects_short_and_wrong_marker() {
        // :37 `< -> ==`: 3-byte input -> original short-circuits NotFlac; the mutant
        // evaluates &data[0..4] on 3 bytes -> panic. Asserting Err(NotFlac) kills it.
        assert_eq!(parse_blocks(b"fLa"), Err(FormatError::NotFlac));
        // :37 `< -> <=`: a 4-byte fLaC-only file. Original proceeds then hits the
        // loop guard -> Malformed; the `<=` mutant short-circuits to NotFlac.
        assert_eq!(parse_blocks(b"fLaC"), Err(FormatError::Malformed));
        assert_eq!(parse_blocks(b"XXXX____"), Err(FormatError::NotFlac));
    }

    #[test]
    fn parse_blocks_guards_truncated_block_header() {
        // 5 bytes: marker + 1 header byte. Original: pos+4=8 > 5 -> Malformed.
        // :43 `+ -> -` (0 > 5 false) and `> -> ==` (8 == 5 false) both fall through
        // and panic reading data[5..8].
        assert_eq!(parse_blocks(b"fLaC\x80"), Err(FormatError::Malformed));
    }

    #[test]
    fn parse_blocks_accepts_header_flush_with_end() {
        // Single last STREAMINFO, empty body, no audio: the final header occupies the
        // last 4 bytes, so pos+4 == data.len() at the loop guard. Original (`>`)
        // proceeds and returns audio_offset == len; the :43 `> -> >=` mutant rejects.
        let file = flac_with(&[raw_block(BLOCK_STREAMINFO, &[], true, None)]);
        let meta = parse_blocks(&file).unwrap();
        assert_eq!(meta.audio_offset, 8);
    }

    #[test]
    fn parse_blocks_decodes_24bit_length_high_byte() {
        // STREAMINFO header claims length 0x010000 (high byte set) over an empty body.
        // Pins the high byte of the 24-bit length decode: len = 65536 -> body_end >
        // data.len() -> Malformed; a decode that drops the high byte gets len 0 -> Ok.
        let file = flac_with(&[raw_block(BLOCK_STREAMINFO, &[], true, Some(0x01_0000))]);
        assert_eq!(parse_blocks(&file), Err(FormatError::Malformed));
    }

    #[test]
    fn parse_blocks_preserves_structural_blocks() {
        // Positive decode: a normal STREAMINFO (34-byte body) + audio boundary.
        let si = vec![0xAA; 34];
        let file = flac_with(&[raw_block(BLOCK_STREAMINFO, &si, true, None)]);
        let meta = parse_blocks(&file).unwrap();
        assert_eq!(meta.audio_offset, 4 + 4 + 34);
        assert_eq!(meta.preserved.len(), 1);
        assert_eq!(meta.preserved[0].block_type, BLOCK_STREAMINFO);
        assert_eq!(meta.preserved[0].body, si);
    }

    /// A VORBIS_COMMENT body: u32-LE vendor length, vendor, u32-LE count, then each
    /// comment as u32-LE length + bytes.
    fn vc_body(vendor: &str, comments: &[&str]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&u32::try_from(vendor.len()).unwrap().to_le_bytes());
        v.extend_from_slice(vendor.as_bytes());
        v.extend_from_slice(&u32::try_from(comments.len()).unwrap().to_le_bytes());
        for c in comments {
            v.extend_from_slice(&u32::try_from(c.len()).unwrap().to_le_bytes());
            v.extend_from_slice(c.as_bytes());
        }
        v
    }

    #[test]
    fn read_vorbis_comments_returns_pairs_and_guards_marker() {
        // Happy path: VC block is the last block with no audio, so body_end == len.
        // This also pins :204 (`>` -> `==`/`>=`): the mutant would reject (Malformed)
        // and the unwrap below would panic.
        let vc = vc_body("v", &["TITLE=Hi", "ARTIST=Me"]);
        let file = flac_with(&[raw_block(BLOCK_VORBIS_COMMENT, &vc, true, None)]);
        let got = read_vorbis_comments(&file).unwrap();
        assert_eq!(
            got,
            vec![
                ("title".to_string(), "Hi".to_string()),
                ("artist".to_string(), "Me".to_string()),
            ]
        );
        // :188 `< -> ==` and `|| -> &&`: 3-byte input -> original NotFlac via
        // short-circuit; both mutants force &data[0..4] -> panic.
        assert_eq!(read_vorbis_comments(b"fLa"), Err(FormatError::NotFlac));
        // :188 `< -> <=`: 4-byte fLaC -> original Malformed; mutant NotFlac.
        assert_eq!(read_vorbis_comments(b"fLaC"), Err(FormatError::Malformed));
    }

    #[test]
    fn read_vorbis_comments_guards_block_walk() {
        // :193 `+ -> -` and `> -> ==`: truncated header -> original Malformed,
        // mutants fall through and panic.
        assert_eq!(
            read_vorbis_comments(b"fLaC\x80"),
            Err(FormatError::Malformed)
        );
        // :193 `> -> >=`: a non-VC last block flush with end -> original returns the
        // empty vec; the `>=` mutant rejects at the loop guard.
        let file = flac_with(&[raw_block(BLOCK_STREAMINFO, &[], true, None)]);
        assert_eq!(read_vorbis_comments(&file).unwrap(), Vec::new());
    }

    #[test]
    fn read_vorbis_comments_decodes_24bit_length() {
        // High length byte set over a short body: len = 0x10000 -> Malformed. Pins
        // the high byte of the 24-bit length decode (dropping it gets len 0 -> Ok).
        let hi = flac_with(&[raw_block(BLOCK_STREAMINFO, &[], true, Some(0x01_0000))]);
        assert_eq!(read_vorbis_comments(&hi), Err(FormatError::Malformed));
        // Mid length byte set, high byte 0: len = 0x100 -> Malformed. Pins the mid
        // byte (dropping it gets len 0 -> Ok).
        let mid = flac_with(&[raw_block(BLOCK_STREAMINFO, &[], true, Some(0x00_0100))]);
        assert_eq!(read_vorbis_comments(&mid), Err(FormatError::Malformed));
    }

    /// A FLAC PICTURE block body (big-endian fields), independent of production.
    fn picture_body(ptype: u32, mime: &str, desc: &str, w: u32, h: u32, data: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&ptype.to_be_bytes());
        v.extend_from_slice(&u32::try_from(mime.len()).unwrap().to_be_bytes());
        v.extend_from_slice(mime.as_bytes());
        v.extend_from_slice(&u32::try_from(desc.len()).unwrap().to_be_bytes());
        v.extend_from_slice(desc.as_bytes());
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&0u32.to_be_bytes()); // depth
        v.extend_from_slice(&0u32.to_be_bytes()); // colors
        v.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
        v.extend_from_slice(data);
        v
    }

    #[test]
    fn parse_picture_block_roundtrips_fields() {
        let body = picture_body(3, "image/png", "desc", 4, 5, b"PIXELS");
        let p = parse_picture_block(&body).unwrap();
        assert_eq!(p.picture_type, 3);
        assert_eq!(p.mime, "image/png");
        assert_eq!(p.description, "desc");
        assert_eq!(p.width, 4);
        assert_eq!(p.height, 5);
        assert_eq!(p.data, b"PIXELS");
    }

    #[test]
    fn parse_picture_block_guards_field_bounds() {
        // :237 `> -> ==` (mime bound): claim mime_len far past the end. Original
        // Malformed; the `==` mutant falls through to slice body[8..8+mime_len] -> panic.
        let mut bad_mime = 3u32.to_be_bytes().to_vec();
        bad_mime.extend_from_slice(&16u32.to_be_bytes()); // mime_len = 16
        bad_mime.extend_from_slice(b"ab"); // only 2 bytes present
        assert_eq!(parse_picture_block(&bad_mime), Err(FormatError::Malformed));

        // :245 `> -> ==` (desc bound): valid mime, then claim desc_len past the end.
        let mut bad_desc = 3u32.to_be_bytes().to_vec();
        bad_desc.extend_from_slice(&3u32.to_be_bytes()); // mime_len = 3
        bad_desc.extend_from_slice(b"png");
        bad_desc.extend_from_slice(&16u32.to_be_bytes()); // desc_len = 16
        bad_desc.extend_from_slice(b"x"); // only 1 byte present
        assert_eq!(parse_picture_block(&bad_desc), Err(FormatError::Malformed));

        // :261 `> -> <` (data bound): a fully valid picture body with TRAILING bytes.
        // Original ignores the trailing byte (data_end < len, not >) and returns Ok;
        // the `<` mutant rejects (data_end < len -> Malformed).
        let mut trailing = picture_body(3, "png", "", 1, 1, b"DA");
        trailing.push(0xFF); // one extra trailing byte
        assert!(parse_picture_block(&trailing).is_ok());
    }

    #[test]
    fn read_pictures_extracts_and_guards_marker() {
        // Happy path: one PICTURE block, last, no audio (body_end == len). Pins :294.
        let pic = picture_body(3, "image/jpeg", "front", 8, 8, b"IMG");
        let file = flac_with(&[raw_block(BLOCK_PICTURE, &pic, true, None)]);
        let pics = read_pictures(&file).unwrap();
        assert_eq!(pics.len(), 1);
        assert_eq!(pics[0].mime, "image/jpeg");
        assert_eq!(pics[0].data, b"IMG");
        // :277 `< -> ==` and `|| -> &&`: 3-byte input -> panic vs NotFlac.
        assert_eq!(read_pictures(b"fLa"), Err(FormatError::NotFlac));
        // :277 `< -> <=`: 4-byte fLaC -> Malformed vs NotFlac.
        assert_eq!(read_pictures(b"fLaC"), Err(FormatError::Malformed));
    }

    #[test]
    fn read_pictures_guards_block_walk_and_length() {
        // :283 `+ -> -`, `> -> ==`: truncated header.
        assert_eq!(read_pictures(b"fLaC\x80"), Err(FormatError::Malformed));
        // :283 `> -> >=`: non-PICTURE last block flush with end -> Ok(empty).
        let none = flac_with(&[raw_block(BLOCK_STREAMINFO, &[], true, None)]);
        assert_eq!(read_pictures(&none).unwrap(), Vec::new());
        // High length byte over short body: pins the 24-bit decode's high byte.
        let hi = flac_with(&[raw_block(BLOCK_STREAMINFO, &[], true, Some(0x01_0000))]);
        assert_eq!(read_pictures(&hi), Err(FormatError::Malformed));
        // Mid length byte (high byte 0): pins the 24-bit decode's mid byte.
        let mid = flac_with(&[raw_block(BLOCK_STREAMINFO, &[], true, Some(0x00_0100))]);
        assert_eq!(read_pictures(&mid), Err(FormatError::Malformed));
    }

    // ---- read_metadata_bounded mutant-kill tests (flac.rs:89-133) ----

    #[test]
    fn bounded_rejects_short_and_wrong_marker() {
        // kills flac L90 `<` -> `==`/`<=` and `||` -> `&&`:
        // a 3-byte prefix is too short. Original short-circuits NotFlac; the `==`
        // mutant (len==4 is false for len 3) and the `&&` mutant force evaluation
        // of &prefix[0..4] on 3 bytes -> panic. NotFlac kills both.
        assert_eq!(read_metadata_bounded(b"fLa"), Err(FormatError::NotFlac));
        // kills flac L90 `<` -> `<=`: a 4-byte "fLaC"-only prefix is exactly the
        // marker. Original (len 4 < 4 is false) proceeds; since pos+4=8 > 4 it
        // returns NeedMore{up_to:8}. The `<=` mutant (4 <= 4 true) wrongly returns
        // NotFlac. Asserting NOT NotFlac (and the exact NeedMore) kills it.
        match read_metadata_bounded(b"fLaC").unwrap() {
            Extent::NeedMore { up_to } => assert_eq!(up_to, 8),
            other @ Extent::Complete(_) => panic!("expected NeedMore{{up_to:8}}, got {other:?}"),
        }
        // kills flac L90 marker check: a non-FLAC 4-byte prefix -> NotFlac.
        assert_eq!(read_metadata_bounded(b"XXXX"), Err(FormatError::NotFlac));
    }

    #[test]
    fn bounded_needmore_up_to_is_pos_plus_4_for_truncated_header() {
        // Marker + a non-last STREAMINFO (empty body) then truncated: after the
        // first block, pos = 4 + 4 + 0 = 8. The prefix ends exactly there, so the
        // loop guard `pos + 4 > prefix.len()` fires with pos == 8.
        let file = flac_with(&[raw_block(BLOCK_STREAMINFO, &[], false, None)]);
        // file is fLaC(4) + header(4) = 8 bytes; pos lands at 8 == prefix.len().
        assert_eq!(file.len(), 8);
        match read_metadata_bounded(&file).unwrap() {
            // kills flac L96 `pos + 4 > prefix.len()` `+` -> `-`: with `pos - 4`
            // (8-4=4) the comparison 4 > 8 is false, so it would NOT return NeedMore
            // and instead panic reading prefix[8..]. NeedMore here kills it.
            // kills flac L99 up_to `(pos + 4)` `+` -> `-`/`*`: pos==8 -> correct
            // up_to == 12. `pos - 4` gives 4; `pos * 4` gives 32. Exact 12 kills both.
            Extent::NeedMore { up_to } => assert_eq!(up_to, 12),
            other @ Extent::Complete(_) => panic!("expected NeedMore{{up_to:12}}, got {other:?}"),
        }
    }

    #[test]
    fn bounded_is_last_flag_continues_past_nonlast_block() {
        // Two blocks: first NON-last STREAMINFO (body 0xAA*2), then a LAST
        // STREAMINFO (body 0xBB*3) + no audio. audio_offset must span BOTH.
        let b1 = raw_block(BLOCK_STREAMINFO, &[0xAA, 0xAA], false, None); // 4+2=6
        let b2 = raw_block(BLOCK_STREAMINFO, &[0xBB, 0xBB, 0xBB], true, None); // 4+3=7
        let file = flac_with(&[b1, b2]);
        let expected_offset = (4 + 6 + 7) as u64; // marker + block1 + block2
        match read_metadata_bounded(&file).unwrap() {
            Extent::Complete(meta) => {
                // kills flac L103 `header & 0x80` `&` -> `|`: `header | 0x80` is always
                // nonzero -> is_last always true -> it would stop after block 1 with
                // audio_offset == 4+6 == 10. Spanning both blocks (17) kills it.
                assert_eq!(meta.audio_offset, expected_offset);
                // both STREAMINFO blocks preserved -> proves we walked past block 1.
                assert_eq!(meta.preserved.len(), 2);
            }
            other @ Extent::NeedMore { .. } => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn bounded_block_type_mask_preserves_streaminfo() {
        // A single last STREAMINFO (type 0) with a known body.
        let body = vec![0x5A; 8];
        let file = flac_with(&[raw_block(BLOCK_STREAMINFO, &body, true, None)]);
        match read_metadata_bounded(&file).unwrap() {
            Extent::Complete(meta) => {
                // kills flac L104 `header & 0x7F` `&` -> `|`/`^`: for a last STREAMINFO
                // the header byte is 0x80 (is_last set, type 0). Correct block_type =
                // 0x80 & 0x7F = 0. `0x80 | 0x7F` = 0xFF, `0x80 ^ 0x7F` = 0xFF -> neither
                // matches the STREAMINFO arm -> preserved stays empty. Asserting the
                // block IS preserved with block_type 0 kills both.
                assert_eq!(meta.preserved.len(), 1);
                assert_eq!(meta.preserved[0].block_type, BLOCK_STREAMINFO);
                assert_eq!(meta.preserved[0].body, body);
            }
            other @ Extent::NeedMore { .. } => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn bounded_decodes_24bit_length_exactly() {
        // Single last block whose declared length = 0x010203 (bytes 0x01,0x02,0x03),
        // exercising all three length positions. Body is that many bytes so the block
        // fits and we get a Complete with an exact audio_offset.
        let len = 0x01_0203usize;
        let body = vec![0u8; len];
        let file = flac_with(&[raw_block(BLOCK_STREAMINFO, &body, true, None)]);
        let expected_offset = (4 + 4 + len) as u64;
        match read_metadata_bounded(&file).unwrap() {
            Extent::Complete(meta) => {
                // The exact audio_offset pins all three bytes of the 24-bit length
                // decode: losing the high, mid, or low byte shifts body_end and
                // yields a wrong audio_offset.
                assert_eq!(meta.audio_offset, expected_offset);
            }
            other @ Extent::NeedMore { .. } => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn bounded_length_decodes_high_and_mid_bytes() {
        // Declare length 0x010100 (high byte 0x01, mid byte 0x01, low 0x00); correct
        // len = 65792. A decode that collapses the high or mid byte asks for a very
        // different body.
        // Use NeedMore: body is absent, so the correct parse asks for the full body.
        let file = flac_with(&[raw_block(BLOCK_STREAMINFO, &[], true, Some(0x01_0100))]);
        match read_metadata_bounded(&file).unwrap() {
            Extent::NeedMore { up_to } => {
                // body_start = 4 + 4 = 8; up_to = body_end = 8 + 0x010100.
                assert_eq!(up_to, 8 + 0x01_0100);
            }
            other @ Extent::Complete(_) => panic!("expected NeedMore, got {other:?}"),
        }
    }

    #[test]
    fn bounded_body_end_equal_to_prefix_is_complete() {
        // A single LAST STREAMINFO whose body ends EXACTLY at the prefix end (no
        // audio). body_end == prefix.len().
        let body = vec![0xCC; 6];
        let file = flac_with(&[raw_block(BLOCK_STREAMINFO, &body, true, None)]);
        let total = file.len() as u64; // 4 + 4 + 6 == 14
        match read_metadata_bounded(&file).unwrap() {
            // kills flac L110 `body_end > prefix.len()` `>` -> `>=`: with `>=`,
            // body_end == prefix.len() is true -> wrongly returns NeedMore. Original
            // (`>`) proceeds and, since is_last, returns Complete{audio_offset==len}.
            Extent::Complete(meta) => assert_eq!(meta.audio_offset, total),
            other @ Extent::NeedMore { .. } => {
                panic!("expected Complete (exact fit), got {other:?}")
            }
        }
    }

    #[test]
    fn bounded_preserves_all_structural_block_types() {
        // kills flac L116 (delete the STREAMINFO|APPLICATION|SEEKTABLE|CUESHEET arm):
        // a prefix containing each preserved type must yield all four in `preserved`.
        // Deleting the arm makes `preserved` empty -> these assertions fail.
        let b_si = raw_block(BLOCK_STREAMINFO, &[0x01], false, None);
        let b_app = raw_block(BLOCK_APPLICATION, &[0x02, 0x02], false, None);
        let b_seek = raw_block(BLOCK_SEEKTABLE, &[0x03, 0x03, 0x03], false, None);
        let b_cue = raw_block(BLOCK_CUESHEET, &[0x04], true, None);
        let file = flac_with(&[b_si, b_app, b_seek, b_cue]);
        match read_metadata_bounded(&file).unwrap() {
            Extent::Complete(meta) => {
                let types: Vec<u8> = meta.preserved.iter().map(|b| b.block_type).collect();
                assert_eq!(
                    types,
                    vec![
                        BLOCK_STREAMINFO,
                        BLOCK_APPLICATION,
                        BLOCK_SEEKTABLE,
                        BLOCK_CUESHEET,
                    ]
                );
                assert_eq!(meta.preserved[0].body, vec![0x01]);
                assert_eq!(meta.preserved[1].body, vec![0x02, 0x02]);
                assert_eq!(meta.preserved[2].body, vec![0x03, 0x03, 0x03]);
                assert_eq!(meta.preserved[3].body, vec![0x04]);
            }
            other @ Extent::NeedMore { .. } => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn split_preserved_classifies_structural_and_binary() {
        use super::{MetadataBlock, split_preserved, structural_block_type};
        // STREAMINFO(0), APPLICATION(2), SEEKTABLE(3), CUESHEET(5) in arbitrary order.
        let blocks = vec![
            MetadataBlock {
                block_type: 0,
                body: vec![0xAA],
            },
            MetadataBlock {
                block_type: 2,
                body: b"testDATA".to_vec(),
            },
            MetadataBlock {
                block_type: 3,
                body: vec![0xBB],
            },
            MetadataBlock {
                block_type: 5,
                body: vec![0xCC; 4],
            },
        ];
        let (structural, binary) = split_preserved(&blocks);

        assert_eq!(
            structural,
            vec![
                ("STREAMINFO".to_string(), vec![0xAA]),
                ("SEEKTABLE".to_string(), vec![0xBB]),
            ]
        );
        assert_eq!(binary.len(), 2);
        assert_eq!(binary[0].key, "APPLICATION");
        assert_eq!(binary[0].payload, b"testDATA");
        assert_eq!(binary[1].key, "CUESHEET");
        assert_eq!(binary[1].payload, vec![0xCC; 4]);

        assert_eq!(structural_block_type("STREAMINFO"), Some(0));
        assert_eq!(structural_block_type("SEEKTABLE"), Some(3));
        assert_eq!(structural_block_type("APPLICATION"), None);
        assert_eq!(structural_block_type("bogus"), None);
    }

    #[test]
    fn synthesize_layout_picture_block_size_boundary_is_inclusive() {
        // body_len = picture_body_framing(art).len() + art.data_len. The guard at
        // flac.rs rejects body_len > 0x00FF_FFFF (FLAC's 24-bit block length).
        let mk = |data_len: u64| ArtInput {
            art_id: 1,
            mime: "image/png".to_string(),
            description: String::new(),
            picture_type: 3,
            width: 0,
            height: 0,
            data_len,
        };
        // Derive the exact framing length from production rather than hardcoding it
        // (it is independent of the data_len *value* — that field is always 4 bytes).
        // This keeps the boundary correct regardless of the framing's field count.
        let framing_len = picture_body_framing(&mk(0)).unwrap().len() as u64;
        let at_limit = 0x00FF_FFFF - framing_len; // body_len == 0x00FF_FFFF exactly
        // original `>` accepts the inclusive boundary; the `>=` mutant rejects it.
        // (data_len is only a count; no large allocation occurs.)
        assert!(synthesize_layout(&[], 0, 0, &[], &[], &[mk(at_limit)]).is_ok());
        // one byte over must still error, pinning the high side of the boundary.
        assert_eq!(
            synthesize_layout(&[], 0, 0, &[], &[], &[mk(at_limit + 1)]),
            Err(FormatError::TooLarge)
        );
    }

    #[test]
    fn synthesize_layout_vorbis_comment_block_size_boundary_is_inclusive() {
        // The regenerated VORBIS_COMMENT body must also fit FLAC's 24-bit block
        // length. Derive the non-value overhead from production, then size the
        // value so the body lands exactly on the limit; one more byte errors.
        // Mirrors the PICTURE/binary-tag boundary tests: the `>` accepts the
        // inclusive limit while the `>=` mutant rejects it.
        let overhead = crate::vorbiscomment::build(&[TagInput::new("title", "")])
            .unwrap()
            .len() as u64;
        let at_limit = "x".repeat(crate::convert::usize_from(MAX_BLOCK_BODY - overhead));
        let tags = [TagInput::new("title", at_limit.as_str())];
        assert!(synthesize_layout(&[], 0, 0, &tags, &[], &[]).is_ok());
        // one byte over must still error, pinning the high side of the boundary.
        let over = format!("{at_limit}x");
        let tags = [TagInput::new("title", over.as_str())];
        assert_eq!(
            synthesize_layout(&[], 0, 0, &tags, &[], &[]),
            Err(FormatError::TooLarge)
        );
    }

    #[test]
    fn synthesize_layout_binary_tag_block_size_boundary_is_inclusive() {
        // The binary-tag guard rejects bt.len > 0x00FF_FFFF (FLAC's 24-bit block
        // length). `len` is only a count — no payload is allocated — so the exact
        // boundary is cheap to pin. Mirrors the PICTURE boundary test; the `>`
        // accepts the inclusive limit while the `>=` mutant rejects it.
        let mk = |len: u64| BinaryTagInput {
            key: "APPLICATION".to_string(),
            payload_id: 1,
            len,
        };
        // len == 0x00FF_FFFF exactly must succeed.
        assert!(synthesize_layout(&[], 0, 0, &[], &[mk(0x00FF_FFFF)], &[]).is_ok());
        // one byte over must still error, pinning the high side of the boundary.
        assert_eq!(
            synthesize_layout(&[], 0, 0, &[], &[mk(0x0100_0000)], &[]),
            Err(FormatError::TooLarge)
        );
    }
}
