use super::{Method, lz4, pglz, toast_compress};

/// Text of at least `len` bytes that has the words of a small list in the order of a linear congruential generator, so each run has the same text. Two of the words have bytes above 127, which `pglz` reads as negative numbers.
fn words(len: usize, seed: u64) -> Vec<u8> {
    const WORDS: [&str; 8] =
        ["select", "from", "where", "table", "index", "value", "naïve", "日本"];
    let mut state = seed;
    let mut out = Vec::new();
    while out.len() < len {
        state =
            state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        out.extend_from_slice(WORDS[(state >> 33) as usize % WORDS.len()].as_bytes());
        out.push(b' ');
    }
    out
}

#[test]
fn round_trip() {
    for (len, seed) in [(0, 1), (12, 1), (13, 2), (100, 3), (5000, 4), (70_000, 5)] {
        let data = words(len, seed);
        assert_eq!(lz4::decompress(&lz4::compress(&data)), data);
        if let Some(block) = pglz::compress(&data) {
            assert_eq!(pglz::decompress(&block), data);
        }
    }
}

/// `VARSIZE` of the compressed values of PostgreSQL 19, from `pg_column_size` of a column with each method. The text of 70000 bytes uses the hash table of 32-bit positions of `lz4`.
#[test]
fn oracle() {
    for (len, seed, lz4, pglz) in [
        (2500, 1, 1074, 491),
        (5000, 2, 2184, 914),
        (20000, 3, 8434, 3322),
        (70000, 4, 29727, 11461),
    ] {
        let data = words(len, seed);
        assert_eq!(toast_compress(Method::Lz4, &data), Some(lz4), "lz4 {len}");
        assert_eq!(toast_compress(Method::Pglz, &data), Some(pglz), "pglz {len}");
    }
}

/// `lz4` keeps data that does not get shorter, and `pglz` keeps data shorter than 32 bytes and data that does not get 25% shorter.
#[test]
fn kept() {
    let mut state = 7u64;
    let noise: Vec<u8> = (0..3000)
        .map(|_| {
            state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            (state >> 56) as u8
        })
        .collect();
    assert_eq!(toast_compress(Method::Lz4, &noise), None);
    assert_eq!(toast_compress(Method::Pglz, &noise), None);
    assert_eq!(pglz::compress(&[b'a'; 31]), None);
    assert!(pglz::compress(&[b'a'; 32]).is_some());
}
