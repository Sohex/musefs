//! Hand-rolled MP4/M4A box layer: parse the structure, read iTunes metadata, and
//! regenerate `moov` (with patched chunk offsets) to synthesize a re-tagged file
//! whose `mdat` audio payload is served verbatim. Strict: anything outside the
//! supported shape (single audio track, one `mdat`, non-fragmented) is rejected.

use crate::error::{FormatError, Result};
use crate::input::{ArtInput, EmbeddedPicture, TagInput};
use crate::layout::{RegionLayout, Segment};

fn be_u32(b: &[u8], pos: usize) -> Result<u32> {
    let s = b.get(pos..pos + 4).ok_or(FormatError::Malformed)?;
    Ok(u32::from_be_bytes(s.try_into().unwrap()))
}

fn be_u64(b: &[u8], pos: usize) -> Result<u64> {
    let s = b.get(pos..pos + 8).ok_or(FormatError::Malformed)?;
    Ok(u64::from_be_bytes(s.try_into().unwrap()))
}

/// A located box header within some buffer. `start` is relative to that buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BoxRef {
    kind: [u8; 4],
    start: usize,
    header_len: usize, // 8, or 16 for 64-bit largesize
    total_len: usize,  // header + payload
}

impl BoxRef {
    fn payload_start(&self) -> usize {
        self.start + self.header_len
    }
    fn end(&self) -> usize {
        self.start + self.total_len
    }
    /// `buf` must be the same buffer `read_box` parsed this header from — offsets
    /// are relative to it. The debug assertion catches a wrong-buffer call in tests.
    fn payload<'a>(&self, buf: &'a [u8]) -> &'a [u8] {
        debug_assert!(
            self.end() <= buf.len(),
            "BoxRef::payload called with a buffer it was not parsed from"
        );
        &buf[self.payload_start()..self.end()]
    }
}

fn read_box(buf: &[u8], pos: usize) -> Result<BoxRef> {
    let size32 = be_u32(buf, pos)? as u64;
    let kind: [u8; 4] = buf
        .get(pos + 4..pos + 8)
        .ok_or(FormatError::Malformed)?
        .try_into()
        .unwrap();
    let (header_len, total) = match size32 {
        1 => (16usize, be_u64(buf, pos + 8)?),
        0 => (8usize, (buf.len() - pos) as u64),
        n => (8usize, n),
    };
    let total = total as usize;
    if total < header_len || pos + total > buf.len() {
        return Err(FormatError::Malformed);
    }
    Ok(BoxRef {
        kind,
        start: pos,
        header_len,
        total_len: total,
    })
}

fn child_boxes(buf: &[u8]) -> Result<Vec<BoxRef>> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos + 8 <= buf.len() {
        let b = read_box(buf, pos)?;
        pos = b.end();
        out.push(b);
    }
    Ok(out)
}

fn find_box(buf: &[u8], kind: &[u8; 4]) -> Result<Option<BoxRef>> {
    Ok(child_boxes(buf)?.into_iter().find(|b| &b.kind == kind))
}

/// Descend a path of box types; return `(payload_start, payload_len)` relative to
/// `buf` for the box at the end of the path, or None if any step is missing.
fn find_path(buf: &[u8], path: &[&[u8; 4]]) -> Result<Option<(usize, usize)>> {
    let mut base = 0usize;
    let mut last = None;
    for kind in path {
        let region = &buf[base..];
        let b = match find_box(region, kind)? {
            Some(b) => b,
            None => return Ok(None),
        };
        let ps = base + b.payload_start();
        last = Some((ps, b.total_len - b.header_len));
        base = ps;
    }
    Ok(last)
}

/// Audio payload bounds within the backing file (the verbatim `mdat` payload).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mp4Bounds {
    pub audio_offset: u64,
    pub audio_length: u64,
}

/// Validate the supported shape; return the ftyp/moov/mdat boxes (absolute offsets
/// in `buf`). Rejects fragmented, video, multi-track, and multi-`mdat` files.
fn locate(buf: &[u8]) -> Result<(BoxRef, BoxRef, BoxRef)> {
    let top = child_boxes(buf).map_err(|_| FormatError::NotMp4)?;
    if top.iter().any(|b| &b.kind == b"moof") {
        return Err(FormatError::NotMp4);
    }
    let one = |kind: &[u8; 4]| -> Result<BoxRef> {
        let mut it = top.iter().filter(|b| &b.kind == kind);
        let first = it.next().copied().ok_or(FormatError::NotMp4)?;
        if it.next().is_some() {
            return Err(FormatError::NotMp4);
        }
        Ok(first)
    };
    let ftyp = one(b"ftyp")?;
    let moov = one(b"moov")?;
    let mdat = one(b"mdat")?;

    let moov_payload = moov.payload(buf);
    if find_box(moov_payload, b"mvex")?.is_some() {
        return Err(FormatError::NotMp4);
    }
    let traks: Vec<_> = child_boxes(moov_payload)?
        .into_iter()
        .filter(|b| &b.kind == b"trak")
        .collect();
    if traks.len() != 1 {
        return Err(FormatError::NotMp4);
    }
    let trak = traks[0].payload(moov_payload);
    let (hp, hl) = find_path(trak, &[b"mdia", b"hdlr"])?.ok_or(FormatError::NotMp4)?;
    if trak[hp..hp + hl].get(8..12) != Some(b"soun") {
        return Err(FormatError::NotMp4);
    }
    Ok((ftyp, moov, mdat))
}

