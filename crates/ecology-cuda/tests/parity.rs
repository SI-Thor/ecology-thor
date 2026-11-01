//! Parity: the three implementations of every sweep must agree.
//!
//! This file is the gate `docs/PLACEMENT.md` and `docs/PTX_NOTES.md` keep
//! referring to. Two legs:
//!
//! 1. **CPU leg (always runs)**: the reference mirrors
//!    (`ecology_cuda::reference`) vs. the ledger's own sweep
//!    (`ecology_core::WhyLedger`) on randomized populations. Bit-exact by
//!    construction — same expressions, same order, no FMA either side.
//!
//! 2. **Device leg (feature `cuda`, real hardware only)**: mirror the
//!    organism into unified memory, run the actual Blackwell kernels,
//!    compare every column bit-for-bit against the CPU reference. This is
//!    the gate that must pass **before** the device path is allowed to
//!    become authoritative (v0.2, PLACEMENT.md).
//!
//! Run on the Thor:  `make kernels && cargo test -p ecology-cuda --features cuda`

use ecology_core::ids::EntityId;
use ecology_core::rng::Pcg32;
use ecology_core::tick::{Config, Ecology, GoalSpec};
use ecology_core::why::WhyState;
use ecology_cuda::reference;

/// Build a messy, deterministic organism: regions (some with deadlines),
/// scenes, drafts, stalls, revives, kills with cascades, bids everywhere.
fn organism(seed: u64, capacity: usize) -> Ecology {
    let mut rng = Pcg32::new(seed);
    let mut eco = Ecology::new(Config {
        capacity,
        ..Config::default()
    });

    let mut regions = Vec::new();
    for _ in 0..8 {
        let with_deadline = rng.next_f32() < 0.3;
        regions.push(
            eco.spawn(GoalSpec {
                until: if with_deadline { Some(30_000) } else { None },
                mass: 5.0,
                ..Default::default()
            })
            .unwrap(),
        );
    }
    let _draft = eco
        .spawn(GoalSpec {
            alive: false,
            ..Default::default()
        })
        .unwrap();

    let mut scenes = Vec::new();
    for i in 0..400u32 {
        let region = regions[(i % regions.len() as u32) as usize];
        let id = eco
            .spawn(GoalSpec {
                parent: Some(region),
                mass: 0.5,
                ..Default::default()
            })
            .unwrap();
        scenes.push(id);
        eco.set_bid(
            id,
            ecology_core::attention::Bid {
                urgency: rng.next_f32(),
                opportunity: rng.next_f32(),
                novelty: rng.next_f32(),
                expected_value: rng.next_f32(),
                class: match i % 50 {
                    0 => ecology_core::attention::Importance::Kernel,
                    1..=15 => ecology_core::attention::Importance::World,
                    _ => ecology_core::attention::Importance::Scenery,
                },
                cost: ecology_core::rails::Rails {
                    gpu_seconds: rng.next_f32(),
                    joules: 2.0 * rng.next_f32(),
                    cents: 0.0,
                },
            },
        );
    }

    // Drive every mechanism: heartbeats, service chains, doubt, a kill with
    // cascade, revives, ticks (auctions + sweeps).
    for round in 0..30u32 {
        for (i, &id) in scenes.iter().enumerate() {
            match (i + round as usize) % 5 {
                0 => {
                    eco.heartbeat(id);
                }
                1 => {
                    eco.run_under(id);
                }
                2 if i % 9 == 0 => {
                    eco.doubt(id);
                }
                _ => {}
            }
        }
        if round == 10 {
            eco.kill(regions[0]);
        }
        if round == 15 {
            eco.revive(scenes[3]);
            eco.tick();
            eco.run_under(scenes[3]);
        }
        for _ in 0..10 {
            eco.tick();
        }
    }
    eco
}

#[test]
fn cpu_leg_reference_matches_ledger() {
    for seed in [1u64, 7, 20261101] {
        let mut eco = organism(seed, 4096);
        // Advance to a fresh auction bucket so aggregates are recomputed,
        // then compare every column against the reference pipeline.
        while eco.now() % 10 != 0 {
            eco.tick();
        }
        let now = eco.now();
        let cfg = eco.config();
        let n = 4096usize.min(cfg.capacity);

        // Reference pipeline over the organism's own tables.
        let states: Vec<WhyState> = (0..n)
            .map(|i| eco.state(EntityId(i as u32)).unwrap())
            .collect();
        let last_beat: Vec<i64> = (0..n).map(|i| eco.last_beat_slice()[i]).collect();
        let born: Vec<u32> = (0..n).map(|i| eco.born_slice()[i]).collect();
        let parent: Vec<u32> = (0..n).map(|i| eco.parent_slice()[i]).collect();
        let harvests: Vec<u8> = (0..n).map(|i| eco.harvests_slice()[i]).collect();

        let mut fresh = vec![0.0f32; n];
        reference::link_freshness_all(
            &states,
            &last_beat,
            &born,
            now,
            cfg.horizon_ticks,
            &mut fresh,
        );

        let levels: Vec<Vec<u32>> = eco.levels().to_vec();
        let mut chain_alive = vec![0u32; n];
        let mut chain_len = vec![0u32; n];
        let mut chain_min_fresh = vec![1.0f32; n];
        let mut chain_dead = vec![false; n];
        let mut chain_parole_cap = vec![1.0f32; n];
        let mut justification = vec![0.0f32; n];
        for level in &levels {
            reference::justify_level(
                level,
                &parent,
                &states,
                &harvests,
                &fresh,
                &mut chain_alive,
                &mut chain_len,
                &mut chain_min_fresh,
                &mut chain_dead,
                &mut chain_parole_cap,
                &mut justification,
            );
        }
        for (i, j_ref) in justification.iter().enumerate().take(n) {
            assert_eq!(
                j_ref.to_bits(),
                eco.justification(EntityId(i as u32)).to_bits(),
                "seed {seed}: justification mismatch at {i}"
            );
        }

        // Score parity.
        let present: Vec<bool> = (0..n).map(|i| eco.present_slice()[i]).collect();
        let mut score = vec![0.0f32; n];
        reference::auction_score_all(
            &states,
            &present,
            eco.urgency_slice(),
            eco.justification_slice(),
            eco.opportunity_slice(),
            eco.novelty_slice(),
            eco.expected_value_slice(),
            &mut score,
        );
        // Compare rows with a live bid: the organism's own scores come from
        // tick() over the same inputs, so any diff is real drift.
        for i in 0..n {
            if present[i] {
                assert_eq!(
                    score[i].to_bits(),
                    eco.score_slice()[i].to_bits(),
                    "seed {seed}: score mismatch at {i}"
                );
            }
        }
    }
}

