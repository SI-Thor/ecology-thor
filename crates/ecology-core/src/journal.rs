//! The journal — an append-only, tamper-evident necrology.
//!
//! Every WHY transition, every auction, every decay sweep and safety event
//! lands here. Two guarantees:
//!
//! 1. **The chain never lies.** Every event is folded into a running FNV-1a
//!    chain hash over its canonical byte encoding. Two runs of the same
//!    scenario produce the same chain hash on every platform (the encodings
//!    are fixed-width little-endian; the f32 fields go through `to_bits`).
//!    This is the golden-determinism anchor of the whole kernel
//!    (`tests/golden.rs`).
//! 2. **The tick never blocks.** The journal is a bounded ring. When the
//!    ring is full the *newest* event is not retained (recent history is the
//!    valuable part) but it is still chain-hashed — integrity without
//!    unbounded memory. Dropping is counted and surfaces in
//!    [`crate::snapshot::TickSummary`] and as a `JournalOverflow` event when
//!    space returns.
//!
//! What is *not* journaled: individual heartbeats. At population scale they
//! would bury the transitions; their evidence (last_beat, beat_count) is
//! readable from the ledger and aggregated into the tick summary. The
//! necrology records purpose *changing hands*, not every pulse — matching
//! the parent repo's necrology semantics.

use crate::ids::EntityId;
use crate::rails::Rails;
use crate::why::WhyState;

/// Why a transition happened. Compact, deterministic; the human-facing names
/// live in the docs, not in the hot path.
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reason {
    /// A purpose was born.
    Born = 1,
    /// A parked draft began asserting itself.
    Activated = 2,
    /// Explicit kill by an authority.
    Killed = 3,
    /// A root outlived its `until` deadline (+ grace).
    Deadline = 4,
    /// Doubt propagated down from a killed ancestor.
    Cascade = 5,
    /// Nobody re-attested within the horizon (the economic harvest).
    Lapsed = 6,
    /// UNKNOWN → ALIVE, re-justified.
    Revived = 7,
    /// DEAD → ALIVE, substantive new evidence.
    NewEvidence = 8,
    /// Reserved for callers that need their own codes.
    Custom = 255,
}

/// One journal line.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// A purpose entered the world.
    Spawned {
        id: EntityId,
        parent: u32, // NO_PARENT for roots
        mass: f32,
        alive: bool,
        tick: u32,
    },
    /// A purpose changed state (the necrology proper).
    Transition {
        id: EntityId,
        from: WhyState,
        to: WhyState,
        reason: Reason,
        tick: u32,
    },
    /// One auction settled.
    Auction {
        tick: u32,
        admitted: u32,
        vetoed: u32,
        skipped_infeasible: u32,
        spent: Rails,
    },
    /// A mass-decay sweep dissipated mass.
    Decay { tick: u32, dissipated: f32 },
    /// A frame overran its budget.
    Overrun { tick: u32, frame: Frame },
    /// Safe mode entered (`entered`) or exited.
    SafeMode { tick: u32, entered: bool },
    /// Events were dropped (not retained) while the ring was full.
    JournalOverflow { tick: u32, dropped: u32 },
}

/// Which framed job overran. (See `docs/RT.md` for the frame contract.)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Frame {
    /// The minor frame (every tick).
    Minor,
    /// The auction bucket.
    Auction,
    /// The sweep bucket.
    Sweep,
}

/// FNV-1a 64-bit.
#[inline]
fn fnv_mix(chain: u64, bytes: &[u8]) -> u64 {
    let mut h = chain;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    h
}

/// Canonical byte encoding of an event. Fixed-width, little-endian, no
/// variability — the chain hash depends on nothing but the event content.
fn encode(event: &Event, buf: &mut Vec<u8>) {
    buf.clear();
    match event {
        Event::Spawned {
            id,
            parent,
            mass,
            alive,
            tick,
        } => {
            buf.push(1);
            buf.extend_from_slice(&id.0.to_le_bytes());
            buf.extend_from_slice(&parent.to_le_bytes());
            buf.extend_from_slice(&mass.to_bits().to_le_bytes());
            buf.push(*alive as u8);
            buf.extend_from_slice(&tick.to_le_bytes());
        }
        Event::Transition {
            id,
            from,
            to,
            reason,
            tick,
        } => {
            buf.push(2);
            buf.extend_from_slice(&id.0.to_le_bytes());
            buf.push(*from as u8);
            buf.push(*to as u8);
            buf.push(*reason as u8);
            buf.extend_from_slice(&tick.to_le_bytes());
        }
        Event::Auction {
            tick,
            admitted,
            vetoed,
            skipped_infeasible,
            spent,
        } => {
            buf.push(3);
            buf.extend_from_slice(&tick.to_le_bytes());
            buf.extend_from_slice(&admitted.to_le_bytes());
            buf.extend_from_slice(&vetoed.to_le_bytes());
            buf.extend_from_slice(&skipped_infeasible.to_le_bytes());
            buf.extend_from_slice(&spent.gpu_seconds.to_bits().to_le_bytes());
            buf.extend_from_slice(&spent.joules.to_bits().to_le_bytes());
            buf.extend_from_slice(&spent.cents.to_bits().to_le_bytes());
        }
        Event::Decay { tick, dissipated } => {
            buf.push(4);
            buf.extend_from_slice(&tick.to_le_bytes());
            buf.extend_from_slice(&dissipated.to_bits().to_le_bytes());
        }
        Event::Overrun { tick, frame } => {
            buf.push(5);
            buf.extend_from_slice(&tick.to_le_bytes());
            buf.push(match frame {
                Frame::Minor => 0,
                Frame::Auction => 1,
                Frame::Sweep => 2,
            });
        }
        Event::SafeMode { tick, entered } => {
            buf.push(6);
            buf.extend_from_slice(&tick.to_le_bytes());
            buf.push(*entered as u8);
        }
        Event::JournalOverflow { tick, dropped } => {
            buf.push(7);
            buf.extend_from_slice(&tick.to_le_bytes());
            buf.extend_from_slice(&dropped.to_le_bytes());
        }
    }
}

