//! # ecology-thor (Python bindings)
//!
//! Thin pyo3 bindings for the kernel — the **orchestration edge**, never
//! the hot path. The intended shape (see `docs/INTEGRATION.md`):
//!
//! * Python builds worlds, files bids, reads snapshots — everything a
//!   world-mcp tool or a builder-ide experiment needs;
//! * the Rust `Ecology` owns the tick loop, and every semantic that must
//!   not drift (conservation laws, veto law, freshness) stays in Rust.
//!
//! Surface rules:
//! * ids cross the bridge as plain `int` (they're slot indices, stable
//!   for the process lifetime — never recycled);
//! * `tick()` returns the tick's summary as JSON (the same stable shape
//!   `Ecology::snapshot_json` produces for the IDE overlay);
//! * importance classes cross as strings: `"kernel" | "world" | "scenery"
//!   | "maintenance"`.
//!
//! Build on the Thor: `maturin build --release` (or `make python` from the
//! repo root), then `pip install <wheel>`.

use ecology_core::attention::{Bid, Importance};
use ecology_core::ids::EntityId;
use ecology_core::rails::Rails;
use ecology_core::tick::{Config, Ecology, GoalSpec};
use ecology_core::why::WhyState;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

fn parse_class(s: &str) -> PyResult<Importance> {
    match s {
        "kernel" => Ok(Importance::Kernel),
        "world" => Ok(Importance::World),
        "scenery" => Ok(Importance::Scenery),
        "maintenance" => Ok(Importance::Maintenance),
        other => Err(PyValueError::new_err(format!(
            "unknown importance class {other:?} (expected kernel|world|scenery|maintenance)"
        ))),
    }
}

fn state_name(s: WhyState) -> &'static str {
    match s {
        WhyState::Alive => "alive",
        WhyState::Dead => "dead",
        WhyState::Unknown => "unknown",
        WhyState::Absent => "absent",
    }
}

/// The organism. All mutating methods must be called from one thread (the
/// single-writer law) — pyo3 enforces this by holding `&mut self`.
#[pyclass]
struct EcologyPy {
    eco: Ecology,
}

