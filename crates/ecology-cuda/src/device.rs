//! The device sweeper — cudarc host shim for the Blackwell kernels.
//!
//! **Zero-copy by construction.** Every hot table lives in
//! [`UnifiedSlice`](cudarc::driver::safe::UnifiedSlice) —
//! `cuMemAllocManaged` memory. On the Thor's unified LPDDR5X this is one
//! physical pool shared by CPU and GPU: `mirror_from` is a plain host-side
//! copy at DRAM speed, with no staging buffers, no `cudaMemcpy`, no
//! pinned-memory dance. This is *the* Tegra-specific win the whole design
//! leans on (see `docs/PLACEMENT.md`).
//!
//! **v0.1 honesty note.** The CPU ledger stays the authoritative writer;
//! this sweeper mirrors state, runs the device sweeps, and exists to
//! (a) run the on-device parity gate (`tests/parity.rs`) asserting
//! bit-exact equality with the CPU reference, and (b) measure real device
//! timings per kernel (`docs/BENCHMARKS.md`). Making the unified tables
//! the *only* copy (the ledger writing through them directly) is the
//! v0.2 adoption step, taken only after parity holds on hardware.
//!
//! ## Safety
//!
//! All `unsafe` here is bounded:
//! * `alloc_unified` — sound for our element types (`u8/u32/i64/f32` are
//!   valid at every bit pattern and zero-initializable); we fill each
//!   allocation immediately after.
//! * `launch` — kernels are grid-stride over `n` elements; every pointer
//!   we pass is a `UnifiedSlice` of exactly the length the kernel indexes.
//!   The `-fmad=false` build flag guarantees the arithmetic contract
//!   (`docs/PTX_NOTES.md`).

use std::path::Path;
use std::sync::Arc;

use cudarc::driver::result::DriverError;
use cudarc::driver::safe::{
    CudaContext, CudaStream, DeviceRepr, LaunchConfig, PushKernelArg, UnifiedSlice, ValidAsZeroBits,
};
use cudarc::nvrtc::Ptx;

use ecology_core::tick::Ecology;

/// One device error, flattened for the caller.
#[derive(Debug)]
pub struct DeviceError(pub String);

impl std::fmt::Display for DeviceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ecology-cuda: {}", self.0)
    }
}

impl std::error::Error for DeviceError {}

impl From<DriverError> for DeviceError {
    fn from(e: DriverError) -> Self {
        DeviceError(format!("{e:?}"))
    }
}

type DevResult<T> = Result<T, DeviceError>;

/// The Thor has 20 SMs; 256-thread blocks give good occupancy for these
/// memory-bound sweeps without tail waste on small levels.
const BLOCK: u32 = 256;

fn grid_for(n: u32) -> (u32, u32, u32) {
    // Cap the grid at 8 blocks/SM — these are grid-stride kernels; more
    // blocks than that just adds launch overhead on a 20-SM part.
    let blocks = n.div_ceil(BLOCK).min(8 * 20);
    (blocks.max(1), 1, 1)
}

fn cfg_for(n: u32) -> LaunchConfig {
    LaunchConfig {
        grid_dim: grid_for(n),
        block_dim: (BLOCK, 1, 1),
        shared_mem_bytes: 0,
    }
}

// -- small Result-returning helpers over cudarc's API -----------------------

fn as_slice_err(e: DriverError) -> DeviceError {
    DeviceError(format!("unified access: {e:?}"))
}

/// The whole device-side state of one population.
pub struct DeviceSweeper {
    ctx: Arc<CudaContext>,
    stream: Arc<CudaStream>,

    f_fresh: cudarc::driver::safe::CudaFunction,
    f_justify: cudarc::driver::safe::CudaFunction,
    f_reap: cudarc::driver::safe::CudaFunction,
    f_decay: cudarc::driver::safe::CudaFunction,
    f_score: cudarc::driver::safe::CudaFunction,
    f_stats: cudarc::driver::safe::CudaFunction,

    n: u32,
    /// Flat depth-ordered node indices + per-level (offset, len).
    nodes: UnifiedSlice<u32>,
    level_ranges: Vec<(u32, u32)>,

    // -- mirrored hot tables (unified: CPU writes, GPU reads) --------------
    state: UnifiedSlice<u8>,
    last_beat: UnifiedSlice<i64>,
    born: UnifiedSlice<u32>,
    parent: UnifiedSlice<u32>,
    /// Harvest counts (parole rungs) — u8 column, the same shape the
    /// ledger's `harvests_slice` produces; the kernel derives each link's
    /// `2^-h` cap in-register.
    harvests: UnifiedSlice<u8>,
    mass: UnifiedSlice<f32>,
    present: UnifiedSlice<u8>,
    urgency: UnifiedSlice<f32>,
    opportunity: UnifiedSlice<f32>,
    novelty: UnifiedSlice<f32>,
    expected_value: UnifiedSlice<f32>,

