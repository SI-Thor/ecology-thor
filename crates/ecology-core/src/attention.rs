//! The attention economy — budgeted arbitration of candidate work.
//!
//! The law, ported unchanged from cognitive-ecology v0.2/v0.3:
//!
//! ```text
//! attention = urgency · justification · opportunity · novelty · expected_value
//! ```
//!
//! with two absolute constraints:
//!
//! * **The veto law** — a bid scoring `<= 0` is *never* admitted, regardless
//!   of importance class. A DEAD purpose has `justification == 0`, so dead
//!   lineages cannot buy attention. Zero-attention bids never transact.
//! * **Conservation** — the auction can only spend what the
//!   [`RailsBank`] holds. Every admission is paid for on every rail
//!   (`gpu_seconds`, `joules`, `cents`); infeasible bids are skipped, not
//!   discounted. Σ spend ≤ Σ balance, always.
//!
//! Selection is deterministic by construction: candidates are ordered by
//! (importance class desc, score desc, id asc) — a strict total order with no
//! ties possible — and admitted greedily while the rails can pay. `Kernel`
//! class work bypasses the `top_k` admission cap (the kernel's own vital
//! work is never crowd out by scenery) but never the veto law or the rails.
//!
//! The score is computed in a **canonical multiply order**
//! (`((((u·j)·o)·n)·ev)`, all operands clamped to [0,1]) so that the CUDA
//! kernel in `crates/ecology-cuda/kernels/auction.cu` — compiled with
//! `-fmad=false` — produces bit-identical scores. Do not "simplify" the
//! multiplication order on either side without updating the other.

use crate::ids::EntityId;
use crate::rails::{Rails, RailsBank};

/// Importance classes (IBM WLM's lesson: goal + importance beats raw
/// priority). Higher class wins arbitration first; within a class, score
/// decides. `Kernel` bypasses the admission cap.
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Importance {
    /// Housekeeping: journal compaction, telemetry, bookkeeping.
    Maintenance = 0,
    /// Optional detail work — props, decoration, nice-to-have polish.
    Scenery = 1,
    /// The world's substance — scenes, terrain, entities the simulation needs.
    World = 2,
    /// The kernel's own vital work. Never capped, never starved — but still
    /// subject to the veto law and the rails.
    Kernel = 3,
}

/// A work bid: "here is what I would do, what it is worth, and what it
/// costs." All value factors are clamped to [0,1] on entry.
#[derive(Clone, Copy, Debug)]
pub struct Bid {
    /// How soon? (deadlines, staleness, user-visible waiting)
    pub urgency: f32,
    /// How much does the world open the door *right now*? (dependencies
    /// satisfied, GPU idle, data resident)
    pub opportunity: f32,
    /// New information content — novelty guards against grinding known ore.
    pub novelty: f32,
    /// Expected value of the outcome (p50 quality estimate).
    pub expected_value: f32,
    /// Importance class for arbitration ordering.
    pub class: Importance,
    /// Cost the auction must pay if this bid wins.
    pub cost: Rails,
}

impl Default for Bid {
    fn default() -> Self {
        // A bid that vetoes itself: all value factors zero. The veto law
        // then guarantees it is never admitted — a safe default is a silent
        // bid, not a loud one.
        Bid {
            urgency: 0.0,
            opportunity: 0.0,
            novelty: 0.0,
            expected_value: 0.0,
            class: Importance::Scenery,
            cost: Rails::ZERO,
        }
    }
}

#[inline]
fn clamp01(x: f32) -> f32 {
    // NaN-safe: a nonsensical factor vetoes (0), never poisons. The
    // device mirror (kernels/ecology.cu, clamp01_eco) uses the identical
    // `!(x >= 0.0)` construction so NaN handling is bit-identical.
    if x.is_nan() || x < 0.0 {
        0.0
    } else if x > 1.0 {
        1.0
    } else {
        x
    }
}

