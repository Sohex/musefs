//! Hand-rolled MP4/M4A box layer: parse the structure, read iTunes and QuickTime
//! keyed metadata, and regenerate `moov` (with patched chunk offsets) to
//! synthesize a re-tagged file whose `mdat` audio payload is served verbatim.
//! Strict: anything outside the supported shape (one audio track plus optional
//! chapter tracks, one `mdat`, non-fragmented) is rejected.

use crate::bytes::{read_u32_be, read_u64_be};
use crate::convert::usize_from;
use crate::error::{FormatError, Result};
use crate::input::{
    ArtInput, BinaryTagInput, EmbeddedBinaryTag, EmbeddedPicture, PictureType, TagInput,
};
use crate::layout::{RegionLayout, Segment};
use crate::size;
use std::collections::HashSet;
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
#[non_exhaustive]
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

/// Like [`child_boxes`] but lenient: stops at the first unreadable box and
/// returns the well-formed ones parsed so far, rather than discarding the whole
/// list. Box sizes chain, so a malformed box leaves no reliable way to find the
/// next — the prefix is the most that can be recovered. Used by the metadata
/// extractors, whose contract is to seed what they can and skip the rest (#524).
fn child_boxes_lenient(buf: &[u8]) -> Vec<BoxRef> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos + 8 <= buf.len() {
        let Ok(b) = read_box(buf, pos) else { break };
        pos = b.end();
        out.push(b);
    }
    out
}

fn find_box(buf: &[u8], kind: &[u8; 4]) -> Result<Option<BoxRef>> {
    Ok(child_boxes(buf)?.into_iter().find(|b| &b.kind == kind))
}

