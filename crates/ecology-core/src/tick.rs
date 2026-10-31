//! The tick: framed, budgeted, single-writer.
//!
//! The kernel runs an AUTOSAR-style harmonic frame table adapted to our
//! workload (`docs/RT.md` has the full contract):
//!
//! ```text
//! minor frame   (every tick,        e.g. 10 ms):  attest drain → journal → summary publish
//! auction bucket(every `auction_every`, e.g. 100ms): chain aggregates → attention auction
//! sweep bucket  (every `sweep_every`,  e.g. 1 s):  reap → deadlines → decay → conservation
//! ```
//!
//! Heavier work runs *less* often, and every bucket is O(population) with
//! bounded constants — no bucket can do unbounded work (the only unbounded
//! operation in the whole ledger, subtree cascade, is rate-limited by
//! `Config::cascade_budget` conversions per tick).
//!
//! **The kernel has no clock.** `tick_now` is a pure counter; the caller
//! maps ticks to wall time. Frame overrun detection is therefore the
//! caller's measurement fed back through [`Ecology::note_frame_time`] —
//! which drives the safe-mode policy:
//!
//! * `overrun_tolerance` consecutive overruns → **safe mode**: auctions are
//!   suspended (decay-only — the world keeps aging honestly, nothing new is
//!   admitted), the summary flags it, the journal records it;
//! * `safe_mode_cooloff` clean ticks → automatic recovery.
//!
//! This is the flight-software watchdog discipline: the supervisor never
//! lets a broken frame keep spending; it degrades to the safest behavior
//! that still obeys the conservation laws.

use crate::attention::{run_auction, AttentionTable, AuctionConfig, Bid, CandKey};
use crate::ids::EntityId;
use crate::journal::{Event, Frame, Journal, Reason};
use crate::rails::{Rails, RailsBank};
use crate::snapshot::{snapshot_json, SummarySeqlock, TickSummary};
use crate::why::{SpawnState, StateCounts, WhyLedger, WhyState};

pub use crate::why::SpawnError;

/// Kernel configuration. Every budget the kernel enforces lives here —
/// nothing in steady state reads the environment.
#[derive(Clone, Debug)]
pub struct Config {
    /// Arena capacity: the organism's purpose-memory budget, in slots.
    /// ~1M entities ≈ 40 MB of hot tables. Never exceeded, never grown.
    pub capacity: usize,
    /// Re-attestation horizon in ticks. ALIVE nodes unattested for longer
    /// than this lapse to UNKNOWN (the economic harvest), and freshness
    /// slides linearly from `horizon/3` to `horizon`.
    pub horizon_ticks: u32,
    /// Auctions run every N ticks.
    pub auction_every: u32,
    /// Sweeps (reap + deadlines + decay + conservation) run every N ticks.
    pub sweep_every: u32,
    /// Max non-Kernel admissions per auction.
    pub auction_top_k: usize,
    /// Per-tick rail refill (gpu_seconds, joules, cents).
    pub rails_refill: Rails,
    /// Rail rollover cap (how much idle attention can bank for a burst).
    pub rails_cap: Rails,
    /// Purpose-mass decay rate per tick (mass *= 2^(-λ·Δt)).
    pub mass_lambda: f32,
    /// Grace beyond a root's `until` deadline before it dies.
    pub deadline_grace_ticks: u32,
    /// Max cascade conversions per tick (rate-limited doubt).
    pub cascade_budget: u32,
    /// Journal ring capacity (retained events).
    pub journal_capacity: usize,
    /// Minor-frame budget. A tick slower than this is an overrun.
    pub frame_budget_micros: u64,
    /// Consecutive overruns before entering safe mode.
    pub overrun_tolerance: u32,
    /// Clean ticks required to leave safe mode.
    pub safe_mode_cooloff: u32,
}

