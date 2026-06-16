use super::{
    ArtInput, BinaryTagInput, FormatError, Mp4Scan, RegionLayout, Result, Segment, TagInput,
    child_boxes, find_path, read_box, read_u32_be, read_u64_be, size,
};

pub(super) fn boxed(kind: &[u8; 4], payload: &[u8]) -> Result<Vec<u8>> {
    let size = u32::try_from(8 + payload.len()).map_err(|_| FormatError::TooLarge)?;
    let mut v = size.to_be_bytes().to_vec();
    v.extend_from_slice(kind);
    v.extend_from_slice(payload);
    Ok(v)
}

/// Build a UTF-8 `data` sub-box: a `1u32` type tag (UTF-8), a `0u32` locale, then
/// the value bytes, wrapped as a `data` box. Shared by `text_atom` and
/// `freeform_atom`, whose value lists emit one of these per value.
fn utf8_data_box(value: &str) -> Result<Vec<u8>> {
    let mut data = 1u32.to_be_bytes().to_vec(); // type 1 = UTF-8
    data.extend_from_slice(&0u32.to_be_bytes()); // locale
    data.extend_from_slice(value.as_bytes());
    boxed(b"data", &data)
}

fn text_atom(kind: &[u8; 4], values: &[&str]) -> Result<Vec<u8>> {
    let mut inner = Vec::new();
    for v in values {
        inner.extend(utf8_data_box(v)?);
    }
    boxed(kind, &inner)
}

fn number_atom(kind: &[u8; 4], n: u16, width: usize) -> Result<Vec<u8>> {
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
    boxed(kind, &boxed(b"data", &data)?)
}

/// Emit a `----` freeform atom: a `mean` and `name` sub-box (each with a 4-byte
/// FullBox prefix) followed by one UTF-8 `data` sub-box per value. Note that the
/// scan path (`read_freeform`) only recovers the first value on read-back, so
/// multi-value freeform tags round-trip only their first value.
fn freeform_atom(mean: &str, name: &str, values: &[&str]) -> Result<Vec<u8>> {
    let mut inner = Vec::new();
    let mut mean_body = 0u32.to_be_bytes().to_vec(); // version/flags
    mean_body.extend_from_slice(mean.as_bytes());
    inner.extend(boxed(b"mean", &mean_body)?);
    let mut name_body = 0u32.to_be_bytes().to_vec();
    name_body.extend_from_slice(name.as_bytes());
    inner.extend(boxed(b"name", &name_body)?);
    for v in values {
        inner.extend(utf8_data_box(v)?);
    }
    boxed(b"----", &inner)
}

/// Parse a `----:<mean>:<name>` opaque key back into `(mean, name)`. `name` may
/// contain `:` (only the first separator splits). `None` if not a freeform key.
fn parse_freeform_key(key: &str) -> Option<(&str, &str)> {
    key.strip_prefix("----:")?.split_once(':')
}

/// Inline framing for an opaque binary `----` atom whose `data` value
/// (`payload_len` bytes) streams next:
/// `[---- size][----][mean box][name box][data size][data][type 0][locale 0]`.
/// Mirrors `freeform_atom` but emits type code 0 (binary) and no value bytes — the
/// value is served from the DB as a `Segment::BinaryTag`.
pub(super) fn freeform_binary_prefix(mean: &str, name: &str, payload_len: u64) -> Result<Vec<u8>> {
    let mut mean_body = 0u32.to_be_bytes().to_vec(); // version/flags
    mean_body.extend_from_slice(mean.as_bytes());
    let mean_box = boxed(b"mean", &mean_body)?;
    let mut name_body = 0u32.to_be_bytes().to_vec();
    name_body.extend_from_slice(name.as_bytes());
    let name_box = boxed(b"name", &name_body)?;

    let data_size = size::checked_add(16, payload_len)?; // data header + type + locale + payload
    let inner_len = size::checked_sum([mean_box.len() as u64, name_box.len() as u64, data_size])?;

    let outer_len = size::checked_add(8, inner_len)?;
    let mut out = u32::try_from(outer_len)
        .map_err(|_| FormatError::TooLarge)?
        .to_be_bytes()
        .to_vec();
    out.extend_from_slice(b"----");
    out.extend_from_slice(&mean_box);
    out.extend_from_slice(&name_box);
    out.extend_from_slice(
        &u32::try_from(data_size)
            .map_err(|_| FormatError::TooLarge)?
            .to_be_bytes(),
    );
    out.extend_from_slice(b"data");
    out.extend_from_slice(&0u32.to_be_bytes()); // type 0 = binary/implicit
    out.extend_from_slice(&0u32.to_be_bytes()); // locale
    Ok(out)
}

