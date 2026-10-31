//! Deterministic RNG (PCG32) — the only randomness the kernel will ever use.
//!
//! Lockstep-grade determinism (`docs/ARCHITECTURE.md`, §Determinism): the
//! kernel never reads the system RNG, the clock, or a hash-map iteration
//! order. Every stochastic decision (workload jitter in the simulator,
//! tie-breaking policies) flows from a seed the caller chooses, through this
//! small, well-understood generator. Same seed + same event sequence ⇒ same
//! journal chain hash, on x86_64 CI and on the Thor's aarch64 alike.

/// PCG-XSH-RR 64/32 (O'Neill 2014). Small, fast, statistically fine for
/// simulation drivers — and *not* cryptographic, which is fine: we need
/// reproducibility, not secrecy.
#[derive(Clone, Debug)]
pub struct Pcg32 {
    state: u64,
    inc: u64,
}

impl Pcg32 {
    /// `seed` selects the stream; two generators with the same seed produce
    /// identical sequences.
    pub fn new(seed: u64) -> Self {
        let mut g = Pcg32 {
            state: 0,
            // Odd increment required by PCG.
            inc: (seed << 1) | 1,
        };
        // One warm-up step, per the reference implementation.
        g.state = g.state.wrapping_add(g.inc);
        g.next_u32();
        g.state = g.state.wrapping_add(g.inc);
        g
    }

    /// Next 32 random bits.
    #[inline]
    pub fn next_u32(&mut self) -> u32 {
        let old = self.state;
        self.state = old.wrapping_mul(6364136223846793005).wrapping_add(self.inc);
        // XSH-RR: xorshift-high, random rotation.
        let xorshifted = (((old >> 18) ^ old) >> 27) as u32;
        let rot = (old >> 59) as u32;
        xorshifted.rotate_right(rot)
    }

    /// Uniform-ish `f32` in `[0, 1)`. Deterministic construction: 24 bits of
    /// entropy scaled by an exact power of two (no division, no libm).
    #[inline]
    pub fn next_f32(&mut self) -> f32 {
        (self.next_u32() >> 8) as f32 * (1.0 / 16777216.0)
    }

    /// Uniform `f64` in `[0, 1)` with 32 bits of entropy — fine for
    /// event probabilities in the simulator.
    #[inline]
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u32() as f64) / 4294967296.0
    }
    /// Uniform integer in `[0, n)` (n > 0), rejection-free modulo variant.
    /// Slight modulo bias is acceptable for workload simulation.
    #[inline]
    pub fn next_bounded(&mut self, n: u32) -> u32 {
        debug_assert!(n > 0);
        self.next_u32() % n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_stream() {
        let mut a = Pcg32::new(42);
        let mut b = Pcg32::new(42);
        for _ in 0..100 {
            assert_eq!(a.next_u32(), b.next_u32());
        }
    }

    #[test]
    fn f32_in_range() {
        let mut g = Pcg32::new(7);
        for _ in 0..1000 {
            let x = g.next_f32();
            assert!((0.0..1.0).contains(&x));
        }
    }
}