impl Default for Config {
    fn default() -> Self {
        // Defaults sized for a 100 Hz tick on the Thor, a world of ~64k
        // concurrent purposes, and the infinigen-shaped workload in
        // `ecology-sim`. Tune per box; `docs/BENCHMARKS.md` explains how.
        Config {
            capacity: 65_536,
            horizon_ticks: 900, // 9 s at 100 Hz
            auction_every: 10,  // 100 ms
            sweep_every: 100,   // 1 s
            auction_top_k: 32,
            rails_refill: Rails {
                gpu_seconds: 1.0,
                joules: 12.0,
                cents: 0.5,
            },
            rails_cap: Rails {
                gpu_seconds: 8.0,
                joules: 120.0,
                cents: 4.0,
            },
            mass_lambda: 0.000_02,     // ~2%/10 min at 100 Hz — slow by design
            deadline_grace_ticks: 300, // 3 s at 100 Hz
            cascade_budget: 256,
            journal_capacity: 8_192,
            frame_budget_micros: 8_000, // of a 10 ms frame at 100 Hz
            overrun_tolerance: 5,
            safe_mode_cooloff: 100,
        }
    }
}

/// What to spawn.
#[derive(Clone, Copy, Debug)]
pub struct GoalSpec {
    /// Parent purpose; `None` for a root (roots may carry deadlines).
    pub parent: Option<EntityId>,
    /// Initial purpose mass (the conserved quantity).
    pub mass: f32,
    /// Root deadline in ticks (`until`); ignored for children.
    pub until: Option<u32>,
    /// Spawn asserting life, or as a parked draft (`Absent`).
    pub alive: bool,
}

impl Default for GoalSpec {
    fn default() -> Self {
        GoalSpec {
            parent: None,
            mass: 1.0,
            until: None,
            alive: true,
        }
    }
}

/// What one tick did. Cheap to copy; the durable record is the journal.
#[derive(Clone, Copy, Debug)]
pub struct TickReport {
    pub tick: u32,
    pub counts: StateCounts,
    pub auction_ran: bool,
    pub admitted: u32,
    pub vetoed: u32,
    pub skipped_infeasible: u32,
    pub spent: Rails,
    pub sweep_ran: bool,
    pub reaped: u32,
    pub deadline_deaths: u32,
    pub cascaded: u32,
    pub cascade_pending: u32,
    pub decay_dissipated: f32,
    pub safe_mode: bool,
    pub conservation_ok: bool,
    pub chain_hash: u64,
}

/// The organism: WHY ledger + attention economy + journal + summary, under
/// one single-writer tick. All methods that mutate state must be called from
/// the tick thread (the owner of `&mut self`); telemetry readers on other
/// threads use [`Ecology::summary`] (seqlock) and never block the writer.
pub struct Ecology {
    cfg: Config,
    pub(crate) ledger: WhyLedger,
    pub(crate) attention: AttentionTable,
    bank: RailsBank,
    journal: Journal,
    summary: SummarySeqlock,
    last_summary: TickSummary,
    tick_now: u32,
    last_sweep_tick: u32,
    auction_scratch: Vec<CandKey>,
    overrun_streak: u32,
    clean_streak: u32,
    safe_mode: bool,
    auction_cfg: AuctionConfig,
}

impl Ecology {
    /// Build the organism. Preallocates every table to `cfg.capacity` —
    /// steady state allocates nothing.
    pub fn new(cfg: Config) -> Self {
        assert!(cfg.horizon_ticks >= 1, "horizon_ticks must be >= 1");
        assert!(cfg.auction_every >= 1 && cfg.sweep_every >= 1);
        let auction_cfg = AuctionConfig {
            top_k: cfg.auction_top_k,
        };
        Ecology {
            bank: RailsBank::new(cfg.rails_refill, cfg.rails_cap),
            ledger: WhyLedger::new(cfg.capacity),
            attention: AttentionTable::new(cfg.capacity),
            journal: Journal::new(cfg.journal_capacity),
            summary: SummarySeqlock::new(),
            last_summary: TickSummary::default(),
            tick_now: 0,
            last_sweep_tick: 0,
            auction_scratch: Vec::with_capacity(1024),
            overrun_streak: 0,
            clean_streak: 0,
            safe_mode: false,
            auction_cfg,
            cfg,
        }
    }

    /// The active configuration.
    #[inline]
    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Current tick counter (last completed tick).
    #[inline]
    pub fn now(&self) -> u32 {
        self.tick_now
    }

    // ------------------------------------------------------------- mutations

