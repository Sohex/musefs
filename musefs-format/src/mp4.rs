//! Hand-rolled MP4/M4A box layer: parse the structure, read iTunes metadata, and
//! regenerate `moov` (with patched chunk offsets) to synthesize a re-tagged file
//! whose `mdat` audio payload is served verbatim. Strict: anything outside the
//! supported shape (single audio track, one `mdat`, non-fragmented) is rejected.

use crate::error::{FormatError, Result};
use crate::input::{ArtInput, EmbeddedPicture, TagInput};
use crate::layout::{RegionLayout, Segment};
use std::io::{self, Read, Seek, SeekFrom};

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

/// A parsed box header (the payload need not be in memory). Public so the core
/// reader can reason about box bounds while seeking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoxHeader {
    /// The 4-byte box type, e.g. `*b"moov"`.
    pub kind: [u8; 4],
    /// 8, or 16 for a 64-bit largesize.
    pub header_len: u64,
    /// Total box length: header + payload.
    pub total_len: u64,
}

/// Parse a box header from `hdr` (>= 8 bytes; >= 16 if it uses a 64-bit
/// largesize). `remaining` is the byte count from this box's start to EOF, used
/// to resolve a `size == 0` ("extends to end") box.
pub fn box_header(hdr: &[u8], remaining: u64) -> Result<BoxHeader> {
    let size32 = be_u32(hdr, 0)? as u64;
    let kind: [u8; 4] = hdr
        .get(4..8)
        .ok_or(FormatError::Malformed)?
        .try_into()
        .unwrap();
    let (header_len, total_len) = match size32 {
        1 => (16u64, be_u64(hdr, 8)?),
        0 => (8u64, remaining),
        n => (8u64, n),
    };
    if total_len < header_len || total_len > remaining {
        return Err(FormatError::Malformed);
    }
    Ok(BoxHeader {
        kind,
        header_len,
        total_len,
    })
}

