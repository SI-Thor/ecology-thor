//! Telemetry snapshots — a seqlock-published tick summary.
//!
//! The tick loop is the **single writer** of kernel state. Telemetry readers
//! (the Python bridge, a future builder-ide overlay, `ecology-sim` reporters)
//! must never block it, lock it, or copy the whole arena. So every tick ends
//! by publishing a small plain-old-data [`TickSummary`] through a **seqlock**
//! (writer: increment to odd, write, increment to even; readers: retry on
//! odd or changed). Readers are wait-free-bounded, the writer is two atomic
//! stores — the classic read-mostly single-writer pattern, and the only
//! `unsafe` in `ecology-core` (documented and reviewed per `docs/SAFETY.md`).
//!
//! `snapshot_json` renders the summary without serde — the JSON surface is
//! tiny, fixed and hand-rolled on purpose (zero-dependency kernel; the JSON
//! shape is a documented contract for the IDE overlay, `docs/INTEGRATION.md`).

use std::sync::atomic::{AtomicU64, Ordering};

use crate::rails::Rails;
use crate::why::StateCounts;

/// One tick's public face. `Copy` plain data, safe to read across threads
/// once published.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TickSummary {
    /// Kernel tick counter (monotonic from 0).
    pub tick: u32,
    /// Purpose population by state.
    pub counts: StateCounts,
    /// Entities admitted by the most recent auction.
    pub admitted_last: u32,
    /// Vetoed (score <= 0) in the most recent auction.
    pub vetoed_last: u32,
    /// Skipped for lack of rail budget in the most recent auction.
    pub skipped_infeasible_last: u32,
    /// Cumulative spend across all auctions.
    pub spent_total: Rails,
    /// Subtree nodes still queued for cascade (doubt propagating).
    pub cascade_pending: u32,
    /// Consecutive overruns at the last frame check.
    pub overrun_streak: u32,
    /// Safe mode engaged (decay-only: auctions admit nothing).
    pub safe_mode: bool,
    /// WHY-mass conservation identity held at the last sweep.
    pub conservation_ok: bool,
    /// Σ current purpose mass.
    pub mass_total: f32,
    /// Running journal chain hash.
    pub chain_hash: u64,
    /// Total events folded into the chain.
    pub events_hashed: u64,
    /// Events dropped (unretained) so far.
    pub events_dropped: u64,
}

/// Seqlock cell. **Single writer** (the tick loop) — enforced by ownership
/// at the `Ecology` level, not by the type system; see the safety note.
pub struct SummarySeqlock {
    seq: AtomicU64,
    cell: std::cell::UnsafeCell<TickSummary>,
}

// SAFETY: `TickSummary` is plain Copy data with no interior mutability.
// The UnsafeCell is only ever written by the single tick-loop writer between
// an odd→even seq transition (Release), and read by other threads only
// between matched even seq loads (Acquire) — the seqlock discipline below.
// A data race on the cell is impossible because readers retry whenever the
// writer was mid-update (odd seq) or overwrote them (seq changed).
unsafe impl Send for SummarySeqlock {}
unsafe impl Sync for SummarySeqlock {}

impl SummarySeqlock {
    pub fn new() -> Self {
        SummarySeqlock {
            seq: AtomicU64::new(0),
            cell: std::cell::UnsafeCell::new(TickSummary::default()),
        }
    }

    /// Writer side. Must only be called from the single tick thread.
    pub fn publish(&self, summary: TickSummary) {
        let s = self.seq.load(Ordering::Relaxed);
        // Odd marks "writing in progress".
        self.seq.store(s + 1, Ordering::Relaxed);
        // SAFETY: single writer, and no reader can be mid-copy of a stable
        // view past this point (they will see odd seq and retry).
        unsafe {
            *self.cell.get() = summary;
        }
        // Release publishes both the data and the even seq together.
        self.seq.store(s + 2, Ordering::Release);
    }

    /// Reader side. Returns `None` if the writer overwhelmed us after
    /// `MAX_RETRIES` attempts (extremely rare; callers should back off).
    pub fn read(&self) -> Option<TickSummary> {
        const MAX_RETRIES: usize = 4;
        for _ in 0..MAX_RETRIES {
            let s1 = self.seq.load(Ordering::Acquire);
            if s1 & 1 == 1 {
                std::thread::yield_now();
                continue;
            }
            // SAFETY: seq is even; the writer is not mid-update. If it
            // starts one, the seq check below catches it.
            let snapshot = unsafe { *self.cell.get() };
            let s2 = self.seq.load(Ordering::Acquire);
            if s1 == s2 {
                return Some(snapshot);
            }
        }
        None
    }
}

impl Default for SummarySeqlock {
    fn default() -> Self {
        Self::new()
    }
}

/// Hand-rolled JSON for the summary. Fixed field order — this is a stable
/// contract for IDE overlays; do not reorder fields, only append.
pub fn snapshot_json(s: &TickSummary) -> String {
    // f32 rendering: 6 decimal places is stable across platforms for our
    // magnitudes and avoids float-formatting drift in the contract.
    format!(
        concat!(
            "{{\"tick\":{},\"alive\":{},\"dead\":{},\"unknown\":{},\"absent\":{},",
            "\"admitted_last\":{},\"vetoed_last\":{},\"skipped_infeasible_last\":{},",
            "\"spent_gpu_seconds\":{:.6},\"spent_joules\":{:.6},\"spent_cents\":{:.6},",
            "\"cascade_pending\":{},\"overrun_streak\":{},\"safe_mode\":{},",
            "\"conservation_ok\":{},\"mass_total\":{:.6},",
            "\"chain_hash\":\"{:016x}\",\"events_hashed\":{},\"events_dropped\":{}}}"
        ),
        s.tick,
        s.counts.alive,
        s.counts.dead,
        s.counts.unknown,
        s.counts.absent,
        s.admitted_last,
        s.vetoed_last,
        s.skipped_infeasible_last,
        s.spent_total.gpu_seconds,
        s.spent_total.joules,
        s.spent_total.cents,
        s.cascade_pending,
        s.overrun_streak,
        s.safe_mode,
        s.conservation_ok,
        s.mass_total,
        s.chain_hash,
        s.events_hashed,
        s.events_dropped,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seqlock_roundtrip_under_concurrent_writes() {
        let lock = SummarySeqlock::new();
        let mut published = TickSummary {
            tick: 0,
            ..Default::default()
        };
        lock.publish(published);

        std::thread::scope(|scope| {
            scope.spawn(|| {
                for t in 1..=2000u32 {
                    published.tick = t;
                    lock.publish(published);
                }
            });
            let mut reads = 0;
            let mut none = 0;
            while reads < 2000 {
                if let Some(s) = lock.read() {
                    assert!(s.tick <= 2000);
                    reads += 1;
                } else {
                    none += 1;
                    if none > 100 {
                        break; // pathological contention; reader backed off
                    }
                }
            }
        });
        // Final stable read.
        let s = lock.read().unwrap();
        assert_eq!(s.tick, 2000);
    }

    #[test]
    fn json_shape_is_stable() {
        let s = TickSummary::default();
        let j = snapshot_json(&s);
        assert!(j.starts_with("{\"tick\":0,\"alive\":0"));
        assert!(j.contains("\"chain_hash\":\"0000000000000000\""));
    }
}
