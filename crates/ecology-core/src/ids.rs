//! Entity identity for a never-deleting world.
//!
//! The WHY conservation law (`docs/ARCHITECTURE.md`, §WHY) says a purpose node
//! is *never deleted*. That law has a mechanical consequence we lean on: an
//! [`EntityId`] is a plain slot index that is **never recycled**. No ABA
//! problem, no generation counters, no tombstone reuse — a slot, once spawned,
//! belongs to that purpose for the lifetime of the process. The arena is
//! preallocated to `Config::capacity` at construction, so steady-state
//! operation performs **zero allocations** (an important property both for
//! real-time determinism and for the Thor's unified-memory budgeting,
//! `docs/RT.md`).
//!
//! If the arena fills, [`crate::SpawnError::Capacity`] is returned — the
//! organism refuses to grow past its memory budget rather than degrade.

use core::fmt;

/// A purpose slot index. Copy, orderable, never recycled while the process lives.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntityId(pub u32);

impl EntityId {
    /// Slot index (position in every table of the arena).
    #[inline]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

impl fmt::Debug for EntityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Stable, compact form used in the journal and snapshots: `#7`.
        write!(f, "#{}", self.0)
    }
}

/// Marker used in parent slots for roots (a purpose with no ancestor).
pub const NO_PARENT: u32 = u32::MAX;

/// `last_beat` sentinel: this node has never been attested alive.
/// (Mirrors Python `why.py`'s `-1`.)
pub const NO_BEAT: i64 = -1;