/// Build the `udta` box as an ordered segment list: `Segment::Inline` for all box
/// framing, with each opaque `----` value and each cover image streamed from the DB
/// (`Segment::BinaryTag`/`Segment::ArtImage`) rather than materialized. Returns
/// `(segments, streamed_total)` where `streamed_total` sums every streamed payload
/// length (binary `----` values + art). Every enclosing box size
/// (`----`/`ilst`/`meta`/`udta`) accounts for `streamed_total` at the right nesting
/// depth, so the streamed bytes splice in correctly at read time.
pub(super) fn build_udta(
    tags: &[TagInput],
    binary_tags: &[BinaryTagInput],
    arts: &[ArtInput],
) -> Result<(Vec<Segment>, u64)> {
    // Group consecutive same-key text values (the DB returns tags ordered by key).
    let mut groups: Vec<(&str, Vec<&str>)> = Vec::new();
    for t in tags {
        match groups.last_mut() {
            Some(g) if g.0 == t.key => g.1.push(&t.value),
            _ => groups.push((&t.key, vec![&t.value])),
        }
    }

    // ilst content: text atoms first (materialized), then opaque `----` (streamed),
    // then cover art (streamed). `ilst_inline` accumulates framing until a streamed
    // segment forces a flush.
    let mut ilst_inline: Vec<u8> = Vec::new();
    for (key, values) in &groups {
        match crate::tagmap::key_to_mp4(key) {
            Some(crate::tagmap::Mp4Slot::Text(atom)) => {
                ilst_inline.extend(text_atom(atom, values)?);
            }
            Some(crate::tagmap::Mp4Slot::Number(atom, width)) => {
                if let Ok(n) = values.first().copied().unwrap_or("").parse::<u16>() {
                    ilst_inline.extend(number_atom(atom, n, width)?);
                }
            }
            Some(crate::tagmap::Mp4Slot::Freeform(mean, name)) => {
                ilst_inline.extend(freeform_atom(mean, name, values)?);
            }
            None => ilst_inline.extend(freeform_atom("com.apple.iTunes", key, values)?),
        }
    }

    let mut ilst_segments: Vec<Segment> = Vec::new();
    let mut streamed_total: u64 = 0;

    for bt in binary_tags {
        let Some((mean, name)) = parse_freeform_key(&bt.key) else {
            // Not a `----:<mean>:<name>` key; skip defensively (no double-store path).
            continue;
        };
        ilst_inline.extend_from_slice(&freeform_binary_prefix(mean, name, bt.len.get())?);
        ilst_segments.push(Segment::Inline(std::mem::take(&mut ilst_inline)));
        ilst_segments.push(Segment::BinaryTag {
            payload_id: bt.payload_id,
            len: bt.len,
        });
        streamed_total = size::checked_add(streamed_total, bt.len.get())?;
    }

    if !arts.is_empty() {
        // One covr atom; each art is its own `data` child (the iTunes
        // convention for multiple artworks).
        let covr_size: u64 = arts.iter().try_fold(8u64, |acc, a| {
            size::checked_add(acc, size::checked_add(16, a.data_len.get())?)
        })?;
        ilst_inline.extend_from_slice(
            &u32::try_from(covr_size)
                .map_err(|_| FormatError::TooLarge)?
                .to_be_bytes(),
        );
        ilst_inline.extend_from_slice(b"covr");
        for a in arts {
            let type_code: u32 = if a.mime == "image/png" { 14 } else { 13 };
            let data_size = size::checked_add(16, a.data_len.get())?; // data header + type + locale + image
            ilst_inline.extend_from_slice(
                &u32::try_from(data_size)
                    .map_err(|_| FormatError::TooLarge)?
                    .to_be_bytes(),
            );
            ilst_inline.extend_from_slice(b"data");
            ilst_inline.extend_from_slice(&type_code.to_be_bytes());
            ilst_inline.extend_from_slice(&0u32.to_be_bytes()); // locale; image streams next
            ilst_segments.push(Segment::Inline(std::mem::take(&mut ilst_inline)));
            ilst_segments.push(Segment::ArtImage {
                art_id: a.art_id,
                len: a.data_len,
            });
            streamed_total = size::checked_add(streamed_total, a.data_len.get())?;
        }
    } else if !ilst_inline.is_empty() {
        ilst_segments.push(Segment::Inline(std::mem::take(&mut ilst_inline)));
    }

    let ilst_inline_len: u64 = ilst_segments
        .iter()
        .map(|s| match s {
            Segment::Inline(b) => b.len() as u64,
            _ => 0,
        })
        .sum();

    let mut hdlr_body = vec![0u8; 8];
    hdlr_body.extend_from_slice(b"mdir");
    hdlr_body.extend_from_slice(b"appl");
    hdlr_body.extend_from_slice(&[0u8; 9]);
    let hdlr = boxed(b"hdlr", &hdlr_body)?;

    // Box sizes. Each enclosing box adds its 8-byte header to the inline content of
    // its child and carries `streamed_total` through unchanged (the streamed bytes
    // live at the deepest level, inside ilst).
    let ilst_size = size::checked_sum([8, ilst_inline_len, streamed_total])?;
    let meta_inline_len = 4 + hdlr.len() as u64 + 8 + ilst_inline_len; // [vf][hdlr][ilst hdr][ilst inline]
    let meta_size = size::checked_sum([8, meta_inline_len, streamed_total])?;
    let udta_inline_len = 8 + meta_inline_len; // [meta hdr][meta inline]
    let udta_size = size::checked_sum([8, udta_inline_len, streamed_total])?;

    // MP4 box sizes are 32-bit. udta encloses all inner boxes, so converting it
    // first bounds them all; refuse oversized metadata at the format boundary
    // rather than emit a silently-truncated (corrupt) size field.
    let udta_size = u32::try_from(udta_size).map_err(|_| FormatError::TooLarge)?;
    let meta_size = u32::try_from(meta_size).map_err(|_| FormatError::TooLarge)?;
    let ilst_size = u32::try_from(ilst_size).map_err(|_| FormatError::TooLarge)?;

    // Leading framing: everything up to the start of ilst content.
    let mut header = udta_size.to_be_bytes().to_vec();
    header.extend_from_slice(b"udta");
    header.extend_from_slice(&meta_size.to_be_bytes());
    header.extend_from_slice(b"meta");
    header.extend_from_slice(&0u32.to_be_bytes()); // meta FullBox version/flags
    header.extend_from_slice(&hdlr);
    header.extend_from_slice(&ilst_size.to_be_bytes());
    header.extend_from_slice(b"ilst");

    // Merge the header into the first ilst inline segment (always Inline when present,
    // since streamed segments are preceded by their framing).
    let mut segments: Vec<Segment> = Vec::new();
    let mut lead = header;
    for seg in ilst_segments {
        match seg {
            Segment::Inline(b) => lead.extend_from_slice(&b),
            other => {
                segments.push(Segment::Inline(std::mem::take(&mut lead)));
                segments.push(other);
            }
        }
    }
    // `lead` is empty only when the loop's last segment was streamed (it was
    // `take`n); otherwise it still holds the udta/meta/ilst header (when there are
    // no streamed segments) or trailing framing. Pushing an empty `lead` would
    // produce an EmptySegment that fails layout validation, so guard on non-empty.
    if !lead.is_empty() {
        segments.push(Segment::Inline(lead));
    }
    Ok((segments, streamed_total))
}