#[pymethods]
impl EcologyPy {
    /// Build the organism.
    ///
    /// Knobs (all optional, kernel defaults otherwise):
    ///   capacity          arena slots (default 65_536; ~1M ≈ 40 MB)
    ///   horizon_ticks     re-attestation horizon (default 900)
    ///   auction_every     auctions per N ticks (default 10)
    ///   sweep_every       sweeps per N ticks (default 100)
    ///   top_k             max non-kernel admissions per auction (default 32)
    ///   gpu_refill        GPU-seconds refilled per tick (default 1.0)
    ///   joules_refill     joules refilled per tick (default 12.0)
    ///   cents_refill      cents refilled per tick (default 0.5)
    #[new]
    #[pyo3(signature = (
        capacity = 65_536, horizon_ticks = 900, auction_every = 10, sweep_every = 100,
        top_k = 32, gpu_refill = 1.0, joules_refill = 12.0, cents_refill = 0.5
    ))]
    fn new(
        capacity: usize,
        horizon_ticks: u32,
        auction_every: u32,
        sweep_every: u32,
        top_k: usize,
        gpu_refill: f32,
        joules_refill: f32,
        cents_refill: f32,
    ) -> PyResult<Self> {
        let cfg = Config {
            capacity,
            horizon_ticks,
            auction_every,
            sweep_every,
            auction_top_k: top_k,
            rails_refill: Rails {
                gpu_seconds: gpu_refill,
                joules: joules_refill,
                cents: cents_refill,
            },
            ..Config::default()
        };
        Ok(EcologyPy { eco: Ecology::new(cfg) })
    }

    /// Spawn a purpose; returns its stable id.
    #[pyo3(signature = (parent = None, mass = 1.0, until = None, alive = true))]
    fn spawn(&mut self, parent: Option<u32>, mass: f32, until: Option<u32>, alive: bool) -> PyResult<u32> {
        let spec = GoalSpec {
            parent: parent.map(EntityId),
            mass,
            until,
            alive,
        };
        match self.eco.spawn(spec) {
            Ok(id) => Ok(id.0),
            Err(e) => Err(PyValueError::new_err(format!("spawn refused: {e:?}"))),
        }
    }

    /// Heartbeat one node (refuses non-alive nodes and stale beats).
    fn heartbeat(&mut self, id: u32) -> bool {
        self.eco.heartbeat(EntityId(id))
    }

    /// Re-attest by use: stamp the node and its whole chain to the root.
    /// Returns how many nodes were stamped.
    fn run_under(&mut self, id: u32) -> u32 {
        self.eco.run_under(EntityId(id))
    }

    /// Kill a node (cascades living descendants to UNKNOWN, rate-limited).
    /// Returns conversions done in this call.
    fn kill(&mut self, id: u32) -> u32 {
        self.eco.kill(EntityId(id))
    }

    /// UNKNOWN → ALIVE (on parole).
    fn revive(&mut self, id: u32) -> bool {
        self.eco.revive(EntityId(id))
    }

    /// DEAD → ALIVE (substantive new evidence).
    fn resurrect(&mut self, id: u32) -> bool {
        self.eco.resurrect(EntityId(id))
    }

    /// ALIVE → UNKNOWN (outside doubt; no cascade).
    fn doubt(&mut self, id: u32) -> bool {
        self.eco.doubt(EntityId(id))
    }

    /// Place or replace a bid. Returns false for an out-of-range id.
    #[pyo3(signature = (
        id, urgency, opportunity, novelty, expected_value,
        class = "world", gpu_seconds = 0.0, joules = 0.0, cents = 0.0
    ))]
    fn set_bid(
        &mut self,
        id: u32,
        urgency: f32,
        opportunity: f32,
        novelty: f32,
        expected_value: f32,
        class: &str,
        gpu_seconds: f32,
        joules: f32,
        cents: f32,
    ) -> PyResult<bool> {
        let class = parse_class(class)?;
        Ok(self.eco.set_bid(
            EntityId(id),
            Bid {
                urgency,
                opportunity,
                novelty,
                expected_value,
                class,
                cost: Rails { gpu_seconds, joules, cents },
            },
        ))
    }

    /// Withdraw a bid.
    fn clear_bid(&mut self, id: u32) {
        self.eco.clear_bid(EntityId(id));
    }

    /// Advance the world one tick; returns the tick summary JSON (the
    /// stable contract shape from `docs/INTEGRATION.md`).
    fn tick(&mut self) -> String {
        self.eco.tick();
        self.eco.snapshot_json()
    }

    /// Feed back the frame measurement (µs) — drives safe mode.
    fn note_frame_time(&mut self, micros: u64) {
        self.eco.note_frame_time(micros);
    }

    /// Latest summary as JSON (no tick).
    fn snapshot_json(&self) -> String {
        self.eco.snapshot_json()
    }

    /// Journal chain hash (hex) — the determinism anchor.
    fn chain_hash(&self) -> String {
        format!("{:016x}", self.eco.chain_hash())
    }

    /// State of one entity: "alive" | "dead" | "unknown" | "absent".
    fn state(&self, id: u32) -> String {
        match self.eco.state(EntityId(id)) {
            Some(s) => state_name(s).to_string(),
            None => "absent".to_string(),
        }
    }

    /// Chain-aggregated justification (0..1) from the last sweep.
    fn justification(&self, id: u32) -> f32 {
        self.eco.justification(EntityId(id))
    }

    /// Last computed attention score.
    fn score(&self, id: u32) -> f32 {
        self.eco.score(EntityId(id))
    }

    /// Admitted by the most recent auction?
    fn is_admitted(&self, id: u32) -> bool {
        self.eco.is_admitted(EntityId(id))
    }

    /// Current tick counter.
    fn now(&self) -> u32 {
        self.eco.now()
    }
}

/// Kernel version string.
#[pyfunction]
fn version() -> String {
    ecology_core::VERSION.to_string()
}

#[pymodule]
fn ecology_thor(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<EcologyPy>()?;
    m.add_function(wrap_pyfunction!(version, m)?)?;
    m.add("__version__", ecology_core::VERSION)?;
    Ok(())
}
