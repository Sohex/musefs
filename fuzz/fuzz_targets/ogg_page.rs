#![no_main]
use libfuzzer_sys::fuzz_target;
use musefs_format::ogg::{parse_page, patch_page_header_algebraic, verify_page_crc};
use musefs_fuzz::MAX_INPUT;

// parse_page must never panic on arbitrary bytes at an arbitrary position, and the
// page machinery the serve path leans on must round-trip: re-sequencing a page
// algebraically preserves its payload, its geometry, and its CRC validity (#625).
fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT {
        return;
    }
    let Ok(h) = parse_page(data, 0) else {
        return;
    };
    let total = h.total_len();
    if data.len() < total {
        // Below total_len the page is truncated, and the CRC helper must say so
        // rather than verify a partial page.
        assert!(
            verify_page_crc(data).is_err(),
            "verify_page_crc accepted a truncated page: len={} total_len={total}",
            data.len(),
        );
        return;
    }
    let page = &data[..total];
    let valid = verify_page_crc(page).expect("a whole parsed page has verifiable geometry");

    // The new sequence number comes from whatever bytes trail the page; inputs that
    // are exactly one page fall back to a neighbouring seq so they stay productive.
    let tail = &data[total..];
    let new_seq = match tail.get(..4) {
        Some(b) => u32::from_le_bytes(b.try_into().expect("4 bytes")),
        None => h.seq.wrapping_add(1),
    };

    let patched = patch_page_header_algebraic(&page[..h.header_len], new_seq)
        .expect("a parsed page's header is exactly header_len bytes");

    // The header keeps its shape: same length, same segment table, same identity
    // fields. Only seq (18..22) and crc (22..26) may move.
    assert_eq!(patched.len(), h.header_len, "patched header changed length");
    assert_eq!(
        &patched[..18],
        &page[..18],
        "patched header changed capture/version/type/granule/serial",
    );
    assert_eq!(
        &patched[26..],
        &page[26..h.header_len],
        "patched header changed the segment table",
    );
    let ph = parse_page(&patched, 0).expect("a patched header re-parses");
    assert_eq!(ph.seq, new_seq, "patched header carries the wrong seq");
    assert_eq!(
        ph.payload_len, h.payload_len,
        "patched header changed the derived payload length",
    );

    // The serve path splices the payload verbatim from the backing file, so the
    // page it emits is the patched header followed by the original payload bytes.
    // That page must be exactly as CRC-valid as the original — the equivalence is
    // what lets musefs re-sequence pages without ever touching audio bytes.
    let mut repaged = patched;
    repaged.extend_from_slice(&page[h.header_len..]);
    assert_eq!(
        repaged.len(),
        total,
        "re-paged length != original page length"
    );
    assert_eq!(
        &repaged[h.header_len..],
        &page[h.header_len..],
        "re-paging altered the payload",
    );
    assert_eq!(
        verify_page_crc(&repaged).expect("the re-paged page has the original geometry"),
        valid,
        "the algebraic patch changed the page's CRC validity",
    );

    // Patching back to the original sequence number restores the header byte for
    // byte: the algebraic CRC update is its own inverse.
    let restored = patch_page_header_algebraic(&repaged[..h.header_len], h.seq)
        .expect("the patched header is still header_len bytes");
    assert_eq!(
        restored.as_slice(),
        &page[..h.header_len],
        "re-sequencing back to the original seq did not restore the header",
    );
});