/// Parse the file and return the `mdat` payload bounds, or an error to skip it.
pub fn locate_audio(buf: &[u8]) -> Result<Mp4Bounds> {
    let (_ftyp, _moov, mdat) = locate(buf)?;
    Ok(Mp4Bounds {
        audio_offset: mdat.payload_start() as u64,
        audio_length: (mdat.total_len - mdat.header_len) as u64,
    })
}

/// Everything `synthesize_layout` needs, read from the backing file once.
#[derive(Debug, Clone)]
pub struct Mp4Scan {
    pub ftyp: Vec<u8>,
    pub moov: Vec<u8>,
    pub mdat_header: Vec<u8>,
    pub mdat_payload_offset: u64,
    pub mdat_payload_len: u64,
}

pub fn read_structure(buf: &[u8]) -> Result<Mp4Scan> {
    let (ftyp, moov, mdat) = locate(buf)?;
    Ok(Mp4Scan {
        ftyp: buf[ftyp.start..ftyp.end()].to_vec(),
        moov: buf[moov.start..moov.end()].to_vec(),
        mdat_header: buf[mdat.start..mdat.payload_start()].to_vec(),
        mdat_payload_offset: mdat.payload_start() as u64,
        mdat_payload_len: (mdat.total_len - mdat.header_len) as u64,
    })
}

fn atom_to_key(kind: &[u8; 4]) -> Option<&'static str> {
    Some(match kind {
        b"\xa9nam" => "title",
        b"\xa9ART" => "artist",
        b"aART" => "albumartist",
        b"\xa9alb" => "album",
        b"\xa9gen" => "genre",
        b"\xa9day" => "date",
        b"\xa9wrt" => "composer",
        _ => return None,
    })
}

/// Locate `moov/udta/meta/ilst`; `meta` is a FullBox (4 version/flags bytes before
/// its children). Returns the ilst payload range absolute within `buf`.
fn ilst_region(buf: &[u8]) -> Option<(usize, usize)> {
    let moov = find_box(buf, b"moov").ok()??;
    let mp = moov.payload(buf);
    let base = moov.payload_start();
    let (up, ul) = find_path(mp, &[b"udta"]).ok()??;
    let udta = &mp[up..up + ul];
    let meta = find_box(udta, b"meta").ok()??;
    let meta_children = udta.get(meta.payload_start() + 4..meta.end())?;
    let il = find_box(meta_children, b"ilst").ok()??;
    let start = base + up + meta.payload_start() + 4 + il.payload_start();
    Some((start, il.total_len - il.header_len))
}

/// Lenient: returns empty / skips any malformed atom and never errors — this only
/// seeds metadata from existing files, so a missing or garbled tag must simply be absent.
pub fn read_tags(buf: &[u8]) -> Vec<(String, String)> {
    let (start, len) = match ilst_region(buf) {
        Some(r) => r,
        None => return Vec::new(),
    };
    let ilst = &buf[start..start + len];
    let mut out = Vec::new();
    for atom in child_boxes(ilst).unwrap_or_default() {
        let inner = atom.payload(ilst);
        let data = match find_box(inner, b"data") {
            Ok(Some(d)) => d,
            _ => continue,
        };
        let dp = data.payload(inner);
        if dp.len() < 8 {
            continue;
        }
        let value = &dp[8..]; // skip [type 4][locale 4]
        if let Some(key) = atom_to_key(&atom.kind) {
            if let Ok(s) = std::str::from_utf8(value) {
                out.push((key.to_string(), s.to_string()));
            }
        } else if &atom.kind == b"trkn" && value.len() >= 4 {
            out.push((
                "tracknumber".into(),
                u16::from_be_bytes([value[2], value[3]]).to_string(),
            ));
        } else if &atom.kind == b"disk" && value.len() >= 4 {
            out.push((
                "discnumber".into(),
                u16::from_be_bytes([value[2], value[3]]).to_string(),
            ));
        }
    }
    out
}

/// Lenient: returns empty / skips any malformed atom and never errors — this only
/// seeds cover art from existing files, so a missing or garbled picture must simply be absent.
pub fn read_pictures(buf: &[u8]) -> Vec<EmbeddedPicture> {
    let (start, len) = match ilst_region(buf) {
        Some(r) => r,
        None => return Vec::new(),
    };
    let ilst = &buf[start..start + len];
    let mut out = Vec::new();
    for atom in child_boxes(ilst).unwrap_or_default() {
        if &atom.kind != b"covr" {
            continue;
        }
        let inner = atom.payload(ilst);
        let data = match find_box(inner, b"data") {
            Ok(Some(d)) => d,
            _ => continue,
        };
        let dp = data.payload(inner);
        if dp.len() < 8 {
            continue;
        }
        let mime = match u32::from_be_bytes([dp[0], dp[1], dp[2], dp[3]]) {
            13 => "image/jpeg",
            14 => "image/png",
            _ => continue,
        };
        out.push(EmbeddedPicture {
            mime: mime.to_string(),
            picture_type: 3,
            description: String::new(),
            width: 0,
            height: 0,
            data: dp[8..].to_vec(),
        });
    }
    out
}

