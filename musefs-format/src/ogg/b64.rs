//! Incremental base64 serving for embedded art: given a requested window of the
//! *output* base64 of an image, compute the bounded raw-input range to read and
//! how to trim the re-encoded result. base64 encodes each 3 input bytes into 4
//! output chars independently, so any output window `[o, o+len)` depends only on
//! input bytes `[⌊o/4⌋·3 .. ⌈(o+len)/4⌉·3)` (clipped to the image length, whose
//! final partial group yields the canonical `=` padding).

use base64::Engine;

/// The raw-input read plan for an output base64 window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct B64Window {
    /// First raw input byte to read.
    pub in_start: u64,
    /// Number of raw input bytes to read (clipped to the image length).
    pub in_len: u64,
    /// Leading base64 chars to drop after encoding the read bytes.
    pub skip: usize,
}

/// Compute the input read plan to serve output base64 chars `[out_offset,
/// out_offset+take)` of `base64(image)`, where the image is `img_total` bytes.
pub fn b64_window(out_offset: u64, take: u64, img_total: u64) -> B64Window {
    debug_assert!(take > 0);
    let g0 = out_offset / 4;
    let g1 = (out_offset + take - 1) / 4;
    let in_start = g0 * 3;
    let in_end = ((g1 + 1) * 3).min(img_total);
    B64Window {
        in_start,
        in_len: in_end.saturating_sub(in_start),
        skip: crate::convert::usize_from(out_offset - g0 * 4),
    }
}

/// Encode `raw` (the bytes named by a `B64Window`) and return exactly `take`
/// output chars starting at `skip`. Returns `None` when the encoded output is
/// shorter than `skip + take` — i.e. `raw` was shorter than the window the
/// caller resolved against `art_total` (a truncated art blob). A checked read
/// rather than a panic, so the serve path can surface this as `BackingChanged`
/// like the other base64-art arms (#526).
pub fn encode_b64_slice(raw: &[u8], skip: usize, take: usize) -> Option<Vec<u8>> {
    let enc = base64::engine::general_purpose::STANDARD.encode(raw);
    let end = skip.checked_add(take)?;
    enc.as_bytes().get(skip..end).map(<[u8]>::to_vec)
}

/// Total base64 output length for an image of `img_total` bytes, or `None` if it
/// overflows `u64`. Only an adversarial `img_total` can overflow; every real
/// image is far below this.
pub fn b64_len_checked(img_total: u64) -> Option<u64> {
    img_total.div_ceil(3).checked_mul(4)
}

/// Total base64 output length for an image of `img_total` bytes.
pub fn b64_len(img_total: u64) -> u64 {
    b64_len_checked(img_total).expect("base64 output length fits u64")
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn full_b64(img: &[u8]) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .encode(img)
            .into_bytes()
    }

    #[test]
    fn any_window_matches_substring_of_full_encode() {
        // Cover image lengths that hit every length-mod-3 case and various windows.
        for &img_total in &[0u64, 1, 2, 3, 4, 5, 6, 7, 100, 257, 1024] {
            let img: Vec<u8> = (0..img_total)
                .map(|i| u8::try_from((i * 7 + 3) % 256).unwrap())
                .collect();
            let full = full_b64(&img);
            assert_eq!(crate::convert::usize_from(b64_len(img_total)), full.len());
            if full.is_empty() {
                continue;
            }
            for o in 0..full.len() as u64 {
                for take in 1..=(full.len() as u64 - o) {
                    let w = b64_window(o, take, img_total);
                    let raw = &img[crate::convert::usize_from(w.in_start)
                        ..crate::convert::usize_from(w.in_start + w.in_len)];
                    let got = encode_b64_slice(raw, w.skip, crate::convert::usize_from(take))
                        .expect("window lies within the encoded output");
                    assert_eq!(
                        got,
                        &full[crate::convert::usize_from(o)..crate::convert::usize_from(o + take)],
                        "img_total={img_total} o={o} take={take}"
                    );
                }
            }
        }
    }

    #[test]
    fn encode_b64_slice_returns_none_when_window_exceeds_output() {
        // A 3-byte blob encodes to exactly 4 base64 chars ("YWJj"). A window that
        // runs past that — as it would for an art blob shorter than its resolved
        // `art_total` — must return None rather than panic on an out-of-range
        // slice (#526).
        assert_eq!(encode_b64_slice(b"abc", 0, 4), Some(b"YWJj".to_vec()));
        assert_eq!(encode_b64_slice(b"abc", 2, 4), None);
        assert_eq!(encode_b64_slice(b"abc", 5, 1), None);
    }

    #[test]
    fn b64_window_fields_are_exact_at_group_boundaries() {
        // out_offset and take chosen so the -1 and /4 in g1 are observable.
        // g0 = out_offset/4, g1 = (out_offset+take-1)/4,
        // in_start = g0*3, in_end = min((g1+1)*3, img_total), skip = out_offset - g0*4.
        let img_total = 1024u64;

        // take=1 at offset 0: g1 = 0 (with -1). The +1 mutant gives g1=0 too here,
        // so choose offset 3 take=1: g0=0,g1=0 vs +1 mutant g1=1 -> in_len differs.
        let w = b64_window(3, 1, img_total);
        assert_eq!(
            w,
            B64Window {
                in_start: 0,
                in_len: 3,
                skip: 3
            }
        );

        // take exactly fills group 0 (offset 0, take 4): g1=0; mutant take+1 -> g1=1.
        let w = b64_window(0, 4, img_total);
        assert_eq!(
            w,
            B64Window {
                in_start: 0,
                in_len: 3,
                skip: 0
            }
        );

        // offset 4 take 4 -> g0=1,g1=1 -> in_start=3,in_len=3,skip=0; /4->*4 mutant
        // makes g1 huge -> in_len clamps to img_total-3 (differs).
        let w = b64_window(4, 4, img_total);
        assert_eq!(
            w,
            B64Window {
                in_start: 3,
                in_len: 3,
                skip: 0
            }
        );

        // Window spanning two groups: offset 2 take 6 -> g0=0,g1=1 -> in 0..6.
        let w = b64_window(2, 6, img_total);
        assert_eq!(
            w,
            B64Window {
                in_start: 0,
                in_len: 6,
                skip: 2
            }
        );
    }

    #[test]
    fn b64_window_is_overflow_free_at_the_max_validated_boundary() {
        // For any layout that passes RegionLayout::validate, an OggArtSlice
        // satisfies offset + len <= b64_len(art_total) AND b64_len_checked(art_total)
        // is Some. Under those bounds b64_window's internal +/* cannot overflow.
        // Pin the worst case: the largest art_total whose b64_len still fits u64,
        // reading the final 4 output chars. In debug, any intermediate overflow
        // would panic here.
        let art_total = u64::MAX / 4 * 3; // b64_len_checked(art_total) is Some
        assert!(b64_len_checked(art_total).is_some());
        let total = b64_len(art_total);
        let w = b64_window(total - 4, 4, art_total);
        assert!(w.in_start <= art_total);
        assert!(w.in_len <= art_total);
    }
}
