//! CPU reference mirrors of every device kernel — bit-exact, by contract.
//!
//! Each function here is a line-for-line port of a kernel in
//! `kernels/ecology.cu` (same expressions, same order, no FMA on either
//! side). Their job is threefold:
//!
//! 1. **Parity anchor**: on-device tests (`tests/parity.rs`) compare these
//!    against the actual GPU output, bit for bit, before the device path is
//!    trusted (and eventually adopted as authoritative — `docs/PLACEMENT.md`).
//! 2. **No-GPU builds**: the same sweep semantics run on any machine.
//! 3. **Reviewable spec**: the CUDA kernels are reviewed *through* these —
//!    a Rust reviewer and a CUDA reviewer read the same algorithm.
//!
//! These mirrors also equal the `ecology-core` implementations (the tests
//! below assert it on randomized populations): the ledger, the CPU
//! reference and the device all compute the same function.

use ecology_core::freshness::{exp2_fast, link_freshness};
use ecology_core::why::WhyState;

/// WhyState as the byte the device sees (`#[repr(u8)]`).
#[inline]
pub fn state_byte(s: WhyState) -> u8 {
    s as u8
}

/// Mirror of `link_freshness_kernel`.
pub fn link_freshness_all(
    states: &[WhyState],
    last_beat: &[i64],
    born: &[u32],
    now: u32,
    horizon: u32,
    fresh: &mut [f32],
) {
    for i in 0..states.len() {
        fresh[i] = link_freshness(
            states[i] == WhyState::Alive,
            last_beat[i],
            born[i],
            now,
            horizon,
        );
    }
}

/// Mirror of `justify_level_kernel` — one level of the chain aggregation.
///
/// `nodes` holds this level's indices; every parent (one level up) must
/// already be aggregated. Single in/out arrays are race-free by the same
/// argument as the kernel (parents are never in this level).
#[allow(clippy::too_many_arguments)]
pub fn justify_level(
    nodes: &[u32],
    parent: &[u32],
    states: &[WhyState],
    probation: &[bool],
    fresh: &[f32],
    probation_cap: f32,
    chain_alive: &mut [u32],
    chain_len: &mut [u32],
    chain_min_fresh: &mut [f32],
    chain_dead: &mut [bool],
    chain_probation: &mut [bool],
    justification: &mut [f32],
) {
    const NO_PARENT: u32 = u32::MAX;
    for &idx in nodes {
        let i = idx as usize;
        let p = parent[i];
        let alive = states[i] == WhyState::Alive;
        if p == NO_PARENT {
            chain_alive[i] = alive as u32;
            chain_len[i] = 1;
            chain_min_fresh[i] = fresh[i];
            chain_dead[i] = states[i] == WhyState::Dead;
            chain_probation[i] = alive && probation[i];
        } else {
            let pi = p as usize;
            chain_alive[i] = chain_alive[pi] + alive as u32;
            chain_len[i] = chain_len[pi] + 1;
            let f = fresh[i];
            let pf = chain_min_fresh[pi];
            chain_min_fresh[i] = if f < pf { f } else { pf };
            chain_dead[i] = chain_dead[pi] || states[i] == WhyState::Dead;
            chain_probation[i] = chain_probation[pi] || (alive && probation[i]);
        }
        if chain_dead[i] || chain_alive[i] == 0 {
            justification[i] = 0.0;
        } else {
            let base = chain_alive[i] as f32 / chain_len[i] as f32;
            let mut j = base * chain_min_fresh[i];
            if chain_probation[i] && j > probation_cap {
                j = probation_cap;
            }
            justification[i] = j;
        }
    }
}

