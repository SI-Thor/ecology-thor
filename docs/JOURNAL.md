# JOURNAL.md — the build log

*Working notes, decisions, and session records. Newest last. This file is
the project's memory of *why* — the code says what, this says why.*

---

## Session 1 — 2026-11-01 — the founding round

**Mission** (from the owner): a purpose-built repo for SI-Thor that is a
better version of the cognitive-ecology work, designed specifically and
optimally for the AGX Thor's hardware abilities — Rust where it fits,
CUDA/PTX for key mechanisms, careful memory review throughout, python
only where it isn't slow. Plus: study the three sibling repos
(infinigen-thor, thor-world, builder-ide), leave thorough proposals, and
branch actual code where the ideas are specific enough.

### Research round (six parallel agents)

Key corrections the research forced (all verified, sources in THOR.md):

1. **Thor is not a bigger Orin.** Blackwell **sm_110** (not sm_87),
   JetPack **7**/CUDA 13 (not 6/12.x), 14× **Neoverse-V3AE** (not
   A78AE), 128 GB LPDDR5X @ **273 GB/s** unified, **no DLA**, **no
   OptiX/RT cores**, R52 island not reachable from Jetson Linux,
   NVLink-C2C not exposed on the devkit. Every one of these would have
   been a design error baked in on day one.
2. **The sibling repos had already met us.** builder-ide vendored our
   WHY kernel (`core/why`, golden-corpus tested) and wrote a fit study
   that deferred the tick loop + attention economy while naming the
   exact condition under which they'd want it ("bidding for actual Thor
   GPU-minutes… BI's rail design is the missing currency exchange").
   infinigen-thor has on-box measurements (Cycles 98% GPU, ~3 GB/scene,
   the EXACT-Boolean hang class) and a SIGUSR1 checkpoint primitive
   already in tree. thor-world's purity law and door architecture is a
   ready-made publication surface.
3. **Real-time patterns worth stealing**: AUTOSAR harmonic frames, E2E
   alive-counters (≈ our heartbeat ledger), Erlang restart-intensity
   (→ rate-limited cascades), IBM WLM importance classes (→
   `Importance`), seqlock summaries, SPSC/chain journals, work-stealing
   (not needed at v0.1 scale — noted).
4. **Tooling**: official PREEMPT_RT kernel packages for Thor exist;
   `jetson_clocks` + nvpmodel-1 for repeatable numbers;
   compute-sanitizer works on Tegra; the pip nvcc wheel gives headless
   ptxas for CI; cudarc 0.19 (driver feature + cuda-13000 +
   dynamic-loading) is the right Rust CUDA stack today; Rust→PTX
   (nvptx64) still not production-viable.

### Decisions (and their one-line reasons)

- **Zero-dependency core** (`ecology-core`): everything in the RT loop
  is reviewable in-repo; that's the memory-safety story (SAFETY.md).
- **Trees, not DAGs**: makes chain justification one level-synchronous
  O(N) sweep — the exact shape that parallelizes per level on the GPU.
  The parent repo's own docstring blesses the tree case as the exact
  score.
- **Rails = (gpu_seconds, joules, cents)**: the currencies builder-ide
  already prices; the auction becomes their currency exchange.
- **Justification = faithful v0.3 port** (weakest-link freshness, grace
  horizon/3, linear to 0; probation cap 0.5 ended by service): the
  semantics are the product; the Thor work is placement, not re-design.
- **exp2 as shared Taylor cubic, not `ex2.approx`**: bit-exactness
  across rustc/nvcc beats 2-ulp hardware speed for the *authoritative*
  path (PTX_NOTES.md). The hardware instruction stays in the hand-PTX
  showcase + telemetry only.
- **One hand-written PTX kernel, one job** (`warp_max_f32.ptx`):
  documents the monotonic-int-transform + integer redux trick that the
  whole sm_80+ line supports. 10 registers, 0 spills, CI-assembled.
- **Device path feature-gated, CPU authoritative in v0.1**: parity first
  (on hardware), adoption second (v0.2). The mirror buffers are already
  the same shape the ledger would write through.
- **Seqlock summary, no threads in core**: single-writer law; telemetry
  never blocks the tick; the only `unsafe` in the crate.
- **Golden chain hash** (`tests/golden.rs`): determinism is a contract;
  `BLESS=1` makes every exception a conscious commit.

### What was built

`ecology-core` (30 tests) · `ecology-cuda` (reference + kernels + PTX +
shim, compiles with and without the cuda feature) · `ecology-sim`
(world-gen + stress profiles, benchmark reports) · `ecology-py` (abi3
bindings) · docs (THOR, ARCHITECTURE, PLACEMENT, RT, PTX_NOTES, SAFETY,
BENCHMARKS, INTEGRATION) · CI (fmt/clippy/tests, aarch64 check, PTX
gate, CUDA-container compile, bindings check) · tools
(check_secrets.py, thor-tune.sh).

Benchmark floor (2-vCPU sandbox, BENCHMARKS.md): world-gen at realistic
population = **130 µs/auction-tick**; stress at 100k = 7.8 ms — the
case the device sweep is for.

### Open threads (next sessions)

1. On-device pass: parity + per-kernel timings + sanitizer run (the
   PLACEMENT thresholds become measured crossovers).
2. v0.2 device-authoritative sweeps; partial-select auction on CPU.
3. The three proposal branches land their reviews with the sibling
   teams; the builder-ide `ecology_bridge` dogfoods the rails mapping.
4. Durable journal (replay basis is already complete; needs a sink +
   snapshot anchor scheme).
5. Consider `world://why` mock data generator for builder-ide's UI
   before the door exists (the JSON contract is frozen — they can build
   against it today).
