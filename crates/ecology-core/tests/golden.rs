//! Golden determinism: same scenario ⇒ same journal chain hash, always.
//!
//! This is the kernel's replay contract made executable. The scenario is
//! fixed (seeded PCG, fixed operations, fixed tick count); its chain hash
//! is committed in `tests/golden/chain_hash.txt`. Any change to event
//! encoding, transition order, float math, or auction ordering changes the
//! hash and must be a *conscious* commit (`BLESS=1 cargo test` rewrites
//! the file — see CONTRIBUTING.md) with the reason in the commit message.
//!
//! The hash is expected to be identical on x86_64 and aarch64: every input
//! to the chain is either an integer or a fixed-order IEEE-754 f32
//! operation, which both platforms evaluate identically (we never emit
//! fma/`pow`/transcendentals inside the kernel — see `docs/PTX_NOTES.md`).

use ecology_core::attention::{Bid, Importance};
use ecology_core::rails::Rails;
use ecology_core::tick::{Config, Ecology, GoalSpec};

/// The canonical scenario: a small world with every mechanism exercised —
/// spawns (root/child/deadline), heartbeats, run_under chains, kills with
/// cascades, lapse-and-revive cycles, doubt, and auctions with mixed
/// classes and rail pressure.
fn run_golden_scenario() -> u64 {
    let mut e = Ecology::new(Config {
        capacity: 4096,
        ..Config::default()
    });
    let mut rng = ecology_core::rng::Pcg32::new(20261101);

    let region_a = e
        .spawn(GoalSpec {
            until: Some(20_000),
            mass: 10.0,
            ..Default::default()
        })
        .unwrap();
    let region_b = e.spawn(GoalSpec::default()).unwrap();
    let _draft = e
        .spawn(GoalSpec {
            alive: false,
            ..Default::default()
        })
        .unwrap();

    let mut scenes = Vec::new();
    for i in 0..120u32 {
        let region = if i % 2 == 0 { region_a } else { region_b };
        let id = e
            .spawn(GoalSpec {
                parent: Some(region),
                mass: 0.5,
                ..Default::default()
            })
            .unwrap();
        scenes.push(id);
    }

    for round in 0..40u32 {
        // Workload traffic.
        for (i, &id) in scenes.iter().enumerate() {
            match round % 7 {
                0 => {
                    e.heartbeat(id);
                }
                1 => {
                    e.run_under(id);
                }
                3 if i % 11 == 0 => {
                    e.doubt(id);
                }
                _ => {}
            }
            if i % 3 == (round as usize) % 3 {
                let u = ((i * 7 + round as usize) % 13) as f32 / 12.0;
                let o = ((i * 5 + round as usize) % 11) as f32 / 10.0;
                let n = ((i * 3 + round as usize) % 7) as f32 / 6.0;
                let ev = ((i + round as usize) % 5) as f32 / 4.0;
                let class = match i % 100 {
                    0 => Importance::Kernel,
                    1..=20 => Importance::World,
                    21..=60 => Importance::Scenery,
                    _ => Importance::Maintenance,
                };
                let cost = Rails {
                    gpu_seconds: ((i % 5) as f32) * 0.3,
                    joules: ((i % 7) as f32) * 1.5,
                    cents: ((i % 3) as f32) * 0.1,
                };
                e.set_bid(
                    id,
                    Bid {
                        urgency: u,
                        opportunity: o,
                        novelty: n,
                        expected_value: ev,
                        class,
                        cost,
                    },
                );
            }
        }
        // A region dies mid-run: cascade, rate-limited across ticks.
        if round == 20 {
            e.kill(region_a);
        }
        // One scene is revived on parole and served.
        if round == 25 {
            e.revive(scenes[2]);
            e.tick();
            e.run_under(scenes[2]);
        }
        // Ten ticks per round, with honest frame feedback.
        for _ in 0..10 {
            e.tick();
            e.note_frame_time(if rng.next_bounded(1000) < 3 {
                9_000
            } else {
                400
            });
        }
    }
    e.chain_hash()
}

#[test]
fn golden_chain_hash_is_stable() {
    let hash = run_golden_scenario();
    let golden_path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/chain_hash.txt");
    let blessed = std::fs::read_to_string(golden_path)
        .expect("golden hash file must exist (BLESS=1 cargo test to create)");
    let blessed = blessed.trim();
    let actual = format!("{hash:016x}");
    if std::env::var("BLESS").is_ok() {
        if blessed != actual {
            std::fs::write(golden_path, format!("{actual}\n")).unwrap();
            panic!("golden hash blessed: {actual} (commit tests/golden/chain_hash.txt with your reason)");
        }
    } else {
        assert_eq!(blessed, actual,
            "journal chain hash changed — if intentional, BLESS=1 cargo test and commit with a reason");
    }
}

#[test]
fn two_runs_same_hash() {
    // Cheap independent double-run (guards against accidental global state).
    assert_eq!(run_golden_scenario(), run_golden_scenario());
}
