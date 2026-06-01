//! Ogg page CRC-32: polynomial 0x04c11db7, init 0, no input/output reflection,
//! no final XOR. The caller passes the full page with the 4 CRC bytes (offset
//! 22..26) zeroed.

const POLY: u32 = 0x04c1_1db7;

const fn build_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = (i as u32) << 24;
        let mut j = 0;
        while j < 8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ POLY
            } else {
                crc << 1
            };
            j += 1;
        }
        t[i] = crc;
        i += 1;
    }
    t
}

const TABLE: [u32; 256] = build_table();

pub fn crc32(buf: &[u8]) -> u32 {
    let mut crc: u32 = 0;
    for &b in buf {
        crc = (crc << 8) ^ TABLE[(((crc >> 24) as u8) ^ b) as usize];
    }
    crc
}


pub fn crc_shift_zeros(crc: u32, n: usize) -> u32 {
    if n == 0 || crc == 0 {
        return crc;
    }
    // The per-step loop costs one table lookup per zero byte (O(n)); the GF(2)
    // matrix-power method costs a fixed ~32 32×32 matrix multiplies regardless of n
    // (O(log n) squarings). For the small pages real Opus/Vorbis streams carry, the
    // loop is cheaper; only large pages (e.g. a single huge packet laced into
    // max-size 65 KB pages) make the matrix win. Below this threshold use the loop;
    // at or above it, use the matrix. (The threshold is conservative: the matrix's
    // fixed cost only clearly beats the loop once n is well into the tens of
    // thousands, so small/typical pages never pay the matrix overhead.)
    const MATRIX_THRESHOLD: usize = 16_384;
    if n < MATRIX_THRESHOLD {
        let mut c = crc;
        for _ in 0..n {
            c = (c << 8) ^ TABLE[(c >> 24) as usize];
        }
        return c;
    }
    // `mat` is the GF(2) transition matrix for ONE zero-BYTE CRC step (poly_step
    // does `<< 8`, i.e. processes a full byte). n zero bytes therefore require
    // mat^n — NOT mat^(8n). (The bit-level "×x^(8n)" identity is correct in the
    // polynomial view, but the matrix here is byte-granular, so the exponent is n.)
    // We raise mat to the n-th power by repeated squaring, then apply it to crc.
    fn poly_step(p: u32) -> u32 {
        (p << 8) ^ TABLE[(p >> 24) as usize]
    }
    // Build the 32-row transition matrix for one zero-byte step.
    // Row i = poly_step applied to the basis vector (1 << (31-i)).
    let mut mat: [u32; 32] = [0u32; 32];
    for i in 0..32u32 {
        mat[i as usize] = poly_step(1u32 << (31 - i));
    }
    // Matrix–matrix multiply in GF(2): result[i][j] = OR of mat_a[i] & mat_b col j.
    fn mat_mul(a: &[u32; 32], b: &[u32; 32]) -> [u32; 32] {
        let mut r = [0u32; 32];
        for (ri, &ai) in r.iter_mut().zip(a.iter()) {
            for (j, &bj) in b.iter().enumerate() {
                if (ai >> (31 - j)) & 1 == 1 {
                    *ri ^= bj;
                }
            }
        }
        r
    }
    // Raise mat to the n-th power via repeated squaring.
    let mut power = mat;
    let mut result = {
        // Identity matrix.
        let mut id = [0u32; 32];
        for (i, slot) in id.iter_mut().enumerate() {
            *slot = 1u32 << (31 - i);
        }
        id
    };
    let mut exp = n;
    while exp > 0 {
        if exp & 1 == 1 {
            result = mat_mul(&result, &power);
        }
        power = mat_mul(&power, &power);
        exp >>= 1;
    }
    // Apply result matrix to crc (matrix-vector multiply).
    let mut out = 0u32;
    for (i, &row) in result.iter().enumerate() {
        if (crc >> (31 - i)) & 1 == 1 {
            out ^= row;
        }
    }
    out
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
    fn crc_shift_zeros_identity() {
        // Advancing 0 by any n stays 0 (TABLE[0] = 0 ⟹ each step: 0 ^ TABLE[0] = 0).
        assert_eq!(super::crc_shift_zeros(0, 0), 0);
        assert_eq!(super::crc_shift_zeros(0, 1), 0);
        assert_eq!(super::crc_shift_zeros(0, 65285), 0);
    }

    #[test]
    fn crc_shift_zeros_matches_appending_zeros() {
        // Semantic contract: crc_shift_zeros(crc32(data), n) == crc32(data ++ zeros×n).
        let data = b"hello world";
        let crc_start = crc32(data);
        // Spans both code paths: < 16384 (per-step loop), == 16384 (boundary),
        // and > 16384 (GF(2) matrix), so the differential check covers each.
        for &n in &[0usize, 1, 10, 1000, 16_383, 16_384, 20_000, 65_285] {
            let mut extended = data.to_vec();
            extended.resize(data.len() + n, 0u8);
            let expected = crc32(&extended);
            assert_eq!(
                super::crc_shift_zeros(crc_start, n),
                expected,
                "n = {n}"
            );
        }
    }
}
