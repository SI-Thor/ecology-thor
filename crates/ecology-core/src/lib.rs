//! # ecology-core
//!
//! The **persistent cognitive ecology kernel, purpose-built for the NVIDIA
//! Jetson AGX Thor** — the successor of
//! [`SuperInstance/cognitive-ecology`](https://github.com/SuperInstance/cognitive-ecology)'s
//! `why.py`/attention economy, re-engineered for a box that spends its life
//! creating worlds rather than chatting.
//!
//! ## The laws (inherited, unchanged in spirit)
//!
//! 1. **The WHY conservation law** — a purpose node is never deleted. Four
//!    states (`Alive`/`Dead`/`Unknown`/`Absent`); killing cascades *living*
//!    descendants to UNKNOWN (questionable, not gone); every transition is
//!    journalized. [`why`]
//! 2. **Re-attestation economics** — purpose is a subscription.
//!    Justification is the chain-aggregated (weakest-link) freshness of the
//!    root path; stale purposes bid weaker and eventually *lapse* to UNKNOWN
//!    on their own; revival is explicit and lands on parole. [`why`]
//! 3. **The veto law** — attention = `urgency · justification · opportunity ·
//!    novelty · expected_value`, and a bid scoring `<= 0` never transacts,
//!    for any importance class. [`attention`]
//! 4. **Conservation of budget and mass** — auctions spend only what the
//!    rails hold (`gpu_seconds`, `joules`, `cents`); purpose mass decays but
//!    the `spawned − dissipated == current` identity is checked every sweep.
//!    [`rails`] [`attention`]
//!
//! ## What the Thor changes (vs. the Python parent)
//!
//! * **Population scale**: SoA tables, preallocated arenas, zero allocation
//!   in steady state, bounded work per frame — the kernel is real-time
//!   shaped (single-writer tick, harmonic frame table, safe-mode
//!   degradation). [`tick`]
//! * **Device sweeps**: every hot table is a flat slice the CUDA layer
//!   mirrors into the Thor's *unified* LPDDR5X — no copies, no pinned
//!   staging. The chain-aggregate sweep is level-synchronous on purpose: the
//!   same algorithm parallelizes per level on the Blackwell GPU
//!   (`crates/ecology-cuda`, `docs/PLACEMENT.md`).
//! * **Bit-exact CPU↔GPU math**: the shared float expressions
//!   ([`freshness`]) are written to compile identically under rustc and
//!   `nvcc -fmad=false`; the golden tests compare both sides.
//! * **Zero dependencies**: std-only, by policy — everything in the
//!   real-time loop is reviewable in this repo (`docs/SAFETY.md`).
//!
//! ## Determinism policy
//!
//! No clock reads, no system randomness, no hash-map iteration, canonical
//! float operation order, a strict total order in the auction. Same scenario
//! ⇒ same journal chain hash on x86_64 CI and aarch64 Thor alike
//! (`tests/golden.rs`).
//!
//! ## Module map
//!
//! | Module | Role |
//! |---|---|
//! | [`ids`] | Stable, never-recycled entity ids |
//! | [`why`] | The WHY ledger: states, heartbeats, cascades, chain aggregates |
//! | [`freshness`] | Bit-exact freshness & decay math shared with CUDA |
//! | [`attention`] | Bids, importance classes, the auction, the veto law |
//! | [`rails`] | Budget currencies: GPU-seconds, joules, cents |
//! | [`journal`] | Append-only, chain-hashed necrology (bounded ring) |
//! | [`snapshot`] | Seqlock tick summary + stable JSON for IDE overlays |
//! | [`tick`] | The `Ecology` facade: framed, budgeted, single-writer |
//! | [`rng`] | PCG32 — the only randomness (simulator-side) |

pub mod attention;
pub mod freshness;
pub mod ids;
pub mod journal;
pub mod rails;
pub mod rng;
pub mod snapshot;
pub mod tick;
pub mod why;

pub use attention::{
    attention_score, AttentionTable, AuctionConfig, AuctionReport, Bid, Importance,
};
pub use ids::{EntityId, NO_BEAT, NO_PARENT};
pub use journal::{Event, Frame, Journal, Reason};
pub use rails::{Rails, RailsBank};
pub use snapshot::{snapshot_json, SummarySeqlock, TickSummary};
pub use tick::{Config, Ecology, GoalSpec, SpawnError, TickReport};
pub use why::{
    SpawnError as WhySpawnError, SpawnState, StateCounts, Transition, WhyLedger, WhyState,
};

/// Crate version (kept in sync with `Cargo.toml` by the release checklist).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
