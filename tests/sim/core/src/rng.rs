//! Deterministic pseudo-random source: SplitMix64 seeding into xoshiro256**. Dependency-free so that the fast
//! layer and the LiteSVM layer produce identical action streams for the same seed on any host.

#[derive(Clone, Debug)]
pub struct Rng {
    s: [u64; 4],
}

fn splitmix(x: &mut u64) -> u64 {
    *x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

impl Rng {
    #[must_use]
    pub fn new(seed: u64) -> Self {
        let mut x = seed;
        Self { s: [splitmix(&mut x), splitmix(&mut x), splitmix(&mut x), splitmix(&mut x)] }
    }
    /// A child generator for an independent stream (for example the rate path versus the trader flow).
    #[must_use]
    pub fn fork(&mut self, tag: u64) -> Self {
        Self::new(self.next_u64() ^ tag.wrapping_mul(0xD1B5_4A32_D192_ED03))
    }
    pub fn next_u64(&mut self) -> u64 {
        let r = self.s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = self.s[3].rotate_left(45);
        r
    }
    /// Uniform in `[0, n)`; `n == 0` returns 0.
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        // Lemire's nearly-divisionless method.
        let mut m = u128::from(self.next_u64()) * u128::from(n);
        let mut l = m as u64;
        if l < n {
            let t = n.wrapping_neg() % n;
            while l < t {
                m = u128::from(self.next_u64()) * u128::from(n);
                l = m as u64;
            }
        }
        (m >> 64) as u64
    }
    /// Uniform in `[lo, hi]`.
    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        if hi <= lo {
            return lo;
        }
        lo + self.below(hi - lo + 1)
    }
    pub fn range_i64(&mut self, lo: i64, hi: i64) -> i64 {
        if hi <= lo {
            return lo;
        }
        lo + self.below((hi - lo) as u64 + 1) as i64
    }
    /// Uniform in `[0, 1)`.
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }
    pub fn uniform(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.unit()
    }
    pub fn log_uniform(&mut self, lo: f64, hi: f64) -> f64 {
        (self.uniform(lo.ln(), hi.ln())).exp()
    }
    pub fn chance(&mut self, p: f64) -> bool {
        self.unit() < p
    }
    /// Standard normal by Box-Muller.
    pub fn normal(&mut self) -> f64 {
        let u1 = self.unit().max(1e-300);
        let u2 = self.unit();
        (-2.0 * u1.ln()).sqrt() * (2.0 * core::f64::consts::PI * u2).cos()
    }
    /// Poisson count. Knuth for small means, normal approximation above 64.
    pub fn poisson(&mut self, lambda: f64) -> u64 {
        if lambda <= 0.0 {
            return 0;
        }
        if lambda > 64.0 {
            let v = lambda + lambda.sqrt() * self.normal();
            return v.round().max(0.0) as u64;
        }
        let l = (-lambda).exp();
        let mut k = 0u64;
        let mut p = 1.0;
        loop {
            p *= self.unit();
            if p <= l {
                return k;
            }
            k += 1;
        }
    }
    pub fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len() as u64) as usize]
    }
    /// Index into a weight table.
    pub fn weighted(&mut self, weights: &[f64]) -> usize {
        let total: f64 = weights.iter().sum();
        let mut x = self.unit() * total;
        for (i, w) in weights.iter().enumerate() {
            if x < *w {
                return i;
            }
            x -= w;
        }
        weights.len().saturating_sub(1)
    }
}
