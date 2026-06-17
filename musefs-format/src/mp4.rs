//! Hand-rolled MP4/M4A box layer: parse the structure, read iTunes metadata, and
//! regenerate `moov` (with patched chunk offsets) to synthesize a re-tagged file
//! whose `mdat` audio payload is served verbatim. Strict: anything outside the
//! supported shape (single audio track, one `mdat`, non-fragmented) is rejected.

use crate::bytes::{read_u32_be, read_u64_be};
use crate::convert::usize_from;
use crate::error::{FormatError, Result};
use crate::input::{
    ArtInput, BinaryTagInput, EmbeddedBinaryTag, EmbeddedPicture, PictureType, TagInput,
};
use crate::layout::{RegionLayout, Segment};
use crate::size;
use std::io::{self, Read, Seek, SeekFrom};

const MAX_MP4_METADATA_BYTES: u64 = 256 * 1024 * 1024;

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
    let size32 = u64::from(read_u32_be(hdr, 0)?);
    let kind: [u8; 4] = hdr
        .get(4..8)
        .ok_or(FormatError::Malformed)?
        .try_into()
        .unwrap();
    let (header_len, total_len) = match size32 {
        1 => (16u64, read_u64_be(hdr, 8)?),
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
    #[error("MP4 {box_kind} box is {size} bytes, exceeds the {cap}-byte metadata cap")]
    MetadataTooLarge {
        box_kind: &'static str,
        size: u64,
        cap: u64,
    },
}