fn boxed(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut v = ((8 + payload.len()) as u32).to_be_bytes().to_vec();
    v.extend_from_slice(kind);
    v.extend_from_slice(payload);
    v
}

fn text_atom(kind: &[u8; 4], values: &[&str]) -> Vec<u8> {
    let mut inner = Vec::new();
    for v in values {
        let mut data = 1u32.to_be_bytes().to_vec(); // type 1 = UTF-8
        data.extend_from_slice(&0u32.to_be_bytes()); // locale
        data.extend_from_slice(v.as_bytes());
        inner.extend(boxed(b"data", &data));
    }
    boxed(kind, &inner)
}

fn number_atom(kind: &[u8; 4], n: u16, width: usize) -> Vec<u8> {
    debug_assert!(
        width >= 4,
        "number_atom width must hold the 4-byte reserved+value prefix"
    );
    let mut data = 0u32.to_be_bytes().to_vec(); // type 0 = binary
    data.extend_from_slice(&0u32.to_be_bytes()); // locale
    let mut body = vec![0u8, 0];
    body.extend_from_slice(&n.to_be_bytes());
    body.resize(width, 0);
    data.extend_from_slice(&body);
    boxed(kind, &boxed(b"data", &data))
}

fn meta_key(key: &str) -> Option<&'static [u8; 4]> {
    Some(match key {
        "title" => b"\xa9nam",
        "artist" => b"\xa9ART",
        "albumartist" => b"aART",
        "album" => b"\xa9alb",
        "genre" => b"\xa9gen",
        "date" => b"\xa9day",
        "composer" => b"\xa9wrt",
        _ => return None,
    })
}

/// Build `udta` up to (not including) the cover image bytes. Returns (prefix,
/// art_len). No art → prefix is the complete udta, art_len 0. All enclosing box
/// sizes include art_len so the image can stream right after the prefix.
fn build_udta(tags: &[TagInput], art: Option<&ArtInput>) -> Result<(Vec<u8>, u64)> {
    // Group consecutive same-key text values (DB returns tags ordered by key).
    let mut text: Vec<(&str, Vec<&str>)> = Vec::new();
    for t in tags {
        if meta_key(&t.key).is_some() {
            match text.last_mut() {
                Some(g) if g.0 == t.key => g.1.push(&t.value),
                _ => text.push((&t.key, vec![&t.value])),
            }
        }
    }

    let mut ilst = Vec::new();
    for (key, values) in &text {
        ilst.extend(text_atom(meta_key(key).unwrap(), values));
    }
    for t in tags {
        if t.key == "tracknumber" {
            if let Ok(n) = t.value.parse::<u16>() {
                ilst.extend(number_atom(b"trkn", n, 8));
            }
        } else if t.key == "discnumber" {
            if let Ok(n) = t.value.parse::<u16>() {
                ilst.extend(number_atom(b"disk", n, 6));
            }
        }
    }

    let mut hdlr_body = vec![0u8; 8];
    hdlr_body.extend_from_slice(b"mdir");
    hdlr_body.extend_from_slice(b"appl");
    hdlr_body.extend_from_slice(&[0u8; 9]);
    let hdlr = boxed(b"hdlr", &hdlr_body);

    let art_len = art.map(|a| a.data_len).unwrap_or(0);

    if let Some(a) = art {
        let type_code: u32 = if a.mime == "image/png" { 14 } else { 13 };
        let data_size = 8 + 8 + a.data_len; // data header + type + locale + image
        let covr_size = 8 + data_size;
        ilst.extend_from_slice(&(covr_size as u32).to_be_bytes());
        ilst.extend_from_slice(b"covr");
        ilst.extend_from_slice(&(data_size as u32).to_be_bytes());
        ilst.extend_from_slice(b"data");
        ilst.extend_from_slice(&type_code.to_be_bytes());
        ilst.extend_from_slice(&0u32.to_be_bytes()); // locale; image streams next
    }

    let ilst_size = 8 + ilst.len() as u64 + art_len;
    let mut meta = 0u32.to_be_bytes().to_vec(); // FullBox version/flags
    meta.extend_from_slice(&hdlr);
    meta.extend_from_slice(&(ilst_size as u32).to_be_bytes());
    meta.extend_from_slice(b"ilst");
    meta.extend_from_slice(&ilst);
    let meta_size = 8 + meta.len() as u64 + art_len;

    let mut udta = (meta_size as u32).to_be_bytes().to_vec();
    udta.extend_from_slice(b"meta");
    udta.extend_from_slice(&meta);
    let udta_size = 8 + udta.len() as u64 + art_len;

    // MP4 box sizes are 32-bit. udta encloses all the inner boxes, so guarding
    // its size bounds them all; refuse oversized metadata at the format boundary
    // rather than emit a silently-truncated (corrupt) size field.
    if udta_size > u32::MAX as u64 {
        return Err(FormatError::TooLarge);
    }

    let mut out = (udta_size as u32).to_be_bytes().to_vec();
    out.extend_from_slice(b"udta");
    out.extend_from_slice(&udta);
    Ok((out, art_len))
}