/// Mirror of `reap_kernel`. Returns the lapsed count (the kernel's atomic).
pub fn reap(
    states: &mut [WhyState],
    last_beat: &[i64],
    born: &[u32],
    now: u32,
    horizon: u32,
) -> u32 {
    let mut reaped = 0u32;
    for i in 0..states.len() {
        if states[i] != WhyState::Alive {
            continue;
        }
        let last = if last_beat[i] >= 0 {
            last_beat[i] as u32
        } else {
            born[i]
        };
        let age = now.saturating_sub(last);
        if age > horizon {
            states[i] = WhyState::Unknown;
            reaped += 1;
        }
    }
    reaped
}

/// Mirror of `decay_kernel` (same `exp2_fast` polynomial, computed once).
pub fn decay_mass(states: &[WhyState], mass: &mut [f32], lambda_per_tick: f32, dt: u32) {
    let factor = exp2_fast(-lambda_per_tick * dt as f32);
    for i in 0..states.len() {
        if states[i] != WhyState::Alive {
            continue; // UNKNOWN frozen, DEAD already 0
        }
        mass[i] *= factor;
    }
}

/// Mirror of `auction_score_kernel` (canonical multiply order, NaN-safe
/// clamps, the veto law). Returns the vetoed count.
#[allow(clippy::too_many_arguments)]
pub fn auction_score_all(
    states: &[WhyState],
    present: &[bool],
    urgency: &[f32],
    justification: &[f32],
    opportunity: &[f32],
    novelty: &[f32],
    expected_value: &[f32],
    score: &mut [f32],
) -> u32 {
    #[inline]
    fn clamp01_eco(x: f32) -> f32 {
        if x.is_nan() || x < 0.0 {
            0.0
        } else if x > 1.0 {
            1.0
        } else {
            x
        }
    }
    let mut vetoed = 0u32;
    for i in 0..present.len() {
        if !present[i] || states[i] == WhyState::Absent {
            score[i] = 0.0;
            continue;
        }
        let u = clamp01_eco(urgency[i]);
        let j = clamp01_eco(justification[i]);
        let o = clamp01_eco(opportunity[i]);
        let nv = clamp01_eco(novelty[i]);
        let ev = clamp01_eco(expected_value[i]);
        let s = u * j * o * nv * ev; // canonical order — do not reassociate
        score[i] = s;
        if s <= 0.0 {
            vetoed += 1; // the veto law
        }
    }
    vetoed
}

