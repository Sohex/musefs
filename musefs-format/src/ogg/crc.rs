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
}