/// Patch every `stco` (4-byte) or `co64` (8-byte) chunk offset in `kept` (moov
/// children minus udta) by `delta`. Errors if a 32-bit offset would overflow.
pub(super) fn patch_chunk_offsets(kept: &mut [u8], delta: i64) -> Result<()> {
    let (range, entry) = match find_path(kept, &[b"trak", b"mdia", b"minf", b"stbl", b"stco"])? {
        Some(r) => (r, 4usize),
        None => match find_path(kept, &[b"trak", b"mdia", b"minf", b"stbl", b"co64"])? {
            Some(r) => (r, 8usize),
            None => return Err(FormatError::Malformed),
        },
    };
    let (start, len) = range;
    let count = read_u32_be(kept, start + 4)? as usize;
    for i in 0..count {
        let pos = start + 8 + i * entry;
        if pos + entry > start + len {
            return Err(FormatError::Malformed);
        }
        if entry == 4 {
            let v = i64::from(read_u32_be(kept, pos)?) + delta;
            let new_val = u32::try_from(v).map_err(|_| FormatError::TooLarge)?;
            kept[pos..pos + 4].copy_from_slice(&new_val.to_be_bytes());
        } else {
            // `checked_add_signed` rejects both a negative relocated offset
            // (underflow) and one past u64::MAX (overflow) — adding `delta` via a
            // signed cast overflowed i64 for offsets near its bounds (fuzz crash).
            let v = read_u64_be(kept, pos)?
                .checked_add_signed(delta)
                .ok_or(FormatError::Malformed)?;
            kept[pos..pos + 8].copy_from_slice(&v.to_be_bytes());
        }
    }
    Ok(())
}