    // -- device-computed tables (GPU writes, CPU reads) --------------------
    fresh: UnifiedSlice<f32>,
    chain_alive: UnifiedSlice<u32>,
    chain_len: UnifiedSlice<u32>,
    chain_min_fresh: UnifiedSlice<f32>,
    chain_dead: UnifiedSlice<u8>,
    /// Weakest parole cap on the path's ALIVE links (f32, 1.0 = uncapped).
    chain_parole_cap: UnifiedSlice<f32>,
    justification: UnifiedSlice<f32>,
    score: UnifiedSlice<f32>,

    // -- scalar outs (atomics land here; host resets + reads) ---------------
    reaped: UnifiedSlice<u32>,
    vetoed: UnifiedSlice<u32>,
    stats_sum: UnifiedSlice<f32>,
    stats_max: UnifiedSlice<f32>,
}

impl DeviceSweeper {
    /// Attach to device 0, load `ecology.ptx` (built by
    /// `make -f kernels/Makefile`), allocate unified tables for a
    /// population of `n` with the given level plan.
    pub fn new(ptx_path: &Path, n: usize, levels: &[Vec<u32>]) -> DevResult<Self> {
        let ctx = CudaContext::new(0)?;
        let stream = ctx.new_stream()?;
        let module = ctx.load_module(Ptx::from_file(ptx_path))?;

        let f = |name: &str| -> DevResult<_> { Ok(module.load_function(name)?) };
        let f_fresh = f("link_freshness_kernel")?;
        let f_justify = f("justify_level_kernel")?;
        let f_reap = f("reap_kernel")?;
        let f_decay = f("decay_kernel")?;
        let f_score = f("auction_score_kernel")?;
        let f_stats = f("score_stats_kernel")?;

        // SAFETY for every alloc below: element types are plain
        // integers/floats (valid and zero-able at every bit pattern); each
        // slice is filled immediately so no uninitialized value is read.
        unsafe fn alloc_filled<T: DeviceRepr + ValidAsZeroBits + Copy>(
            ctx: &Arc<CudaContext>,
            len: usize,
            fill: T,
        ) -> DevResult<UnifiedSlice<T>> {
            let mut s = ctx.alloc_unified::<T>(len.max(1), true)?;
            s.as_mut_slice().map_err(as_slice_err)?.fill(fill);
            Ok(s)
        }

        unsafe {
            // Flat nodes + level ranges.
            let flat: Vec<u32> = levels.iter().flatten().copied().collect();
            let level_ranges: Vec<(u32, u32)> = {
                let mut out = Vec::with_capacity(levels.len());
                let mut off = 0u32;
                for lv in levels {
                    out.push((off, lv.len() as u32));
                    off += lv.len() as u32;
                }
                out
            };
            let mut nodes = alloc_filled(&ctx, flat.len(), 0u32)?;
            nodes.as_mut_slice().map_err(as_slice_err)?[..flat.len()].copy_from_slice(&flat);

            let n = n.max(1);
            Ok(DeviceSweeper {
                f_fresh,
                f_justify,
                f_reap,
                f_decay,
                f_score,
                f_stats,
                ctx: ctx.clone(),
                stream,
                n: n as u32,
                nodes,
                level_ranges,
                state: alloc_filled(&ctx, n, 0u8)?,
                last_beat: alloc_filled(&ctx, n, 0i64)?,
                born: alloc_filled(&ctx, n, 0u32)?,
                parent: alloc_filled(&ctx, n, u32::MAX)?,
                harvests: alloc_filled(&ctx, n, 0u8)?,
                mass: alloc_filled(&ctx, n, 0.0f32)?,
                present: alloc_filled(&ctx, n, 0u8)?,
                urgency: alloc_filled(&ctx, n, 0.0f32)?,
                opportunity: alloc_filled(&ctx, n, 0.0f32)?,
                novelty: alloc_filled(&ctx, n, 0.0f32)?,
                expected_value: alloc_filled(&ctx, n, 0.0f32)?,
                fresh: alloc_filled(&ctx, n, 0.0f32)?,
                chain_alive: alloc_filled(&ctx, n, 0u32)?,
                chain_len: alloc_filled(&ctx, n, 1u32)?,
                chain_min_fresh: alloc_filled(&ctx, n, 1.0f32)?,
                chain_dead: alloc_filled(&ctx, n, 0u8)?,
                chain_parole_cap: alloc_filled(&ctx, n, 1.0f32)?,
                justification: alloc_filled(&ctx, n, 0.0f32)?,
                score: alloc_filled(&ctx, n, 0.0f32)?,
                reaped: alloc_filled(&ctx, 1, 0u32)?,
                vetoed: alloc_filled(&ctx, 1, 0u32)?,
                stats_sum: alloc_filled(&ctx, 1, 0.0f32)?,
                stats_max: alloc_filled(&ctx, 1, 0.0f32)?,
            })
        }
    }

