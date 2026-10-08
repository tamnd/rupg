//! The four-lane checksum of spec/08 section 8.3.2.
//!
//! The function keeps four 64-bit lanes and reads 32 bytes in each round. The four lanes are independent, so the processor can run their multiplies at the same time. The result is the same as XXH64 with seed 0. The function, its constants and its test vectors are fixed at M1. A change is a new algorithm number in the identity block and an incompatible feature flag.

// Lifted from rudb crates/rudb-native/src/lib.rs at cd9f9676 (2026-10-05).
// Changes: the seed is fixed at 0, the word reads use as_chunks and do not call expect, and the public name is `checksum`.
// Reviewed at b0e2dfd5 (2026-10-08): the 33 rudb commits to the file after the lift change other functions. `checksum`, `seeded_checksum`, the round, word, block and tail functions and the test vectors are the same, so there is nothing to port.

/// The algorithm number of this function in the identity block.
pub const CHECKSUM_ALGORITHM: u8 = 1;

const P1: u64 = 11_400_714_785_074_694_791;
const P2: u64 = 14_029_467_366_897_019_727;
const P3: u64 = 1_609_587_929_392_839_161;
const P4: u64 = 9_650_029_242_287_828_579;
const P5: u64 = 2_870_177_450_012_600_261;

/// The checksum of `bytes`.
pub fn checksum(bytes: &[u8]) -> u64 {
    let length = bytes.len() as u64;
    let (blocks, rest) = bytes.as_chunks::<32>();
    if bytes.len() < 32 {
        return tail(P5.wrapping_add(length), rest);
    }
    let mut lanes = [P1.wrapping_add(P2), P2, 0, 0u64.wrapping_sub(P1)];
    for block in blocks {
        let (words, _) = block.as_chunks::<8>();
        for (lane, w) in lanes.iter_mut().zip(words) {
            *lane = round(*lane, u64::from_le_bytes(*w));
        }
    }
    let [one, two, three, four] = lanes;
    let merge = |hash: u64, lane: u64| (hash ^ round(0, lane)).wrapping_mul(P1).wrapping_add(P4);
    let combined = one
        .rotate_left(1)
        .wrapping_add(two.rotate_left(7))
        .wrapping_add(three.rotate_left(12))
        .wrapping_add(four.rotate_left(18));
    let hash = merge(merge(merge(merge(combined, one), two), three), four);
    tail(hash.wrapping_add(length), rest)
}

fn round(state: u64, word: u64) -> u64 {
    state.wrapping_add(word.wrapping_mul(P2)).rotate_left(31).wrapping_mul(P1)
}

fn tail(mut hash: u64, rest: &[u8]) -> u64 {
    let (words, mut rest) = rest.as_chunks::<8>();
    for w in words {
        hash ^= round(0, u64::from_le_bytes(*w));
        hash = hash.rotate_left(27).wrapping_mul(P1).wrapping_add(P4);
    }
    if let Some((w, after)) = rest.split_first_chunk::<4>() {
        hash ^= u64::from(u32::from_le_bytes(*w)).wrapping_mul(P1);
        hash = hash.rotate_left(23).wrapping_mul(P2).wrapping_add(P3);
        rest = after;
    }
    for &b in rest {
        hash ^= u64::from(b).wrapping_mul(P5);
        hash = hash.rotate_left(11).wrapping_mul(P1);
    }
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(P2);
    hash ^= hash >> 29;
    hash = hash.wrapping_mul(P3);
    hash ^ (hash >> 32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The XXH64 vectors with seed 0, from the reference implementation.
    #[test]
    fn vectors() {
        assert_eq!(checksum(b""), 0xef46_db37_51d8_e999);
        assert_eq!(checksum(b"a"), 0xd24e_c4f1_a98c_6e5b);
        assert_eq!(checksum(b"abc"), 0x44bc_2cf5_ad77_0999);
        assert_eq!(checksum(b"Nobody inspects the spammish repetition"), 0xfbce_a83c_8a37_8bf1);
        // Lengths around the 32-byte round and the 4-byte and 8-byte tail steps.
        let data: Vec<u8> = (0..100u8).map(|i| i.wrapping_mul(37)).collect();
        let expected = [
            (31, 0x4513_ec5e_bc46_d0d3),
            (32, 0x2c1f_2ffa_2ace_16d3),
            (33, 0x62f3_b71c_d826_3c47),
            (35, 0xfb98_0d74_45cb_cd78),
            (36, 0x385c_bf15_ac07_bd09),
            (64, 0xcb86_cfdc_04a5_148a),
            (100, 0x3f99_fd12_63b5_4f01),
        ];
        for (n, sum) in expected {
            assert_eq!(checksum(&data[..n]), sum, "length {n}");
        }
    }

    /// Each length from 0 to 99 gives a different result, and a change of one bit changes the result.
    #[test]
    fn lengths() {
        let data: Vec<u8> = (0..100u8).map(|i| i.wrapping_mul(37)).collect();
        let mut seen = std::collections::HashSet::new();
        for n in 0..data.len() {
            let sum = checksum(&data[..n]);
            assert!(seen.insert(sum), "length {n}");
            let mut flipped = data[..n].to_vec();
            if let Some(b) = flipped.last_mut() {
                *b ^= 1;
                assert_ne!(checksum(&flipped), sum, "length {n}");
            }
        }
    }
}