/// Patch every `stco` (4-byte) or `co64` (8-byte) chunk offset in `kept` (moov
/// children minus udta) by `delta`. Errors if a 32-bit offset would overflow.
fn patch_chunk_offsets(kept: &mut [u8], delta: i64) -> Result<()> {
    let (range, entry) = match find_path(kept, &[b"trak", b"mdia", b"minf", b"stbl", b"stco"])? {
        Some(r) => (r, 4usize),
        None => match find_path(kept, &[b"trak", b"mdia", b"minf", b"stbl", b"co64"])? {
            Some(r) => (r, 8usize),
            None => return Err(FormatError::Malformed),
        },
    };
    let (start, len) = range;
    let count = be_u32(kept, start + 4)? as usize;
    for i in 0..count {
        let pos = start + 8 + i * entry;
        if pos + entry > start + len {
            return Err(FormatError::Malformed);
        }
        if entry == 4 {
            let v = be_u32(kept, pos)? as i64 + delta;
            if v < 0 || v > u32::MAX as i64 {
                return Err(FormatError::TooLarge);
            }
            kept[pos..pos + 4].copy_from_slice(&(v as u32).to_be_bytes());
        } else {
            let v = be_u64(kept, pos)? as i64 + delta;
            if v < 0 {
                return Err(FormatError::Malformed);
            }
            kept[pos..pos + 8].copy_from_slice(&(v as u64).to_be_bytes());
        }
    }
    Ok(())
}

