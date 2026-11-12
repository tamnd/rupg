//! The random numbers of the builds: SplitMix64 from a seed, so that a build of the same rows with the same seed gives the same index.

/// A generator of SplitMix64.
#[derive(Clone, Debug)]
pub(crate) struct Rng(u64);

impl Rng {
    /// A generator from `seed`.
    pub(crate) fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    /// The next 64 bits.
    pub(crate) fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number below `n`, which must not be 0.
    pub(crate) fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }

    /// A number in `[0, 1)`.
    pub(crate) fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    #[cfg(test)]
    /// A number of the standard normal distribution, by the method of Box and Muller.
    pub(crate) fn gaussian(&mut self) -> f64 {
        let u1 = 1.0 - self.unit();
        let u2 = self.unit();
        (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }

    /// The numbers `0..n` in a random order, by the shuffle of Fisher and Yates.
    pub(crate) fn permutation(&mut self, n: usize) -> Vec<u32> {
        let mut out: Vec<u32> = (0..n as u32).collect();
        for i in (1..n).rev() {
            out.swap(i, self.below(i + 1));
        }
        out
    }
}