fn read_box(buf: &[u8], pos: usize) -> Result<BoxRef> {
    let size32 = u64::from(read_u32_be(buf, pos)?);
    let kind: [u8; 4] = buf
        .get(pos + 4..pos + 8)
        .ok_or(FormatError::Malformed)?
        .try_into()
        .unwrap();
    let (header_len, total) = match size32 {
        1 => (16usize, read_u64_be(buf, pos + 8)?),
        0 => (8usize, (buf.len() - pos) as u64),
        n => (8usize, n),
    };
    let total = usize_from(total);
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

    for (box_kind, total_len) in [("ftyp", ftyp_h.total_len), ("moov", moov_h.total_len)] {
        if total_len > MAX_MP4_METADATA_BYTES {
            return Err(Mp4ScanError::MetadataTooLarge {
                box_kind,
                size: total_len,
                cap: MAX_MP4_METADATA_BYTES,
            });
        }
    }

    // `try_from` rather than `as usize`: on a 32-bit target an oversized box would
    // truncate silently; a box larger than `usize` is malformed for our purposes.
    let ftyp_len = usize::try_from(ftyp_h.total_len).map_err(|_| FormatError::Malformed)?;
    let moov_len = usize::try_from(moov_h.total_len).map_err(|_| FormatError::Malformed)?;
    let ftyp_bytes = region(r, ftyp_s, ftyp_len)?;
    let moov_bytes = region(r, moov_s, moov_len)?;
    let mdat_header = region(r, mdat_s, usize_from(mdat_h.header_len))?;

    validate_moov(&moov_bytes[usize_from(moov_h.header_len)..])?;

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

/// Parse a `----` freeform atom payload into `(key, value)` pairs. Folds
/// (mean, name) to a canonical key via the vocabulary, else keys on the verbatim
/// `name`. One pair per UTF-8 (`type 1`) `data` sub-box — the iTunes multi-value
/// convention; binary-typed `data` boxes are left to [`read_binary_tags`]. Empty
/// if malformed.
fn read_freeform(inner: &[u8]) -> Vec<(String, String)> {
    let Some(name_box) = find_box(inner, b"name").ok().flatten() else {
        return Vec::new();
    };
    let np = name_box.payload(inner);
    if np.len() < 4 {
        return Vec::new();
    }
    // name/mean payloads start with a 4-byte FullBox [version 1][flags 3] prefix.
    let Ok(name) = std::str::from_utf8(&np[4..]) else {
        return Vec::new();
    };
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
    let mut out = Vec::new();
    for data in child_boxes(inner).unwrap_or_default() {
        if &data.kind != b"data" {
            continue;
        }
        let dp = data.payload(inner);
        if dp.len() < 8 {
            continue;
        }
        // The `data` box is `[type: u32][locale: u32][value]`; type 1 == UTF-8 text.
        // Binary-typed freeform values are not text tags, so skip them.
        let type_code = u32::from_be_bytes([dp[0], dp[1], dp[2], dp[3]]);
        if type_code != 1 {
            continue;
        }
        if let Ok(value) = std::str::from_utf8(&dp[8..]) {
            out.push((key.clone(), value.to_string()));
        }
    }
    out
}

/// Format a `trkn`/`disk` value body `[reserved 2][number 2][total 2]…` as the
/// canonical `"N"` or `"N/M"` string. The `"N/M"` form matches how ID3
/// `TRCK`/`TPOS` carry the total in the shared `tracknumber`/`discnumber` value;
/// a zero or absent total drops the `/M`. Caller guarantees `value.len() >= 4`.
fn number_total(value: &[u8]) -> String {
    debug_assert!(
        value.len() >= 4,
        "number_total requires the 4-byte number prefix"
    );
    let number = u16::from_be_bytes([value[2], value[3]]);
    let total = if value.len() >= 6 {
        u16::from_be_bytes([value[4], value[5]])
    } else {
        0
    };
    if total != 0 {
        format!("{number}/{total}")
    } else {
        number.to_string()
    }
}

/// Lenient: returns empty / skips any malformed atom and never errors — this only
/// seeds metadata from existing files, so a missing or garbled tag must simply be
/// absent. Text atoms map via the vocabulary; `trkn`/`disk` yield track/disc
/// numbers as `"N"`/`"N/M"`; `----` freeform atoms key on their name (folded when
/// known). Every `data` sub-box of an atom is read, so multi-value atoms recover
/// all their values. Other atoms are skipped.
pub fn read_tags(buf: &[u8]) -> Vec<(String, String)> {
    let Some((start, len)) = ilst_region(buf) else {
        return Vec::new();
    };
    let ilst = &buf[start..start + len];
    let mut out = Vec::new();
    for atom in child_boxes(ilst).unwrap_or_default() {
        let inner = atom.payload(ilst);
        if &atom.kind == b"----" {
            out.extend(read_freeform(inner));
            continue;
        }
        let text_key = crate::tagmap::mp4_atom_to_key(&atom.kind);
        for data in child_boxes(inner).unwrap_or_default() {
            if &data.kind != b"data" {
                continue;
            }
            let dp = data.payload(inner);
            if dp.len() < 8 {
                continue;
            }
            let value = &dp[8..]; // skip [type 4][locale 4]
            if let Some(key) = text_key {
                if let Ok(s) = std::str::from_utf8(value) {
                    out.push((key.to_string(), s.to_string()));
                }
            } else if &atom.kind == b"trkn" && value.len() >= 4 {
                out.push(("tracknumber".into(), number_total(value)));
            } else if &atom.kind == b"disk" && value.len() >= 4 {
                out.push(("discnumber".into(), number_total(value)));
            } else if let Some(key) = crate::tagmap::mp4_integer_atom_to_key(&atom.kind) {
                // tmpo/cpil/pgap: a big-endian unsigned integer in the value bytes.
                let mut n: u64 = 0;
                for &b in value.iter().take(8) {
                    n = (n << 8) | u64::from(b);
                }
                out.push((key.to_string(), n.to_string()));
            }
        }
    }
    out
}

/// An embedded `covr` image or binary `----` payload that a reader skipped
/// because it exceeded the caller's size cap. Carries only a descriptor and the
/// payload's byte size — never the bytes themselves — so the caller can log the
/// lossy drop (the format layer has no logging facade) without materializing the
/// oversized item out of a potentially large `moov` (#343).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OversizeDrop {
    /// Cover-art MIME type, or the binary tag's `----:<mean>:<name>` key.
    pub descriptor: String,
    /// Size of the dropped payload body in bytes (after the 8-byte `data` header).
    pub bytes: usize,
}

/// Like [`read_pictures`], but also returns the oversized `covr` images skipped
/// over `max_art_bytes`, so the caller can log each lossy drop. The size check
/// still happens before any copy — an oversized image is described, never
/// materialized. See [`OversizeDrop`].
pub fn read_pictures_reporting(
    buf: &[u8],
    max_art_bytes: usize,
) -> (Vec<EmbeddedPicture>, Vec<OversizeDrop>) {
    let Some((start, len)) = ilst_region(buf) else {
        return (Vec::new(), Vec::new());
    };
    let ilst = &buf[start..start + len];
    let mut out = Vec::new();
    let mut dropped = Vec::new();
    for atom in child_boxes(ilst).unwrap_or_default() {
        if &atom.kind != b"covr" {
            continue;
        }
        let inner = atom.payload(ilst);
        for data in child_boxes(inner).unwrap_or_default() {
            if &data.kind != b"data" {
                continue;
            }
            let dp = data.payload(inner);
            if dp.len() < 8 {
                continue;
            }
            let mime = match u32::from_be_bytes([dp[0], dp[1], dp[2], dp[3]]) {
                13 => "image/jpeg",
                14 => "image/png",
                _ => continue,
            };
            if dp.len() - 8 > max_art_bytes {
                dropped.push(OversizeDrop {
                    descriptor: mime.to_string(),
                    bytes: dp.len() - 8,
                });
                continue;
            }
            out.push(EmbeddedPicture {
                mime: mime.to_string(),
                picture_type: PictureType::new(3).expect("3 is in range"),
                description: String::new(),
                width: 0,
                height: 0,
                data: dp[8..].to_vec(),
            });
        }
    }
    (out, dropped)
}

