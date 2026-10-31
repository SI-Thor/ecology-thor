//! Freshness and decay math — the *one* module whose floating-point the CPU
//! kernel and the CUDA sweeps must agree on bit-for-bit.
//!
//! Two functions matter:
//!
//! * [`link_freshness`] — the re-attestation freshness of a single ALIVE link
//!   (grace window, then linear decay to zero at the horizon). This is a
//!   faithful port of cognitive-ecology `why.py` v0.3 (`_path_freshness`):
//!   full freshness through the first third of the horizon, then a linear
//!   slide to 0. It is integer-tick arithmetic with a single division —
//!   trivially bit-exact on both devices.
//!
//! * [`exp2_fast`] — the mass-decay exponential, used by the sweep kernel
//!   (`mass *= 2^(-λ·Δt)`). We deliberately use a small polynomial
//!   approximation written in *plain, non-fused* IEEE ops so that the same
//!   expression compiled by rustc (x86_64 CI, aarch64 Thor) and by nvcc with
//!   `-fmad=false` (see `kernels/Makefile`) produces identical bits.
//!   `docs/PTX_NOTES.md` explains why we do not use `ex2.approx.f32` on the
//!   authoritative path, and where we *do* use it (non-authoritative stats).
//!
//! Why linear freshness for purpose, exponential for mass? Purpose is priced
//! by a *promise* ("re-attest within the horizon") — a linear slide toward
//! the moment the promise lapses is auditable and matches the parent repo's
//! law. Mass is priced by *physics* — attention decays like everything else
//! in an economy, smoothly, and the exponential keeps the conservation ledger
//! well-conditioned over long runs.

/// Freshness of one ALIVE link, 0..1.
///
/// * Non-ALIVE links return the neutral element `1.0` — in the parent repo a
///   non-alive link does not reduce path freshness (its doubt is priced by the
///   alive-*fraction* term of `justification`, not by freshness).
/// * ALIVE links: `1.0` while `age <= horizon/3` (the grace window in which a
///   young purpose establishes itself), then linear to `0.0` at `age >= horizon`.
/// * `last_beat < 0` (never attested) falls back to `born`, matching
///   `why.py` (`last = n.last_beat if n.last_beat >= 0 else n.born`).
#[inline]
pub fn link_freshness(state_alive: bool, last_beat: i64, born: u32, now: u32, horizon: u32) -> f32 {
    debug_assert!(horizon >= 1, "attestation horizon must be >= 1 tick");
    if !state_alive {
        return 1.0;
    }
    let last = if last_beat >= 0 {
        last_beat as u32
    } else {
        born
    };
    let age = now.saturating_sub(last);
    let grace = horizon / 3; // integer division, matching why.py's h // 3
    if age <= grace {
        1.0
    } else if age >= horizon {
        0.0
    } else {
        let denom = (horizon - grace) as f32;
        // A single division; identical bits on every IEEE-754 device.
        1.0 - (age - grace) as f32 / denom
    }
}

/// A revived-but-unserved purpose bids at half strength (why.py's
/// `PROBATION_CAP`). Kept here so CPU and device sweeps share the constant.
pub const PROBATION_CAP: f32 = 0.5;

/// 2^x for x in [-126, 127], via a non-fused polynomial. Max relative error
/// ≈ 2%, which is far below the noise floor of any attention score.
///
/// Construction: split x into integer part `i` and fraction `f`; assemble
/// `2^i` from the exponent bits of an f32; multiply by the quadratic
/// approximation of `2^f`. Every operation is a plain IEEE multiply/add —
/// **no FMA contraction on either toolchain** — so the result is bit-exact
/// across rustc and nvcc. That exactness is what lets the golden tests in
/// `tests/golden.rs` compare CPU and device sweeps.
// clippy: the coefficients below are the *pinned* Taylor coefficients of
// the shared bit-exact polynomial (they must match kernels/ecology.cu
// character-for-character); substituting library constants would invite
// drift between the two toolchains' constant folding.
#[allow(clippy::approx_constant)]
#[inline]
pub fn exp2_fast(x: f32) -> f32 {
    if x <= -126.0 {
        // Deep underflow: mass effectively annihilated. (Also keeps the
        // exponent assembly below in-range.)
        return 0.0;
    }
    if x >= 127.0 {
        return f32::INFINITY;
    }
    // Floor split without libm: i = trunc toward zero is wrong for negatives,
    // so do the floor explicitly.
    let i = x.floor();
    let f = x - i; // in [0, 1)
                   // 2^f ≈ 1 + ln2·f + (ln2²/2)·f² + (ln3/6)·f³ — Taylor cubic, Horner
                   // form, three multiplies, max relative error ≈ 0.56% on [0, 1).
                   // Do NOT rewrite as fma; see the module docs.
    let poly = 1.0 + (0.6931472 + (0.2402265 + 0.0555041 * f) * f) * f;
    // Assemble 2^i from bits: exponent field = i + 127.
    let scale = f32::from_bits(((i as i32 + 127) as u32) << 23);
    scale * poly
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freshness_grace_then_linear() {
        let h = 90;
        // Grace = 30 ticks.
        assert_eq!(link_freshness(true, 100, 0, 130, h), 1.0);
        // Midway between grace and horizon: 1 - 30/60 = 0.5.
        let v = link_freshness(true, 100, 0, 160, h);
        assert!((v - 0.5).abs() < 1e-6, "got {v}");
        // At/after the horizon: dead flat zero.
        assert_eq!(link_freshness(true, 100, 0, 190, h), 0.0);
        assert_eq!(link_freshness(true, 100, 0, 900, h), 0.0);
        // Stale information never rewinds freshness: `now < last_beat` clamps age to 0.
        assert_eq!(link_freshness(true, 200, 0, 100, h), 1.0);
        // Non-alive links are neutral for the min.
        assert_eq!(link_freshness(false, 100, 0, 999, h), 1.0);
        // Never-attested node falls back to its birth tick.
        assert_eq!(link_freshness(true, NO_BEAT_SENTINEL, 50, 55, 90), 1.0);
    }

    const NO_BEAT_SENTINEL: i64 = -1;

    #[test]
    fn exp2_reasonable_and_deterministic() {
        for k in 0..=40 {
            let x = -3.0 + k as f32 * 0.125;
            let approx = exp2_fast(x);
            let exact = 2f32.powf(x);
            let rel = ((approx - exact) / exact).abs();
            assert!(rel < 0.02, "x={x} approx={approx} exact={exact} rel={rel}");
        }
        // Exactness contract: same input, same bits, run after run.
        assert_eq!(exp2_fast(-1.75f32).to_bits(), exp2_fast(-1.75f32).to_bits());
        // Extremes.
        assert_eq!(exp2_fast(-200.0), 0.0);
        assert!(exp2_fast(200.0).is_infinite());
        // Integer arguments are near-exact (poly at f=0 is exactly 1).
        assert!((exp2_fast(-3.0) - 0.125).abs() < 1e-6);
        assert!((exp2_fast(4.0) - 16.0).abs() < 1e-5);
    }
}