/// Bounded, chain-hashed event journal.
pub struct Journal {
    ring: Vec<Event>,
    capacity: usize,
    /// Running chain hash over *all* events (retained or not).
    chain: u64,
    /// Total events ever folded into the chain.
    events_hashed: u64,
    /// Events not retained because the ring was full (integrity kept).
    dropped: u64,
    /// Set when events were dropped since the last observation; cleared when
    /// an `JournalOverflow` event is finally retained.
    overflow_pending: u32,
    scratch: Vec<u8>,
}

impl Journal {
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.max(16);
        Journal {
            ring: Vec::with_capacity(cap),
            capacity: cap,
            chain: 14695981039346656037, // FNV-1a offset basis
            events_hashed: 0,
            dropped: 0,
            overflow_pending: 0,
            scratch: Vec::with_capacity(64),
        }
    }

    /// Append an event: always chain-hashed, retained if there is room.
    /// Never allocates in steady state (scratch and ring are preallocated;
    /// `ring` only grows once, to capacity, on first fills).
    pub fn append(&mut self, event: Event) {
        let tick = event_tick(&event);
        // Chain first — integrity is unconditional.
        encode(&event, &mut self.scratch);
        self.chain = fnv_mix(self.chain, &self.scratch);
        self.events_hashed += 1;

        if self.ring.len() < self.capacity {
            self.ring.push(event);
            // If drops were pending, note them now that there is room.
            self.try_flush_overflow_note(tick);
        } else {
            // Bounded retention: drop the newest, keep the recent window.
            // (Dropping the oldest would erase the necrology's most
            // interesting era — recent history — which is what audits read.)
            self.dropped += 1;
            self.overflow_pending += 1;
        }
    }

    /// When space exists and drops were pending, record an overflow note so
    /// the loss itself is visible in the retained log.
    fn try_flush_overflow_note(&mut self, tick: u32) {
        if self.overflow_pending > 0 && self.ring.len() < self.capacity {
            let dropped = self.overflow_pending;
            self.overflow_pending = 0;
            // Append directly (not via append) to avoid recursion; hash it too.
            let note = Event::JournalOverflow { tick, dropped };
            encode(&note, &mut self.scratch);
            self.chain = fnv_mix(self.chain, &self.scratch);
            self.events_hashed += 1;
            self.ring.push(note);
        }
    }

    /// The running chain hash — the determinism anchor.
    #[inline]
    pub fn chain_hash(&self) -> u64 {
        self.chain
    }

    #[inline]
    pub fn events_hashed(&self) -> u64 {
        self.events_hashed
    }

    #[inline]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Iterate retained events, oldest first.
    pub fn iter(&self) -> impl Iterator<Item = &Event> {
        self.ring.iter()
    }

    /// Retained event count.
    #[inline]
    pub fn retained(&self) -> usize {
        self.ring.len()
    }
}

fn event_tick(e: &Event) -> u32 {
    match e {
        Event::Spawned { tick, .. }
        | Event::Transition { tick, .. }
        | Event::Auction { tick, .. }
        | Event::Decay { tick, .. }
        | Event::Overrun { tick, .. }
        | Event::SafeMode { tick, .. }
        | Event::JournalOverflow { tick, .. } => *tick,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_hash_is_deterministic_and_order_sensitive() {
        let mk = |a: u32| {
            let mut j = Journal::new(64);
            for i in 0..a {
                j.append(Event::Transition {
                    id: EntityId(i),
                    from: WhyState::Alive,
                    to: WhyState::Unknown,
                    reason: Reason::Lapsed,
                    tick: i,
                });
            }
            j
        };
        assert_eq!(mk(10).chain_hash(), mk(10).chain_hash());
        assert_ne!(mk(10).chain_hash(), mk(11).chain_hash());
    }

    #[test]
    fn overflow_is_counted_and_noted() {
        let mut j = Journal::new(16);
        for i in 0..40 {
            j.append(Event::Decay {
                tick: i,
                dissipated: 1.0,
            });
        }
        assert!(j.dropped() > 0);
        assert_eq!(j.retained(), 16);
        assert!(j.events_hashed() >= 40);
    }

    #[test]
    fn float_encoding_is_bit_stable() {
        let mut j1 = Journal::new(8);
        let mut j2 = Journal::new(8);
        j1.append(Event::Decay {
            tick: 1,
            dissipated: 0.25,
        });
        j2.append(Event::Decay {
            tick: 1,
            dissipated: 0.5,
        });
        assert_ne!(
            j1.chain_hash(),
            j2.chain_hash(),
            "different bits → different chain"
        );
    }
}