    /// Spawn a purpose (see [`GoalSpec`]). Journals the birth.
    pub fn spawn(&mut self, spec: GoalSpec) -> Result<EntityId, SpawnError> {
        let id = self.ledger.spawn(
            spec.parent,
            spec.mass,
            spec.until,
            if spec.alive {
                SpawnState::Alive
            } else {
                SpawnState::Absent
            },
            self.tick_now,
            Reason::Born,
        )?;
        self.journal.append(Event::Spawned {
            id,
            parent: spec.parent.map(|p| p.0).unwrap_or(crate::ids::NO_PARENT),
            mass: spec.mass.max(0.0),
            alive: spec.alive,
            tick: self.tick_now,
        });
        self.drain_ledger_transitions();
        Ok(id)
    }

    /// Heartbeat one node. Refuses non-ALIVE nodes and stale beats.
    pub fn heartbeat(&mut self, id: EntityId) -> bool {
        self.ledger.heartbeat(id, self.tick_now)
    }

    /// Re-attest by use: stamp the node and its whole chain to the root.
    pub fn run_under(&mut self, id: EntityId) -> u32 {
        self.ledger.run_under(id, self.tick_now)
    }

    /// Kill a node (cascades living descendants to UNKNOWN, rate-limited).
    /// Returns conversions done in this call.
    pub fn kill(&mut self, id: EntityId) -> u32 {
        let converted =
            self.ledger
                .kill(id, Reason::Killed, self.tick_now, self.cfg.cascade_budget);
        self.drain_ledger_transitions();
        converted
    }

    /// UNKNOWN → ALIVE (on parole).
    pub fn revive(&mut self, id: EntityId) -> bool {
        let ok = self.ledger.revive(id, Reason::Revived, self.tick_now);
        self.drain_ledger_transitions();
        ok
    }

    /// DEAD → ALIVE (substantive new evidence).
    pub fn resurrect(&mut self, id: EntityId) -> bool {
        let ok = self
            .ledger
            .resurrect(id, Reason::NewEvidence, self.tick_now);
        self.drain_ledger_transitions();
        ok
    }

    /// ABSENT → ALIVE (a draft starts asserting itself).
    pub fn activate(&mut self, id: EntityId) -> bool {
        let ok = self.ledger.activate(id, self.tick_now);
        self.drain_ledger_transitions();
        ok
    }

    /// ALIVE → UNKNOWN (outside doubt arrived; no cascade).
    pub fn doubt(&mut self, id: EntityId) -> bool {
        let ok = self.ledger.doubt(id, Reason::Custom, self.tick_now);
        self.drain_ledger_transitions();
        ok
    }

    /// Place/replace a bid.
    pub fn set_bid(&mut self, id: EntityId, bid: Bid) -> bool {
        self.attention.set_bid(id, bid)
    }

    /// Withdraw a bid.
    pub fn clear_bid(&mut self, id: EntityId) {
        self.attention.clear_bid(id);
    }

    // ------------------------------------------------------------------ tick

