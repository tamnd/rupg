//! [`Prng`], the generator of `pg_prng.c`: xoroshiro128** with a seed from splitmix64. `random`, `setseed` and the random functions of the other types use it, so a session after `setseed` gives the same numbers as PostgreSQL.

/// The state of the generator, as `pg_prng_state` holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prng {
    s0: u64,
    s1: u64,
}

/// `splitmix64`: the next value of the generator of the seed.
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut val = *state;
    val = (val ^ (val >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    val = (val ^ (val >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    val ^ (val >> 31)
}

impl Prng {
    /// `pg_prng_seed`: the state of a seed of 64 bits.
    pub fn seed(seed: u64) -> Prng {
        let mut seed = seed;
        let s0 = splitmix64(&mut seed);
        let s1 = splitmix64(&mut seed);
        Prng::checked(s0, s1)
    }

    /// `pg_prng_strong_seed`: the state of 16 random bytes.
    pub fn from_bytes(bytes: [u8; 16]) -> Prng {
        let (s0, s1) = bytes.split_at(8);
        let word = |b: &[u8]| u64::from_ne_bytes(b.try_into().unwrap_or_default());
        Prng::checked(word(s0), word(s1))
    }

    /// `pg_prng_fseed`: the state of a seed in the range -1 to 1, as `setseed` gives it.
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss, clippy::cast_sign_loss)]
    pub fn fseed(fseed: f64) -> Prng {
        let seed = (((1u64 << 52) - 1) as f64 * fseed) as i64;
        Prng::seed(seed as u64)
    }

    /// `pg_prng_seed_check`: a state of all zeros becomes the parameters of the LCG of Knuth.
    fn checked(s0: u64, s1: u64) -> Prng {
        if s0 == 0 && s1 == 0 {
            return Prng { s0: 0x5851_F42D_4C95_7F2D, s1: 0x1405_7B7E_F767_814F };
        }
        Prng { s0, s1 }
    }

    /// `xoroshiro128ss`: the next value, `pg_prng_uint64`.
    pub fn next_u64(&mut self) -> u64 {
        let s0 = self.s0;
        let sx = self.s1 ^ s0;
        let val = s0.wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        self.s0 = s0.rotate_left(24) ^ sx ^ (sx << 16);
        self.s1 = sx.rotate_left(37);
        val
    }

    /// `pg_prng_uint64_range`: a value from `rmin` to `rmax`, with the bitmask rejection method.
    pub fn uint64_range(&mut self, rmin: u64, rmax: u64) -> u64 {
        if rmax <= rmin {
            return rmin;
        }
        let range = rmax - rmin;
        let shift = range.leading_zeros();
        loop {
            let val = self.next_u64() >> shift;
            if val <= range {
                return rmin + val;
            }
        }
    }

    /// `pg_prng_int64_range`: a value from `rmin` to `rmax`.
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_wrap)]
    pub fn int64_range(&mut self, rmin: i64, rmax: i64) -> i64 {
        if rmax <= rmin {
            return rmin;
        }
        let offset = self.uint64_range(0, (rmax as u64).wrapping_sub(rmin as u64));
        (rmin as u64).wrapping_add(offset) as i64
    }

    /// `pg_prng_double`: a value from 0 up to but not including 1, with 52 random bits.
    #[allow(clippy::cast_precision_loss)]
    pub fn double(&mut self) -> f64 {
        let v = self.next_u64() >> (64 - 52);
        v as f64 / (1u64 << 52) as f64
    }

    /// `pg_prng_double_normal`: a value of the standard normal distribution, with the basic Box-Muller transform.
    pub fn double_normal(&mut self) -> f64 {
        let u1 = 1.0 - self.double();
        let u2 = 1.0 - self.double();
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).sin()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_is_inclusive() {
        let mut prng = Prng::seed(1);
        for _ in 0..1000 {
            let v = prng.int64_range(-3, 3);
            assert!((-3..=3).contains(&v));
        }
        assert_eq!(prng.int64_range(5, 5), 5);
        assert_eq!(prng.uint64_range(0, 0), 0);
    }

    #[test]
    fn double_is_below_one() {
        let mut prng = Prng::fseed(0.5);
        for _ in 0..1000 {
            let v = prng.double();
            assert!((0.0..1.0).contains(&v));
        }
    }

    #[test]
    fn zero_state_is_replaced() {
        assert_eq!(Prng::from_bytes([0; 16]), Prng::checked(0, 0));
        assert_ne!(Prng::checked(0, 0), Prng { s0: 0, s1: 0 });
    }
}
