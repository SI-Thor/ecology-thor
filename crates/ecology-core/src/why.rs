//! The WHY ledger — purpose as a conserved quantity, at population scale.
//!
//! This is a faithful SoA (struct-of-arrays) port of cognitive-ecology's
//! `why.py` v0.3 semantics, reorganized for the Thor's realities:
//! contiguous arrays that a CUDA sweep can walk coalesced, no per-node
//! allocation, no pointers, and *bounded work per call* (see
//! [`WhyLedger::kill`] for the rate-limited cascade).
//!
//! THE WHY CONSERVATION LAW (unchanged from the parent repo):
//!
//! 1. A WHY node is never deleted. No API exists for it. Slots are never
//!    recycled; [`crate::ids::EntityId`] is stable for the process lifetime.
//! 2. Purpose has four states: [`WhyState::Alive`], [`WhyState::Dead`],
//!    [`WhyState::Unknown`], [`WhyState::Absent`].
//! 3. Killing a node cascades its *living* descendants to UNKNOWN —
//!    *questionable*, not gone. They can be revived.
//! 4. Every transition is recorded (the caller drains
//!    [`WhyLedger::transitions`] into the append-only journal).
//!
//! Re-attestation economics (v0.3, "purpose is a subscription"):
//!
//! * [`WhyLedger::heartbeat`] stamps one node; [`WhyLedger::run_under`]
//!   stamps a leaf and its whole chain to the root — *use* of a purpose
//!   re-attests it. Stale beats (older than the last) are no-ops: stale
//!   information must never rewind freshness.
//! * [`WhyLedger::recompute_aggregates`] computes, per node, the
//!   chain-aggregated [`WhyLedger::justification`] in one level-synchronous
//!   sweep: alive-fraction of the root path × weakest-link freshness, zeroed
//!   by any DEAD ancestor, capped at [`crate::freshness::PROBATION_CAP`]
//!   while any ALIVE link is on parole. This is the exact tree case of
//!   `why.py`'s `justification()` (a tree has exactly one path).
//! * [`WhyLedger::reap`] lapses ALIVE nodes unattested for longer than the
//!   horizon to UNKNOWN — per-node, no cascade (an unserved purpose questions
//!   *itself*; a kill says "the lineage is wrong").
//! * Revival is explicit: [`WhyLedger::revive`] (UNKNOWN→ALIVE, on parole)
//!   and [`WhyLedger::resurrect`] (DEAD→ALIVE, requires new evidence).
//! * Roots may carry a deadline (`until`) — the builder-ide world makes its
//!   roots mortal that way — enforced by [`WhyLedger::deadline_check`] with a
//!   grace window.
//!
//! GPU note: every table here is a flat `Vec` the CUDA sweeps mirror in
//! unified memory (`crates/ecology-cuda`). The aggregate math in
//! [`WhyLedger::recompute_aggregates`] is written level-synchronously *on
//! purpose*: the same level loop is what the device version parallelizes.

use crate::freshness::{exp2_fast, link_freshness, PROBATION_CAP};
use crate::ids::{EntityId, NO_BEAT, NO_PARENT};
use crate::journal::Reason;

/// The four states of purpose. Byte-sized so the whole state column is one
/// cache-friendly array the GPU can stream.
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WhyState {
    /// Asserting life; heartbeats accepted.
    Alive = 1,
    /// The lineage is wrong (root death, explicit kill). Terminal for the
    /// node itself; descendants became UNKNOWN, not DEAD.
    Dead = 3,
    /// Questionable — lapsed, doubted, or orphaned by a cascade. Revivable.
    Unknown = 2,
    /// A draft/parked purpose that never asserted itself. Activatable.
    Absent = 0,
}

/// How a node enters the world.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SpawnState {
    /// Asserting life immediately.
    Alive,
    /// Parked draft; `activate()` later.
    Absent,
}

/// Why a spawn was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SpawnError {
    /// The arena is full. The organism refuses to exceed its memory budget
    /// rather than degrade — grow `Config::capacity` and restart.
    Capacity,
    /// Parent does not exist (out of range) — a bug in the caller.
    NoSuchParent,
    /// Deeper than `MAX_DEPTH` (a pathological grammar, not a workload).
    TooDeep,
}

/// Hard cap on chain depth. Cascades and aggregate sweeps are O(depth)
/// sequential in the worst case; 1024 is far beyond any sane purpose grammar
/// and keeps the level list small.
pub const MAX_DEPTH: u16 = 1024;

