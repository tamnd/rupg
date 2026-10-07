//! CRC32C, the checksum of each log block (spec/11 section 11.13.2).
//!
//! This is the Castagnoli polynomial in reflected form, eight bytes at a time through eight tables that are made at compile time. The crate has no unsafe code, so it does not use the instruction of the processor. Document 19 measures whether replay needs it.

/// The reflected Castagnoli polynomial.
const POLY: u32 = 0x82F6_3B78;

/// Table `k` moves a byte that is `k` bytes before the end of an eight-byte step.
static TABLES: [[u32; 256]; 8] = tables();

const fn tables() -> [[u32; 256]; 8] {
    let mut tables = [[0u32; 256]; 8];
    let mut byte = 0;
    while byte < 256 {
        let mut crc = byte as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 0 { crc >> 1 } else { (crc >> 1) ^ POLY };
            bit += 1;
        }
        tables[0][byte] = crc;
        byte += 1;
    }
    let mut byte = 0;
    while byte < 256 {
        let mut t = 1;
        while t < 8 {
            let before = tables[t - 1][byte];
            tables[t][byte] = (before >> 8) ^ tables[0][(before & 0xff) as usize];
            t += 1;
        }
        byte += 1;
    }
    tables
}

/// The CRC32C of `bytes` that continues from `crc`, which is a seed or the result of an earlier call. Two calls on two pieces give the same result as one call on the two pieces together.
pub(crate) fn extend(crc: u32, bytes: &[u8]) -> u32 {
    let mut crc = !crc;
    let (steps, rest) = bytes.as_chunks::<8>();
    for &[a, b, c, d, e, f, g, h] in steps {
        let low = u32::from_le_bytes([a, b, c, d]) ^ crc;
        let high = u32::from_le_bytes([e, f, g, h]);
        crc = TABLES[7][(low & 0xff) as usize]
            ^ TABLES[6][((low >> 8) & 0xff) as usize]
            ^ TABLES[5][((low >> 16) & 0xff) as usize]
            ^ TABLES[4][(low >> 24) as usize]
            ^ TABLES[3][(high & 0xff) as usize]
            ^ TABLES[2][((high >> 8) & 0xff) as usize]
            ^ TABLES[1][((high >> 16) & 0xff) as usize]
            ^ TABLES[0][(high >> 24) as usize];
    }
    for &b in rest {
        crc = TABLES[0][((crc ^ u32::from(b)) & 0xff) as usize] ^ (crc >> 8);
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One byte at a time, from the definition.
    fn slow(crc: u32, bytes: &[u8]) -> u32 {
        let mut crc = !crc;
        for &b in bytes {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 == 0 { crc >> 1 } else { (crc >> 1) ^ POLY };
            }
        }
        !crc
    }

    #[test]
    fn known_values() {
        assert_eq!(extend(0, b"123456789"), 0xe306_9283);
        assert_eq!(extend(0, b""), 0);
        // RFC 3720 appendix B.4.
        assert_eq!(extend(0, &[0; 32]), 0x8a91_36aa);
        assert_eq!(extend(0, &[0xff; 32]), 0x62a8_ab43);
    }

    #[test]
    fn tables_match_the_definition() {
        let bytes: Vec<u8> = (0..1000u32).map(|n| (n * 131 + 17) as u8).collect();
        for len in [0, 1, 7, 8, 9, 63, 64, 65, 1000] {
            for seed in [0, 1, 0xdead_beef] {
                assert_eq!(extend(seed, &bytes[..len]), slow(seed, &bytes[..len]), "{len}");
            }
        }
        for cut in [0, 1, 8, 9, 500, 1000] {
            let (a, b) = bytes.split_at(cut);
            assert_eq!(extend(extend(7, a), b), extend(7, &bytes), "cut at {cut}");
        }
    }
}