/// Like [`find_box`] but lenient: scans only the well-formed prefix
/// ([`child_boxes_lenient`]), so a malformed trailing child of a `----` atom
/// can't drop a valid `name`/`mean` that precedes it — keeping the metadata
/// extractors' "seed what you can" contract end-to-end (#524).
fn find_box_lenient(buf: &[u8], kind: &[u8; 4]) -> Option<BoxRef> {
    child_boxes_lenient(buf)
        .into_iter()
        .find(|b| &b.kind == kind)
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
#[non_exhaustive]
pub struct Mp4Bounds {
    pub audio_offset: u64,
    pub audio_length: u64,
}

/// Track handler types musefs serves alongside the audio track: QuickTime-style
/// chapter tracks, carried as a second `text` (or `sbtl`) track. Chapters are
/// the reason the `.m4b` extension exists, so rejecting every chaptered file
/// rejected most of an audiobook library (#672).
const CHAPTER_HANDLERS: [[u8; 4]; 2] = [*b"text", *b"sbtl"];

/// Validate the internal `moov` shape: no fragmentation (`mvex`), exactly one
/// audio (`soun`) track, and every other track a chapter track
/// ([`CHAPTER_HANDLERS`]). `moov_payload` is the bytes inside the `moov` box
/// (after its header).
///
/// On rejection the error names the handler types found, so a skipped file
/// explains itself instead of landing in an `unparseable` tally.
fn validate_moov(moov_payload: &[u8]) -> Result<()> {
    if find_box(moov_payload, b"mvex")?.is_some() {
        return Err(FormatError::NotMp4);
    }
    let traks: Vec<_> = child_boxes(moov_payload)?
        .into_iter()
        .filter(|b| &b.kind == b"trak")
        .collect();

    let mut handlers: Vec<String> = Vec::new();
    let mut soun = 0usize;
    let mut unsupported = false;
    for t in &traks {
        let trak = t.payload(moov_payload);
        // A `trak` with no readable `hdlr` cannot be classified; describe it and
        // let the tally below reject, rather than erroring with no explanation.
        let handler = find_path(trak, &[b"mdia", b"hdlr"])?
            .and_then(|(hp, hl)| trak[hp..hp + hl].get(8..12))
            .and_then(|h| <[u8; 4]>::try_from(h).ok());
        match handler {
            Some(h) if h == *b"soun" => soun += 1,
            Some(h) if CHAPTER_HANDLERS.contains(&h) => {}
            _ => unsupported = true,
        }
        handlers.push(match handler {
            Some(h) => String::from_utf8_lossy(&h).into_owned(),
            None => "<no hdlr>".to_string(),
        });
    }

    if soun != 1 || unsupported {
        return Err(FormatError::Mp4Tracks(format!(
            "expected one audio (soun) track, optionally with text/sbtl chapter \
             tracks; found [{}]",
            handlers.join(", ")
        )));
    }
    Ok(())
}

/// Validate the supported shape; return the ftyp/moov/mdat boxes (absolute offsets
/// in `buf`). Rejects fragmented, video, and multi-`mdat` files; accepts one audio
/// track plus any number of chapter tracks (see [`validate_moov`]).
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
#[non_exhaustive]
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

/// The children of a `meta` box, given its payload.
///
/// `meta` is normally an ISO FullBox — 4 version/flags bytes precede its children —
/// but QuickTime also uses a bare `meta` with no such prefix. Per ISO 14496-12 the
/// version/flags word is always zero, so a zero first word marks the FullBox variant
/// (skip 4) and any other value marks the bare variant (skip 0) (#543). A bare
/// first child declaring `size == 0` ("extends to end") is the one ambiguous case
/// and is misread as a FullBox, mirroring the wider tooling's heuristic.
fn meta_children(meta_payload: &[u8]) -> &[u8] {
    match meta_payload.split_first_chunk::<4>() {
        Some(([0, 0, 0, 0], children)) => children,
        _ => meta_payload,
    }
}

/// The handler type of QuickTime keyed metadata: a `keys` table plus an `ilst`
/// whose item atom types are 1-based indexes into it, instead of FourCCs.
const KEYED_HANDLER: [u8; 4] = *b"mdta";

/// Whether a `meta` box (given its payload) holds QuickTime keyed metadata: its
/// `hdlr` declares the `mdta` handler (`[version/flags][pre_defined][handler_type]`).
/// The handler, not the presence of `keys`, is what ffmpeg's reader also gates on
/// (`found_hdlr_mdta`). A `meta` with no readable `hdlr` is not keyed.
fn is_keyed_meta(meta_payload: &[u8]) -> bool {
    let children = meta_children(meta_payload);
    find_box_lenient(children, b"hdlr")
        .and_then(|h| h.payload(children).get(8..12))
        .is_some_and(|handler| handler == KEYED_HANDLER)
}

/// The payloads of `buf`'s well-formed child boxes of type `kind`, in order.
fn child_payloads<'a>(buf: &'a [u8], kind: &[u8; 4]) -> Vec<&'a [u8]> {
    child_boxes_lenient(buf)
        .into_iter()
        .filter(|b| &b.kind == kind)
        .map(|b| b.payload(buf))
        .collect()
}

/// Locate the iTunes `moov/udta/meta/ilst` and return its payload. The walk is
/// lenient ([`find_box_lenient`]) at every level: a single malformed sibling box
/// anywhere on the path must not suppress an otherwise well-formed `ilst`,
/// matching the metadata extractors' "seed what you can" contract (#542).
/// Strictness is reserved for the audio/structure path.
///
/// The iTunes `meta` is the first one in `udta` that is not keyed metadata:
/// ffmpeg's `-movflags use_metadata_tags` puts an `mdta` `meta` in `udta` too,
/// and its index-typed `ilst` is read by [`keyed_items`] instead (#771).
fn itunes_ilst(buf: &[u8]) -> Option<&[u8]> {
    let moov = find_box_lenient(buf, b"moov")?;
    let mp = moov.payload(buf);
    let udta = find_box_lenient(mp, b"udta")?;
    let meta = child_payloads(udta.payload(mp), b"meta")
        .into_iter()
        .find(|meta| !is_keyed_meta(meta))?;
    let children = meta_children(meta);
    Some(find_box_lenient(children, b"ilst")?.payload(children))
}