    /// Copy the organism's hot tables into unified memory (plain host
    /// writes at LPDDR speed — no CUDA API involved on Tegra).
    pub fn mirror_from(&mut self, eco: &Ecology) -> DevResult<()> {
        let n = self.n as usize;

        {
            let dst = self.state.as_mut_slice().map_err(as_slice_err)?;
            for (i, s) in eco.states_slice().iter().take(n).enumerate() {
                dst[i] = *s as u8;
            }
        }
        {
            let dst = self.last_beat.as_mut_slice().map_err(as_slice_err)?;
            dst[..n].copy_from_slice(&eco.last_beat_slice()[..n]);
        }
        {
            let dst = self.born.as_mut_slice().map_err(as_slice_err)?;
            dst[..n].copy_from_slice(&eco.born_slice()[..n]);
        }
        {
            let dst = self.parent.as_mut_slice().map_err(as_slice_err)?;
            dst[..n].copy_from_slice(&eco.parent_slice()[..n]);
        }
        {
            let dst = self.harvests.as_mut_slice().map_err(as_slice_err)?;
            dst[..n].copy_from_slice(&eco.harvests_slice()[..n]);
        }
        {
            let dst = self.mass.as_mut_slice().map_err(as_slice_err)?;
            dst[..n].copy_from_slice(&eco.mass_slice()[..n]);
        }
        {
            let dst = self.present.as_mut_slice().map_err(as_slice_err)?;
            for (i, p) in eco.present_slice().iter().take(n).enumerate() {
                dst[i] = *p as u8;
            }
        }
        {
            let dst = self.urgency.as_mut_slice().map_err(as_slice_err)?;
            dst[..n].copy_from_slice(&eco.urgency_slice()[..n]);
        }
        {
            let dst = self.opportunity.as_mut_slice().map_err(as_slice_err)?;
            dst[..n].copy_from_slice(&eco.opportunity_slice()[..n]);
        }
        {
            let dst = self.novelty.as_mut_slice().map_err(as_slice_err)?;
            dst[..n].copy_from_slice(&eco.novelty_slice()[..n]);
        }
        {
            let dst = self.expected_value.as_mut_slice().map_err(as_slice_err)?;
            dst[..n].copy_from_slice(&eco.expected_value_slice()[..n]);
        }
        // Atomic counters reset for this round.
        self.reaped.as_mut_slice().map_err(as_slice_err)?[0] = 0;
        self.vetoed.as_mut_slice().map_err(as_slice_err)?[0] = 0;
        self.stats_sum.as_mut_slice().map_err(as_slice_err)?[0] = 0.0;
        self.stats_max.as_mut_slice().map_err(as_slice_err)?[0] = 0.0;
        Ok(())
    }

    /// `link_freshness_kernel`.
    pub fn sweep_freshness(&mut self, now: u32, horizon: u32) -> DevResult<()> {
        let cfg = cfg_for(self.n);
        // SAFETY: all pointers are unified slices of length n; scalars match
        // the kernel signature in kernels/ecology.cu.
        unsafe {
            self.stream
                .launch_builder(&self.f_fresh)
                .arg(&self.n)
                .arg(&now)
                .arg(&horizon)
                .arg(&self.state)
                .arg(&self.last_beat)
                .arg(&self.born)
                .arg(&self.fresh)
                .launch(cfg)?;
        }
        Ok(())
    }

    /// `justify_level_kernel` for every level, in order (parents first —
    /// the level plan is the dependency graph). The parole cap is not a
    /// parameter any more: the kernel derives each link's `2^-harvests`
    /// in-register, so the ladder's arithmetic has exactly one
    /// definition — `ecology_core::why::parole_cap` — mirrored by bits.
    pub fn sweep_justify(&mut self) -> DevResult<()> {
        for &(off, len) in &self.level_ranges {
            if len == 0 {
                continue;
            }
            let cfg = cfg_for(len);
            // This level's slice of the flat node array. The kernel reads
            // nodes[0..len) from this base — the offset IS the base.
            let level_nodes = self.nodes.slice((off as usize)..((off + len) as usize));
            // SAFETY: `level_nodes` covers exactly this level; the kernel
            // only writes these entries, whose parents (previous launches)
            // are already final. All other pointers are length-n tables.
            unsafe {
                self.stream
                    .launch_builder(&self.f_justify)
                    .arg(&len)
                    .arg(&level_nodes)
                    .arg(&self.parent)
                    .arg(&self.state)
                    .arg(&self.harvests)
                    .arg(&self.fresh)
                    .arg(&self.chain_alive)
                    .arg(&self.chain_len)
                    .arg(&self.chain_min_fresh)
                    .arg(&self.chain_dead)
                    .arg(&self.chain_parole_cap)
                    .arg(&self.justification)
                    .launch(cfg)?;
            }
        }
        Ok(())
    }

