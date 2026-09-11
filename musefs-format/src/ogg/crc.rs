//! Ogg page CRC-32: polynomial 0x04c11db7, init 0, no input/output reflection,
//! no final XOR. The caller passes the full page with the 4 CRC bytes (offset
//! 22..26) zeroed.

const POLY: u32 = 0x04c1_1db7;

const fn build_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0u32;
    while i < 256 {
        let mut crc = i << 24;
        let mut j = 0;
        while j < 8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ POLY
            } else {
                crc << 1
            };
            j += 1;
        }
        t[i as usize] = crc;
        i += 1;
    }
    t
}

const TABLE: [u32; 256] = build_table();

/// Fold `buf` into a running CRC `state`. `crc32(x) == crc32_update(0, x)` and
/// `crc32(a ++ b) == crc32_update(crc32_update(0, &a), &b)`, so a page CRC can be
/// computed in bounded windows without assembling the whole page.
pub(crate) fn crc32_update(mut state: u32, buf: &[u8]) -> u32 {
    for &b in buf {
        state = (state << 8) ^ TABLE[(((state >> 24) as u8) ^ b) as usize];
    }
    state
}

pub fn crc32(buf: &[u8]) -> u32 {
    crc32_update(0, buf)
}

/// Advance a CRC state across `n` zero bytes: `crc_shift_zeros(crc32(m), n)
/// == crc32(m ++ zeros×n)`.
///
/// Appending one zero byte is a fixed linear map on the 32-bit state, so
/// appending `n` of them is that map raised to the `n`-th power over GF(2).
/// `POWERS` holds the power-of-two exponents, so the shift costs
/// `n.count_ones()` matrix applies rather than `n` serially-dependent table
/// steps — the difference between ~1,890 dependent lookups and six 32-word XOR
/// folds on a typical Vorbis page (#666).
pub fn crc_shift_zeros(crc: u32, n: usize) -> u32 {
    // A zero state stays zero under every power, and it is the common case on the
    // serve path: a page whose sequence number is unchanged has an all-zero DELTA,
    // so `patch_page_header_algebraic` hands us `crc32([0; 4]) == 0`.
    if crc == 0 {
        return 0;
    }
    let mut state = crc;
    for (k, mat) in POWERS.iter().enumerate() {
        if (n >> k) & 1 == 1 {
            state = apply(mat, state);
        }
    }
    state
}

/// A GF(2) transition matrix on the CRC state: row `i` is the image of the basis
/// vector `1 << (31 - i)`, so bit `i` counts from the MSB in both the rows and
/// the vectors they are applied to.
type Matrix = [u32; 32];

/// `POWERS[k]` advances the state across `1 << k` zero bytes. Built at compile
/// time by repeated squaring from the single-byte matrix; `usize::BITS` entries
/// cover every `n` the public function accepts.
const POWERS: [Matrix; usize::BITS as usize] = build_powers();

/// Apply a matrix to a state vector: XOR the rows its set bits select. Masking
/// rather than branching keeps the fold free of the data-dependent, essentially
/// unpredictable branch a CRC state's bits would produce.
fn apply(mat: &Matrix, state: u32) -> u32 {
    let mut out = 0u32;
    for (i, &row) in mat.iter().enumerate() {
        out ^= row & ((state >> (31 - i)) & 1).wrapping_neg();
    }
    out
}

const fn build_powers() -> [Matrix; usize::BITS as usize] {
    // One zero-byte step, matching the table loop in `crc32_update`: the matrix is
    // byte-granular, so `n` zero bytes need the n-th power — not the 8n-th.
    const fn poly_step(p: u32) -> u32 {
        (p << 8) ^ TABLE[(p >> 24) as usize]
    }
    let mut base: Matrix = [0u32; 32];
    let mut i = 0;
    while i < 32 {
        base[i] = poly_step(1u32 << (31 - i));
        i += 1;
    }
    let mut powers = [base; usize::BITS as usize];
    let mut k = 1;
    while k < usize::BITS as usize {
        powers[k] = mat_mul(&powers[k - 1], &powers[k - 1]);
        k += 1;
    }
    powers
}

/// Compose two transition matrices: applying the product is applying `first`, then
/// `then`. Row `i` of the product is `first`'s row `i` pushed through `then`.
const fn mat_mul(first: &Matrix, then: &Matrix) -> Matrix {
    let mut product = [0u32; 32];
    let mut i = 0;
    while i < 32 {
        let mut j = 0;
        while j < 32 {
            if (first[i] >> (31 - j)) & 1 == 1 {
                product[i] ^= then[j];
            }
            j += 1;
        }
        i += 1;
    }
    product
}

#[cfg(test)]
mod tests {
    use super::crc32;

    fn reference(data: &[u8]) -> u32 {
        // Independent implementation via the `crc` crate, configured with Ogg's
        // exact parameters (init 0, no reflection, no xorout).
        const ALG: crc::Algorithm<u32> = crc::Algorithm {
            width: 32,
            poly: 0x04c1_1db7,
            init: 0,
            refin: false,
            refout: false,
            xorout: 0,
            check: 0,
            residue: 0,
        };
        let c = crc::Crc::<u32>::new(&ALG);
        c.checksum(data)
    }

    #[test]
    fn matches_independent_reference() {
        assert_eq!(crc32(b""), reference(b""));
        assert_eq!(crc32(b"123456789"), reference(b"123456789"));
        let blob: Vec<u8> = (0..=255u8).cycle().take(5000).collect();
        assert_eq!(crc32(&blob), reference(&blob));
    }

    #[test]
    fn crc32_update_matches_oneshot_across_a_split() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        for split in [0usize, 1, 3, 255, 256, 700, 1000] {
            let (a, b) = data.split_at(split);
            let streamed = super::crc32_update(super::crc32_update(0, a), b);
            assert_eq!(streamed, super::crc32(&data), "split at {split}");
        }
    }

    #[test]
    fn crc_shift_zeros_identity() {
        // Advancing 0 by any n stays 0 (TABLE[0] = 0 ⟹ each step: 0 ^ TABLE[0] = 0).
        assert_eq!(super::crc_shift_zeros(0, 0), 0);
        assert_eq!(super::crc_shift_zeros(0, 1), 0);
        assert_eq!(super::crc_shift_zeros(0, 65285), 0);
    }

    #[test]
    fn crc_shift_zeros_composes_across_the_whole_power_ladder() {
        // The differential test above only reaches the low matrix powers. Each
        // higher power must equal its predecessor applied twice, which pins every
        // entry of the precomputed ladder back to the single-byte step.
        let crc = crc32(b"hello world");
        for k in 1..usize::BITS {
            let half = 1usize << (k - 1);
            let doubled = super::crc_shift_zeros(super::crc_shift_zeros(crc, half), half);
            assert_eq!(super::crc_shift_zeros(crc, 1usize << k), doubled, "k = {k}");
        }
    }

    #[test]
    fn crc_shift_zeros_matches_appending_zeros() {
        // Semantic contract: crc_shift_zeros(crc32(data), n) == crc32(data ++ zeros×n).
        let data = b"hello world";
        let crc_start = crc32(data);
        // A sweep of real page sizes (a Vorbis page averages ~1,890 bytes, an Ogg
        // page maxes out at 65,307) plus the degenerate and single-byte ends.
        for &n in &[0usize, 1, 7, 10, 255, 1_000, 1_890, 4_096, 8_192, 65_285] {
            let mut extended = data.to_vec();
            extended.resize(data.len() + n, 0u8);
            let expected = crc32(&extended);
            assert_eq!(super::crc_shift_zeros(crc_start, n), expected, "n = {n}");
        }
    }
}