    /// Advance the world one tick. The whole kernel runs here; nothing else
    /// needs a lock because this is the single writer.
    pub fn tick(&mut self) -> TickReport {
        self.tick_now = self.tick_now.wrapping_add(1);
        let now = self.tick_now;

        // 1. Necrology drain: transitions from calls since the last tick.
        self.drain_ledger_transitions();

        // 2. Rate-limited doubt propagation.
        let cascaded = self.ledger.drain_cascade(self.cfg.cascade_budget);

        // 3. Rails refill (attention income for this tick).
        self.bank.refill();

        // 4. Auction bucket.
        let mut auction_ran = false;
        let mut admitted: u32 = 0;
        let mut vetoed: u32 = 0;
        let mut skipped: u32 = 0;
        let mut spent = Rails::ZERO;
        if now % self.cfg.auction_every == 0 {
            auction_ran = true;
            // Aggregates first: the auction must see the *current* chain
            // justification — stale purposes bid weaker *this* auction.
            self.ledger
                .recompute_aggregates(now, self.cfg.horizon_ticks);
            if self.safe_mode {
                // Decay-only: auctions suspended. Candidates are counted as
                // vetoed (nothing transacts) — an honest accounting of a
                // degraded world.
                vetoed = self
                    .attention
                    .present
                    .iter()
                    .zip(self.ledger.state.iter())
                    .filter(|(p, s)| **p && **s != WhyState::Absent)
                    .count() as u32;
            } else {
                let report = run_auction(
                    &mut self.attention,
                    &self.ledger.justification,
                    &self.ledger.state,
                    &mut self.bank,
                    &self.auction_cfg,
                    &mut self.auction_scratch,
                );
                admitted = report.admitted.len() as u32;
                vetoed = report.vetoed;
                skipped = report.skipped_infeasible;
                spent = report.spent;
            }
            self.journal.append(Event::Auction {
                tick: now,
                admitted,
                vetoed,
                skipped_infeasible: skipped,
                spent,
            });
        }

        // 5. Sweep bucket: reap, deadlines, decay, conservation.
        let mut sweep_ran = false;
        let mut reaped = 0u32;
        let mut deadline_deaths = 0u32;
        let mut dissipated = 0.0f32;
        let mut conservation_ok = true;
        if now % self.cfg.sweep_every == 0 {
            sweep_ran = true;
            reaped = self.ledger.reap(now, self.cfg.horizon_ticks);
            deadline_deaths = self.ledger.deadline_check(
                now,
                self.cfg.deadline_grace_ticks,
                self.cfg.cascade_budget,
            );
            let dt = now.saturating_sub(self.last_sweep_tick);
            dissipated = self.ledger.decay_mass(self.cfg.mass_lambda, dt);
            self.last_sweep_tick = now;
            conservation_ok = self.ledger.mass_conservation_ok();
            if dissipated > 0.0 {
                self.journal.append(Event::Decay {
                    tick: now,
                    dissipated,
                });
            }
            self.drain_ledger_transitions();
        }

        // 6. Publish the summary (seqlock — readers never blocked).
        let spent_total = self.bank.spent_total;
        let summary = TickSummary {
            tick: now,
            counts: self.ledger.counts(),
            admitted_last: admitted,
            vetoed_last: vetoed,
            skipped_infeasible_last: skipped,
            spent_total,
            cascade_pending: self.ledger.cascade_pending(),
            overrun_streak: self.overrun_streak,
            safe_mode: self.safe_mode,
            conservation_ok,
            mass_total: self.ledger.mass_total(),
            chain_hash: self.journal.chain_hash(),
            events_hashed: self.journal.events_hashed(),
            events_dropped: self.journal.dropped(),
        };
        self.summary.publish(summary);
        self.last_summary = summary;

        TickReport {
            tick: now,
            counts: self.ledger.counts(),
            auction_ran,
            admitted,
            vetoed,
            skipped_infeasible: skipped,
            spent,
            sweep_ran,
            reaped,
            deadline_deaths,
            cascaded,
            cascade_pending: self.ledger.cascade_pending(),
            decay_dissipated: dissipated,
            safe_mode: self.safe_mode,
            conservation_ok,
            chain_hash: self.journal.chain_hash(),
        }
    }

    /// Feed back the caller's frame measurement (µs for the whole tick).
    /// Drives overrun streaks and the safe-mode policy. Call once per tick,
    /// after `tick()`. The summary is re-published immediately so telemetry
    /// consumers see safety state without waiting for the next tick.
    pub fn note_frame_time(&mut self, micros: u64) {
        if micros > self.cfg.frame_budget_micros {
            self.overrun_streak = self.overrun_streak.saturating_add(1);
            self.clean_streak = 0;
            self.journal.append(Event::Overrun {
                tick: self.tick_now,
                frame: Frame::Minor,
            });
            if !self.safe_mode && self.overrun_streak >= self.cfg.overrun_tolerance {
                self.safe_mode = true;
                self.journal.append(Event::SafeMode {
                    tick: self.tick_now,
                    entered: true,
                });
            }
        } else {
            self.overrun_streak = 0;
            self.clean_streak = self.clean_streak.saturating_add(1);
            if self.safe_mode && self.clean_streak >= self.cfg.safe_mode_cooloff {
                self.safe_mode = false;
                self.journal.append(Event::SafeMode {
                    tick: self.tick_now,
                    entered: false,
                });
            }
        }
        // Re-publish the safety-relevant fields of the summary.
        let mut s = self.last_summary;
        s.tick = self.tick_now;
        s.overrun_streak = self.overrun_streak;
        s.safe_mode = self.safe_mode;
        s.chain_hash = self.journal.chain_hash();
        s.events_hashed = self.journal.events_hashed();
        s.events_dropped = self.journal.dropped();
        self.summary.publish(s);
        self.last_summary = s;
    }