#[cfg(feature = "cuda")]
mod device_leg {
    use super::*;
    use ecology_cuda::device::DeviceSweeper;

    /// THE GATE (docs/PLACEMENT.md): device output == CPU reference,
    /// bit for bit, every column, before the device path becomes
    /// authoritative. Requires `make kernels` first
    /// (crates/ecology-cuda/kernels/build/ecology.ptx) and a real GPU —
    /// i.e. run it on the Thor.
    #[test]
    fn device_leg_kernels_match_reference_bit_for_bit() {
        let ptx =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("kernels/build/ecology.ptx");
        if !ptx.exists() {
            panic!(
                "kernels not built: run `make -C crates/ecology-cuda/kernels` first \
                 (this test is meant to run on the AGX Thor)"
            );
        }
        let mut eco = organism(42, 8192);
        while eco.now() % 10 != 0 {
            eco.tick();
        }
        let now = eco.now();
        let cfg = eco.config();
        let n = 8192usize;

        let mut sweeper = DeviceSweeper::new(&ptx, n, eco.levels()).expect("device init");
        sweeper.mirror_from(&eco).expect("mirror");

        // Freshness + justification, level by level, on device.
        sweeper
            .sweep_freshness(now, cfg.horizon_ticks)
            .expect("freshness kernel");
        sweeper
            .sweep_justify()
            .expect("justify kernels");
        sweeper.synchronize();

        // CPU reference, same inputs.
        let states: Vec<WhyState> = (0..n)
            .map(|i| eco.state(EntityId(i as u32)).unwrap())
            .collect();
        let last_beat: Vec<i64> = (0..n).map(|i| eco.last_beat_slice()[i]).collect();
        let born: Vec<u32> = (0..n).map(|i| eco.born_slice()[i]).collect();
        let parent: Vec<u32> = (0..n).map(|i| eco.parent_slice()[i]).collect();
        let harvests: Vec<u8> = (0..n).map(|i| eco.harvests_slice()[i]).collect();
        let mut fresh = vec![0.0f32; n];
        reference::link_freshness_all(
            &states,
            &last_beat,
            &born,
            now,
            cfg.horizon_ticks,
            &mut fresh,
        );
        let levels: Vec<Vec<u32>> = eco.levels().to_vec();
        let mut chain_alive = vec![0u32; n];
        let mut chain_len = vec![0u32; n];
        let mut chain_min_fresh = vec![1.0f32; n];
        let mut chain_dead = vec![false; n];
        let mut chain_parole_cap = vec![1.0f32; n];
        let mut justification = vec![0.0f32; n];
        for level in &levels {
            reference::justify_level(
                level,
                &parent,
                &states,
                &harvests,
                &fresh,
                &mut chain_alive,
                &mut chain_len,
                &mut chain_min_fresh,
                &mut chain_dead,
                &mut chain_parole_cap,
                &mut justification,
            );
        }

        // The bit-exact comparison. Any single differing bit fails the gate.
        let dev_just = sweeper.justification().expect("readback");
        for (i, (d, r)) in dev_just
            .iter()
            .zip(justification.iter())
            .enumerate()
            .take(n)
        {
            assert_eq!(
                d.to_bits(),
                r.to_bits(),
                "DEVICE PARITY FAILED at {i}: device {d} vs reference {r} — do NOT adopt \
                 the device path until this is understood (docs/PTX_NOTES.md)"
            );
        }

        // Auction scores on device vs reference.
        let vetoed = sweeper.sweep_score().expect("score kernel");
        let present: Vec<bool> = (0..n).map(|i| eco.present_slice()[i]).collect();
        let mut score = vec![0.0f32; n];
        let ref_vetoed = reference::auction_score_all(
            &states,
            &present,
            eco.urgency_slice(),
            &justification,
            eco.opportunity_slice(),
            eco.novelty_slice(),
            eco.expected_value_slice(),
            &mut score,
        );
        let dev_score = sweeper.score().expect("score readback");
        for (i, (d, r)) in dev_score.iter().zip(score.iter()).enumerate().take(n) {
            assert_eq!(
                d.to_bits(),
                r.to_bits(),
                "DEVICE SCORE PARITY FAILED at {i}"
            );
        }
        assert_eq!(vetoed, ref_vetoed, "veto counts differ");

        println!("device parity: OK ({} entities, all columns bit-exact)", n);
    }
}