/// One necrology line: purpose changed hands, but never vanished.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Transition {
    pub id: EntityId,
    pub from: WhyState,
    pub to: WhyState,
    pub reason: Reason,
    pub tick: u32,
}

/// The ledger: every table below is parallel and preallocated to capacity.
/// Steady-state operation allocates nothing.
pub struct WhyLedger {
    len: usize,
    capacity: usize,

    // -- identity & topology (append-only, frozen after spawn) --------------
    pub(crate) parent: Vec<u32>,
    pub(crate) first_child: Vec<u32>,
    pub(crate) next_sibling: Vec<u32>,
    pub(crate) depth: Vec<u16>,
    /// Nodes grouped by depth — the level-synchronous sweep schedule.
    /// Built at spawn time; never mutated afterwards.
    pub(crate) levels: Vec<Vec<u32>>,

    // -- liveness state -----------------------------------------------------
    pub(crate) state: Vec<WhyState>,
    pub(crate) born: Vec<u32>,
    /// Tick of last accepted heartbeat; [`NO_BEAT`] until the first one.
    pub(crate) last_beat: Vec<i64>,
    /// Monotonic E2E-style alive counter (AUTOSAR-style masquerade check:
    /// a repeated or rewound counter is a stale or forged beat).
    pub(crate) beat_count: Vec<u32>,
    /// Root deadline (tick); `u32::MAX` = immortal.
    pub(crate) until: Vec<u32>,
    /// Revived-but-unserved: justification capped while true.
    pub(crate) probation: Vec<bool>,
    /// Purpose mass — the conserved quantity of the WHY economy.
    pub(crate) mass: Vec<f32>,

    // -- chain aggregates (recomputed by `recompute_aggregates`) ------------
    /// ALIVE nodes on the root path (inclusive), last sweep.
    pub(crate) chain_alive: Vec<u32>,
    /// Path length (inclusive).
    pub(crate) chain_len: Vec<u32>,
    /// Weakest ALIVE-link freshness on the path (1.0 if no ALIVE links).
    pub(crate) chain_min_fresh: Vec<f32>,
    /// Any DEAD ancestor on the path (inclusive of self)?
    pub(crate) chain_dead: Vec<bool>,
    /// Any ALIVE-and-probation link on the path?
    pub(crate) chain_probation: Vec<bool>,
    /// Chain-aggregated justification, 0..1 — the attention economy's `j`.
    pub(crate) justification: Vec<f32>,

    // -- mass conservation bookkeeping ---------------------------------------
    /// Σ mass ever spawned.
    pub(crate) mass_spawned_total: f64,
    /// Σ mass dissipated by decay or death (never by deletion — there is none).
    pub(crate) mass_dissipated_total: f64,

    // -- transition outbox & cascade machinery -------------------------------
    /// Transitions since the caller last drained. Reused buffer.
    pub(crate) transitions: Vec<Transition>,
    /// Pending cascade frontier (a stack of subtree roots still to walk).
    /// Rate-limited by `cascade_budget` per drain.
    cascade_stack: Vec<u32>,
    /// Subtree nodes still marked for cascade (for reports).
    pub(crate) cascade_pending: u32,

    // -- live counters --------------------------------------------------------
    pub(crate) counts: StateCounts,
}

/// Population counts, maintained incrementally on every transition.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StateCounts {
    pub alive: u32,
    pub dead: u32,
    pub unknown: u32,
    pub absent: u32,
}