/// The attention product, canonical form. `j` (justification) comes from the
/// WHY ledger's chain aggregate; everything else from the bid.
///
/// NaN inputs clamp to 0 (a nonsensical factor vetoes, it does not poison).
#[inline]
pub fn attention_score(u: f32, j: f32, o: f32, n: f32, ev: f32) -> f32 {
    let u = clamp01(u);
    let j = clamp01(j);
    let o = clamp01(o);
    let n = clamp01(n);
    let ev = clamp01(ev);
    // Canonical order — see the module docs. No reassociation.
    u * j * o * n * ev
}

/// Per-entity bid storage, SoA, preallocated, GPU-mirrorable.
pub struct AttentionTable {
    pub(crate) urgency: Vec<f32>,
    pub(crate) opportunity: Vec<f32>,
    pub(crate) novelty: Vec<f32>,
    pub(crate) expected_value: Vec<f32>,
    pub(crate) cost: Vec<Rails>,
    pub(crate) class: Vec<Importance>,
    /// Is there a live bid on this entity?
    pub(crate) present: Vec<bool>,
    /// Last computed score (candidate view — written by every auction).
    pub(crate) score: Vec<f32>,
    /// Admitted by the most recent auction?
    pub(crate) admitted: Vec<bool>,
}

impl AttentionTable {
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.max(1);
        AttentionTable {
            urgency: vec![0.0; cap],
            opportunity: vec![0.0; cap],
            novelty: vec![0.0; cap],
            expected_value: vec![0.0; cap],
            cost: vec![Rails::ZERO; cap],
            class: vec![Importance::Scenery; cap],
            present: vec![false; cap],
            score: vec![0.0; cap],
            admitted: vec![false; cap],
        }
    }

    /// Place or replace a bid. Returns false if `id` is out of range.
    pub fn set_bid(&mut self, id: EntityId, bid: Bid) -> bool {
        let i = id.index();
        if i >= self.present.len() {
            return false;
        }
        self.urgency[i] = bid.urgency;
        self.opportunity[i] = bid.opportunity;
        self.novelty[i] = bid.novelty;
        self.expected_value[i] = bid.expected_value;
        self.cost[i] = bid.cost;
        self.class[i] = bid.class;
        self.present[i] = true;
        true
    }

    /// Withdraw a bid (the work finished, or the caller changed its mind).
    pub fn clear_bid(&mut self, id: EntityId) {
        if let Some(p) = self.present.get_mut(id.index()) {
            *p = false;
        }
    }

    #[inline]
    pub fn is_admitted(&self, id: EntityId) -> bool {
        self.admitted.get(id.index()).copied().unwrap_or(false)
    }

    #[inline]
    pub fn score_of(&self, id: EntityId) -> f32 {
        self.score.get(id.index()).copied().unwrap_or(0.0)
    }

    /// Clear all admission flags (start of an auction).
    fn reset_admitted(&mut self) {
        for a in self.admitted.iter_mut() {
            *a = false;
        }
    }
}

/// Auction tuning.
#[derive(Clone, Copy, Debug)]
pub struct AuctionConfig {
    /// Maximum admissions per auction for non-Kernel classes.
    pub top_k: usize,
}

impl Default for AuctionConfig {
    fn default() -> Self {
        AuctionConfig { top_k: 32 }
    }
}

/// Result of one auction. `admitted` reuses an internal buffer — consume
/// before the next auction.
#[derive(Debug, Default)]
pub struct AuctionReport {
    pub admitted: Vec<EntityId>,
    pub vetoed: u32,
    pub skipped_infeasible: u32,
    pub spent: Rails,
    /// Rails never went negative and spend == Σ admitted costs.
    pub conservation_ok: bool,
}

/// Scratch ordering key: (class, score-bits, id). Scores are >= 0, so
/// `f32::to_bits` is a monotonic total order — no NaN, no partiality, and
/// identical ordering on every device.
pub type CandKey = (u8, u32, u32);