    /// `reap_kernel`. Returns the number lapsed on device.
    pub fn sweep_reap(&mut self, now: u32, horizon: u32) -> DevResult<u32> {
        let cfg = cfg_for(self.n);
        // SAFETY: as sweep_freshness; `reaped` is length 1 and zeroed in
        // mirror_from.
        unsafe {
            self.stream
                .launch_builder(&self.f_reap)
                .arg(&self.n)
                .arg(&now)
                .arg(&horizon)
                .arg(&self.state)
                .arg(&self.last_beat)
                .arg(&self.born)
                .arg(&self.reaped)
                .launch(cfg)?;
        }
        self.synchronize();
        Ok(self.reaped.as_slice().map_err(as_slice_err)?[0])
    }

    /// `decay_kernel`.
    pub fn sweep_decay(&mut self, lambda: f32, dt: u32) -> DevResult<()> {
        let cfg = cfg_for(self.n);
        // SAFETY: as above.
        unsafe {
            self.stream
                .launch_builder(&self.f_decay)
                .arg(&self.n)
                .arg(&lambda)
                .arg(&dt)
                .arg(&self.state)
                .arg(&self.mass)
                .launch(cfg)?;
        }
        Ok(())
    }

    /// `auction_score_kernel`. Returns the vetoed count.
    pub fn sweep_score(&mut self) -> DevResult<u32> {
        let cfg = cfg_for(self.n);
        // SAFETY: as above.
        unsafe {
            self.stream
                .launch_builder(&self.f_score)
                .arg(&self.n)
                .arg(&self.state)
                .arg(&self.present)
                .arg(&self.urgency)
                .arg(&self.justification)
                .arg(&self.opportunity)
                .arg(&self.novelty)
                .arg(&self.expected_value)
                .arg(&self.score)
                .arg(&self.vetoed)
                .launch(cfg)?;
        }
        self.synchronize();
        Ok(self.vetoed.as_slice().map_err(as_slice_err)?[0])
    }

    /// `score_stats_kernel` → (sum, max). Telemetry only.
    pub fn sweep_stats(&mut self) -> DevResult<(f32, f32)> {
        let cfg = cfg_for(self.n);
        // SAFETY: score has n entries; out arrays are length 1.
        unsafe {
            self.stream
                .launch_builder(&self.f_stats)
                .arg(&self.n)
                .arg(&self.score)
                .arg(&self.stats_sum)
                .arg(&self.stats_max)
                .launch(cfg)?;
        }
        self.synchronize();
        Ok((
            self.stats_sum.as_slice().map_err(as_slice_err)?[0],
            self.stats_max.as_slice().map_err(as_slice_err)?[0],
        ))
    }

    /// Wait for all launched work. (On Tegra, host reads of unified memory
    /// written by the GPU must be ordered after the stream — this is that
    /// order; see `docs/PTX_NOTES.md` §fences.)
    pub fn synchronize(&self) {
        let _ = self.stream.synchronize();
    }

    // -- read-backs (post-synchronize) ---------------------------------------

    /// Copy the device justification column out of unified memory.
    pub fn justification(&self) -> DevResult<Vec<f32>> {
        Ok(self.justification.as_slice().map_err(as_slice_err)?[..self.n as usize].to_vec())
    }

    /// Copy the device score column out of unified memory.
    pub fn score(&self) -> DevResult<Vec<f32>> {
        Ok(self.score.as_slice().map_err(as_slice_err)?[..self.n as usize].to_vec())
    }

    /// Copy the device state column (WhyState bytes) out of unified memory.
    pub fn state_bytes(&self) -> DevResult<Vec<u8>> {
        Ok(self.state.as_slice().map_err(as_slice_err)?[..self.n as usize].to_vec())
    }

    /// Copy the device mass column out of unified memory.
    pub fn mass(&self) -> DevResult<Vec<f32>> {
        Ok(self.mass.as_slice().map_err(as_slice_err)?[..self.n as usize].to_vec())
    }

    /// Context handle (for callers that want to manage MIG or priorities).
    pub fn context(&self) -> &Arc<CudaContext> {
        &self.ctx
    }
}