/// Regenerate a re-tagged `moov` and produce the serving layout
/// `[ftyp][regenerated moov][mdat header][mdat payload]`. The mdat payload is
/// served verbatim, merely relocated, so every chunk offset shifts by a constant
/// `delta`. Patching only offset VALUES (never box sizes) means `new_moov_size`
/// is computable before `delta` — no circular dependency. Cover art (every non-empty art row, in input order) and opaque `----`
/// binary tags stream from the DB at read time, splicing into the layout.
pub fn synthesize_layout(
    scan: &Mp4Scan,
    tags: &[TagInput],
    binary_tags: &[BinaryTagInput],
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

    // All art inputs are non-zero-length (the bridge drops zero-length at construction).
    let arts: Vec<ArtInput> = arts.to_vec();
    let (udta_segments, _streamed_total) = build_udta(tags, binary_tags, &arts)?;
    let udta_total: u64 = udta_segments.iter().map(Segment::len).sum();

    let new_moov_size = size::checked_sum([8, kept.len() as u64, udta_total])?;
    // MP4 box sizes are 32-bit; mirror build_udta's bound. The try_from below
    // (writing the size field) is the enforcing check.
    let new_moov_size_u32 = u32::try_from(new_moov_size).map_err(|_| FormatError::TooLarge)?;
    let new_mdat_payload_pos = size::checked_sum([
        scan.ftyp.len() as u64,
        new_moov_size,
        scan.mdat_header.len() as u64,
    ])?;
    let delta = new_mdat_payload_pos.cast_signed() - scan.mdat_payload_offset.cast_signed();

    patch_chunk_offsets(&mut kept, delta)?;

    let mut head = Vec::new();
    head.extend_from_slice(&scan.ftyp);
    head.extend_from_slice(&new_moov_size_u32.to_be_bytes());
    head.extend_from_slice(b"moov");
    head.extend_from_slice(&kept);

    // Splice the udta segment list after the moov head. `build_udta` guarantees a
    // non-empty list whose first element is the leading `Inline` framing (it opens
    // with the udta/meta/ilst header), so fold that into `head`; the rest (streamed
    // payloads + interleaved inline) follow. Finally append the truncated mdat header
    // to the last inline run before backing audio.
    let mut udta_iter = udta_segments.into_iter();
    let Some(Segment::Inline(first)) = udta_iter.next() else {
        // build_udta always yields a leading Inline; anything else is a producer bug.
        return Err(FormatError::ProducerBug(
            "build_udta did not yield a leading Inline framing segment",
        ));
    };
    head.extend_from_slice(&first);
    let mut segments: Vec<Segment> = vec![Segment::Inline(head)];
    segments.extend(udta_iter);
    match segments.last_mut() {
        Some(Segment::Inline(b)) => b.extend_from_slice(&scan.mdat_header),
        _ => segments.push(Segment::Inline(scan.mdat_header.clone())),
    }
    segments.push(Segment::BackingAudio {
        offset: scan.mdat_payload_offset,
        len: scan.mdat_payload_len,
    });
    Ok(RegionLayout::validated(segments)?)
}