/// Regenerate a re-tagged `moov` and produce the serving layout
/// `[ftyp][regenerated moov][mdat header][mdat payload]`. The mdat payload is
/// served verbatim, merely relocated, so every chunk offset shifts by a constant
/// `delta`. Patching only offset VALUES (never box sizes) means `new_moov_size`
/// is computable before `delta` — no circular dependency. With cover art the
/// layout splits so the image streams from the DB blob at read time.
pub fn synthesize_layout(
    scan: &Mp4Scan,
    tags: &[TagInput],
    arts: &[ArtInput],
) -> Result<RegionLayout> {
    let moov_payload_start = read_box(&scan.moov, 0)?.payload_start();
    let moov_payload = &scan.moov[moov_payload_start..];
    let mut kept = Vec::new();
    for b in child_boxes(moov_payload)? {
        if &b.kind != b"udta" {
            kept.extend_from_slice(&moov_payload[b.start..b.end()]);
        }
    }

    let art = arts.first();
    let (udta_prefix, art_len) = build_udta(tags, art)?;
    let udta_total = udta_prefix.len() as u64 + art_len;

    let new_moov_size = 8 + kept.len() as u64 + udta_total;
    // MP4 box sizes are 32-bit; mirror build_udta's guard so a moov that grows
    // past u32 (e.g. huge art) errors at the format boundary rather than emitting
    // a truncated, corrupt size field.
    if new_moov_size > u32::MAX as u64 {
        return Err(FormatError::TooLarge);
    }
    let new_mdat_payload_pos =
        scan.ftyp.len() as u64 + new_moov_size + scan.mdat_header.len() as u64;
    let delta = new_mdat_payload_pos as i64 - scan.mdat_payload_offset as i64;

    patch_chunk_offsets(&mut kept, delta)?;

    let mut head = Vec::new();
    head.extend_from_slice(&scan.ftyp);
    head.extend_from_slice(&(new_moov_size as u32).to_be_bytes());
    head.extend_from_slice(b"moov");
    head.extend_from_slice(&kept);
    head.extend_from_slice(&udta_prefix);

    let mut segments = Vec::new();
    if let Some(a) = art {
        segments.push(Segment::Inline(head));
        segments.push(Segment::ArtImage {
            art_id: a.art_id,
            len: a.data_len,
        });
        segments.push(Segment::Inline(scan.mdat_header.clone()));
    } else {
        head.extend_from_slice(&scan.mdat_header);
        segments.push(Segment::Inline(head));
    }
    segments.push(Segment::BackingAudio {
        offset: scan.mdat_payload_offset,
        len: scan.mdat_payload_len,
    });
    Ok(RegionLayout::new(segments))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a 32-bit-size box: [size][type][payload].
    fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = ((8 + payload.len()) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn walks_top_level_boxes() {
        let mut buf = bx(b"ftyp", b"M4A ");
        buf.extend(bx(b"free", b"\x00\x00"));
        let boxes = child_boxes(&buf).unwrap();
        assert_eq!(boxes.len(), 2);
        assert_eq!(&boxes[0].kind, b"ftyp");
        assert_eq!(boxes[0].payload(&buf), b"M4A ");
        assert_eq!(&boxes[1].kind, b"free");
    }

    #[test]
    fn find_box_and_nested_path() {
        let mut hdlr_payload = vec![0u8; 8];
        hdlr_payload.extend_from_slice(b"soun");
        hdlr_payload.extend_from_slice(&[0u8; 12]);
        let moov = bx(
            b"moov",
            &bx(b"trak", &bx(b"mdia", &bx(b"hdlr", &hdlr_payload))),
        );

        let m = find_box(&moov, b"moov").unwrap().unwrap();
        let (start, len) = find_path(m.payload(&moov), &[b"trak", b"mdia", b"hdlr"])
            .unwrap()
            .unwrap();
        assert_eq!(&m.payload(&moov)[start..start + len][8..12], b"soun");
    }

    #[test]
    fn rejects_truncated_box() {
        let buf = [0u8, 0, 0, 99, b'm', b'o', b'o', b'v']; // claims 99, only 8 present
        assert!(child_boxes(&buf).is_err());
    }

    /// Minimal accepted MP4: ftyp, then (per `moov_first`) moov(one soun trak with
    /// an stco) and mdat. `mdat_payload` is the verbatim audio.
    fn mk_mp4(moov_first: bool, mdat_payload: &[u8], stco_entries: &[u32]) -> Vec<u8> {
        let mut stco = vec![0u8; 4];
        stco.extend_from_slice(&(stco_entries.len() as u32).to_be_bytes());
        for e in stco_entries {
            stco.extend_from_slice(&e.to_be_bytes());
        }
        let mut hdlr_p = vec![0u8; 8];
        hdlr_p.extend_from_slice(b"soun");
        hdlr_p.extend_from_slice(&[0u8; 12]);
        let minf = bx(b"minf", &bx(b"stbl", &bx(b"stco", &stco)));
        let mdia = bx(b"mdia", &[bx(b"hdlr", &hdlr_p), minf].concat());
        let trak = bx(b"trak", &mdia);
        let moov = bx(b"moov", &[bx(b"mvhd", &[0u8; 8]), trak].concat());
        let mdat = bx(b"mdat", mdat_payload);
        let ftyp = bx(b"ftyp", b"M4A isom");
        if moov_first {
            [ftyp, moov, mdat].concat()
        } else {
            [ftyp, mdat, moov].concat()
        }
    }

    #[test]
    fn locates_audio_moov_first_and_last() {
        for moov_first in [true, false] {
            let buf = mk_mp4(moov_first, b"AUDIODATA", &[0]);
            let b = locate_audio(&buf).unwrap();
            assert_eq!(b.audio_length, 9);
            assert_eq!(&buf[b.audio_offset as usize..][..9], b"AUDIODATA");
        }
    }

    #[test]
    fn rejects_fragmented_video_and_multi_mdat() {
        let base = mk_mp4(true, b"X", &[0]);
        let mut frag = base.clone();
        frag.extend(bx(b"moof", b"\x00"));
        assert!(locate_audio(&frag).is_err());

        let mut two = base.clone();
        two.extend(bx(b"mdat", b"Y"));
        assert!(locate_audio(&two).is_err());

        let mut hdlr_p = vec![0u8; 8];
        hdlr_p.extend_from_slice(b"vide");
        hdlr_p.extend_from_slice(&[0u8; 12]);
        let video_moov = bx(b"moov", &bx(b"trak", &bx(b"mdia", &bx(b"hdlr", &hdlr_p))));
        let vbuf = [bx(b"ftyp", b"M4A "), video_moov, bx(b"mdat", b"Z")].concat();
        assert!(locate_audio(&vbuf).is_err());
    }

    /// A `soun` trak built the way `mk_mp4` does (hdlr + minf/stbl/stco), for
    /// reuse when hand-assembling a moov to exercise a specific reject branch.
    fn soun_trak() -> Vec<u8> {
        let mut stco = vec![0u8; 4];
        stco.extend_from_slice(&1u32.to_be_bytes());
        stco.extend_from_slice(&0u32.to_be_bytes());
        let mut hdlr_p = vec![0u8; 8];
        hdlr_p.extend_from_slice(b"soun");
        hdlr_p.extend_from_slice(&[0u8; 12]);
        let minf = bx(b"minf", &bx(b"stbl", &bx(b"stco", &stco)));
        let mdia = bx(b"mdia", &[bx(b"hdlr", &hdlr_p), minf].concat());
        bx(b"trak", &mdia)
    }

    #[test]
    fn rejects_mvex_in_moov() {
        // A moov carrying an mvex box (movie-extends header => fragmented) is
        // rejected even though it otherwise holds a single valid soun trak.
        let moov = bx(
            b"moov",
            &[bx(b"mvhd", &[0u8; 8]), bx(b"mvex", b"\x00"), soun_trak()].concat(),
        );
        let buf = [bx(b"ftyp", b"M4A isom"), moov, bx(b"mdat", b"X")].concat();
        assert!(locate_audio(&buf).is_err());
    }

    #[test]
    fn rejects_multi_trak() {
        // Two trak children in moov is rejected (musefs serves single-track audio).
        let moov = bx(
            b"moov",
            &[bx(b"mvhd", &[0u8; 8]), soun_trak(), soun_trak()].concat(),
        );
        let buf = [bx(b"ftyp", b"M4A isom"), moov, bx(b"mdat", b"X")].concat();
        assert!(locate_audio(&buf).is_err());
    }

    #[test]
    fn reads_structure_parts() {
        let buf = mk_mp4(false, b"AUDIODATA", &[0]); // moov last
        let s = read_structure(&buf).unwrap();
        assert_eq!(&s.ftyp[4..8], b"ftyp");
        assert_eq!(&s.moov[4..8], b"moov");
        assert_eq!(&s.mdat_header[4..8], b"mdat");
        assert_eq!(s.mdat_payload_len, 9);
        assert_eq!(&buf[s.mdat_payload_offset as usize..][..9], b"AUDIODATA");
    }

    fn data_atom(type_code: u32, value: &[u8]) -> Vec<u8> {
        let mut p = type_code.to_be_bytes().to_vec();
        p.extend_from_slice(&0u32.to_be_bytes()); // locale
        p.extend_from_slice(value);
        bx(b"data", &p)
    }

    /// Accepted file with udta/meta/ilst injected (meta is a FullBox).
    fn mp4_with_ilst(ilst_atoms: &[u8], moov_first: bool) -> Vec<u8> {
        let ilst = bx(b"ilst", ilst_atoms);
        let mut hdlr = vec![0u8; 8];
        hdlr.extend_from_slice(b"mdir");
        hdlr.extend_from_slice(b"appl");
        hdlr.extend_from_slice(&[0u8; 9]);
        let mut meta = vec![0u8; 4]; // FullBox version/flags
        meta.extend(bx(b"hdlr", &hdlr));
        meta.extend(ilst);
        let udta = bx(b"udta", &bx(b"meta", &meta));

        let mut hdlr_p = vec![0u8; 8];
        hdlr_p.extend_from_slice(b"soun");
        hdlr_p.extend_from_slice(&[0u8; 12]);
        let mut stco = vec![0u8; 4];
        stco.extend_from_slice(&1u32.to_be_bytes());
        stco.extend_from_slice(&0u32.to_be_bytes());
        let minf = bx(b"minf", &bx(b"stbl", &bx(b"stco", &stco)));
        let trak = bx(
            b"trak",
            &bx(b"mdia", &[bx(b"hdlr", &hdlr_p), minf].concat()),
        );
        let moov = bx(b"moov", &[bx(b"mvhd", &[0u8; 8]), trak, udta].concat());
        let ftyp = bx(b"ftyp", b"M4A ");
        let mdat = bx(b"mdat", b"AUDIO");
        if moov_first {
            [ftyp, moov, mdat].concat()
        } else {
            [ftyp, mdat, moov].concat()
        }
    }

    #[test]
    fn reads_text_and_track_tags() {
        let atoms = [
            bx(b"\xa9nam", &data_atom(1, b"Song")),
            bx(b"aART", &data_atom(1, b"Band")),
            bx(b"trkn", &data_atom(0, &[0, 0, 0, 3, 0, 0, 0, 0])),
        ]
        .concat();
        let buf = mp4_with_ilst(&atoms, true);
        let tags = read_tags(&buf);
        assert!(tags.contains(&("title".into(), "Song".into())));
        assert!(tags.contains(&("albumartist".into(), "Band".into())));
        assert!(tags.contains(&("tracknumber".into(), "3".into())));
    }

    #[test]
    fn reads_cover_art() {
        let jpeg = [0xff, 0xd8, 0xff, 0xe0, 1, 2, 3];
        let buf = mp4_with_ilst(&bx(b"covr", &data_atom(13, &jpeg)), false);
        let pics = read_pictures(&buf);
        assert_eq!(pics.len(), 1);
        assert_eq!(pics[0].mime, "image/jpeg");
        assert_eq!(pics[0].data, jpeg);
    }

    #[test]
    fn read_side_never_panics_on_garbage() {
        // Empty buffer.
        assert!(read_tags(&[]).is_empty());
        assert!(read_pictures(&[]).is_empty());

        // Random non-MP4 bytes.
        let garbage = b"not an mp4 file at all............";
        assert!(read_tags(garbage).is_empty());
        assert!(read_pictures(garbage).is_empty());

        // Valid moov but no udta/meta/ilst.
        let no_ilst = mk_mp4(true, b"AUDIO", &[0]);
        assert!(read_tags(&no_ilst).is_empty());
        assert!(read_pictures(&no_ilst).is_empty());

        // A meta FullBox whose payload is shorter than the 4 version/flags bytes it
        // needs: exercises the `udta.get(meta.payload_start()+4..meta.end())?` guard.
        let truncated_meta = bx(b"udta", &bx(b"meta", &[0u8, 0]));
        let moov = bx(b"moov", &[bx(b"mvhd", &[0u8; 8]), truncated_meta].concat());
        let ftyp = bx(b"ftyp", b"M4A ");
        let mdat = bx(b"mdat", b"AUDIO");
        let lying = [ftyp, moov, mdat].concat();
        assert!(read_tags(&lying).is_empty());
        assert!(read_pictures(&lying).is_empty());
    }

    #[test]
    fn build_udta_no_art_round_trips() {
        let tags = vec![
            TagInput::new("title", "Song"),
            TagInput::new("tracknumber", "5"),
        ];
        let (prefix, art_len) = build_udta(&tags, None).unwrap();
        assert_eq!(art_len, 0);
        let b = read_box(&prefix, 0).unwrap();
        assert_eq!(&b.kind, b"udta");
        assert_eq!(b.total_len, prefix.len());
        // Wrap in a moov and read back through our own reader.
        let buf = [
            bx(b"ftyp", b"M4A "),
            bx(b"moov", &prefix),
            bx(b"mdat", b"A"),
        ]
        .concat();
        let tags = read_tags(&buf);
        assert!(tags.contains(&("title".into(), "Song".into())));
        assert!(tags.contains(&("tracknumber".into(), "5".into())));
    }

    #[test]
    fn build_udta_with_art_reserves_size_without_image() {
        let art = ArtInput {
            art_id: 1,
            mime: "image/png".into(),
            description: String::new(),
            picture_type: 3,
            width: 0,
            height: 0,
            data_len: 100,
        };
        let (prefix, art_len) = build_udta(&[TagInput::new("title", "T")], Some(&art)).unwrap();
        assert_eq!(art_len, 100);
        // The udta size field accounts for the 100 streamed image bytes.
        let declared = u32::from_be_bytes(prefix[0..4].try_into().unwrap()) as usize;
        assert_eq!(declared, prefix.len() + 100);
        // The prefix ends right after the covr/data header (image streams next).
        assert!(prefix.windows(4).any(|w| w == b"covr"));
    }

    #[test]
    fn build_udta_rejects_oversize_art() {
        let art = ArtInput {
            art_id: 1,
            mime: "image/jpeg".into(),
            description: String::new(),
            picture_type: 3,
            width: 0,
            height: 0,
            data_len: u32::MAX as u64 + 1,
        };
        assert!(matches!(
            build_udta(&[TagInput::new("title", "T")], Some(&art)),
            Err(FormatError::TooLarge)
        ));
    }

    #[test]
    fn build_udta_groups_multi_value_text() {
        // Two consecutive same-key text tags must collapse into ONE ilst atom
        // carrying REPEATED `data` sub-boxes (iTunes multi-value convention),
        // not two separate atoms and not a dropped value.
        let tags = vec![
            TagInput::new("genre", "Rock"),
            TagInput::new("genre", "Metal"),
        ];
        let (prefix, art_len) = build_udta(&tags, None).unwrap();
        assert_eq!(art_len, 0);

        // Exactly one `©gen` atom.
        let gen_count = prefix.windows(4).filter(|w| *w == b"\xa9gen").count();
        assert_eq!(
            gen_count, 1,
            "expected exactly one genre atom, got {gen_count}"
        );

        // Locate the `©gen` atom header and parse its children: must be two `data`
        // sub-boxes. The 4-byte kind sits at offset +4 of the box, so back up 4.
        let kind_at = prefix
            .windows(4)
            .position(|w| w == b"\xa9gen")
            .expect("genre atom present");
        let atom = read_box(&prefix, kind_at - 4).unwrap();
        assert_eq!(&atom.kind, b"\xa9gen");
        let children = child_boxes(atom.payload(&prefix)).unwrap();
        let data_count = children.iter().filter(|c| &c.kind == b"data").count();
        assert_eq!(
            data_count, 2,
            "expected two data sub-boxes, got {data_count}"
        );

        // Both values survive into the bytes.
        assert!(prefix.windows(4).any(|w| w == b"Rock"));
        assert!(prefix.windows(5).any(|w| w == b"Metal"));
    }

    #[test]
    fn build_udta_empty_tags_is_valid() {
        // A real file with no tags must still yield a structurally valid (empty)
        // udta, not a malformed box.
        let (prefix, art_len) = build_udta(&[], None).unwrap();
        assert_eq!(art_len, 0);
        let b = read_box(&prefix, 0).unwrap();
        assert_eq!(&b.kind, b"udta");
        assert_eq!(b.total_len, prefix.len());
        // Round-trips as having no tags.
        let buf = [
            bx(b"ftyp", b"M4A "),
            bx(b"moov", &prefix),
            bx(b"mdat", b"A"),
        ]
        .concat();
        assert!(read_tags(&buf).is_empty());
    }

    fn inline_head(layout: &RegionLayout) -> Vec<u8> {
        match &layout.segments()[0] {
            Segment::Inline(b) => b.clone(),
            _ => panic!("expected Inline head"),
        }
    }
    /// Locate `moov` by reading complete boxes from the front, stopping before
    /// the trailing `mdat` header (whose declared size includes the payload that
    /// is *not* present in the synthesized head — it streams as BackingAudio).
    fn find_moov_in_head(head: &[u8]) -> BoxRef {
        let mut pos = 0;
        loop {
            let b = read_box(head, pos).unwrap();
            if &b.kind == b"moov" {
                return b;
            }
            pos = b.end();
        }
    }
    fn first_stco(head: &[u8]) -> Vec<u32> {
        let moov = find_moov_in_head(head);
        let mp = moov.payload(head);
        let (sp, sl) = find_path(mp, &[b"trak", b"mdia", b"minf", b"stbl", b"stco"])
            .unwrap()
            .unwrap();
        let stco = &mp[sp..sp + sl];
        let count = u32::from_be_bytes(stco[4..8].try_into().unwrap()) as usize;
        (0..count)
            .map(|i| u32::from_be_bytes(stco[8 + i * 4..12 + i * 4].try_into().unwrap()))
            .collect()
    }

    #[test]
    fn synthesize_no_art_patches_stco() {
        let buf = mk_mp4(true, b"AUDIODATA", &[42, 100]);
        let scan = read_structure(&buf).unwrap();
        let layout = synthesize_layout(&scan, &[TagInput::new("title", "New")], &[]).unwrap();

        match layout.segments().last().unwrap() {
            Segment::BackingAudio { offset, len } => {
                assert_eq!(*offset, scan.mdat_payload_offset);
                assert_eq!(*len, scan.mdat_payload_len);
            }
            _ => panic!("expected BackingAudio tail"),
        }
        let head = inline_head(&layout);
        // The synthesized head is [ftyp][moov][mdat header]; the mdat payload is
        // served verbatim as the BackingAudio tail, so its new position is exactly
        // where the head ends.
        let new_mdat = head.len() as u64;
        let delta = new_mdat - scan.mdat_payload_offset;
        assert_eq!(
            first_stco(&head),
            vec![42 + delta as u32, 100 + delta as u32]
        );
        // The new file head re-parses as a valid moov of the declared size.
        let moov = find_moov_in_head(&head);
        assert_eq!(moov.end(), head.len() - scan.mdat_header.len());
    }

    /// Like `mk_mp4` but the soun trak's stbl carries a `co64` (8-byte offsets)
    /// box instead of an `stco`. moov-first, since that's all this exercises.
    fn mk_mp4_co64(mdat_payload: &[u8], co64_entries: &[u64]) -> Vec<u8> {
        let mut co64 = vec![0u8; 4];
        co64.extend_from_slice(&(co64_entries.len() as u32).to_be_bytes());
        for e in co64_entries {
            co64.extend_from_slice(&e.to_be_bytes());
        }
        let mut hdlr_p = vec![0u8; 8];
        hdlr_p.extend_from_slice(b"soun");
        hdlr_p.extend_from_slice(&[0u8; 12]);
        let minf = bx(b"minf", &bx(b"stbl", &bx(b"co64", &co64)));
        let mdia = bx(b"mdia", &[bx(b"hdlr", &hdlr_p), minf].concat());
        let trak = bx(b"trak", &mdia);
        let moov = bx(b"moov", &[bx(b"mvhd", &[0u8; 8]), trak].concat());
        let mdat = bx(b"mdat", mdat_payload);
        let ftyp = bx(b"ftyp", b"M4A isom");
        [ftyp, moov, mdat].concat()
    }

    fn first_co64(head: &[u8]) -> Vec<u64> {
        let moov = find_moov_in_head(head);
        let mp = moov.payload(head);
        let (sp, sl) = find_path(mp, &[b"trak", b"mdia", b"minf", b"stbl", b"co64"])
            .unwrap()
            .unwrap();
        let co64 = &mp[sp..sp + sl];
        let count = u32::from_be_bytes(co64[4..8].try_into().unwrap()) as usize;
        (0..count)
            .map(|i| u64::from_be_bytes(co64[8 + i * 8..16 + i * 8].try_into().unwrap()))
            .collect()
    }

    #[test]
    fn synthesize_patches_co64() {
        let buf = mk_mp4_co64(b"AUDIODATA", &[42, 100]);
        let scan = read_structure(&buf).unwrap();
        let layout = synthesize_layout(&scan, &[TagInput::new("title", "New")], &[]).unwrap();

        match layout.segments().last().unwrap() {
            Segment::BackingAudio { offset, len } => {
                assert_eq!(*offset, scan.mdat_payload_offset);
                assert_eq!(*len, scan.mdat_payload_len);
            }
            _ => panic!("expected BackingAudio tail"),
        }
        let head = inline_head(&layout);
        // mdat payload is served as the BackingAudio tail, so its new position is
        // exactly where the head ends; co64 offsets shift by the same delta.
        let new_mdat = head.len() as u64;
        let delta = new_mdat - scan.mdat_payload_offset;
        assert_eq!(first_co64(&head), vec![42 + delta, 100 + delta]);
        // The new file head re-parses as a valid moov of the declared size.
        let moov = find_moov_in_head(&head);
        assert_eq!(moov.end(), head.len() - scan.mdat_header.len());
    }

    #[test]
    fn synthesize_with_art_splits_for_streaming() {
        let buf = mk_mp4(false, b"AUDIODATA", &[0]);
        let scan = read_structure(&buf).unwrap();
        let art = ArtInput {
            art_id: 7,
            mime: "image/jpeg".into(),
            description: String::new(),
            picture_type: 3,
            width: 0,
            height: 0,
            data_len: 50,
        };
        let layout = synthesize_layout(&scan, &[TagInput::new("title", "T")], &[art]).unwrap();
        let segs = layout.segments();
        assert!(matches!(segs[1], Segment::ArtImage { art_id: 7, len: 50 }));
        assert!(matches!(segs[2], Segment::Inline(_))); // mdat header
        assert!(matches!(segs.last().unwrap(), Segment::BackingAudio { .. }));
    }

    #[test]
    fn synthesize_handles_zero_length_mdat() {
        let buf = mk_mp4(true, b"", &[0]); // empty mdat payload
        let scan = read_structure(&buf).unwrap();
        assert_eq!(scan.mdat_payload_len, 0);
        let layout = synthesize_layout(&scan, &[TagInput::new("title", "Z")], &[]).unwrap();
        match layout.segments().last().unwrap() {
            Segment::BackingAudio { offset, len } => {
                assert_eq!(*offset, scan.mdat_payload_offset);
                assert_eq!(*len, 0);
            }
            _ => panic!("expected BackingAudio tail"),
        }
    }
}