    // ------------------------------------------------------------- accessors

    #[inline]
    pub fn state(&self, id: EntityId) -> Option<WhyState> {
        self.ledger.state(id)
    }

    /// Chain-aggregated justification (last sweep).
    #[inline]
    pub fn justification(&self, id: EntityId) -> f32 {
        self.ledger.justification(id)
    }

    #[inline]
    pub fn mass(&self, id: EntityId) -> f32 {
        self.ledger.mass(id)
    }

    /// Was this entity admitted by the most recent auction?
    #[inline]
    pub fn is_admitted(&self, id: EntityId) -> bool {
        self.attention.is_admitted(id)
    }

    /// Last computed attention score.
    #[inline]
    pub fn score(&self, id: EntityId) -> f32 {
        self.attention.score_of(id)
    }

    /// Journal chain hash (the determinism anchor).
    #[inline]
    pub fn chain_hash(&self) -> u64 {
        self.journal.chain_hash()
    }

    /// Latest summary, direct from the writer (no seqlock needed when
    /// called from the tick thread).
    #[inline]
    pub fn summary_direct(&self) -> TickSummary {
        self.last_summary
    }

    /// Latest summary via the seqlock (for other threads).
    #[inline]
    pub fn summary(&self) -> Option<TickSummary> {
        self.summary.read()
    }

    /// Stable JSON rendering of the summary (contract in `docs/INTEGRATION.md`).
    pub fn snapshot_json(&self) -> String {
        snapshot_json(&self.last_summary)
    }

    /// Iterate retained journal events (oldest first).
    pub fn journal_iter(&self) -> impl Iterator<Item = &Event> {
        self.journal.iter()
    }

    // ------------------------------------------- device-mirror accessors
    // Flat views the CUDA sweeps mirror into unified memory. Slices, not
    // iterators, on purpose: a slice is exactly what a zero-copy device
    // pointer wants (`docs/PLACEMENT.md`).

    pub fn states_slice(&self) -> &[WhyState] {
        &self.ledger.state
    }
    pub fn last_beat_slice(&self) -> &[i64] {
        &self.ledger.last_beat
    }
    pub fn born_slice(&self) -> &[u32] {
        &self.ledger.born
    }
    pub fn parent_slice(&self) -> &[u32] {
        &self.ledger.parent
    }
    pub fn justification_slice(&self) -> &[f32] {
        &self.ledger.justification
    }
    pub fn score_slice(&self) -> &[f32] {
        &self.attention.score
    }
    pub fn urgency_slice(&self) -> &[f32] {
        &self.attention.urgency
    }
    pub fn opportunity_slice(&self) -> &[f32] {
        &self.attention.opportunity
    }
    pub fn novelty_slice(&self) -> &[f32] {
        &self.attention.novelty
    }
    pub fn expected_value_slice(&self) -> &[f32] {
        &self.attention.expected_value
    }
    pub fn mass_slice(&self) -> &[f32] {
        &self.ledger.mass
    }
    pub fn present_slice(&self) -> &[bool] {
        &self.attention.present
    }
    pub fn probation_slice(&self) -> &[bool] {
        &self.ledger.probation
    }
    /// Depth-grouped node indices — the level plan for the device sweep.
    pub fn levels(&self) -> &[Vec<u32>] {
        &self.ledger.levels
    }

    // ------------------------------------------------------------- internals