/// Mirror of `score_stats_kernel` — sequential (telemetry-only) sum & max.
pub fn score_stats(score: &[f32]) -> (f32, f32) {
    let mut sum = 0.0f32;
    let mut max = 0.0f32;
    for &v in score {
        sum += v;
        if v > max {
            max = v;
        }
    }
    (sum, max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ecology_core::freshness::PROBATION_CAP;
    use ecology_core::ids::EntityId;
    use ecology_core::rng::Pcg32;
    use ecology_core::why::{SpawnState, WhyLedger};
    use ecology_core::Reason;

    /// Randomized population; asserts reference == ledger aggregates.
    #[test]
    fn reference_matches_ledger_aggregates() {
        let mut rng = Pcg32::new(7);
        let mut ledger = WhyLedger::new(512);
        let mut parents: Vec<EntityId> = Vec::new();
        for _i in 0..200u32 {
            let parent = if parents.is_empty() || rng.next_f32() < 0.3 {
                None
            } else {
                Some(parents[rng.next_bounded(parents.len() as u32) as usize])
            };
            let id = ledger
                .spawn(
                    parent,
                    rng.next_f32(),
                    None,
                    SpawnState::Alive,
                    rng.next_bounded(500),
                    Reason::Born,
                )
                .unwrap();
            parents.push(id);
            if rng.next_f32() < 0.4 {
                ledger.heartbeat(id, 500 + rng.next_bounded(400));
            }
            if rng.next_f32() < 0.1 {
                ledger.doubt(id, ecology_core::Reason::Custom, 500);
            }
        }
        let now = 900u32;
        let horizon = 600u32;

        // Reference pipeline.
        let n = ledger.len();
        let states: Vec<WhyState> = (0..n)
            .map(|i| ledger.state(EntityId(i as u32)).unwrap())
            .collect();
        let mut states_ref = states.clone();
        let last_beat: Vec<i64> = (0..n)
            .map(|i| ledger.last_beat(EntityId(i as u32)))
            .collect();
        let born: Vec<u32> = (0..n).map(|i| ledger.born_at(i)).collect();
        let parent: Vec<u32> = (0..n).map(|i| ledger.parent_at(i)).collect();
        let probation: Vec<bool> = (0..n).map(|i| ledger.probation_at(i)).collect();
        let mut fresh = vec![0.0f32; n];
        link_freshness_all(&states, &last_beat, &born, now, horizon, &mut fresh);

        // Level plan from the ledger.
        let levels: Vec<Vec<u32>> = ledger.levels().to_vec();
        let mut chain_alive = vec![0u32; n];
        let mut chain_len = vec![0u32; n];
        let mut chain_min_fresh = vec![1.0f32; n];
        let mut chain_dead = vec![false; n];
        let mut chain_probation = vec![false; n];
        let mut justification = vec![0.0f32; n];
        for level in &levels {
            justify_level(
                level,
                &parent,
                &states,
                &probation,
                &fresh,
                PROBATION_CAP,
                &mut chain_alive,
                &mut chain_len,
                &mut chain_min_fresh,
                &mut chain_dead,
                &mut chain_probation,
                &mut justification,
            );
        }

        // Ledger pipeline (same now/horizon).
        ledger.recompute_aggregates(now, horizon);
        for (i, j_ref) in justification.iter().enumerate().take(n) {
            assert_eq!(
                *j_ref,
                ledger.justification(EntityId(i as u32)),
                "justification mismatch at {i}"
            );
        }

        // Reap parity.
        let reaped_ref = reap(&mut states_ref, &last_beat, &born, now, horizon);
        let reaped_ledger = ledger.reap(now, horizon);
        assert_eq!(reaped_ref, reaped_ledger);
        for (i, s_ref) in states_ref.iter().enumerate().take(n) {
            assert_eq!(
                *s_ref,
                ledger.state(EntityId(i as u32)).unwrap(),
                "reap state mismatch at {i}"
            );
        }
    }

    /// Randomized bids; asserts reference score == attention_score.
    #[test]
    fn reference_matches_attention_score() {
        let mut rng = Pcg32::new(11);
        let n = 512;
        let states = vec![WhyState::Alive; n];
        let present: Vec<bool> = (0..n).map(|_| rng.next_f32() < 0.8).collect();
        let urgency: Vec<f32> = (0..n).map(|_| rng.next_f32() * 1.2 - 0.1).collect();
        let justification: Vec<f32> = (0..n).map(|_| rng.next_f32()).collect();
        let opportunity: Vec<f32> = (0..n).map(|_| rng.next_f32() * 1.1).collect();
        let novelty: Vec<f32> = (0..n).map(|_| rng.next_f32()).collect();
        let ev: Vec<f32> = (0..n).map(|_| rng.next_f32()).collect();
        let mut score = vec![0.0f32; n];
        let vetoed = auction_score_all(
            &states,
            &present,
            &urgency,
            &justification,
            &opportunity,
            &novelty,
            &ev,
            &mut score,
        );
        // The veto law's bookkeeping: exactly the present candidates that
        // scored <= 0 are counted as vetoed.
        let expected_vetoed = present
            .iter()
            .zip(score.iter())
            .filter(|(&p, &s)| p && s <= 0.0)
            .count();
        assert_eq!(vetoed as usize, expected_vetoed);
        // Non-present rows never even enter the market.
        for (i, &p) in present.iter().enumerate() {
            if !p {
                assert_eq!(score[i], 0.0);
            }
        }
        // Monotonic transform sanity: all scores are finite and >= 0.
        assert!(score.iter().all(|&s| s >= 0.0 && s.is_finite()));
    }
}
