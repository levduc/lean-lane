//! A tiny deterministic PRNG (SplitMix64). Self-contained on purpose: a
//! reproducer is `(seed, index)` and must mean the same bytes forever,
//! whatever `rand` does between versions.

#[derive(Clone, Debug)]
pub struct Rng(u64);

/// Mix two words into a well-spread seed (per-chain / per-iteration seeds).
pub fn mix(a: u64, b: u64) -> u64 {
    let mut r = Rng(a ^ b.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(17));
    r.next_u64();
    r.next_u64()
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed ^ 0xD1B5_4A32_D192_ED03)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n` (`0` if `n == 0`).
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        ((self.next_u64() as u128 * n as u128) >> 64) as u64
    }

    /// Uniform in `0..n` for u128 bounds.
    pub fn below_u128(&mut self, n: u128) -> u128 {
        if n == 0 {
            return 0;
        }
        let x = ((self.next_u64() as u128) << 64) | self.next_u64() as u128;
        x % n
    }

    /// Uniform in `lo..=hi`.
    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.below(hi - lo + 1)
    }

    pub fn usize(&mut self, lo: usize, hi: usize) -> usize {
        self.range(lo as u64, hi as u64) as usize
    }

    pub fn chance(&mut self, p: f64) -> bool {
        ((self.next_u64() >> 11) as f64 / (1u64 << 53) as f64) < p
    }

    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        let mut v = vec![0u8; n];
        self.fill(&mut v);
        v
    }

    pub fn fill(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let w = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&w[..chunk.len()]);
        }
    }

    pub fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len() as u64) as usize]
    }

    /// Index drawn with the given integer weights.
    pub fn weighted(&mut self, weights: &[u32]) -> usize {
        let total: u64 = weights.iter().map(|&w| w as u64).sum();
        let mut x = self.below(total.max(1));
        for (i, &w) in weights.iter().enumerate() {
            if x < w as u64 {
                return i;
            }
            x -= w as u64;
        }
        weights.len() - 1
    }
}