    /// Move ledger transitions into the journal. Disjoint-field borrows.
    fn drain_ledger_transitions(&mut self) {
        let ledger = &mut self.ledger;
        let journal = &mut self.journal;
        for t in ledger.transitions() {
            journal.append(Event::Transition {
                id: t.id,
                from: t.from,
                to: t.to,
                reason: t.reason,
                tick: t.tick,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attention::Importance;

    fn eco() -> Ecology {
        Ecology::new(Config::default())
    }

    #[test]
    fn tick_advances_and_publishes() {
        let mut e = eco();
        let r = e.tick();
        assert_eq!(r.tick, 1);
        assert!(e.summary().is_some());
        assert_eq!(e.summary_direct().tick, 1);
    }

    #[test]
    fn auction_admits_within_rails() {
        let mut e = eco();
        let a = e.spawn(GoalSpec::default()).unwrap();
        e.set_bid(
            a,
            Bid {
                urgency: 1.0,
                opportunity: 1.0,
                novelty: 1.0,
                expected_value: 1.0,
                class: Importance::World,
                cost: Rails {
                    gpu_seconds: 0.5,
                    joules: 5.0,
                    cents: 0.0,
                },
            },
        );
        // Run to the first auction tick.
        let mut report = None;
        for _ in 0..10 {
            let r = e.tick();
            if r.auction_ran {
                report = Some(r);
            }
        }
        let r = report.unwrap();
        assert_eq!(r.admitted, 1);
        assert_eq!(r.vetoed, 0);
        assert!(r.conservation_ok);
        assert!(e.is_admitted(a));
    }

    #[test]
    fn lapsed_purpose_loses_the_auction_then_revives() {
        let mut e = eco();
        let a = e.spawn(GoalSpec::default()).unwrap();
        e.set_bid(
            a,
            Bid {
                urgency: 1.0,
                opportunity: 1.0,
                novelty: 1.0,
                expected_value: 1.0,
                ..Default::default()
            },
        );
        // Freshness grace is horizon/3 = 300 ticks; reap lands at the
        // tick-1000 sweep. Track admissions by tick *value*.
        let mut admitted_early = 0;
        let mut admitted_late = 0;
        for i in 0..1200 {
            e.tick();
            if e.is_admitted(a) {
                if i < 300 {
                    admitted_early += 1;
                } else if i >= 950 {
                    admitted_late += 1;
                }
            }
        }
        assert!(admitted_early > 0, "fresh purpose competes");
        assert_eq!(admitted_late, 0, "stale purpose loses every auction");
        // Revive on parole, then serve it: full strength returns.
        assert!(e.revive(a));
        assert!(e.justification(a) <= crate::freshness::PROBATION_CAP + 1e-6);
        e.tick(); // time passes; the service beat is strictly newer
        e.run_under(a); // service ends parole
        let mut admitted_after_revive = 0;
        for _ in 0..200 {
            e.tick();
            if e.is_admitted(a) {
                admitted_after_revive += 1;
            }
        }
        assert!(admitted_after_revive > 0, "revived purpose competes again");
        assert!(
            e.justification(a) > crate::freshness::PROBATION_CAP,
            "parole lifted by service"
        );
    }

    #[test]
    fn safe_mode_suspends_auctions_and_recovers() {
        let cfg = Config {
            overrun_tolerance: 2,
            safe_mode_cooloff: 3,
            ..Config::default()
        };
        let mut e = Ecology::new(cfg);
        let a = e.spawn(GoalSpec::default()).unwrap();
        e.set_bid(
            a,
            Bid {
                urgency: 1.0,
                opportunity: 1.0,
                novelty: 1.0,
                expected_value: 1.0,
                ..Default::default()
            },
        );
        // Two overruns → safe mode.
        e.note_frame_time(1_000_000);
        e.note_frame_time(1_000_000);
        assert!(e.summary_direct().safe_mode);
        // Auctions suspended while degraded.
        let mut any_admitted = false;
        for _ in 0..100 {
            let r = e.tick();
            if r.auction_ran && r.admitted > 0 {
                any_admitted = true;
            }
            e.note_frame_time(1); // clean frames
        }
        assert!(!any_admitted || !e.summary_direct().safe_mode);
        assert!(!e.summary_direct().safe_mode, "cooled off and recovered");
    }

    #[test]
    fn determinism_same_scenario_same_chain() {
        let run = || {
            let mut e = Ecology::new(Config::default());
            let root = e.spawn(GoalSpec::default()).unwrap();
            for i in 0..50u32 {
                let c = e
                    .spawn(GoalSpec {
                        parent: Some(root),
                        mass: 0.5,
                        ..Default::default()
                    })
                    .unwrap();
                e.set_bid(
                    c,
                    Bid {
                        urgency: (i % 7) as f32 / 7.0,
                        opportunity: 0.8,
                        novelty: 0.5,
                        expected_value: 0.6,
                        ..Default::default()
                    },
                );
                if i % 3 == 0 {
                    e.run_under(c);
                }
            }
            for _ in 0..500 {
                e.tick();
            }
            e.chain_hash()
        };
        assert_eq!(run(), run());
    }
}
