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

pub(crate) fn push_block_header(out: &mut Vec<u8>, block_type: u8, body_len: usize, is_last: bool) {
    let first = (if is_last { 0x80 } else { 0 }) | (block_type & 0x7F);
    out.push(first);
    out.push(((body_len >> 16) & 0xFF) as u8);
    out.push(((body_len >> 8) & 0xFF) as u8);
    out.push((body_len & 0xFF) as u8);
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
pub fn synthesize_layout(
    scan: &FlacScan,
    tags: &[TagInput],
    arts: &[ArtInput],
) -> Result<RegionLayout> {
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

    let vc = crate::vorbiscomment::build(tags);
    push_block_header(&mut buf, BLOCK_VORBIS_COMMENT, vc.len(), idx == last_index);
    buf.extend_from_slice(&vc);
    idx += 1;

    for art in arts {
        let framing = picture_body_framing(art);
        let body_len = framing.len() as u64 + art.data_len;
        // FLAC metadata block lengths are 24-bit (max ~16 MiB). Ingestion caps art
        // well under this, but guard at the format boundary so an oversized block is
        // a hard error rather than a silently-truncated (corrupt) file.
        if body_len > 0x00FF_FFFF {
            return Err(FormatError::TooLarge);
        }
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

    RegionLayout::validated(segments).map_err(|_| FormatError::InvalidLayout)
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
            return crate::vorbiscomment::parse(&data[body_start..body_end]);
        }
        pos = body_end;
        if is_last {
            break;
        }
    }
    Ok(Vec::new())
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

#[cfg(test)]
mod tests {
    use super::*;

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
        push_block_header(&mut out, BLOCK_PICTURE, 0x12_3456, false);
        assert_eq!(out, vec![BLOCK_PICTURE, 0x12, 0x34, 0x56]);
        // :99 is equivalent, but exercise the is_last/0x80 path anyway.
        let mut last = Vec::new();
        push_block_header(&mut last, BLOCK_VORBIS_COMMENT, 0, true);
        assert_eq!(last, vec![0x80 | BLOCK_VORBIS_COMMENT, 0x00, 0x00, 0x00]);
    }

    /// One FLAC metadata block: 4-byte header (last-flag, type, 24-bit BE length)
    /// + body, built independently of production framing so a mutation in
    ///   `push_block_header` cannot mask a fixture. `len_override` lets a test claim a
    ///   length different from `body.len()`.
    fn raw_block(block_type: u8, body: &[u8], last: bool, len_override: Option<usize>) -> Vec<u8> {
        let n = len_override.unwrap_or(body.len());
        let mut v = vec![(if last { 0x80 } else { 0 }) | (block_type & 0x7F)];
        v.push((n >> 16) as u8);
        v.push((n >> 8) as u8);
        v.push(n as u8);
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
        // Original: len = 65536 -> body_end > data.len() -> Malformed.
        // :49 `<<16 -> >>16`: (0x01 >> 16) = 0 -> len = 0 -> body fits -> Ok.
        // (:50/:51 `| -> ^` are equivalent here: the shifted bytes are disjoint.)
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
        v.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        v.extend_from_slice(vendor.as_bytes());
        v.extend_from_slice(&(comments.len() as u32).to_le_bytes());
        for c in comments {
            v.extend_from_slice(&(c.len() as u32).to_le_bytes());
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
        // :199 `<<16 -> >>16` AND :200 `| -> &`: high length byte set over a short
        // body. Original len = 0x10000 -> Malformed. `>>16` -> len 0 -> Ok; `&` ->
        // (0x10000 & 0) -> len 0 -> Ok. Either mutant returns Ok instead of Malformed.
        let hi = flac_with(&[raw_block(BLOCK_STREAMINFO, &[], true, Some(0x01_0000))]);
        assert_eq!(read_vorbis_comments(&hi), Err(FormatError::Malformed));
        // :200 `<<8 -> >>8`: mid length byte set, high byte 0. Original len = 0x100
        // -> Malformed; `>>8` -> len 0 -> Ok.
        let mid = flac_with(&[raw_block(BLOCK_STREAMINFO, &[], true, Some(0x00_0100))]);
        assert_eq!(read_vorbis_comments(&mid), Err(FormatError::Malformed));
        // (:200 `| -> ^` and :201 `| -> ^` are equivalent: disjoint shifted bytes.)
    }

    /// A FLAC PICTURE block body (big-endian fields), independent of production.
    fn picture_body(ptype: u32, mime: &str, desc: &str, w: u32, h: u32, data: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&ptype.to_be_bytes());
        v.extend_from_slice(&(mime.len() as u32).to_be_bytes());
        v.extend_from_slice(mime.as_bytes());
        v.extend_from_slice(&(desc.len() as u32).to_be_bytes());
        v.extend_from_slice(desc.as_bytes());
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&0u32.to_be_bytes()); // depth
        v.extend_from_slice(&0u32.to_be_bytes()); // colors
        v.extend_from_slice(&(data.len() as u32).to_be_bytes());
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
        // :289 `<<16 -> >>16` and :290 `| -> &`: high length byte over short body.
        let hi = flac_with(&[raw_block(BLOCK_STREAMINFO, &[], true, Some(0x01_0000))]);
        assert_eq!(read_pictures(&hi), Err(FormatError::Malformed));
        // :290 `<<8 -> >>8`: mid length byte, high byte 0.
        let mid = flac_with(&[raw_block(BLOCK_STREAMINFO, &[], true, Some(0x00_0100))]);
        assert_eq!(read_pictures(&mid), Err(FormatError::Malformed));
    }
}
