//! # ecology-sim — the world-creation workload model & benchmark harness.
//!
//! This crate drives the kernel with a workload shaped like the real job on
//! the box: **procedural world creation** (infinigen-style) under the
//! ecology's arbitration. It is both:
//!
//! * a **stress/bench harness** (`--profile stress`): fill the arena, make
//!   everyone bid, keep everyone fresh — measures raw kernel throughput;
//! * a **world-gen scenario** (`--profile world-gen`): region goals spawn
//!   scene jobs; jobs render (service → `run_under`, GPU-second costs),
//!   stall silently (the EXACT-Boolean hang class from infinigen —
//!   heartbeats stop, freshness slides, the job is *harvested* to UNKNOWN
//!   by the re-attestation economics, never deleted), get checkpointed and
//!   revived (the SIGUSR1 semantics), and region roots sometimes die
//!   (cascading doubt, rate-limited).
//!
//! Everything is seeded (`--seed`): same seed ⇒ same journal chain hash
//! (`--golden`), on this x86_64 CI box and on the Thor alike.
//!
//! Timing methodology (mirrors `docs/BENCHMARKS.md`):
//! * each tick is timed with `Instant` around `Ecology::tick`;
//! * `note_frame_time` feeds the measurement back for safe-mode policy;
//! * the report splits plain ticks / auction ticks / sweep ticks — the
//!   three frame classes have different WCETs by design.

use std::time::Instant;

use ecology_core::attention::{Bid, Importance};
use ecology_core::ids::EntityId;
use ecology_core::rails::Rails;
use ecology_core::tick::{Config, Ecology, GoalSpec};
use ecology_core::why::WhyState;

// --------------------------------------------------------------------- CLI

struct Args {
    profile: Profile,
    capacity: usize,
    ticks: u32,
    seed: u64,
    regions: usize,
    scenes_per_region: usize,
    stall_rate: f64,
    recover_rate: f64,
    revive_rate: f64,
    arrival_per_tick: f64,
    golden: bool,
    report: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Profile {
    WorldGen,
    Stress,
}

fn parse_args() -> Args {
    let mut a = Args {
        profile: Profile::WorldGen,
        capacity: 50_000,
        ticks: 60_000, // 10 min at 100 Hz
        seed: 42,
        regions: 8,
        scenes_per_region: 64,
        stall_rate: 0.0002,
        recover_rate: 0.001,
        revive_rate: 0.01,
        arrival_per_tick: 0.5,
        golden: false,
        report: None,
    };
    let argv: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < argv.len() {
        let arg = argv[i].as_str();
        let take = |i: &mut usize| -> String {
            *i += 1;
            argv.get(*i).cloned().unwrap_or_default()
        };
        match arg {
            "--profile" => {
                a.profile = match take(&mut i).as_str() {
                    "stress" => Profile::Stress,
                    _ => Profile::WorldGen,
                };
            }
            "--capacity" => a.capacity = take(&mut i).parse().unwrap_or(a.capacity),
            "--ticks" => a.ticks = take(&mut i).parse().unwrap_or(a.ticks),
            "--seed" => a.seed = take(&mut i).parse().unwrap_or(a.seed),
            "--regions" => a.regions = take(&mut i).parse().unwrap_or(a.regions),
            "--scenes-per-region" => {
                a.scenes_per_region = take(&mut i).parse().unwrap_or(a.scenes_per_region)
            }
            "--stall-rate" => a.stall_rate = take(&mut i).parse().unwrap_or(a.stall_rate),
            "--recover-rate" => a.recover_rate = take(&mut i).parse().unwrap_or(a.recover_rate),
            "--revive-rate" => a.revive_rate = take(&mut i).parse().unwrap_or(a.revive_rate),
            "--arrival" => a.arrival_per_tick = take(&mut i).parse().unwrap_or(a.arrival_per_tick),
            "--golden" => a.golden = true,
            "--report" => a.report = Some(take(&mut i)),
            _ => {
                eprintln!("unknown arg: {arg}");
                eprintln!(
                    "usage: ecology-sim [--profile world-gen|stress] [--capacity N] [--ticks N] \
                     [--seed N] [--regions N] [--scenes-per-region N] [--stall-rate F] \
                     [--revive-rate F] [--arrival F] [--golden] [--report PATH]"
                );
                std::process::exit(2);
            }
        }
        i += 1;
    }
    a
}

// ------------------------------------------------------------------ timings

#[derive(Default)]
struct TimingStats {
    samples: Vec<u64>, // ns per tick, indexed class-wise below
    auction_ns: Vec<u64>,
    sweep_ns: Vec<u64>,
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

// ------------------------------------------------------------------- model

/// One scene-generation job in the world-creation workload.
struct SceneJob {
    id: EntityId,
    /// Ticks until this scene's render deadline (drives urgency).
    deadline_in: u32,
    /// Estimated render cost (GPU-seconds, p50-shaped from the spike data:
    /// Hello Room render ≈ 1307 s, Hello World ≈ 48 s — we use small units).
    gpu_seconds: f32,
    /// Hung (the EXACT-Boolean class): no service until it recovers or is
    /// harvested to UNKNOWN by the re-attestation economics.
    stalled: bool,
}

struct WorldGenSim {
    eco: Ecology,
    rng: ecology_core::rng::Pcg32,
    args: Args,
    regions: Vec<EntityId>,
    jobs: Vec<SceneJob>,
    stats: SimStats,
}

#[derive(Default)]
struct SimStats {
    renders_started: u64,
    scenes_stalled: u64,
    scenes_revived: u64,
    regions_died: u64,
    spawns_rejected: u64,
}

impl WorldGenSim {
    fn new(args: Args) -> Self {
        let cfg = Config {
            capacity: args.capacity,
            // World-gen pacing at 100 Hz: scenes must re-attest every ~9 s
            // of kernel time; auctions every 100 ms; sweeps every 1 s.
            ..Config::default()
        };
        WorldGenSim {
            eco: Ecology::new(cfg),
            rng: ecology_core::rng::Pcg32::new(args.seed),
            args,
            regions: Vec::new(),
            jobs: Vec::new(),
            stats: SimStats::default(),
        }
    }

