//! Budget rails — the currencies attention spends.
//!
//! A bid does not compete for an abstract "attention score" alone; it
//! declares a cost in **real currencies of the Thor box**, and the auction
//! only admits work the rails can pay for:
//!
//! * `gpu_seconds` — GPU time (Cycles renders, terrain kernels, our sweeps).
//! * `joules` — energy. On a box with a 120 W default envelope and a
//!   24/7 duty cycle, joules are the honest long-run currency.
//! * `cents` — money, for work delegated to cloud helpers (the builder-ide
//!   world already prices jobs in GPU-minutes/cents/joules — this rail set
//!   is deliberately compatible with that ledger, `docs/INTEGRATION.md`).
//!
//! This is the "currency exchange" builder-ide's fit study
//! (`docs/research/cogeco.md` §"what stays open") asked for: an attention
//! economy that bids for *actual Thor GPU-minutes* finally has a real
//! currency to bid in.
//!
//! Money is `f32` here on purpose: the kernel's rails are **admission-control
//! envelopes**, not accounting truth. The authoritative ledger (SQLite in
//! builder-ide, or the journal replay) does the accounting; if a float rail
//! is off by an ulp, the worst case is one job admitted or deferred one
//! auction early — never a corrupted balance.

use crate::ids::EntityId;

/// A vector of rail costs / budgets / spends. Add, subtract and compare are
/// componentwise.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rails {
    /// GPU-seconds this work is expected to consume (p50 estimate).
    pub gpu_seconds: f32,
    /// Joules (p50 estimate).
    pub joules: f32,
    /// Cents (p50 estimate) — nonzero for cloud-delegated work.
    pub cents: f32,
}

impl Rails {
    /// Zero on every rail.
    pub const ZERO: Rails = Rails {
        gpu_seconds: 0.0,
        joules: 0.0,
        cents: 0.0,
    };

    /// Componentwise `min(a, b)` — used to clamp rollover.
    #[inline]
    pub fn min(a: Rails, b: Rails) -> Rails {
        Rails {
            gpu_seconds: a.gpu_seconds.min(b.gpu_seconds),
            joules: a.joules.min(b.joules),
            cents: a.cents.min(b.cents),
        }
    }

    /// Componentwise difference (may go negative if misused; callers guard).
    #[inline]
    pub fn diff(a: Rails, b: Rails) -> Rails {
        Rails {
            gpu_seconds: a.gpu_seconds - b.gpu_seconds,
            joules: a.joules - b.joules,
            cents: a.cents - b.cents,
        }
    }

    /// True if every component of `self` is finite and >= 0.
    #[inline]
    pub fn is_sane(self) -> bool {
        self.gpu_seconds.is_finite()
            && self.joules.is_finite()
            && self.cents.is_finite()
            && self.gpu_seconds >= 0.0
            && self.joules >= 0.0
            && self.cents >= 0.0
    }
}

/// The bank: a refillable, roll-over-able balance on each rail.
///
/// Each tick the balance is topped up by [`RailsBank::refill`] and clamped to
/// a rollover cap, so an idle period banks at most a fixed amount of
/// attention for a later burst — the same shape as builder-ide's rails, and
/// the same reason: unbounded rollover lets ancient savings starve fresh
/// work.
#[derive(Clone, Copy, Debug)]
pub struct RailsBank {
    /// Current spendable balance.
    pub balance: Rails,
    /// Per-tick refill rate.
    pub refill_per_tick: Rails,
    /// Rollover ceiling (balance is clamped here after refill).
    pub cap: Rails,
    /// Cumulative spend, for reports and conservation checks.
    pub spent_total: Rails,
}

impl RailsBank {
    /// A bank with zero balance and the given refill/cap profile.
    pub fn new(refill_per_tick: Rails, cap: Rails) -> Self {
        RailsBank {
            balance: Rails::ZERO,
            refill_per_tick,
            cap,
            spent_total: Rails::ZERO,
        }
    }

    /// Top up and clamp. Called once per tick by the kernel.
    #[inline]
    pub fn refill(&mut self) {
        self.balance = Rails::min(self.balance + self.refill_per_tick, self.cap);
    }

    /// Can the bank pay this cost on every rail?
    #[inline]
    pub fn can_afford(&self, cost: Rails) -> bool {
        cost.gpu_seconds <= self.balance.gpu_seconds
            && cost.joules <= self.balance.joules
            && cost.cents <= self.balance.cents
    }

    /// Pay the cost. Only call after [`RailsBank::can_afford`].
    #[inline]
    pub fn spend(&mut self, cost: Rails) {
        self.balance = Rails::diff(self.balance, cost);
        self.spent_total = self.spent_total + cost;
    }
}

/// Componentwise sum (the natural `a + b` on a budget vector).
impl std::ops::Add for Rails {
    type Output = Rails;
    #[inline]
    fn add(self, rhs: Rails) -> Rails {
        Rails {
            gpu_seconds: self.gpu_seconds + rhs.gpu_seconds,
            joules: self.joules + rhs.joules,
            cents: self.cents + rhs.cents,
        }
    }
}

/// Per-bid cost declaration. Estimated by the bidder (p50 from its own
/// history, or from builder-ide's measured cost model).
#[derive(Clone, Copy, Debug, Default)]
pub struct BidCost(pub Rails);

/// Identifier of the entity a cost is attributed to (kept for journaling).
#[derive(Clone, Copy, Debug)]
pub struct SpendRecord {
    pub id: EntityId,
    pub cost: Rails,
}