/// Run one auction. Deterministic; allocates nothing (scratch is reused).
///
/// `justification` and `states` come from the WHY ledger; `present` bids on
/// `Absent` entities are ignored (a parked draft asserts nothing — not even
/// a bid), and `Dead`/`Unknown` entities bid with their ledger-priced `j`
/// (0 for anything under a dead ancestor; discounted for questionable ones).
pub fn run_auction(
    table: &mut AttentionTable,
    justification: &[f32],
    states: &[crate::why::WhyState],
    bank: &mut RailsBank,
    cfg: &AuctionConfig,
    scratch: &mut Vec<CandKey>,
) -> AuctionReport {
    let mut report = AuctionReport::default();
    scratch.clear();
    table.reset_admitted();

    let n = table.present.len();
    for i in 0..n {
        table.score[i] = 0.0;
        if !table.present[i] {
            continue;
        }
        // Absent drafts cannot bid at all.
        if matches!(states.get(i), Some(crate::why::WhyState::Absent)) {
            continue;
        }
        let j = justification.get(i).copied().unwrap_or(0.0);
        let s = attention_score(
            table.urgency[i],
            j,
            table.opportunity[i],
            table.novelty[i],
            table.expected_value[i],
        );
        table.score[i] = s;
        // Score >= 0 always (clamped factors); <= 0 is the veto.
        scratch.push((table.class[i] as u8, s.to_bits(), i as u32));
    }

    // Deterministic strict total order:
    // class desc, score desc, id asc.
    scratch.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));

    let mut admitted_n: usize = 0;
    for &(class_bits, score_bits, idx) in scratch.iter() {
        let score = f32::from_bits(score_bits);
        let i = idx as usize;
        // THE VETO LAW: zero-attention bids never transact. Applies to the
        // Kernel class too — no law is waived for the powerful.
        if score <= 0.0 {
            report.vetoed += 1;
            continue;
        }
        let class = class_bits;
        // top_k caps non-Kernel admissions; Kernel bypasses the cap.
        if class != Importance::Kernel as u8 && admitted_n >= cfg.top_k {
            // Sorted by class then score — everything after is lower or
            // equal priority. Nothing more can be admitted.
            break;
        }
        let cost = table.cost[i];
        if !bank.can_afford(cost) {
            report.skipped_infeasible += 1;
            // Continue, not break: a cheaper, lower-priority bid may still
            // fit the rails.
            continue;
        }
        bank.spend(cost);
        table.admitted[i] = true;
        report.admitted.push(EntityId(idx));
        report.spent = report.spent + cost;
        admitted_n += 1;
    }

    // Conservation: balances never negative (structurally impossible given
    // can_afford, but the check is cheap and the invariant worth asserting).
    let b = bank.balance;
    report.conservation_ok = b.gpu_seconds >= 0.0 && b.joules >= 0.0 && b.cents >= 0.0;
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::why::WhyState;

    fn bank() -> RailsBank {
        RailsBank::new(
            Rails {
                gpu_seconds: 10.0,
                joules: 100.0,
                cents: 5.0,
            },
            Rails {
                gpu_seconds: 30.0,
                joules: 300.0,
                cents: 15.0,
            },
        )
    }

    #[test]
    fn veto_law_is_absolute() {
        let mut t = AttentionTable::new(8);
        let mut bank = bank();
        let id = EntityId(0);
        // All value factors zero → score 0 → vetoed even as Kernel class.
        t.set_bid(
            id,
            Bid {
                class: Importance::Kernel,
                ..Default::default()
            },
        );
        let just = [1.0f32];
        let states = [WhyState::Alive];
        let mut scratch = Vec::new();
        let r = run_auction(
            &mut t,
            &just,
            &states,
            &mut bank,
            &AuctionConfig::default(),
            &mut scratch,
        );
        assert_eq!(r.vetoed, 1);
        assert!(r.admitted.is_empty());
        assert!(!t.is_admitted(id));
    }

    #[test]
    fn kernel_bypasses_top_k_but_not_rails() {
        let mut t = AttentionTable::new(16);
        let mut bank = bank();
        bank.refill(); // one tick of income: 10 GPU-seconds
        let just = vec![1.0f32; 16];
        let states = vec![WhyState::Alive; 16];
        // 10 Kernel bids each costing 4 GPU-seconds (bank has 10/tick).
        for i in 0..10 {
            t.set_bid(
                EntityId(i),
                Bid {
                    urgency: 1.0,
                    opportunity: 1.0,
                    novelty: 1.0,
                    expected_value: 1.0,
                    class: Importance::Kernel,
                    cost: Rails {
                        gpu_seconds: 4.0,
                        joules: 1.0,
                        cents: 0.0,
                    },
                },
            );
        }
        let mut scratch = Vec::new();
        let r = run_auction(
            &mut t,
            &just,
            &states,
            &mut bank,
            &AuctionConfig { top_k: 2 },
            &mut scratch,
        );
        // top_k=2 would cap non-Kernel; Kernel bypasses, but rails allow
        // only 2 admissions of cost 4 from a balance of 10.
        assert_eq!(r.admitted.len(), 2);
        assert_eq!(r.skipped_infeasible, 8);
        assert!(r.conservation_ok);
    }

    #[test]
    fn class_then_score_then_id_ordering() {
        let mut t = AttentionTable::new(8);
        let mut bank = bank();
        let just = vec![1.0f32; 8];
        let states = vec![WhyState::Alive; 8];
        // id 1: World, score 0.5; id 0: Scenery, score 1.0; id 2: World 0.5.
        // World outranks Scenery despite lower score; ties break by id asc.
        t.set_bid(
            EntityId(0),
            Bid {
                urgency: 1.0,
                opportunity: 1.0,
                novelty: 1.0,
                expected_value: 1.0,
                class: Importance::Scenery,
                cost: Rails::ZERO,
            },
        );
        t.set_bid(
            EntityId(1),
            Bid {
                urgency: 0.5,
                opportunity: 1.0,
                novelty: 1.0,
                expected_value: 1.0,
                class: Importance::World,
                cost: Rails::ZERO,
            },
        );
        t.set_bid(
            EntityId(2),
            Bid {
                urgency: 0.5,
                opportunity: 1.0,
                novelty: 1.0,
                expected_value: 1.0,
                class: Importance::World,
                cost: Rails::ZERO,
            },
        );
        let mut scratch = Vec::new();
        let r = run_auction(
            &mut t,
            &just,
            &states,
            &mut bank,
            &AuctionConfig { top_k: 2 },
            &mut scratch,
        );
        assert_eq!(r.admitted, vec![EntityId(1), EntityId(2)]);
    }

    #[test]
    fn dead_ancestor_vetos_via_zero_justification() {
        let mut t = AttentionTable::new(4);
        let mut bank = bank();
        let states = vec![WhyState::Alive; 4];
        let just = vec![0.0f32, 1.0, 0.25, 0.0];
        t.set_bid(
            EntityId(0),
            Bid {
                urgency: 1.0,
                opportunity: 1.0,
                novelty: 1.0,
                expected_value: 1.0,
                ..Default::default()
            },
        );
        t.set_bid(
            EntityId(1),
            Bid {
                urgency: 1.0,
                opportunity: 1.0,
                novelty: 1.0,
                expected_value: 1.0,
                ..Default::default()
            },
        );
        let mut scratch = Vec::new();
        let r = run_auction(
            &mut t,
            &just,
            &states,
            &mut bank,
            &AuctionConfig::default(),
            &mut scratch,
        );
        assert_eq!(r.vetoed, 1, "j == 0 vetoes");
        assert_eq!(r.admitted.len(), 1);
    }

    #[test]
    fn absent_drafts_cannot_bid() {
        let mut t = AttentionTable::new(4);
        let mut bank = bank();
        let states = vec![WhyState::Absent];
        let just = vec![1.0f32];
        t.set_bid(
            EntityId(0),
            Bid {
                urgency: 1.0,
                opportunity: 1.0,
                novelty: 1.0,
                expected_value: 1.0,
                ..Default::default()
            },
        );
        let mut scratch = Vec::new();
        let r = run_auction(
            &mut t,
            &just,
            &states,
            &mut bank,
            &AuctionConfig::default(),
            &mut scratch,
        );
        assert_eq!(r.vetoed, 0);
        assert_eq!(r.admitted.len(), 0, "absent entities do not transact");
    }
}