/// Error from the seeking MP4 reader: an IO failure reading the file, or a
/// structural/format problem. Kept distinct so the core layer can map IO to
/// `CoreError::Io` (preserving errno) and format to `CoreError::Format`.
#[derive(Debug, thiserror::Error)]
pub enum Mp4ScanError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Format(#[from] FormatError),
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
    let Some(end) = pos.checked_add(total) else {
        return Err(FormatError::Malformed);
    };
    if total < header_len || end > buf.len() {
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
        let Some(b) = find_box(region, kind)? else {
            return Ok(None);
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

/// Validate the internal `moov` shape: no fragmentation (`mvex`), exactly one
/// track, and that track is audio (`soun`). `moov_payload` is the bytes inside
/// the `moov` box (after its header).
fn validate_moov(moov_payload: &[u8]) -> Result<()> {
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
    Ok(())
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

    validate_moov(moov.payload(buf))?;
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
#[derive(Debug, Clone, PartialEq)]
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

/// Read the structural boxes (`ftyp`, `moov`, and the `mdat` header) by seeking,
/// **never** reading the `mdat` payload — for audiobooks that payload is hundreds
/// of MB and is served from the backing file at read time. Produces an `Mp4Scan`
/// byte-identical to `read_structure` on the same file, so synthesis is unchanged.
///
/// The header walk reads only 8 bytes per top-level box (16 for a 64-bit
/// largesize), so it skips over the `mdat` payload to reach a trailing `moov`.
pub fn read_structure_from<R: Read + Seek>(
    r: &mut R,
    file_len: u64,
) -> std::result::Result<Mp4Scan, Mp4ScanError> {
    fn region<R: Read + Seek>(r: &mut R, off: u64, len: usize) -> io::Result<Vec<u8>> {
        r.seek(SeekFrom::Start(off))?;
        let mut buf = vec![0u8; len];
        r.read_exact(&mut buf)?;
        Ok(buf)
    }

    // (start_offset, header) for each box we care about.
    let mut ftyp: Option<(u64, BoxHeader)> = None;
    let mut moov: Option<(u64, BoxHeader)> = None;
    let mut mdat: Option<(u64, BoxHeader)> = None;
    let mut dup = false;

    let mut pos = 0u64;
    while pos + 8 <= file_len {
        // Read exactly the header — 8 bytes, plus 8 more only for a largesize box.
        // This guarantees we never touch a box's payload (notably mdat's).
        let first8 = region(r, pos, 8)?;
        let size32 = u32::from_be_bytes(first8[0..4].try_into().unwrap());
        // A largesize box needs 8 more header bytes; if the file is truncated
        // mid-header this read surfaces as Mp4ScanError::Io (UnexpectedEof).
        let hdr = if size32 == 1 {
            let mut h = first8;
            h.extend_from_slice(&region(r, pos + 8, 8)?);
            h
        } else {
            first8
        };
        let bh = box_header(&hdr, file_len - pos)?;
        let total = bh.total_len;
        match &bh.kind {
            b"moof" => return Err(FormatError::NotMp4.into()),
            b"ftyp" => dup |= ftyp.replace((pos, bh)).is_some(),
            b"moov" => dup |= moov.replace((pos, bh)).is_some(),
            b"mdat" => dup |= mdat.replace((pos, bh)).is_some(),
            _ => {}
        }
        pos += total;
    }
    if dup {
        return Err(FormatError::NotMp4.into());
    }

    let (ftyp_s, ftyp_h) = ftyp.ok_or(FormatError::NotMp4)?;
    let (moov_s, moov_h) = moov.ok_or(FormatError::NotMp4)?;
    let (mdat_s, mdat_h) = mdat.ok_or(FormatError::NotMp4)?;

    // `try_from` rather than `as usize`: on a 32-bit target an oversized box would
    // truncate silently; a box larger than `usize` is malformed for our purposes.
    let ftyp_len = usize::try_from(ftyp_h.total_len).map_err(|_| FormatError::Malformed)?;
    let moov_len = usize::try_from(moov_h.total_len).map_err(|_| FormatError::Malformed)?;
    let ftyp_bytes = region(r, ftyp_s, ftyp_len)?;
    let moov_bytes = region(r, moov_s, moov_len)?;
    let mdat_header = region(r, mdat_s, mdat_h.header_len as usize)?;

    validate_moov(&moov_bytes[moov_h.header_len as usize..])?;

    Ok(Mp4Scan {
        ftyp: ftyp_bytes,
        moov: moov_bytes,
        mdat_header,
        mdat_payload_offset: mdat_s + mdat_h.header_len,
        mdat_payload_len: mdat_h.total_len - mdat_h.header_len,
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

/// Parse a `----` freeform atom payload into `(key, value)`. Folds (mean, name)
/// to a canonical key via the vocabulary, else keys on the verbatim `name`. Only
/// the first `data` atom is read (multi-value freeform is rare). None if malformed.
fn read_freeform(inner: &[u8]) -> Option<(String, String)> {
    let name_box = find_box(inner, b"name").ok()??;
    let data_box = find_box(inner, b"data").ok()??;
    let np = name_box.payload(inner);
    let dp = data_box.payload(inner);
    if np.len() < 4 || dp.len() < 8 {
        return None;
    }
    // The `data` box is `[type: u32][locale: u32][value]`; type 1 == UTF-8 text.
    // Binary-typed freeform values are not text tags, so skip them.
    let type_code = u32::from_be_bytes([dp[0], dp[1], dp[2], dp[3]]);
    if type_code != 1 {
        return None;
    }
    // name/mean payloads start with a 4-byte FullBox [version 1][flags 3] prefix.
    let name = std::str::from_utf8(&np[4..]).ok()?;
    let value = std::str::from_utf8(&dp[8..]).ok()?;
    let mean = find_box(inner, b"mean")
        .ok()
        .flatten()
        .map_or("com.apple.iTunes", |m| {
            let p = m.payload(inner);
            if p.len() >= 4 {
                std::str::from_utf8(&p[4..]).unwrap_or("com.apple.iTunes")
            } else {
                "com.apple.iTunes"
            }
        });
    let key = crate::tagmap::mp4_freeform_to_key(mean, name)
        .map_or_else(|| name.to_string(), str::to_string);
    Some((key, value.to_string()))
}

/// Lenient: returns empty / skips any malformed atom and never errors — this only
/// seeds metadata from existing files, so a missing or garbled tag must simply be
/// absent. Text atoms map via the vocabulary; `trkn`/`disk` yield track/disc
/// numbers; `----` freeform atoms key on their name (folded when known). Other
/// atoms are skipped.
pub fn read_tags(buf: &[u8]) -> Vec<(String, String)> {
    let Some((start, len)) = ilst_region(buf) else {
        return Vec::new();
    };
    let ilst = &buf[start..start + len];
    let mut out = Vec::new();
    for atom in child_boxes(ilst).unwrap_or_default() {
        let inner = atom.payload(ilst);
        if &atom.kind == b"----" {
            if let Some(pair) = read_freeform(inner) {
                out.push(pair);
            }
            continue;
        }
        let Ok(Some(data)) = find_box(inner, b"data") else {
            continue;
        };
        let dp = data.payload(inner);
        if dp.len() < 8 {
            continue;
        }
        let value = &dp[8..]; // skip [type 4][locale 4]
        if let Some(key) = crate::tagmap::mp4_atom_to_key(&atom.kind) {
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
    let Some((start, len)) = ilst_region(buf) else {
        return Vec::new();
    };
    let ilst = &buf[start..start + len];
    let mut out = Vec::new();
    for atom in child_boxes(ilst).unwrap_or_default() {
        if &atom.kind != b"covr" {
            continue;
        }
        let inner = atom.payload(ilst);
        let Ok(Some(data)) = find_box(inner, b"data") else {
            continue;
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

/// Emit a `----` freeform atom: a `mean` and `name` sub-box (each with a 4-byte
/// FullBox prefix) followed by one UTF-8 `data` sub-box per value. Note that the
/// scan path (`read_freeform`) only recovers the first value on read-back, so
/// multi-value freeform tags round-trip only their first value.
fn freeform_atom(mean: &str, name: &str, values: &[&str]) -> Vec<u8> {
    let mut inner = Vec::new();
    let mut mean_body = 0u32.to_be_bytes().to_vec(); // version/flags
    mean_body.extend_from_slice(mean.as_bytes());
    inner.extend(boxed(b"mean", &mean_body));
    let mut name_body = 0u32.to_be_bytes().to_vec();
    name_body.extend_from_slice(name.as_bytes());
    inner.extend(boxed(b"name", &name_body));
    for v in values {
        let mut data = 1u32.to_be_bytes().to_vec(); // type 1 = UTF-8
        data.extend_from_slice(&0u32.to_be_bytes()); // locale
        data.extend_from_slice(v.as_bytes());
        inner.extend(boxed(b"data", &data));
    }
    boxed(b"----", &inner)
}

/// Build `udta` up to (not including) the cover image bytes. Returns (prefix,
/// art_len). No art → prefix is the complete udta, art_len 0. All enclosing box
/// sizes include art_len so the image can stream right after the prefix.
fn build_udta(tags: &[TagInput], art: Option<&ArtInput>) -> Result<(Vec<u8>, u64)> {
    // Group consecutive same-key values (the DB returns tags ordered by key).
    let mut groups: Vec<(&str, Vec<&str>)> = Vec::new();
    for t in tags {
        match groups.last_mut() {
            Some(g) if g.0 == t.key => g.1.push(&t.value),
            _ => groups.push((&t.key, vec![&t.value])),
        }
    }

    let mut ilst = Vec::new();
    for (key, values) in &groups {
        match crate::tagmap::key_to_mp4(key) {
            Some(crate::tagmap::Mp4Slot::Text(atom)) => ilst.extend(text_atom(atom, values)),
            Some(crate::tagmap::Mp4Slot::Number(atom, width)) => {
                if let Ok(n) = values.first().copied().unwrap_or("").parse::<u16>() {
                    ilst.extend(number_atom(atom, n, width));
                }
            }
            Some(crate::tagmap::Mp4Slot::Freeform(mean, name)) => {
                ilst.extend(freeform_atom(mean, name, values));
            }
            None => ilst.extend(freeform_atom("com.apple.iTunes", key, values)),
        }
    }

    let mut hdlr_body = vec![0u8; 8];
    hdlr_body.extend_from_slice(b"mdir");
    hdlr_body.extend_from_slice(b"appl");
    hdlr_body.extend_from_slice(&[0u8; 9]);
    let hdlr = boxed(b"hdlr", &hdlr_body);

    let art_len = art.map_or(0, |a| a.data_len);

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
    RegionLayout::validated(segments).map_err(|_| FormatError::InvalidLayout)
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

    #[test]
    fn box_header_parses_8_byte_16_byte_and_size0() {
        // 8-byte header: size 16, type "moov".
        let mut h = 16u32.to_be_bytes().to_vec();
        h.extend_from_slice(b"moov");
        let bh = box_header(&h, 1000).unwrap();
        assert_eq!(&bh.kind, b"moov");
        assert_eq!(bh.header_len, 8);
        assert_eq!(bh.total_len, 16);

        // 64-bit largesize: size32==1, then u64 size = 40.
        let mut h = 1u32.to_be_bytes().to_vec();
        h.extend_from_slice(b"mdat");
        h.extend_from_slice(&40u64.to_be_bytes());
        let bh = box_header(&h, 1000).unwrap();
        assert_eq!(bh.header_len, 16);
        assert_eq!(bh.total_len, 40);

        // size32==0 means "extends to EOF" -> total_len == remaining.
        let mut h = 0u32.to_be_bytes().to_vec();
        h.extend_from_slice(b"mdat");
        let bh = box_header(&h, 500).unwrap();
        assert_eq!(bh.header_len, 8);
        assert_eq!(bh.total_len, 500);
    }

    #[test]
    fn box_header_rejects_impossible_sizes() {
        // total_len < header_len.
        let mut h = 4u32.to_be_bytes().to_vec();
        h.extend_from_slice(b"moov");
        assert_eq!(box_header(&h, 1000), Err(FormatError::Malformed));
        // total_len > remaining.
        let mut h = 2000u32.to_be_bytes().to_vec();
        h.extend_from_slice(b"moov");
        assert_eq!(box_header(&h, 100), Err(FormatError::Malformed));
        // header shorter than 8 bytes.
        assert_eq!(box_header(&[0u8; 4], 1000), Err(FormatError::Malformed));
    }

    #[test]
    fn read_structure_from_matches_buffer_path() {
        // Both moov-first and moov-last (moov-last is the audiobook spike case).
        for moov_first in [true, false] {
            let buf = mk_mp4(moov_first, &vec![0xABu8; 4096], &[0]);
            let from_buf = read_structure(&buf).unwrap();
            let mut cur = std::io::Cursor::new(buf.clone());
            let from_stream = read_structure_from(&mut cur, buf.len() as u64).unwrap();
            assert_eq!(from_stream, from_buf);
        }
    }

    #[test]
    fn read_structure_from_never_reads_mdat_payload() {
        // moov LAST: reaching it requires skipping the mdat payload.
        let buf = mk_mp4(false, &vec![0xCDu8; 100_000], &[0]);
        let scan = read_structure(&buf).unwrap();
        let pay_start = scan.mdat_payload_offset;
        let pay_end = pay_start + scan.mdat_payload_len;

        // A reader that records every byte range it is asked to read.
        struct Tracking {
            inner: std::io::Cursor<Vec<u8>>,
            touched: Vec<(u64, u64)>,
        }
        impl std::io::Read for Tracking {
            fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
                let off = self.inner.position();
                let n = std::io::Read::read(&mut self.inner, b)?;
                self.touched.push((off, off + n as u64));
                Ok(n)
            }
        }
        impl std::io::Seek for Tracking {
            fn seek(&mut self, p: std::io::SeekFrom) -> std::io::Result<u64> {
                self.inner.seek(p)
            }
        }

        let mut tr = Tracking {
            inner: std::io::Cursor::new(buf.clone()),
            touched: Vec::new(),
        };
        let from_stream = read_structure_from(&mut tr, buf.len() as u64).unwrap();
        assert_eq!(from_stream, scan);
        for (s, e) in &tr.touched {
            assert!(
                *e <= pay_start || *s >= pay_end,
                "read [{s},{e}) overlaps mdat payload [{pay_start},{pay_end})"
            );
        }
    }

    #[test]
    fn read_freeform_extracts_name_and_value() {
        // Build a minimal `----` atom: mean + name + data(UTF-8).
        let mut mean_body = 0u32.to_be_bytes().to_vec();
        mean_body.extend_from_slice(b"com.apple.iTunes");
        let mut name_body = 0u32.to_be_bytes().to_vec();
        name_body.extend_from_slice(b"MusicBrainz Album Id");
        let mut data = 1u32.to_be_bytes().to_vec(); // type 1 = UTF-8
        data.extend_from_slice(&0u32.to_be_bytes()); // locale
        data.extend_from_slice(b"abc-123");
        let mut inner = boxed(b"mean", &mean_body);
        inner.extend(boxed(b"name", &name_body));
        inner.extend(boxed(b"data", &data));

        let (key, value) = read_freeform(&inner).unwrap();
        assert_eq!(key, "musicbrainz_albumid"); // folded via vocabulary
        assert_eq!(value, "abc-123");
    }

    #[test]
    fn read_freeform_unknown_name_passes_through_verbatim() {
        let mut mean_body = 0u32.to_be_bytes().to_vec();
        mean_body.extend_from_slice(b"com.apple.iTunes");
        let mut name_body = 0u32.to_be_bytes().to_vec();
        name_body.extend_from_slice(b"My Custom Field");
        let mut data = 1u32.to_be_bytes().to_vec(); // type 1 = UTF-8
        data.extend_from_slice(&0u32.to_be_bytes()); // locale
        data.extend_from_slice(b"hello");
        let mut inner = boxed(b"mean", &mean_body);
        inner.extend(boxed(b"name", &name_body));
        inner.extend(boxed(b"data", &data));

        let (key, value) = read_freeform(&inner).unwrap();
        assert_eq!(key, "My Custom Field"); // not in vocabulary -> verbatim name
        assert_eq!(value, "hello");
    }

    #[test]
    fn read_freeform_skips_binary_typed_data() {
        let mut name_body = 0u32.to_be_bytes().to_vec();
        name_body.extend_from_slice(b"My Custom Field");
        let mut data = 0u32.to_be_bytes().to_vec(); // type 0 = binary, not text
        data.extend_from_slice(&0u32.to_be_bytes()); // locale
        data.extend_from_slice(&[0xff, 0x00, 0x01]);
        let mut inner = boxed(b"name", &name_body);
        inner.extend(boxed(b"data", &data));

        assert!(read_freeform(&inner).is_none()); // binary-typed data is skipped
    }

    #[test]
    fn build_udta_round_trips_freeform_and_vocabulary() {
        let tags = vec![
            TagInput::new("title", "Song"),
            TagInput::new("tracknumber", "3"),
            TagInput::new("MyRating", "5"), // user-defined -> ----
            TagInput::new("musicbrainz_albumid", "abc-123"), // vocabulary -> ----
        ];
        let (udta, _art_len) = build_udta(&tags, None).unwrap();
        // build_udta returns a full `udta` box; read_tags expects a buffer containing
        // moov/udta/meta/ilst, so wrap udta in a minimal moov for the round trip.
        let moov = boxed(b"moov", &udta);

        let tags = read_tags(&moov);
        for expected in [
            ("title", "Song"),
            ("tracknumber", "3"),
            ("MyRating", "5"),
            ("musicbrainz_albumid", "abc-123"),
        ] {
            assert!(
                tags.contains(&(expected.0.to_string(), expected.1.to_string())),
                "missing {expected:?} in {tags:?}"
            );
        }
    }

    #[test]
    fn read_box_rejects_overflowing_extended_size() {
        // The extended-size path (size32 == 1) reads a 64-bit box length from
        // untrusted input. Before the checked_add fix, `pos + total` overflowed
        // usize in debug (panic) or wrapped silently in release (accepting a
        // bogus length). This test feeds size32=1 with a u64::MAX extended size
        // and asserts the parser returns an error rather than panicking.
        // Bytes: [00 00 00 01] (size32=1) + b"moov" + [FF FF FF FF FF FF FF FF] (u64::MAX)
        let mut bytes = 1u32.to_be_bytes().to_vec(); // size32 = 1 → extended-size
        bytes.extend_from_slice(b"moov");
        bytes.extend_from_slice(&u64::MAX.to_be_bytes()); // huge 64-bit size
        assert!(
            read_structure(&bytes).is_err(),
            "must return an error, not panic"
        );
    }

    #[test]
    fn read_structure_from_handles_largesize_mdat() {
        // Re-encode a normal fixture's mdat with a 64-bit largesize header (the
        // real >4GB audiobook shape) and confirm both readers agree.
        fn largesize_mdat(payload: &[u8]) -> Vec<u8> {
            let total = 16 + payload.len() as u64;
            let mut v = 1u32.to_be_bytes().to_vec(); // size32 == 1
            v.extend_from_slice(b"mdat");
            v.extend_from_slice(&total.to_be_bytes()); // 64-bit largesize
            v.extend_from_slice(payload);
            v
        }
        let normal = mk_mp4(true, &[0xABu8; 64], &[0]); // [ftyp][moov][mdat]
        let scan = read_structure(&normal).unwrap();
        let payload_start = scan.mdat_payload_offset as usize;
        let mdat_box_start = payload_start - scan.mdat_header.len(); // normal 8-byte header
        let payload = normal[payload_start..].to_vec();
        let mut buf = normal[..mdat_box_start].to_vec(); // ftyp + moov
        buf.extend(largesize_mdat(&payload));

        let from_buf = read_structure(&buf).unwrap();
        let mut cur = std::io::Cursor::new(buf.clone());
        let from_stream = read_structure_from(&mut cur, buf.len() as u64).unwrap();
        assert_eq!(from_stream, from_buf);
        assert_eq!(from_stream.mdat_header.len(), 16); // largesize header
        assert_eq!(from_stream.mdat_payload_len, payload.len() as u64);
    }

    #[test]
    fn box_header_accepts_empty_payload_box() {
        // total_len == header_len (an 8-byte box, no payload) must be accepted.
        // `< -> <=` would make the equal case reject.
        let mut h = 8u32.to_be_bytes().to_vec();
        h.extend_from_slice(b"free");
        let bh = box_header(&h, 1000).unwrap();
        assert_eq!(bh.header_len, 8);
        assert_eq!(bh.total_len, 8);
    }

    #[test]
    fn read_box_size0_extends_to_end_from_offset() {
        // A size-0 box ("extends to EOF") at pos > 0: total_len must be
        // buf.len() - pos. `- -> +` (buf.len() + pos) and `- -> /` (buf.len() / pos)
        // both diverge. The box is placed at pos = 8 with pos + 8 <= buf.len() so the
        // be_u32 size read and the kind slice both succeed BEFORE the size-0 branch.
        let mut buf = bx(b"free", b""); // 8-byte box at pos 0
        buf.extend_from_slice(&0u32.to_be_bytes()); // size32 = 0 at pos 8
        buf.extend_from_slice(b"mdat"); // kind at pos 12..16
        buf.extend_from_slice(b"AUDIOPAYLOAD"); // 12 payload bytes
        assert_eq!(buf.len(), 28);
        let b = read_box(&buf, 8).unwrap();
        assert_eq!(&b.kind, b"mdat");
        assert_eq!(b.total_len, buf.len() - 8); // 20
    }

    #[test]
    fn read_structure_from_rejects_box_overrunning_eof() {
        // box_header's `remaining` arg is `file_len - pos`. Inflating the mdat box's
        // declared size past the bytes remaining must be rejected. `- -> +` inflates
        // `remaining` to `file_len + pos`, wrongly accepting the overrun (returns Ok).
        let mut buf = mk_mp4(true, b"AUDIO", &[0]); // [ftyp][moov][mdat], mdat last
        let scan = read_structure(&buf).unwrap();
        let mdat_start = (scan.mdat_payload_offset - scan.mdat_header.len() as u64) as usize;
        let real = u32::from_be_bytes(buf[mdat_start..mdat_start + 4].try_into().unwrap());
        buf[mdat_start..mdat_start + 4].copy_from_slice(&(real + 100).to_be_bytes());
        let mut cur = std::io::Cursor::new(buf.clone());
        assert!(read_structure_from(&mut cur, buf.len() as u64).is_err());
    }

    #[test]
    fn read_structure_from_rejects_moof() {
        // A `moof` (fragmented MP4) top-level box must be rejected via the seeking
        // path. Deleting the `b"moof"` match arm drops it to `_ => {}` and accepts.
        let mut buf = mk_mp4(true, b"AUDIO", &[0]);
        buf.extend(bx(b"moof", b"\x00\x00\x00\x00"));
        let mut cur = std::io::Cursor::new(buf.clone());
        assert!(read_structure_from(&mut cur, buf.len() as u64).is_err());
    }

    #[test]
    fn read_structure_from_rejects_duplicate_top_level_boxes() {
        // Each `dup |= X.replace(..).is_some()` accumulates a duplicate. `|= -> &=`
        // can never set `dup` (it starts false), so a duplicate box is wrongly
        // accepted. One duplicated box per kind isolates each of the three `|=` lines.
        let dup = |extra: Vec<u8>| {
            let mut buf = mk_mp4(true, b"AUDIO", &[0]);
            buf.extend(extra);
            let mut cur = std::io::Cursor::new(buf.clone());
            read_structure_from(&mut cur, buf.len() as u64).is_err()
        };
        assert!(dup(bx(b"ftyp", b"M4A isom")), "duplicate ftyp must reject"); // ftyp |= line
                                                                              // duplicate moov: reuse the moov from a fresh fixture so it is structurally valid.
        let extra_moov = {
            let other = mk_mp4(true, b"AUDIO", &[0]);
            let s = read_structure(&other).unwrap();
            s.moov
        };
        assert!(dup(extra_moov), "duplicate moov must reject"); // moov |= line
        assert!(dup(bx(b"mdat", b"Y")), "duplicate mdat must reject"); // mdat |= line
    }

    #[test]
    fn read_freeform_accepts_minimal_name_and_data() {
        // name payload == 4 (empty name) and data payload == 8 (empty value) is the
        // boundary of `np.len() < 4 || dp.len() < 8`. Both operands at the boundary,
        // so flipping EITHER `<` to `==`/`<=` makes that side true -> None.
        let name_body = 0u32.to_be_bytes().to_vec(); // exactly 4 bytes
        let mut data = 1u32.to_be_bytes().to_vec(); // type 1 = UTF-8
        data.extend_from_slice(&0u32.to_be_bytes()); // locale -> dp.len() == 8
        let mut inner = boxed(b"name", &name_body);
        inner.extend(boxed(b"data", &data));
        let (key, value) = read_freeform(&inner).unwrap();
        assert_eq!(key, ""); // empty name, not in vocabulary -> verbatim ""
        assert_eq!(value, "");
    }

    #[test]
    fn read_freeform_short_name_returns_none() {
        // name payload 3 bytes (< 4) with a valid 8-byte data payload. `|| -> &&`
        // makes `true && false == false`, falling through to `&np[4..]` (out of bounds
        // -> panic).
        let name_body = vec![0u8, 0, 0]; // 3 bytes
        let mut data = 1u32.to_be_bytes().to_vec();
        data.extend_from_slice(&0u32.to_be_bytes());
        let mut inner = boxed(b"name", &name_body);
        inner.extend(boxed(b"data", &data));
        assert!(read_freeform(&inner).is_none());
    }

    #[test]
    fn read_freeform_mean_payload_exactly_4_uses_empty_mean() {
        // mean payload == 4 (FullBox prefix, empty mean). `p.len() >= 4` must take the
        // utf8 branch (mean ""), so the vocabulary does NOT fold the iTunes name.
        // `>= -> <` falls to the default "com.apple.iTunes" mean and wrongly folds.
        let mean_body = vec![0u8, 0, 0, 0]; // exactly 4 bytes
        let mut name_body = 0u32.to_be_bytes().to_vec();
        name_body.extend_from_slice(b"MusicBrainz Album Id");
        let mut data = 1u32.to_be_bytes().to_vec();
        data.extend_from_slice(&0u32.to_be_bytes());
        data.extend_from_slice(b"abc-123");
        let mut inner = boxed(b"mean", &mean_body);
        inner.extend(boxed(b"name", &name_body));
        inner.extend(boxed(b"data", &data));
        let (key, value) = read_freeform(&inner).unwrap();
        assert_eq!(key, "MusicBrainz Album Id"); // empty mean -> not folded
        assert_eq!(value, "abc-123");
    }

    #[test]
    fn read_tags_data_payload_exactly_8_is_read() {
        // A `data` payload of exactly 8 bytes (type+locale, empty value) is the
        // boundary of `dp.len() < 8`. The (empty) value must be read; `< -> ==`/`<= `
        // would skip it.
        let atoms = bx(b"\xa9nam", &data_atom(1, b"")); // dp.len() == 8
        let buf = mp4_with_ilst(&atoms, true);
        assert!(read_tags(&buf).contains(&("title".into(), String::new())));
    }

    #[test]
    fn read_tags_disk_exact_4_byte_value_yields_discnumber() {
        // disk atom, value exactly 4 bytes: `kind == disk` (== branch) `&&`
        // `value.len() >= 4` (>= branch). Kills `== -> !=` (mutant skips a real disk)
        // and `>= -> <` (mutant skips the boundary length).
        let atoms = bx(b"disk", &data_atom(0, &[0, 0, 0, 2])); // disc 2, value len 4
        let buf = mp4_with_ilst(&atoms, true);
        assert!(read_tags(&buf).contains(&("discnumber".into(), "2".into())));
    }

    #[test]
    fn read_tags_disk_short_value_is_skipped() {
        // disk with a value shorter than 4 bytes: the guard is false. `&& -> ||`
        // makes it true and indexes value[2]/value[3] out of bounds (panic).
        let atoms = bx(b"disk", &data_atom(0, &[0, 0])); // value len 2
        let buf = mp4_with_ilst(&atoms, true);
        assert!(!read_tags(&buf).iter().any(|(k, _)| k == "discnumber"));
    }

    #[test]
    fn read_tags_trkn_short_value_is_skipped() {
        // trkn with a value shorter than 4 bytes: `kind == trkn && value.len() >= 4`
        // is false. `&& -> ||` makes it true and indexes value[2]/value[3] (panic).
        let atoms = bx(b"trkn", &data_atom(0, &[0, 0])); // value len 2
        let buf = mp4_with_ilst(&atoms, true);
        assert!(!read_tags(&buf).iter().any(|(k, _)| k == "tracknumber"));
    }

    #[test]
    fn read_pictures_data_payload_exactly_8_is_read() {
        // covr/data payload of exactly 8 bytes (type+locale, empty image) is the
        // boundary of `dp.len() < 8`; the (empty) picture must be read.
        let buf = mp4_with_ilst(&bx(b"covr", &data_atom(13, b"")), true);
        let pics = read_pictures(&buf);
        assert_eq!(pics.len(), 1);
        assert_eq!(pics[0].mime, "image/jpeg");
        assert!(pics[0].data.is_empty());
    }

    #[test]
    fn read_pictures_recognizes_png() {
        // A covr `data` atom with type code 14 is PNG. Deleting the `14 =>` match arm
        // drops it to `_ => continue` and yields no picture.
        let png = [0x89, b'P', b'N', b'G', 1, 2, 3];
        let buf = mp4_with_ilst(&bx(b"covr", &data_atom(14, &png)), false);
        let pics = read_pictures(&buf);
        assert_eq!(pics.len(), 1);
        assert_eq!(pics[0].mime, "image/png");
        assert_eq!(pics[0].data, png);
    }

    #[test]
    fn build_udta_png_art_uses_type_code_14() {
        // PNG art => covr/data type code 14; JPEG => 13. `== -> !=` flips them.
        for (mime, expected) in [("image/png", 14u32), ("image/jpeg", 13u32)] {
            let art = ArtInput {
                art_id: 1,
                mime: mime.into(),
                description: String::new(),
                picture_type: 3,
                width: 0,
                height: 0,
                data_len: 10,
            };
            let (prefix, _) = build_udta(&[TagInput::new("title", "T")], Some(&art)).unwrap();
            // covr layout: [covr_size u32]["covr"][data_size u32]["data"][type u32][locale u32]
            let cpos = prefix.windows(4).position(|w| w == b"covr").expect("covr");
            assert_eq!(&prefix[cpos + 8..cpos + 12], b"data");
            let type_code = u32::from_be_bytes(prefix[cpos + 12..cpos + 16].try_into().unwrap());
            assert_eq!(type_code, expected, "mime {mime}");
        }
    }

    #[test]
    fn build_udta_art_box_sizes_are_exact() {
        // data_size = 8 + 8 + data_len; covr_size = 8 + data_size. The `+ -> -`/`+ -> *`
        // mutations change the emitted box sizes.
        let art = ArtInput {
            art_id: 1,
            mime: "image/jpeg".into(),
            description: String::new(),
            picture_type: 3,
            width: 0,
            height: 0,
            data_len: 10,
        };
        let (prefix, _) = build_udta(&[TagInput::new("title", "T")], Some(&art)).unwrap();
        let cpos = prefix.windows(4).position(|w| w == b"covr").expect("covr");
        let covr_size = u32::from_be_bytes(prefix[cpos - 4..cpos].try_into().unwrap());
        let data_size = u32::from_be_bytes(prefix[cpos + 4..cpos + 8].try_into().unwrap());
        assert_eq!(data_size, 8 + 8 + 10); // 26
        assert_eq!(covr_size, 8 + data_size); // 34
    }

    #[test]
    fn build_udta_udta_size_exactly_u32_max_is_ok() {
        // The guard is `udta_size > u32::MAX` (strict). udta_size == u32::MAX must be
        // accepted; `> -> >=` rejects the exact boundary. data_len is reserved as a
        // number (no image bytes), so the boundary is cheap to hit.
        fn art(data_len: u64) -> ArtInput {
            ArtInput {
                art_id: 1,
                mime: "image/jpeg".into(),
                description: String::new(),
                picture_type: 3,
                width: 0,
                height: 0,
                data_len,
            }
        }
        // Derive the fixed overhead: with data_len 0, udta_size == overhead.
        let (p0, _) = build_udta(&[TagInput::new("title", "T")], Some(&art(0))).unwrap();
        let overhead = u32::from_be_bytes(p0[0..4].try_into().unwrap()) as u64;
        let max_len = u32::MAX as u64 - overhead;

        let (p_max, art_len) =
            build_udta(&[TagInput::new("title", "T")], Some(&art(max_len))).unwrap();
        assert_eq!(art_len, max_len);
        assert_eq!(
            u32::from_be_bytes(p_max[0..4].try_into().unwrap()),
            u32::MAX
        );

        assert!(matches!(
            build_udta(&[TagInput::new("title", "T")], Some(&art(max_len + 1))),
            Err(FormatError::TooLarge)
        ));
    }

    #[test]
    fn patch_chunk_offsets_stco_overflow_and_underflow_boundaries() {
        // kept = a single soun trak with one stco entry (offset 0). v = 0 + delta is
        // guarded by `v < 0 || v > u32::MAX`. Boundary deltas pin every guard mutant;
        // delta 0 (accepted) also pins the `:590` `+ -> *` bound at i = 0.
        let mut k = soun_trak();
        assert!(patch_chunk_offsets(&mut k, 0).is_ok()); // v == 0

        let mut k = soun_trak();
        assert!(patch_chunk_offsets(&mut k, u32::MAX as i64).is_ok()); // v == u32::MAX

        let mut k = soun_trak();
        assert!(matches!(
            patch_chunk_offsets(&mut k, u32::MAX as i64 + 1), // v == u32::MAX + 1
            Err(FormatError::TooLarge)
        ));

        let mut k = soun_trak();
        assert!(matches!(
            patch_chunk_offsets(&mut k, -1), // v == -1
            Err(FormatError::TooLarge)
        ));
    }

    #[test]
    fn patch_chunk_offsets_rejects_count_past_table() {
        // stco declares 2 entries but only 1 entry's bytes are present (followed by an
        // unrelated `free` box for padding). `pos + entry > start + len` must reject
        // the 2nd entry. `+ -> -` shrinks the bound and reads into the `free` box
        // instead of erroring (returns Ok).
        let mut stco = vec![0u8; 4]; // version/flags
        stco.extend_from_slice(&2u32.to_be_bytes()); // count = 2 (a lie)
        stco.extend_from_slice(&0u32.to_be_bytes()); // only 1 entry present
        let stbl = bx(
            b"stbl",
            &[bx(b"stco", &stco), bx(b"free", &[0u8; 8])].concat(),
        );
        let mut kept = bx(b"trak", &bx(b"mdia", &bx(b"minf", &stbl)));
        assert!(matches!(
            patch_chunk_offsets(&mut kept, 0),
            Err(FormatError::Malformed)
        ));
    }

    #[test]
    fn patch_chunk_offsets_co64_zero_offset_is_ok() {
        // co64 path guard is `v < 0`. offset 0 + delta 0 => v == 0 must be accepted;
        // `< -> ==`/`<= ` reject the boundary.
        let mut co64 = vec![0u8; 4]; // version/flags
        co64.extend_from_slice(&1u32.to_be_bytes()); // count 1
        co64.extend_from_slice(&0u64.to_be_bytes()); // offset 0
        let stbl = bx(b"stbl", &bx(b"co64", &co64));
        let mut kept = bx(b"trak", &bx(b"mdia", &bx(b"minf", &stbl)));
        assert!(patch_chunk_offsets(&mut kept, 0).is_ok());
    }
}