impl WhyLedger {
    /// Preallocate every table to `capacity`. This is the organism's memory
    /// budget for purpose; it will not silently grow.
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.max(1);
        WhyLedger {
            len: 0,
            capacity: cap,
            parent: vec![NO_PARENT; cap],
            first_child: vec![NO_PARENT; cap],
            next_sibling: vec![NO_PARENT; cap],
            depth: vec![0; cap],
            levels: vec![Vec::new()],
            state: vec![WhyState::Absent; cap],
            born: vec![0; cap],
            last_beat: vec![NO_BEAT; cap],
            beat_count: vec![0; cap],
            until: vec![u32::MAX; cap],
            probation: vec![false; cap],
            mass: vec![0.0; cap],
            chain_alive: vec![0; cap],
            chain_len: vec![0; cap],
            chain_min_fresh: vec![1.0; cap],
            chain_dead: vec![false; cap],
            chain_probation: vec![false; cap],
            justification: vec![0.0; cap],
            mass_spawned_total: 0.0,
            mass_dissipated_total: 0.0,
            transitions: Vec::new(),
            cascade_stack: Vec::new(),
            cascade_pending: 0,
            counts: StateCounts::default(),
        }
    }

    /// Number of slots in use.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// True iff no entity has ever been spawned.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    #[inline]
    pub fn counts(&self) -> StateCounts {
        self.counts
    }

    // ------------------------------------------------------------------ spawn

    /// Spawn a purpose. `parent == None` makes a root; roots may carry an
    /// `until` deadline (tick). Returns the entity's stable id.
    pub fn spawn(
        &mut self,
        parent: Option<EntityId>,
        mass: f32,
        until: Option<u32>,
        initial: SpawnState,
        tick: u32,
        reason: Reason,
    ) -> Result<EntityId, SpawnError> {
        if self.len >= self.capacity {
            return Err(SpawnError::Capacity);
        }
        let idx = self.len as u32;
        let (parent_idx, depth) = match parent {
            None => (NO_PARENT, 0u16),
            Some(p) => {
                let pi = p.index();
                if pi >= self.len {
                    return Err(SpawnError::NoSuchParent);
                }
                let d = self.depth[pi];
                if d >= MAX_DEPTH {
                    return Err(SpawnError::TooDeep);
                }
                (p.0, d + 1)
            }
        };

        // Link into the sibling list (append at head; order never matters to
        // semantics because cascades/aggregate sweeps are order-independent).
        self.parent[idx as usize] = parent_idx;
        if parent_idx != NO_PARENT {
            let pi = parent_idx as usize;
            self.next_sibling[idx as usize] = self.first_child[pi];
            self.first_child[pi] = idx;
        }

        self.depth[idx as usize] = depth;
        while self.levels.len() <= depth as usize {
            self.levels.push(Vec::new());
        }
        self.levels[depth as usize].push(idx);

        let state = match initial {
            SpawnState::Alive => WhyState::Alive,
            SpawnState::Absent => WhyState::Absent,
        };
        self.state[idx as usize] = state;
        self.born[idx as usize] = tick;
        self.last_beat[idx as usize] = if state == WhyState::Alive {
            tick as i64
        } else {
            NO_BEAT
        };
        self.beat_count[idx as usize] = 0;
        self.until[idx as usize] = until.unwrap_or(u32::MAX);
        self.probation[idx as usize] = false;
        self.mass[idx as usize] = mass.max(0.0);
        self.mass_spawned_total += mass.max(0.0) as f64;
        self.chain_len[idx as usize] = depth as u32 + 1;
        match state {
            WhyState::Alive => self.counts.alive += 1,
            WhyState::Absent => self.counts.absent += 1,
            _ => unreachable!("spawn only produces Alive/Absent"),
        }

        self.len += 1;
        let id = EntityId(idx);
        self.transitions.push(Transition {
            id,
            from: WhyState::Absent,
            to: state,
            reason,
            tick,
        });
        Ok(id)
    }

    // ------------------------------------------------------------ transitions

    /// Record an internal transition and maintain counters + mass ledger.
    fn transition(&mut self, idx: usize, to: WhyState, reason: Reason, tick: u32) {
        let from = self.state[idx];
        if from == to {
            return;
        }
        // Mass law: DEAD annihilates remaining mass (dissipated, conserved
        // in the bookkeeping); UNKNOWN freezes it (revivable); ALIVE keeps it.
        if to == WhyState::Dead {
            self.mass_dissipated_total += self.mass[idx] as f64;
            self.mass[idx] = 0.0;
        }
        self.state[idx] = to;
        match from {
            WhyState::Alive => self.counts.alive -= 1,
            WhyState::Dead => self.counts.dead -= 1,
            WhyState::Unknown => self.counts.unknown -= 1,
            WhyState::Absent => self.counts.absent -= 1,
        }
        match to {
            WhyState::Alive => self.counts.alive += 1,
            WhyState::Dead => self.counts.dead += 1,
            WhyState::Unknown => self.counts.unknown += 1,
            WhyState::Absent => self.counts.absent += 1,
        }
        self.transitions.push(Transition {
            id: EntityId(idx as u32),
            from,
            to,
            reason,
            tick,
        });
    }

    /// Heartbeat: stamp liveness on one node.
    ///
    /// Returns `false` if the node is not ALIVE (beats on dead/questionable
    /// purposes are refused — revive first) or the beat is stale (older than
    /// the recorded one — stale information never rewinds freshness).
    /// An accepted beat ends parole (why.py: "revived-but-*unserved*" — the
    /// cap exists to price unproven purposes, and service is the proof).
    pub fn heartbeat(&mut self, id: EntityId, now: u32) -> bool {
        let idx = id.index();
        if idx >= self.len || self.state[idx] != WhyState::Alive {
            return false;
        }
        let t = now as i64;
        if t <= self.last_beat[idx] {
            return false; // stale or duplicate beat: no-op, no rewind
        }
        self.last_beat[idx] = t;
        self.beat_count[idx] = self.beat_count[idx].wrapping_add(1);
        self.probation[idx] = false;
        true
    }

    /// Re-attest by USE: stamp this node and every ALIVE ancestor up to the
    /// root (why.py's `run_under`). Serving a purpose ends its parole.
    /// Returns how many nodes were stamped.
    pub fn run_under(&mut self, id: EntityId, now: u32) -> u32 {
        let mut stamped = 0u32;
        let mut cur = id.0 as usize;
        let t = now as i64;
        while cur != NO_PARENT as usize {
            if self.state[cur] == WhyState::Alive && t > self.last_beat[cur] {
                self.last_beat[cur] = t;
                self.beat_count[cur] = self.beat_count[cur].wrapping_add(1);
                self.probation[cur] = false;
                stamped += 1;
            }
            cur = self.parent[cur] as usize;
        }
        stamped
    }

    /// UNKNOWN → ALIVE (questionable purposes may be re-justified). The
    /// node returns *on parole* — its justification is capped at
    /// [`PROBATION_CAP`] until it is served again.
    pub fn revive(&mut self, id: EntityId, reason: Reason, tick: u32) -> bool {
        let idx = id.index();
        if idx >= self.len || self.state[idx] != WhyState::Unknown {
            return false;
        }
        self.probation[idx] = true;
        self.last_beat[idx] = tick as i64; // revival is itself an attestation
        self.transition(idx, WhyState::Alive, reason, tick);
        true
    }

    /// DEAD → ALIVE. Requires substantive new evidence (the caller passes a
    /// reason; the journal keeps it). Also returns on parole.
    pub fn resurrect(&mut self, id: EntityId, reason: Reason, tick: u32) -> bool {
        let idx = id.index();
        if idx >= self.len || self.state[idx] != WhyState::Dead {
            return false;
        }
        self.probation[idx] = true;
        self.last_beat[idx] = tick as i64;
        self.transition(idx, WhyState::Alive, reason, tick);
        true
    }

    /// ABSENT → ALIVE: a draft/parked purpose starts asserting itself.
    pub fn activate(&mut self, id: EntityId, tick: u32) -> bool {
        let idx = id.index();
        if idx >= self.len || self.state[idx] != WhyState::Absent {
            return false;
        }
        self.last_beat[idx] = tick as i64;
        self.transition(idx, WhyState::Alive, Reason::Activated, tick);
        true
    }

    /// ALIVE → UNKNOWN: doubt arrived from OUTSIDE the organism (an
    /// external question, a failing dependency, a supervisor's suspicion).
    /// No cascade — this questions one node, unlike `kill`.
    pub fn doubt(&mut self, id: EntityId, reason: Reason, tick: u32) -> bool {
        let idx = id.index();
        if idx >= self.len || self.state[idx] != WhyState::Alive {
            return false;
        }
        self.transition(idx, WhyState::Unknown, reason, tick);
        true
    }

    /// Kill a node: it becomes DEAD and its *living* descendants cascade to
    /// UNKNOWN — questionable, not gone.
    ///
    /// **Bounded work, on purpose.** A subtree can be arbitrarily large, and
    /// this runs inside the real-time tick. The walk converts at most
    /// `budget` descendants per call and parks the remaining frontier on an
    /// internal stack; [`WhyLedger::drain_cascade`] continues it (again
    /// budgeted) on later ticks. A cascade therefore *always* completes
    /// eventually, and never blows the frame — the Erlang/OTP lesson
    /// (restart intensity) applied to doubt propagation.
    pub fn kill(&mut self, id: EntityId, reason: Reason, tick: u32, budget: u32) -> u32 {
        let idx = id.index();
        if idx >= self.len {
            return 0;
        }
        // Enqueue this node's children (the node itself transitions below).
        self.push_children(idx);
        self.transition(idx, WhyState::Dead, reason, tick);
        self.drain_cascade(budget)
    }

    /// Continue a rate-limited cascade. Returns nodes converted this call.
    pub fn drain_cascade(&mut self, budget: u32) -> u32 {
        let mut converted = 0u32;
        while converted < budget {
            let Some(next) = self.cascade_stack.pop() else {
                break;
            };
            let idx = next as usize;
            self.cascade_pending = self.cascade_pending.saturating_sub(1);
            // Walk this subtree node: cascade only the living.
            if self.state[idx] == WhyState::Alive {
                // A cascading node keeps its mass (frozen, revivable) but
                // its stale-ness will be priced by justification.
                self.transition(idx, WhyState::Unknown, Reason::Cascade, self.now_hint(idx));
                converted += 1;
            }
            self.push_children(idx);
        }
        converted
    }

    /// Number of subtree nodes still queued for cascade.
    #[inline]
    pub fn cascade_pending(&self) -> u32 {
        self.cascade_pending
    }

    fn push_children(&mut self, idx: usize) {
        let mut child = self.first_child[idx];
        while child != NO_PARENT {
            self.cascade_stack.push(child);
            self.cascade_pending += 1;
            child = self.next_sibling[child as usize];
        }
    }

    /// Best-effort "now" for cascades that fire outside a tick context:
    /// cascaded nodes are marked with the dying ancestor's last beat, which
    /// is the honest time of the event that doomed them.
    fn now_hint(&self, idx: usize) -> u32 {
        let lb = self.last_beat[idx];
        if lb >= 0 {
            lb as u32
        } else {
            self.born[idx]
        }
    }

    // ----------------------------------------------------------------- sweeps

    /// Lapse ALIVE nodes unattested for longer than `horizon` ticks to
    /// UNKNOWN. Per-node — no cascade. Returns the number lapsed.
    ///
    /// This is the economic harvest: purpose that nobody maintains decays
    /// into doubt *by itself*, before any arbitration happens.
    pub fn reap(&mut self, now: u32, horizon: u32) -> u32 {
        let mut lapsed = 0u32;
        for i in 0..self.len {
            if self.state[i] != WhyState::Alive {
                continue;
            }
            let last = if self.last_beat[i] >= 0 {
                self.last_beat[i] as u32
            } else {
                self.born[i]
            };
            if now.saturating_sub(last) > horizon {
                self.transition(i, WhyState::Unknown, Reason::Lapsed, now);
                lapsed += 1;
            }
        }
        lapsed
    }

    /// Root deadlines: roots with `until + grace < now` die and cascade.
    /// Returns the number of roots that died this call.
    pub fn deadline_check(&mut self, now: u32, grace: u32, budget: u32) -> u32 {
        let mut died = 0u32;
        for i in 0..self.len {
            if self.parent[i] != NO_PARENT || self.state[i] != WhyState::Alive {
                continue; // only living roots can die of deadline
            }
            let deadline = self.until[i];
            if deadline != u32::MAX && now.saturating_sub(deadline) > grace {
                self.kill(EntityId(i as u32), Reason::Deadline, now, budget);
                died += 1;
            }
        }
        died
    }

    /// Mass decay sweep: ALIVE mass decays `mass *= 2^(-λ·Δt)`, UNKNOWN mass
    /// is frozen (revivable), DEAD mass is already zero. `dt` is in ticks
    /// since the last decay sweep. Returns the dissipated mass.
    ///
    /// Uses the bit-exact [`exp2_fast`] so the device sweep (same formula,
    /// `-fmad=false`) produces identical results — the golden tests compare
    /// them.
    pub fn decay_mass(&mut self, lambda_per_tick: f32, dt: u32) -> f32 {
        let factor = exp2_fast(-lambda_per_tick * dt as f32);
        let mut dissipated = 0.0f32;
        for i in 0..self.len {
            if self.state[i] != WhyState::Alive {
                continue;
            }
            let before = self.mass[i];
            let after = before * factor;
            self.mass[i] = after;
            dissipated += before - after;
        }
        self.mass_dissipated_total += dissipated as f64;
        dissipated
    }

    /// Level-synchronous chain-aggregate sweep.
    ///
    /// For every node, in depth order (parents strictly before children):
    ///
    /// * `chain_alive/chain_len` — the alive fraction of the root path;
    /// * `chain_min_fresh` — the weakest ALIVE-link freshness on the path;
    /// * `chain_dead` — any DEAD ancestor (inclusive) ⇒ justification 0;
    /// * `chain_probation` — any ALIVE-and-parole link ⇒ cap 0.5;
    /// * `justification` — the product, the attention economy's `j`.
    ///
    /// This is exactly `why.py::justification()` specialized to trees (a
    /// tree has one path), computed for the whole population in one O(N)
    /// pass whose inner loop is embarrassingly parallel per level — the
    /// device version in `crates/ecology-cuda/kernels` parallelizes each
    /// level as one kernel launch (or one grid-stride phase).
    pub fn recompute_aggregates(&mut self, now: u32, horizon: u32) {
        for level_nodes in &self.levels {
            for &idx in level_nodes.iter() {
                let i = idx as usize;
                let p = self.parent[i];
                let alive = self.state[i] == WhyState::Alive;
                let fresh = link_freshness(alive, self.last_beat[i], self.born[i], now, horizon);
                if p == NO_PARENT {
                    self.chain_alive[i] = alive as u32;
                    self.chain_len[i] = 1;
                    self.chain_min_fresh[i] = fresh;
                    self.chain_dead[i] = self.state[i] == WhyState::Dead;
                    self.chain_probation[i] = alive && self.probation[i];
                } else {
                    let pi = p as usize;
                    self.chain_alive[i] = self.chain_alive[pi] + alive as u32;
                    self.chain_len[i] = self.chain_len[pi] + 1;
                    // min() of f32: plain IEEE compare, bit-exact both sides.
                    self.chain_min_fresh[i] = if fresh < self.chain_min_fresh[pi] {
                        fresh
                    } else {
                        self.chain_min_fresh[pi]
                    };
                    self.chain_dead[i] = self.chain_dead[pi] || self.state[i] == WhyState::Dead;
                    self.chain_probation[i] =
                        self.chain_probation[pi] || (alive && self.probation[i]);
                }
                // justification()
                if self.chain_dead[i] || self.chain_alive[i] == 0 {
                    self.justification[i] = 0.0;
                } else {
                    let base = self.chain_alive[i] as f32 / self.chain_len[i] as f32;
                    let mut j = base * self.chain_min_fresh[i];
                    if self.chain_probation[i] {
                        j = if j < PROBATION_CAP { j } else { PROBATION_CAP };
                    }
                    self.justification[i] = j;
                }
            }
        }
    }

    // ------------------------------------------------------------- accessors

    #[inline]
    pub fn state(&self, id: EntityId) -> Option<WhyState> {
        self.state.get(id.index()).copied()
    }

    /// Chain-aggregated justification from the last sweep (0..1). May be
    /// stale by up to one auction interval — by design: aggregates are a
    /// framed job, not an on-demand query (see `docs/RT.md`).
    #[inline]
    pub fn justification(&self, id: EntityId) -> f32 {
        self.justification[id.index()]
    }

    #[inline]
    pub fn mass(&self, id: EntityId) -> f32 {
        self.mass[id.index()]
    }

    #[inline]
    pub fn last_beat(&self, id: EntityId) -> i64 {
        self.last_beat[id.index()]
    }

    #[inline]
    pub fn beat_count(&self, id: EntityId) -> u32 {
        self.beat_count[id.index()]
    }

    #[inline]
    pub fn parent_of(&self, id: EntityId) -> Option<EntityId> {
        let p = self.parent[id.index()];
        if p == NO_PARENT {
            None
        } else {
            Some(EntityId(p))
        }
    }

    /// Birth tick (device-mirror accessor).
    #[inline]
    pub fn born_at(&self, i: usize) -> u32 {
        self.born[i]
    }

    /// Raw parent index (device-mirror accessor; `NO_PARENT` for roots).
    #[inline]
    pub fn parent_at(&self, i: usize) -> u32 {
        self.parent[i]
    }

    /// Parole flag (device-mirror accessor).
    #[inline]
    pub fn probation_at(&self, i: usize) -> bool {
        self.probation[i]
    }

    /// Depth-grouped node indices — the level plan the device sweep walks
    /// (one kernel launch per level; see `kernels/ecology.cu`).
    #[inline]
    pub fn levels(&self) -> &[Vec<u32>] {
        &self.levels
    }

    #[inline]
    pub fn depth_of(&self, id: EntityId) -> u16 {
        self.depth[id.index()]
    }

    /// Σ current mass over all slots (the conserved quantity).
    pub fn mass_total(&self) -> f32 {
        self.mass.iter().sum()
    }

    /// Conservation identity: `spawned - dissipated == current` (to f32
    /// accumulation tolerance). Checked every sweep by the kernel; a
    /// violation is a bug in this file, not a workload condition.
    pub fn mass_conservation_ok(&self) -> bool {
        let expected = self.mass_spawned_total - self.mass_dissipated_total;
        let actual = self.mass_total() as f64;
        let tol = (self.mass_spawned_total.max(1.0) * 1e-3) + 1e-3;
        (expected - actual).abs() <= tol
    }

    /// Drain pending transitions (the necrology lines) for journaling.
    /// The buffer keeps its capacity — steady state allocates nothing.
    pub fn transitions(&mut self) -> std::vec::Drain<'_, Transition> {
        self.transitions.drain(..)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ledger() -> WhyLedger {
        WhyLedger::new(64)
    }

    #[test]
    fn spawn_alive_counts_and_journal() {
        let mut w = ledger();
        let id = w
            .spawn(None, 1.0, None, SpawnState::Alive, 0, Reason::Born)
            .unwrap();
        assert_eq!(w.state(id), Some(WhyState::Alive));
        assert_eq!(w.counts().alive, 1);
        assert_eq!(w.transitions().count(), 1);
    }

    #[test]
    fn heartbeat_stale_never_rewinds() {
        let mut w = ledger();
        let id = w
            .spawn(None, 1.0, None, SpawnState::Alive, 10, Reason::Born)
            .unwrap();
        assert!(w.heartbeat(id, 20));
        assert!(!w.heartbeat(id, 15), "stale beat must be a no-op");
        assert!(!w.heartbeat(id, 20), "duplicate beat must be a no-op");
        assert_eq!(w.last_beat(id), 20);
        assert_eq!(w.beat_count(id), 1);
    }

    #[test]
    fn kill_cascades_living_to_unknown_not_dead() {
        let mut w = ledger();
        let root = w
            .spawn(None, 1.0, None, SpawnState::Alive, 0, Reason::Born)
            .unwrap();
        let a = w
            .spawn(Some(root), 1.0, None, SpawnState::Alive, 0, Reason::Born)
            .unwrap();
        let b = w
            .spawn(Some(a), 1.0, None, SpawnState::Alive, 0, Reason::Born)
            .unwrap();
        // b lapses first; a killed subtree must not touch b's DEAD-ness —
        // here we verify the living descendant becomes UNKNOWN.
        w.kill(root, Reason::Killed, 10, 100);
        assert_eq!(w.state(root), Some(WhyState::Dead));
        assert_eq!(w.state(a), Some(WhyState::Unknown));
        assert_eq!(w.state(b), Some(WhyState::Unknown));
        assert_eq!(
            w.counts(),
            StateCounts {
                alive: 0,
                dead: 1,
                unknown: 2,
                absent: 0
            }
        );
        // Dead is terminal for the node itself.
        assert!(!w.heartbeat(root, 20));
        assert!(!w.revive(root, Reason::Revived, 20));
        // But resurrection exists, on parole.
        assert!(w.resurrect(root, Reason::NewEvidence, 21));
        assert_eq!(w.state(root), Some(WhyState::Alive));
    }

    #[test]
    fn cascade_is_rate_limited() {
        let mut w = WhyLedger::new(256);
        let root = w
            .spawn(None, 1.0, None, SpawnState::Alive, 0, Reason::Born)
            .unwrap();
        for _ in 0..100 {
            w.spawn(Some(root), 0.1, None, SpawnState::Alive, 0, Reason::Born)
                .unwrap();
        }
        let converted = w.kill(root, Reason::Killed, 1, 10);
        assert_eq!(converted, 10, "budget bounds per-call conversions");
        assert!(w.cascade_pending() > 0, "the rest stays queued");
        // Draining finishes the job across calls.
        let mut total = converted;
        while w.cascade_pending() > 0 {
            total += w.drain_cascade(50);
        }
        assert_eq!(total, 100);
        assert_eq!(w.counts().unknown, 100);
    }

    #[test]
    fn justification_matches_why_py_semantics() {
        // Chain: root(alive, fresh) - mid(alive, stale) - leaf(alive, fresh).
        // horizon 90, grace 30. mid last beat 0, now 150 => age 150 => 0.0.
        // Weakest link = 0 => leaf j = 0. Also alive fraction 3/3 = 1.
        let mut w = WhyLedger::new(16);
        let root = w
            .spawn(None, 1.0, None, SpawnState::Alive, 0, Reason::Born)
            .unwrap();
        let mid = w
            .spawn(Some(root), 1.0, None, SpawnState::Alive, 0, Reason::Born)
            .unwrap();
        let leaf = w
            .spawn(Some(mid), 1.0, None, SpawnState::Alive, 0, Reason::Born)
            .unwrap();
        w.heartbeat(root, 150);
        w.heartbeat(leaf, 150);
        // mid never re-attested: last beat 0, age 150 > horizon 90 => fresh 0.
        w.recompute_aggregates(150, 90);
        assert_eq!(w.justification(root), 1.0);
        assert_eq!(w.justification(mid), 0.0);
        assert_eq!(w.justification(leaf), 0.0, "weakest link dominates");
        // Re-attest mid: chain returns to full strength.
        w.heartbeat(mid, 151);
        w.recompute_aggregates(151, 90);
        assert!((w.justification(leaf) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn unknown_ancestor_discounts_proportionally() {
        // root alive, mid unknown, leaf alive: alive fraction 2/3, fresh 1.
        let mut w = WhyLedger::new(16);
        let root = w
            .spawn(None, 1.0, None, SpawnState::Alive, 0, Reason::Born)
            .unwrap();
        let mid = w
            .spawn(Some(root), 1.0, None, SpawnState::Alive, 0, Reason::Born)
            .unwrap();
        let leaf = w
            .spawn(Some(mid), 1.0, None, SpawnState::Alive, 0, Reason::Born)
            .unwrap();
        w.transition(mid.index(), WhyState::Unknown, Reason::Lapsed, 5);
        w.recompute_aggregates(6, 90);
        let expected = 2.0 / 3.0;
        assert!((w.justification(leaf) - expected).abs() < 1e-6);
    }

    #[test]
    fn probation_caps_at_half() {
        let mut w = WhyLedger::new(16);
        let root = w
            .spawn(None, 1.0, None, SpawnState::Alive, 0, Reason::Born)
            .unwrap();
        // Let it lapse, then revive (parole) — justification capped at 0.5.
        w.transition(root.index(), WhyState::Unknown, Reason::Lapsed, 100);
        assert!(w.revive(root, Reason::Revived, 100));
        w.recompute_aggregates(100, 90);
        assert_eq!(w.justification(root), PROBATION_CAP);
    }

    #[test]
    fn reap_lapses_only_the_unserved() {
        let mut w = WhyLedger::new(16);
        let a = w
            .spawn(None, 1.0, None, SpawnState::Alive, 0, Reason::Born)
            .unwrap();
        let b = w
            .spawn(None, 1.0, None, SpawnState::Alive, 0, Reason::Born)
            .unwrap();
        w.heartbeat(a, 50);
        let lapsed = w.reap(100, 90);
        assert_eq!(lapsed, 1, "only b (age 100 > 90) lapses");
        assert_eq!(w.state(b), Some(WhyState::Unknown));
        assert_eq!(w.state(a), Some(WhyState::Alive));
    }

    #[test]
    fn root_deadline_kills_with_grace() {
        let mut w = WhyLedger::new(16);
        let root = w
            .spawn(None, 1.0, Some(100), SpawnState::Alive, 0, Reason::Born)
            .unwrap();
        assert_eq!(w.deadline_check(110, 30, 100), 0, "still inside grace");
        assert_eq!(w.deadline_check(131, 30, 100), 1, "past until+grace");
        assert_eq!(w.state(root), Some(WhyState::Dead));
    }

    #[test]
    fn mass_conservation_holds_through_decay_and_death() {
        let mut w = WhyLedger::new(64);
        let a = w
            .spawn(None, 2.0, None, SpawnState::Alive, 0, Reason::Born)
            .unwrap();
        let _b = w
            .spawn(None, 1.0, None, SpawnState::Alive, 0, Reason::Born)
            .unwrap();
        w.decay_mass(0.001, 100);
        w.kill(a, Reason::Killed, 1, 100);
        w.decay_mass(0.001, 100);
        assert!(w.mass_conservation_ok(), "spawned - dissipated == current");
    }

    #[test]
    fn capacity_is_a_hard_budget() {
        let mut w = WhyLedger::new(2);
        assert!(w
            .spawn(None, 1.0, None, SpawnState::Alive, 0, Reason::Born)
            .is_ok());
        assert!(w
            .spawn(None, 1.0, None, SpawnState::Alive, 0, Reason::Born)
            .is_ok());
        assert_eq!(
            w.spawn(None, 1.0, None, SpawnState::Alive, 0, Reason::Born),
            Err(SpawnError::Capacity)
        );
    }
}