/// One QuickTime keyed-metadata item: the key its index resolved to, and its
/// `data` box payloads (`[type][locale][value]`, each at least 8 bytes) in file
/// order.
struct KeyedItem<'a> {
    name: &'a str,
    datas: Vec<&'a [u8]>,
}

/// Every keyed-metadata `meta` in the file, as its children, in precedence
/// order: the movie-level `moov/meta`, then `moov/udta/meta` (where ffmpeg
/// writes it), then each track's `trak/meta` and `trak/mdia/meta` — the three
/// locations the QuickTime File Format allows, plus ffmpeg's. Lenient
/// throughout: a malformed box ends only its own sibling list. These are exactly
/// the boxes synthesis drops, which the fuzz oracle checks against.
pub(crate) fn keyed_metas(buf: &[u8]) -> Vec<&[u8]> {
    let Some(moov) = find_box_lenient(buf, b"moov") else {
        return Vec::new();
    };
    let mp = moov.payload(buf);
    let mut containers = vec![mp];
    containers.extend(child_payloads(mp, b"udta"));
    for trak in child_payloads(mp, b"trak") {
        containers.push(trak);
        containers.extend(child_payloads(trak, b"mdia"));
    }
    containers
        .into_iter()
        .flat_map(|container| child_payloads(container, b"meta"))
        .filter(|meta| is_keyed_meta(meta))
        .map(meta_children)
        .collect()
}

/// Every track's chunk offsets, in track order, from a whole `moov` box: the
/// `stco` entries widened, or the `co64` ones. The relocation oracle in
/// `fuzz_check` compares a served file's against its source's.
#[cfg(any(test, feature = "fuzzing"))]
pub(crate) fn chunk_offsets(moov: &[u8]) -> Result<Vec<Vec<u64>>> {
    let payload = read_box(moov, 0)?.payload(moov);
    child_boxes(payload)?
        .into_iter()
        .filter(|b| &b.kind == b"trak")
        .map(|t| {
            let trak = t.payload(payload);
            let (range, width) = match find_path(trak, &[b"mdia", b"minf", b"stbl", b"stco"])? {
                Some(r) => (r, 4),
                None => (
                    find_path(trak, &[b"mdia", b"minf", b"stbl", b"co64"])?
                        .ok_or(FormatError::Malformed)?,
                    8,
                ),
            };
            let table = &trak[range.0..range.0 + range.1];
            let count = usize_from(u64::from(read_u32_be(table, 4)?));
            (0..count)
                .map(|i| {
                    let pos = 8 + i * width;
                    if width == 4 {
                        read_u32_be(table, pos).map(u64::from)
                    } else {
                        read_u64_be(table, pos)
                    }
                })
                .collect()
        })
        .collect()
}

/// Every keyed-metadata item in the file: [`keyed_metas`] order, and `ilst`
/// order within one `meta`.
fn keyed_items(buf: &[u8]) -> Vec<KeyedItem<'_>> {
    keyed_metas(buf)
        .into_iter()
        .flat_map(read_keyed_meta)
        .collect()
}

/// The items of one keyed `meta` (given its children): each `ilst` item's box
/// type is a big-endian 1-based index into `keys`. An item whose index is 0
/// (reserved), past the table, or names an unusable key slot is skipped.
fn read_keyed_meta(children: &[u8]) -> Vec<KeyedItem<'_>> {
    let keys = find_box_lenient(children, b"keys")
        .map_or_else(Vec::new, |k| parse_keys(k.payload(children)));
    let Some(ilst) = find_box_lenient(children, b"ilst") else {
        return Vec::new();
    };
    let ilst = ilst.payload(children);
    child_boxes_lenient(ilst)
        .into_iter()
        .filter_map(|item| {
            let index = usize_from(u64::from(u32::from_be_bytes(item.kind)));
            let name = (*keys.get(index.checked_sub(1)?)?)?;
            let datas = child_payloads(item.payload(ilst), b"data")
                .into_iter()
                .filter(|dp| dp.len() >= 8)
                .collect();
            Some(KeyedItem { name, datas })
        })
        .collect()
}