/// Lenient: returns empty / skips any malformed atom and never errors — this only
/// seeds cover art from existing files, so a missing or garbled picture must simply be absent.
/// Every `data` child of every `covr` atom yields one picture (the iTunes
/// multiple-artwork convention); non-`data` children are skipped.
///
/// `max_art_bytes` caps each image body: a `data` payload whose image bytes
/// (after the 8-byte `[type][locale]` header) exceed it is skipped before any
/// copy, so an oversized `covr` in a large `moov` is never materialized. Use
/// [`read_pictures_reporting`] to also recover the oversized drops for logging.
pub fn read_pictures(buf: &[u8], max_art_bytes: usize) -> Vec<EmbeddedPicture> {
    read_pictures_reporting(buf, max_art_bytes).0
}

/// Like [`read_binary_tags`], but also returns the oversized `----` values
/// skipped over `max_binary_tag_bytes`, so the caller can log each lossy drop.
/// The size check still happens before any copy — an oversized value is
/// described, never materialized. See [`OversizeDrop`].
pub fn read_binary_tags_reporting(
    buf: &[u8],
    max_binary_tag_bytes: usize,
) -> (Vec<EmbeddedBinaryTag>, Vec<OversizeDrop>) {
    let Some((start, len)) = ilst_region(buf) else {
        return (Vec::new(), Vec::new());
    };
    let ilst = &buf[start..start + len];
    let mut out = Vec::new();
    let mut dropped = Vec::new();
    for atom in child_boxes(ilst).unwrap_or_default() {
        if &atom.kind != b"----" {
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
        // `data` body is `[type: u32][locale: u32][value]`; type 1 == UTF-8 text,
        // which is the text path's job. Everything else is opaque binary.
        let type_code = u32::from_be_bytes([dp[0], dp[1], dp[2], dp[3]]);
        if type_code == 1 {
            continue;
        }
        // name/mean payloads carry a 4-byte FullBox prefix; default mean to iTunes.
        let Some(name) = find_box(inner, b"name").ok().flatten().and_then(|n| {
            let p = n.payload(inner);
            (p.len() >= 4)
                .then(|| std::str::from_utf8(&p[4..]).ok())
                .flatten()
        }) else {
            continue;
        };
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
        let key = format!("----:{mean}:{name}");
        if dp.len() - 8 > max_binary_tag_bytes {
            dropped.push(OversizeDrop {
                descriptor: key,
                bytes: dp.len() - 8,
            });
            continue;
        }
        out.push(EmbeddedBinaryTag {
            key,
            payload: dp[8..].to_vec(),
        });
    }
    (out, dropped)
}

/// Extract opaque (non-text) MP4 `----` freeform atoms for binary-tag passthrough.
/// One `EmbeddedBinaryTag` per `----` atom whose first `data` sub-box is
/// binary-typed (type code != 1): key `----:<mean>:<name>`, payload the `data`
/// value bytes (after the 8-byte `[type][locale]` header). Text freeform atoms
/// (type 1) are handled by `read_tags`, so the two paths never double-store.
/// Lenient: malformed atoms are skipped. Only the first `data` sub-box is read
/// (multi-value freeform is rare; mirrors `read_freeform`).
///
/// `max_binary_tag_bytes` caps each value: a `data` payload whose value bytes
/// (after the 8-byte `[type][locale]` header) exceed it is skipped before any
/// copy, so an oversized `----` in a large `moov` is never materialized. Use
/// [`read_binary_tags_reporting`] to also recover the oversized drops for logging.
pub fn read_binary_tags(buf: &[u8], max_binary_tag_bytes: usize) -> Vec<EmbeddedBinaryTag> {
    read_binary_tags_reporting(buf, max_binary_tag_bytes).0
}

mod synth;
pub use synth::synthesize_layout;

#[cfg(test)]
mod tests;