    fn bid_for(&mut self, job: &SceneJob) -> Bid {
        // Deadline pressure: 1.0 at spawn, flat as the deadline nears.
        let urgency = 1.0 - (job.deadline_in as f32 / 600.0).clamp(0.0, 1.0);
        // GPU opportunity: high while the population of live renders is low.
        let live_renders = self.jobs.len() as f32;
        let opportunity = (1.0 - live_renders / 64.0).clamp(0.1, 1.0);
        let novelty = 0.3 + 0.7 * self.rng.next_f32();
        let ev = 0.5 + 0.5 * self.rng.next_f32();
        Bid {
            urgency,
            opportunity,
            novelty,
            expected_value: ev,
            class: Importance::World,
            cost: Rails {
                gpu_seconds: job.gpu_seconds,
                joules: job.gpu_seconds * 25.0, // ~25 W of GPU while rendering
                cents: 0.0,                     // local work costs no money
            },
        }
    }

    fn spawn_region(&mut self) {
        let spec = GoalSpec {
            parent: None,
            mass: 10.0,
            // Regions carry deadlines: a region must finish its scenes or
            // die (and its doubt cascades, rate-limited).
            until: Some(self.args.ticks),
            alive: true,
        };
        match self.eco.spawn(spec) {
            Ok(id) => self.regions.push(id),
            Err(_) => self.stats.spawns_rejected += 1,
        }
    }

    fn spawn_scene(&mut self, region: EntityId) {
        let spec = GoalSpec {
            parent: Some(region),
            mass: 1.0,
            until: None,
            alive: true,
        };
        if let Ok(id) = self.eco.spawn(spec) {
            let gpu_seconds = 0.5 + 3.0 * self.rng.next_f32();
            let job = SceneJob {
                id,
                deadline_in: 400 + self.rng.next_bounded(400),
                gpu_seconds,
                stalled: false,
            };
            let bid = self.bid_for(&job);
            self.eco.set_bid(id, bid);
            self.jobs.push(job);
        } else {
            self.stats.spawns_rejected += 1;
        }
    }