/// The key names of a `keys` table payload (`[version/flags][entry_count]`, then
/// per entry `[key_size][key_namespace][key_value]`); slot `i` is index `i + 1`.
/// An entry outside the `mdta` namespace, or whose name is not UTF-8, is a `None`
/// slot, so the indexes after it stay aligned. Lenient: the table ends at
/// `entry_count` or at the first entry that does not fit, and grows only with
/// entries actually present — never allocated from the declared count.
fn parse_keys(payload: &[u8]) -> Vec<Option<&str>> {
    let Ok(count) = read_u32_be(payload, 4) else {
        return Vec::new();
    };
    let mut keys = Vec::new();
    let mut pos = 8;
    while keys.len() < usize_from(u64::from(count)) {
        let Ok(size) = read_u32_be(payload, pos) else {
            break;
        };
        let size = usize_from(u64::from(size));
        let Some(entry) = pos.checked_add(size).and_then(|end| payload.get(pos..end)) else {
            break;
        };
        let Some((header, name)) = entry.split_at_checked(8) else {
            break;
        };
        keys.push(if header[4..] == KEYED_HANDLER {
            std::str::from_utf8(name).ok()
        } else {
            None
        });
        pos += size;
    }
    keys
}

/// Choose one value from a keyed item's `data` boxes. Several `data` boxes are
/// alternative representations of one datum — by locale or storage type — ordered
/// most-specific first (QTFF "Data ordering"), not multiple values. So: the first
/// default-locale (locale 0) value `decode` accepts, else the last one it accepts,
/// the most general.
fn pick_value<'a, T>(datas: &[&'a [u8]], decode: impl Fn(u32, &'a [u8]) -> Option<T>) -> Option<T> {
    let mut fallback = None;
    for dp in datas {
        let type_code = u32::from_be_bytes([dp[0], dp[1], dp[2], dp[3]]);
        let Some(value) = decode(type_code, &dp[8..]) else {
            continue;
        };
        if dp[4..8] == [0, 0, 0, 0] {
            return Some(value);
        }
        fallback = Some(value);
    }
    fallback
}

/// Render a keyed `data` value as tag text, by its QTFF well-known type: UTF-8
/// (1) and UTF-16BE (2, a leading byte-order mark dropped); the big-endian
/// integers — variable-width signed (21) and unsigned (22), and the fixed-width
/// 8/16/32/64-bit signed (65/66/67/74) and unsigned (75/76/77/78) — as decimal;
/// and finite float32 (23) / float64 (24) as their shortest decimal form. Every
/// other type (sort-only strings, S/JIS, images, structured types) and every value
/// whose length does not fit its type is not text: `None`.
fn keyed_text(type_code: u32, value: &[u8]) -> Option<String> {
    match (type_code, value.len()) {
        (1, _) => std::str::from_utf8(value).ok().map(str::to_string),
        (2, _) => utf16_be(value),
        (21, 1..=8) | (65, 1) | (66, 2) | (67, 4) | (74, 8) => Some(be_signed(value).to_string()),
        (22, 1..=8) | (75, 1) | (76, 2) | (77, 4) | (78, 8) => Some(be_unsigned(value).to_string()),
        (23, 4) => {
            let f = f32::from_be_bytes(value.try_into().ok()?);
            f.is_finite().then(|| f.to_string())
        }
        (24, 8) => {
            let f = f64::from_be_bytes(value.try_into().ok()?);
            f.is_finite().then(|| f.to_string())
        }
        _ => None,
    }
}

/// A big-endian unsigned integer of up to 8 bytes.
fn be_unsigned(value: &[u8]) -> u64 {
    debug_assert!(value.len() <= 8, "at most 8 bytes");
    value.iter().fold(0, |n, &b| (n << 8) | u64::from(b))
}

