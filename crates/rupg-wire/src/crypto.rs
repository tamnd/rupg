//! The hash functions that authentication needs.
//!
//! [`Hashes`] implements [`Crypto`] with the pure Rust crates `sha2`, `hmac`, `pbkdf2` and `md-5`, as spec/22 section 22.5 says. The trait stays, so that a server can give another provider. The tests also implement it in plain Rust below, and check one against the other.
//!
//! Lifted from `crates/rudb-pgwire/src/crypto.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

/// SHA-256, HMAC-SHA-256 and PBKDF2 for SCRAM, and MD5 for the `md5` method.
///
/// Each function takes its input in parts, which it hashes as if they were one byte string. So the caller does not have to join the SCRAM messages before it signs them.
pub trait Crypto {
    fn sha256(&self, parts: &[&[u8]]) -> [u8; 32];
    fn hmac_sha256(&self, key: &[u8], parts: &[&[u8]]) -> [u8; 32];
    /// PBKDF2 with HMAC-SHA-256 and one block of output, which is `Hi` in RFC 5802.
    fn pbkdf2_sha256(&self, password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32];
    fn md5(&self, parts: &[&[u8]]) -> [u8; 16];
}

/// [`Crypto`] with the pure Rust crates of RustCrypto.
#[derive(Clone, Copy, Debug, Default)]
pub struct Hashes;

impl Crypto for Hashes {
    fn sha256(&self, parts: &[&[u8]]) -> [u8; 32] {
        use sha2::Digest;
        let mut hash = sha2::Sha256::new();
        for part in parts {
            hash.update(part);
        }
        hash.finalize().into()
    }

    fn hmac_sha256(&self, key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
        use hmac::{KeyInit, Mac};
        let Ok(mut mac) = hmac::Hmac::<sha2::Sha256>::new_from_slice(key) else {
            unreachable!("HMAC takes a key of any length")
        };
        for part in parts {
            mac.update(part);
        }
        mac.finalize().into_bytes().into()
    }

    fn pbkdf2_sha256(&self, password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
        let mut out = [0; 32];
        pbkdf2::pbkdf2_hmac::<sha2::Sha256>(password, salt, iterations, &mut out);
        out
    }

    fn md5(&self, parts: &[&[u8]]) -> [u8; 16] {
        use md5::Digest;
        let mut hash = md5::Md5::new();
        for part in parts {
            hash.update(part);
        }
        hash.finalize().into()
    }
}

/// MD5, which neither crypto provider of the server offers, for the stored secrets and the answers of the `md5` method. MD5 is not safe for new uses, and PostgreSQL keeps it only for old roles, so speed does not matter here.
pub fn md5(parts: &[&[u8]]) -> [u8; 16] {
    const S: [u32; 16] = [7, 12, 17, 22, 5, 9, 14, 20, 4, 11, 16, 23, 6, 10, 15, 21];
    let mut h: [u32; 4] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476];
    for block in pad(parts, false).chunks(64) {
        let m: Vec<u32> = block.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect();
        let [mut a, mut b, mut c, mut d] = h;
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let k = (((i + 1) as f64).sin().abs() * 4_294_967_296.0) as u32;
            let f = f.wrapping_add(a).wrapping_add(k).wrapping_add(m[g]);
            (a, d, c) = (d, c, b);
            b = b.wrapping_add(f.rotate_left(S[i / 16 * 4 + i % 4]));
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d]) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0; 16];
    for (i, word) in h.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&word.to_le_bytes());
    }
    out
}

/// The message of `parts` with the padding of MD5 and SHA-256, and its length in bits at the end in the byte order of the hash.
fn pad(parts: &[&[u8]], big_endian: bool) -> Vec<u8> {
    let mut msg = parts.concat();
    let bits = (msg.len() as u64) * 8;
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&if big_endian { bits.to_be_bytes() } else { bits.to_le_bytes() });
    msg
}

#[cfg(test)]
pub(crate) mod soft {
    //! A slow implementation of [`Crypto`] for the tests, checked against the vectors of FIPS 180-2, RFC 1321, RFC 4231 and RFC 7914.

