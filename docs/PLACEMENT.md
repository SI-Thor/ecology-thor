# PLACEMENT.md — where compute happens, and why

*The AGX Thor is one LPDDR5X pool with fourteen cores, a 20-SM Blackwell
GPU and some fixed-function engines (see THOR.md). Placement is the
design. This page is the standing map — updated when measurements on the
box say so, not before.*

## The one-sentence policy

**The control plane never leaves the pinned Arm cores; the data-plane
columns run wherever they are cheapest at the current population; the
GPU's big blocks belong to the world (infinigen), not the scheduler.**

## Mechanism-by-mechanism placement

| Mechanism | Shape | v0.1 placement | Why | Trigger to move |
|---|---|---|---|---|
| Necrology append, chain hash | O(transitions) | CPU (tick thread) | Cheap, strictly ordered, must never reorder (chain) | Never |
| Attestation (`heartbeat`, `run_under`) | O(depth) per call | CPU | Depth ≤ 5 in practice; a chain walk is nanoseconds | Never |
| Cascade propagation | O(subtree), rate-limited | CPU (budget per tick) | Bounded by `cascade_budget`; a GPU version would add launch latency > savings | Populations where pending queue backs up for > 1k ticks |
| Chain-aggregate justification | O(N), level-synchronous | CPU below ~10k entities; **GPU above** | The hinge mechanism: per-level parallelism maps 1:1 to kernel launches. At 3k entities (realistic world-gen) CPU is 130 µs; at 100k on this 2-vCPU dev box the sweep+tick hits ~8 ms — GPU territory | Measured: population × tick budget |
| Auction scoring (the product u·j·o·n·ev) | O(N), embarrassingly parallel | Same threshold as aggregates | One grid-stride kernel; bandwidth-bound stream of SoA columns | Same |
| Auction selection (top-K by class/score/id) | O(C log C) | CPU | K is tens; C is the candidate set; a 100k-key sort is ~ms — but a heap/partial select is the fix if it ever shows (see roadmap), not the GPU | If C·log C > 2 ms sustained |
| Reap, decay | O(N), independent | Same threshold | Trivially parallel columns | Same |
| Telemetry summary | one struct | CPU, seqlock publish | Two atomic stores; done | Never |
| Journal overflow accounting | counters | CPU | Arithmetic | Never |
| Score stats (sum/max) | O(N) reduction | CPU (sequential) or GPU (block reduce + monotonic-int atomicMax) | Telemetry-only — the hand PTX microkernel `warp_max_f32.ptx` exists to make this path cheap and to document the trick | Whenever stats join the device sweep pass |

## The zero-copy table (the Tegra superpower)

On a discrete GPU, "CPU computes, GPU helps" costs PCIe round trips —
nobody offloads a 100 µs job across a 25 GB/s toll booth. On Thor there
is no toll booth: `cuMemAllocManaged` memory is the same physical
LPDDR5X the Arm cores cache-coherently read. The device sweeper's
`mirror_from` is a `memcpy` at DRAM speed; results are read back the same
way. The design consequence is severe and pleasant: **the threshold for
offloading is "is the kernel launch overhead (~5–10 µs) smaller than the
CPU time saved", not "is it worth a data transfer"**.

That is why the *same* code has three interchangeable implementations
(ledger / reference / kernel): placement is a runtime/feature decision,
not a rewrite.

## Sharing the GPU with the actual world

The 20-SM Blackwell also runs Cycles renders (98% GPU utilization for
minutes at a time — the team's measured numbers). Our sweeps are designed
to coexist with that:

1. **They are tiny.** An O(100k) f32 column walk at 273 GB/s is ~tens of
   microseconds. The render loses one frame's worth of nothing.
2. **They are periodic, not sporadic** (every auction/sweep bucket), so
   the render pipeline's scheduler can reason about them.
3. **The arbiter of *render* admission is the auction itself** — the
   rails hold gpu_seconds, and a Cycles render bids in exactly that
   currency (that is the infinigen-thor proposal in one sentence).
4. **MIG is the escalation path** if interference is ever measured:
   Thor's GPU splits into 2 GPC-granular instances (4+6 TPCs); park the
   ecology's sweeps (and only them) in the small instance. v0.1 does not
   do this — it is a config change on the box, documented here so nobody
   forgets it exists. (Early firmware had 1g-profile hang bugs; verify on
   your L4T before relying on it.)

## Engines we deliberately ignore (for now)

- **Tensor cores / FP4 / FP8**: the box's headline numbers are for the
  LLM crowd. We use none of it — attention here is a market, not a
  matmul. If a learned bid policy ever lands, its inference belongs on
  the cloud side (cents rail), not on the render GPU.
- **PVA v3 / VIC**: fixed-function image engines. Irrelevant to v0.1;
  relevant later if world-telemetry (heatmaps, occupancy maps) becomes a
  raster-shaped workload.
- **NVENC/NVDEC**: streaming the world view out and asset video in —
  a thor-world concern before it is ours.
- **R52 safety island**: not reachable from Jetson Linux; our watchdog is
  software (docs/RT.md). If the project ever moves to DRIVE OS, the
  kernel's frame discipline is already R52-shaped.

## Roadmap (placement work, in order)

1. **On-device parity + timing pass** (`make test-device`): the golden
   moment — device vs CPU, bit for bit, per kernel, per population size.
2. **Device-authoritative sweeps** (v0.2): the ledger writes *through*
   the unified tables; the mirror disappears; the CPU keeps selection and
   journal. Cuts the auction-bucket cost to launch + kernel time.
3. **Partial-select auction** (CPU): replace the full sort with a bounded
   heap when C is large — O(C log K), removes the last superlinear term.
4. **MIG partition** if and only if interference measurements demand it.
5. **CUDA Graph** capture of the per-level justify launches (D launches →
   1 graph submit) once profiling shows launch overhead matters at our
   level counts.

## Rules for changing this page

Placement claims are measurement claims. Every "CPU vs GPU" threshold in
this doc carries the population and the machine it was measured on; when
you re-measure on newer firmware, you update the numbers *with the
measurement context* (clocks pinned? which power mode? thermal state), or
you delete the claim. A stale placement map is worse than none.