/// A big-endian two's-complement integer of 1 to 8 bytes, sign-extended.
fn be_signed(value: &[u8]) -> i64 {
    debug_assert!((1..=8).contains(&value.len()), "1 to 8 bytes");
    let unused = 64 - 8 * u32::try_from(value.len()).expect("1 to 8 bytes");
    (be_unsigned(value) << unused).cast_signed() >> unused
}

/// Decode UTF-16BE text, dropping a leading byte-order mark. An odd byte count
/// or an unpaired surrogate is not text.
fn utf16_be(value: &[u8]) -> Option<String> {
    let (pairs, []) = value.as_chunks::<2>() else {
        return None;
    };
    let units: Vec<u16> = pairs.iter().map(|&pair| u16::from_be_bytes(pair)).collect();
    String::from_utf16(units.strip_prefix(&[0xFEFF]).unwrap_or(&units)).ok()
}

/// The image MIME type for a `covr`/artwork `data` type code: JPEG (13) and PNG
/// (14) only.
fn image_mime(type_code: u32) -> Option<&'static str> {
    match type_code {
        13 => Some("image/jpeg"),
        14 => Some("image/png"),
        _ => None,
    }
}

/// Parse a `----` freeform atom payload into `(key, value)` pairs. Folds
/// (mean, name) to a canonical key via the vocabulary, else keys on the verbatim
/// `name`. One pair per UTF-8 (`type 1`) `data` sub-box — the iTunes multi-value
/// convention; binary-typed `data` boxes are left to [`read_binary_tags_reporting`]. Empty
/// if malformed.
fn read_freeform(inner: &[u8]) -> Vec<(String, String)> {
    let Some(name_box) = find_box_lenient(inner, b"name") else {
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
    let mean = find_box_lenient(inner, b"mean").map_or("com.apple.iTunes", |m| {
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
    for data in child_boxes_lenient(inner) {
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
/// absent.
///
/// The iTunes `ilst` is read first: text atoms map via the vocabulary;
/// `trkn`/`disk` yield track/disc numbers as `"N"`/`"N/M"`; `----` freeform atoms
/// key on their name (folded when known). Every `data` sub-box of an atom is
/// read, so multi-value atoms recover all their values. Other atoms are skipped.
///
/// QuickTime keyed metadata (`keyed_items`, #771) then fills in keys the `ilst`
/// does not define. A key name folds onto the vocabulary when it has a mapping
/// (`com.apple.quicktime.artist` → `artist`), and is otherwise kept verbatim, as
/// an unknown `----` name is. Precedence is all-or-nothing per key, compared
/// case-insensitively: a key the `ilst` carries takes nothing from keyed
/// metadata, and among keyed items the first (in `keyed_items` order) that
/// yields a value supplies the key. Each item yields at most one value
/// (`pick_value`, `keyed_text`); the artwork key is art, not text.
pub fn read_tags(buf: &[u8]) -> Vec<(String, String)> {
    let mut out = itunes_ilst(buf).map_or_else(Vec::new, read_ilst_tags);
    let mut present: HashSet<String> = out.iter().map(|(k, _)| k.to_ascii_lowercase()).collect();
    for item in keyed_items(buf) {
        if item
            .name
            .eq_ignore_ascii_case(crate::tagmap::MP4_KEYED_ARTWORK)
        {
            continue;
        }
        let key = crate::tagmap::mp4_keyed_to_key(item.name).unwrap_or(item.name);
        if present.contains(&key.to_ascii_lowercase()) {
            continue;
        }
        if let Some(value) = pick_value(&item.datas, keyed_text) {
            present.insert(key.to_ascii_lowercase());
            out.push((key.to_string(), value));
        }
    }
    out
}

/// The text tags of an iTunes `ilst` payload (see [`read_tags`]).
fn read_ilst_tags(ilst: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for atom in child_boxes_lenient(ilst) {
        let inner = atom.payload(ilst);
        if &atom.kind == b"----" {
            out.extend(read_freeform(inner));
            continue;
        }
        let text_key = crate::tagmap::mp4_atom_to_key(&atom.kind);
        for data in child_boxes_lenient(inner) {
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
                let n = be_unsigned(&value[..value.len().min(8)]);
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
#[non_exhaustive]
pub struct OversizeDrop {
    /// Cover-art MIME type, or the binary tag's `----:<mean>:<name>` key.
    pub descriptor: String,
    /// Size of the dropped payload body in bytes (after the 8-byte `data` header).
    pub bytes: usize,
}

/// Like [`read_pictures`], but also returns the oversized images skipped over
/// `max_art_bytes`, so the caller can log each lossy drop. The size check still
/// happens before any copy — an oversized image is described, never
/// materialized. See [`OversizeDrop`].
pub fn read_pictures_reporting(
    buf: &[u8],
    max_art_bytes: usize,
) -> (Vec<EmbeddedPicture>, Vec<OversizeDrop>) {
    let mut out = Vec::new();
    let mut dropped = Vec::new();
    let covr_atoms = itunes_ilst(buf).map_or_else(Vec::new, |ilst| child_payloads(ilst, b"covr"));
    for covr in covr_atoms {
        for dp in child_payloads(covr, b"data") {
            if dp.len() < 8 {
                continue;
            }
            let Some(mime) = image_mime(u32::from_be_bytes([dp[0], dp[1], dp[2], dp[3]])) else {
                continue;
            };
            push_picture(mime, &dp[8..], max_art_bytes, &mut out, &mut dropped);
        }
    }
    // The `covr` art wins outright, as the `ilst` does for text; an oversize
    // `covr` still claims the slot, since it fails the file rather than
    // quietly giving way to a different image.
    if out.is_empty() && dropped.is_empty() {
        let artwork = keyed_items(buf)
            .iter()
            .filter(|item| {
                item.name
                    .eq_ignore_ascii_case(crate::tagmap::MP4_KEYED_ARTWORK)
            })
            .find_map(|item| {
                pick_value(&item.datas, |type_code, value| {
                    image_mime(type_code).map(|mime| (mime, value))
                })
            });
        if let Some((mime, image)) = artwork {
            push_picture(mime, image, max_art_bytes, &mut out, &mut dropped);
        }
    }
    (out, dropped)
}

/// Record one embedded image: a front-cover picture, or an [`OversizeDrop`] when
/// `image` exceeds `max_art_bytes` (checked before the copy).
fn push_picture(
    mime: &str,
    image: &[u8],
    max_art_bytes: usize,
    out: &mut Vec<EmbeddedPicture>,
    dropped: &mut Vec<OversizeDrop>,
) {
    if image.len() > max_art_bytes {
        dropped.push(OversizeDrop {
            descriptor: mime.to_string(),
            bytes: image.len(),
        });
        return;
    }
    out.push(EmbeddedPicture {
        mime: mime.to_string(),
        picture_type: PictureType::new(3).expect("3 is in range"),
        description: String::new(),
        // `covr` and keyed artwork are image bytes and a type flag; neither
        // declares geometry, so these are all "not stated" as with `APIC`.
        width: 0,
        height: 0,
        depth: 0,
        colors: 0,
        data: image.to_vec(),
    });
}

/// Lenient: returns empty / skips any malformed atom and never errors — this only
/// seeds cover art from existing files, so a missing or garbled picture must simply be absent.
/// Every `data` child of every `covr` atom yields one picture (the iTunes
/// multiple-artwork convention); non-`data` children are skipped. Only when the
/// `covr` atoms yield nothing (not even an oversize drop) is QuickTime keyed
/// metadata's `com.apple.quicktime.artwork` consulted: the first artwork item, in
/// `keyed_items` order, holding a JPEG/PNG value gives one picture.
///
/// `max_art_bytes` caps each image body: a `data` payload whose image bytes
/// (after the 8-byte `[type][locale]` header) exceed it is skipped before any
/// copy, so an oversized `covr` in a large `moov` is never materialized. Use
/// [`read_pictures_reporting`] to also recover the oversized drops for logging.
pub fn read_pictures(buf: &[u8], max_art_bytes: usize) -> Vec<EmbeddedPicture> {
    read_pictures_reporting(buf, max_art_bytes).0
}

/// Every binary-typed `----` atom's value, as `read_binary_tags` describes, and
/// also the oversized `----` values
/// skipped over `max_binary_tag_bytes`, so the caller can log each lossy drop.
/// The size check still happens before any copy — an oversized value is
/// described, never materialized. See [`OversizeDrop`].
pub fn read_binary_tags_reporting(
    buf: &[u8],
    max_binary_tag_bytes: usize,
) -> (Vec<EmbeddedBinaryTag>, Vec<OversizeDrop>) {
    // Keyed metadata has no binary passthrough: its non-text values are either
    // artwork or dropped (see `keyed_text`), so only the iTunes `ilst` is read.
    let Some(ilst) = itunes_ilst(buf) else {
        return (Vec::new(), Vec::new());
    };
    let mut out = Vec::new();
    let mut dropped = Vec::new();
    for atom in child_boxes_lenient(ilst) {
        if &atom.kind != b"----" {
            continue;
        }
        let inner = atom.payload(ilst);
        // name/mean payloads carry a 4-byte FullBox prefix; default mean to iTunes.
        let Some(name) = find_box_lenient(inner, b"name").and_then(|n| {
            let p = n.payload(inner);
            (p.len() >= 4)
                .then(|| std::str::from_utf8(&p[4..]).ok())
                .flatten()
        }) else {
            continue;
        };
        let mean = find_box_lenient(inner, b"mean").map_or("com.apple.iTunes", |m| {
            let p = m.payload(inner);
            if p.len() >= 4 {
                std::str::from_utf8(&p[4..]).unwrap_or("com.apple.iTunes")
            } else {
                "com.apple.iTunes"
            }
        });
        let key = format!("----:{mean}:{name}");
        // Iterate every `data` sub-box, mirroring the text path: a `----` atom can
        // carry a type-1 text value and a separate binary value, so inspecting only
        // the first `data` would lose the binary one (#525).
        for data in child_boxes_lenient(inner) {
            if &data.kind != b"data" {
                continue;
            }
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
            if dp.len() - 8 > max_binary_tag_bytes {
                dropped.push(OversizeDrop {
                    descriptor: key.clone(),
                    bytes: dp.len() - 8,
                });
                continue;
            }
            out.push(EmbeddedBinaryTag {
                key: key.clone(),
                payload: dp[8..].to_vec(),
            });
        }
    }
    (out, dropped)
}

/// Extract opaque (non-text) MP4 `----` freeform atoms for binary-tag passthrough.
/// One `EmbeddedBinaryTag` per binary-typed (type code != 1) `data` sub-box of
/// each `----` atom: key `----:<mean>:<name>`, payload the `data` value bytes
/// (after the 8-byte `[type][locale]` header). Text freeform atoms (type 1) are
/// handled by `read_tags`, so the two paths never double-store. Lenient:
/// malformed atoms are skipped. Every `data` sub-box is inspected, so a mixed
/// atom carrying both a text and a binary value recovers the binary one.
///
/// `max_binary_tag_bytes` caps each value: a `data` payload whose value bytes
/// (after the 8-byte `[type][locale]` header) exceed it is skipped before any
/// copy, so an oversized `----` in a large `moov` is never materialized. Use
/// [`read_binary_tags_reporting`] to also recover the oversized drops for logging.
///
/// Test scaffolding, behind `fuzzing` (#710): the scanner reads through
/// [`read_binary_tags_reporting`].
#[cfg(any(test, feature = "fuzzing"))]
pub fn read_binary_tags(buf: &[u8], max_binary_tag_bytes: usize) -> Vec<EmbeddedBinaryTag> {
    read_binary_tags_reporting(buf, max_binary_tag_bytes).0
}

mod synth;
pub use synth::synthesize_layout;

#[cfg(test)]
mod tests;