    use super::{Crypto, pad};

    #[derive(Debug)]
    pub(crate) struct Soft;

    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    impl Crypto for Soft {
        fn sha256(&self, parts: &[&[u8]]) -> [u8; 32] {
            let mut h: [u32; 8] = [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ];
            for block in pad(parts, true).chunks(64) {
                let mut w = [0u32; 64];
                for i in 0..16 {
                    w[i] = u32::from_be_bytes(block[4 * i..4 * i + 4].try_into().unwrap());
                }
                for i in 16..64 {
                    let s0 =
                        w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ w[i - 15] >> 3;
                    let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ w[i - 2] >> 10;
                    w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
                }
                let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
                for i in 0..64 {
                    let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
                    let ch = (e & f) ^ (!e & g);
                    let t1 =
                        hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
                    let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
                    let maj = (a & b) ^ (a & c) ^ (b & c);
                    let t2 = s0.wrapping_add(maj);
                    (hh, g, f, e, d, c, b, a) =
                        (g, f, e, d.wrapping_add(t1), c, b, a, t1.wrapping_add(t2));
                }
                for (x, y) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
                    *x = x.wrapping_add(y);
                }
            }
            let mut out = [0; 32];
            for (i, word) in h.iter().enumerate() {
                out[4 * i..4 * i + 4].copy_from_slice(&word.to_be_bytes());
            }
            out
        }

        fn hmac_sha256(&self, key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
            let mut block = [0u8; 64];
            if key.len() > 64 {
                block[..32].copy_from_slice(&self.sha256(&[key]));
            } else {
                block[..key.len()].copy_from_slice(key);
            }
            let inner_pad = block.map(|b| b ^ 0x36);
            let outer_pad = block.map(|b| b ^ 0x5c);
            let mut inner = vec![&inner_pad[..]];
            inner.extend_from_slice(parts);
            let inner = self.sha256(&inner);
            self.sha256(&[&outer_pad, &inner])
        }

        fn pbkdf2_sha256(&self, password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
            let mut u = self.hmac_sha256(password, &[salt, &1u32.to_be_bytes()]);
            let mut out = u;
            for _ in 1..iterations {
                u = self.hmac_sha256(password, &[&u]);
                for (o, x) in out.iter_mut().zip(u) {
                    *o ^= x;
                }
            }
            out
        }

        fn md5(&self, parts: &[&[u8]]) -> [u8; 16] {
            super::md5(parts)
        }
    }

    pub(crate) fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn vectors() {
        assert_eq!(
            hex(&Soft.sha256(&[b"a", b"bc"])),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&Soft.sha256(&[b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"])),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        assert_eq!(hex(&Soft.md5(&[])), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(
            hex(&Soft.md5(&[b"The quick brown fox jumps over the lazy dog"])),
            "9e107d9d372bb6826bd81d3542a419d6"
        );
        // RFC 4231, test case 2.
        assert_eq!(
            hex(&Soft.hmac_sha256(b"Jefe", &[b"what do ya want ", b"for nothing?"])),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // RFC 7914, section 11, the first 32 bytes.
        assert_eq!(
            hex(&Soft.pbkdf2_sha256(b"passwd", b"salt", 1)),
            "55ac046e56e3089fec1691c22544b605f94185216dde0465e68b9d57c20dacbc"
        );
    }

    #[test]
    fn the_crates_agree() {
        use super::Hashes;
        let long = [b'x'; 200];
        for parts in [&[][..], &[&b"a"[..], b"bc"], &[&long[..], b"", &long[..63]]] {
            assert_eq!(Hashes.sha256(parts), Soft.sha256(parts));
            assert_eq!(Hashes.md5(parts), Soft.md5(parts));
            for key in [&b""[..], b"Jefe", &long] {
                assert_eq!(Hashes.hmac_sha256(key, parts), Soft.hmac_sha256(key, parts));
            }
        }
        for iterations in [1, 2, 4096] {
            assert_eq!(
                Hashes.pbkdf2_sha256(b"pencil", b"salt", iterations),
                Soft.pbkdf2_sha256(b"pencil", b"salt", iterations)
            );
        }
    }
}