    // The state-machine match below is intentionally explicit (one arm per
    // WhyState, each with its own workload semantics); collapsing the
    // guard into the arm pattern would trade clarity for brevity.
    #[allow(clippy::collapsible_if, clippy::collapsible_match)]
    fn run(&mut self) -> (TimingStats, SimStats, String) {
        // Copy the scalar knobs out so &mut self calls below don't fight a
        // field borrow.
        let (
            regions,
            scenes_per_region,
            ticks,
            stall_rate,
            recover_rate,
            revive_rate,
            arrival_per_tick,
        ) = (
            self.args.regions,
            self.args.scenes_per_region,
            self.args.ticks,
            self.args.stall_rate,
            self.args.recover_rate,
            self.args.revive_rate,
            self.args.arrival_per_tick,
        );

        // Populate: regions then their first scenes.
        for _ in 0..regions {
            self.spawn_region();
        }
        for r in 0..regions {
            let region = self.regions[r];
            for _ in 0..scenes_per_region {
                self.spawn_scene(region);
            }
        }

        let mut timing = TimingStats::default();
        let mut pending_arrivals = 0.0f64;

        for tick in 1..=ticks {
            // --- workload events (before the kernel tick) ----------------
            // 1. Live jobs render: service their purpose chain (leaf→root
            //    re-attestation). A rendering job is a *running* job.
            // 2. Stalls: with probability stall_rate a job hangs silently
            //    (the EXACT-Boolean class) — no heartbeat, no bid change.
            // 3. Checkpoint/revive: a supervisor revives lapsed jobs
            //    (SIGUSR1 + checkpoint) with probability revive_rate.
            for ji in 0..self.jobs.len() {
                let (jid, deadline_in, gpu_seconds, stalled) = {
                    let j = &self.jobs[ji];
                    (j.id, j.deadline_in, j.gpu_seconds, j.stalled)
                };
                let probe = SceneJob {
                    id: jid,
                    deadline_in,
                    gpu_seconds,
                    stalled,
                };
                match self.eco.state(jid) {
                    Some(WhyState::Alive) if !stalled => {
                        // A running job services its purpose chain.
                        if self.rng.next_f32() < 0.05 {
                            self.eco.run_under(jid);
                        }
                        // Rare silent hang (the EXACT-Boolean class).
                        if self.rng.next_f64() < stall_rate {
                            self.jobs[ji].stalled = true;
                            self.eco.clear_bid(jid);
                            self.stats.scenes_stalled += 1;
                        }
                    }
                    Some(WhyState::Alive) => {
                        // Hung: no service — freshness slides; the sweep
                        // will harvest it to UNKNOWN at the horizon.
                        // Sometimes the job recovers on its own
                        // (checkpoint semantics: state saved, not lost).
                        if self.rng.next_f64() < recover_rate {
                            self.jobs[ji].stalled = false;
                            self.eco.run_under(jid);
                            let bid = self.bid_for(&probe);
                            self.eco.set_bid(jid, bid);
                            self.stats.scenes_revived += 1;
                        }
                    }
                    Some(WhyState::Unknown) => {
                        // Harvested. A supervisor may re-justify it
                        // (revive, on parole) and put it back to work.
                        if self.rng.next_f64() < revive_rate && self.eco.revive(jid) {
                            self.eco.tick(); // time passes (checkpoint)
                            self.eco.run_under(jid);
                            self.jobs[ji].stalled = false;
                            let bid = self.bid_for(&probe);
                            self.eco.set_bid(jid, bid);
                            self.stats.scenes_revived += 1;
                        }
                    }
                    _ => {} // Dead (region died): leave it.
                }
            }

            // New scenes arrive under (still-alive) regions.
            pending_arrivals += arrival_per_tick;
            while pending_arrivals >= 1.0 {
                pending_arrivals -= 1.0;
                let alive_regions: Vec<EntityId> = self
                    .regions
                    .iter()
                    .copied()
                    .filter(|r| self.eco.state(*r) == Some(WhyState::Alive))
                    .collect();
                if let Some(&r) = alive_regions.first() {
                    self.spawn_scene(r);
                }
            }

            // Region root death (rare): cascade, rate-limited.
            if self.rng.next_f64() < 0.00005 {
                if let Some(&r) = self
                    .regions
                    .iter()
                    .find(|r| self.eco.state(**r) == Some(WhyState::Alive))
                {
                    self.eco.kill(r);
                    self.stats.regions_died += 1;
                }
            }

            // --- the kernel tick ----------------------------------------
            let t0 = Instant::now();
            let report = self.eco.tick();
            let dt_ns = t0.elapsed().as_nanos() as u64;
            self.eco.note_frame_time(dt_ns / 1000);

            if report.sweep_ran {
                // Sweep ticks co-occur with auction ticks (harmonic frames);
                // classify them as the heavier class.
                timing.sweep_ns.push(dt_ns);
            } else if report.auction_ran {
                timing.auction_ns.push(dt_ns);
            } else {
                timing.samples.push(dt_ns);
            }
            if report.auction_ran && report.admitted > 0 {
                self.stats.renders_started += report.admitted as u64;
            }

            // Rebid live jobs as their deadlines approach.
            if tick % 50 == 0 {
                for ji in 0..self.jobs.len() {
                    let (id, deadline_in, gpu_seconds) = {
                        let j = &self.jobs[ji];
                        (j.id, j.deadline_in, j.gpu_seconds)
                    };
                    if self.eco.state(id) == Some(WhyState::Alive) && !self.jobs[ji].stalled {
                        let job = SceneJob {
                            id,
                            deadline_in: deadline_in.saturating_sub(50),
                            gpu_seconds,
                            stalled: false,
                        };
                        let bid = self.bid_for(&job);
                        self.eco.set_bid(id, bid);
                    }
                }
            }
        }

        // Sweep out finished/dead jobs from the bookkeeping list.
        let stats = std::mem::take(&mut self.stats);
        let snapshot = self.eco.snapshot_json();
        (timing, stats, snapshot)
    }
}

// -------------------------------------------------------------------- main

fn main() {
    let args = parse_args();
    println!(
        "ecology-sim v{} — profile {:?}, capacity {}, ticks {}, seed {}",
        ecology_core::VERSION,
        args.profile,
        args.capacity,
        args.ticks,
        args.seed
    );

    match args.profile {
        Profile::Stress => run_stress(&args),
        Profile::WorldGen => {
            let mut sim = WorldGenSim::new(args);
            let (timing, stats, snapshot) = sim.run();
            print_report(&timing, &stats, &snapshot);
            if sim.args.golden {
                println!("chain_hash: {:016x}", sim.eco.chain_hash());
            }
            if let Some(path) = &sim.args.report {
                let mut report = format!(
                    "{{\"profile\":\"world-gen\",\"chain_hash\":\"{:016x}\",",
                    sim.eco.chain_hash()
                );
                report.push_str(&timing_json(&timing));
                report.push('}');
                std::fs::write(path, report).expect("write report");
                println!("report written: {path}");
            }
        }
    }
}

/// Stress profile: everyone bids, everyone stays fresh, nobody dies —
/// measures the pure arbitration throughput of the kernel.
fn run_stress(args: &Args) {
    let cfg = Config {
        capacity: args.capacity,
        ..Config::default()
    };
    let mut eco = Ecology::new(cfg);
    let mut rng = ecology_core::rng::Pcg32::new(args.seed);

    // Fill the arena: one root, N children, all bidding.
    let root = eco.spawn(GoalSpec::default()).expect("root spawn");
    let n = args.capacity.saturating_sub(1);
    let mut ids = Vec::with_capacity(n);
    for i in 0..n {
        let id = eco
            .spawn(GoalSpec {
                parent: Some(root),
                mass: 1.0,
                ..Default::default()
            })
            .expect("arena should fit the stress population");
        ids.push(id);
        eco.set_bid(
            id,
            Bid {
                urgency: ((i % 13) as f32) / 13.0 + 0.05,
                opportunity: ((i % 7) as f32) / 7.0 + 0.05,
                novelty: ((i % 11) as f32) / 11.0 + 0.05,
                expected_value: ((i % 5) as f32) / 5.0 + 0.05,
                class: if i % 1000 == 0 {
                    Importance::Kernel
                } else {
                    Importance::World
                },
                cost: Rails {
                    gpu_seconds: 0.1,
                    joules: 2.0,
                    cents: 0.0,
                },
            },
        );
    }
    println!("population: {}", ids.len() + 1);

    let mut timing = TimingStats::default();
    for _tick in 1..=args.ticks {
        // Keep ~10% of the population fresh each tick (service traffic).
        for _ in 0..(n / 10).max(1) {
            let idx = rng.next_bounded(n as u32) as usize;
            eco.run_under(ids[idx]);
        }
        let t0 = Instant::now();
        let report = eco.tick();
        let dt_ns = t0.elapsed().as_nanos() as u64;
        eco.note_frame_time(dt_ns / 1000);
        if report.sweep_ran {
            timing.sweep_ns.push(dt_ns);
        } else if report.auction_ran {
            timing.auction_ns.push(dt_ns);
        } else {
            timing.samples.push(dt_ns);
        }
    }

    let stats = SimStats {
        renders_started: 0,
        ..Default::default()
    };
    print_report(&timing, &stats, &eco.snapshot_json());

    if args.golden {
        println!("chain_hash: {:016x}", eco.chain_hash());
    }
    if let Some(path) = &args.report {
        let mut report = format!(
            "{{\"profile\":\"stress\",\"chain_hash\":\"{:016x}\",",
            eco.chain_hash()
        );
        report.push_str(&timing_json(&timing));
        report.push('}');
        std::fs::write(path, report).expect("write report");
        println!("report written: {path}");
    }
}

fn timing_json(t: &TimingStats) -> String {
    let mut plain = t.samples.clone();
    plain.sort_unstable();
    let mut auction = t.auction_ns.clone();
    auction.sort_unstable();
    let mut sweep = t.sweep_ns.clone();
    sweep.sort_unstable();
    format!(
        "\"plain_ns\":{{\"p50\":{},\"p95\":{},\"max\":{}}},\
         \"auction_ns\":{{\"p50\":{},\"p95\":{},\"max\":{}}},\
         \"sweep_ns\":{{\"p50\":{},\"p95\":{},\"max\":{}}}",
        percentile(&plain, 0.5),
        percentile(&plain, 0.95),
        plain.last().copied().unwrap_or(0),
        percentile(&auction, 0.5),
        percentile(&auction, 0.95),
        auction.last().copied().unwrap_or(0),
        percentile(&sweep, 0.5),
        percentile(&sweep, 0.95),
        sweep.last().copied().unwrap_or(0),
    )
}

fn print_report(timing: &TimingStats, stats: &SimStats, snapshot: &str) {
    println!("\n--- kernel timings (ns per tick) ---");
    for (name, v) in [
        ("plain ", &timing.samples),
        ("auction", &timing.auction_ns),
        ("sweep ", &timing.sweep_ns),
    ] {
        let mut sorted = v.clone();
        sorted.sort_unstable();
        println!(
            "{name}: p50 {:>9}  p95 {:>9}  max {:>10}  (n={})",
            percentile(&sorted, 0.5),
            percentile(&sorted, 0.95),
            sorted.last().copied().unwrap_or(0),
            sorted.len()
        );
    }
    println!("\n--- workload stats ---");
    println!("renders started : {}", stats.renders_started);
    println!("scenes stalled  : {}", stats.scenes_stalled);
    println!("scenes revived  : {}", stats.scenes_revived);
    println!("regions died    : {}", stats.regions_died);
    println!("\n--- final snapshot ---");
    println!("{snapshot}");
}
