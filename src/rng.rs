//! A tiny, fast, dependency-free PRNG (SplitMix64).
//!
//! We avoid the `rand` crate so the whole project builds and runs offline with
//! zero external dependencies. SplitMix64 is more than good enough for generating
//! a synthetic player population in the load generator.

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        // Avoid a zero state degenerate-ish start.
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform f64 in [0, 1).
    #[inline]
    pub fn next_f64(&mut self) -> f64 {
        // Use the top 53 bits for a uniform double.
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}
