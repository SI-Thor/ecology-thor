# BENCHMARKS.md — protocol and honest numbers

*Every number here carries its machine, build, and population. Sandbox
numbers are indicative: they come from a 2-vCPU x86_64 box that is
~1/7th of the Thor's CPU — they set the floor, not the ceiling.
On-device numbers land in the "AGX Thor (measured)" table as they are
taken; until then those rows say NOT YET MEASURED, not guesses.*

## Protocol

1. **Build**: `cargo build --release` (codegen-units=1, thin LTO — see
   `Cargo.toml`; single CGU removes a variance source).
2. **Clocks pinned** on the box being measured (`tools/thor-tune.sh`;
   on x86 dev boxes, just close your browser).
3. **Run**: `make bench` (stress at 10k/50k/100k + world-gen at 50k
   capacity). Reports land in `data/` as JSON.
4. **Read**: p50/p95/max per frame class (plain / auction / sweep).
   The interesting number is auction-p95 vs the 8 ms frame budget.
5. **Context required when quoting**: machine, population, power mode,
   clock state. No naked numbers.

## x86_64 dev sandbox (this repo's CI-class machine)

*2 vCPU, release, seed 42, 30k ticks (stress) / 6k ticks (world-gen).
The stress profile makes every entity bid and stay fresh — the worst
case for the auction scan. World-gen is the realistic shape (~3.2k live
entities, stalls and revives flowing).*

| Profile | Population | plain p50 | plain p95 | auction p50 | auction p95 | sweep p50 | sweep p95 |
|---|---|---|---|---|---|---|---|
| stress | 10,000 | 5.6 µs | 6.1 µs | 555 µs | 613 µs | 591 µs | 647 µs |
| stress | 50,000 | 28 µs | 33 µs | 3.68 ms | 3.98 ms | 3.88 ms | 4.18 ms |
| stress | 100,000 | 56 µs | 65 µs | 7.82 ms | 8.19 ms | 8.21 ms | 8.56 ms |
| world-gen | ~3,200 live (50k capacity) | 28 µs | 31 µs | 130 µs | 201 µs | 170 µs | 243 µs |

### Reading the table

- **Realistic world-gen costs 130 µs per auction tick on a 2-vCPU box**
  — 1.6% of the 8 ms budget. The kernel is effectively free at the
  populations the world builder actually runs.
- The stress profile's superlinear growth is the **candidate sort**
  (O(C log C)), not the scan: at 100k candidates it dominates. Two
  mitigations, in order: (1) partial-select (bounded heap, O(C log K))
  on CPU — roadmap item, `attention.rs`; (2) move scoring+selection
  threshold to the device sweep (PLACEMENT.md) — the scan is
  embarrassingly parallel and the sort shrinks to K.
- The `max` column (241 µs–17 ms) is scheduler noise on a shared
  sandbox — the p95s are the honest signal. On an isolated Thor with
  pinned clocks, expect max≈p95×1.2 (that's the claim to verify).

## AGX Thor (measured)

*To be filled by `make bench` on the box, after `tools/thor-tune.sh`
(mode 1 + jetson_clocks for stability; also record a MAXN pass for
contrast). Population ladder: 10k / 50k / 100k / 1M capacity. Then the
device-sweep ladder: `make test-device` runs the parity pass and prints
per-kernel timings that slot into the PLACEMENT.md threshold table.*

| Profile | Population | plain p50 | auction p50 | sweep p50 | device sweep (all kernels) |
|---|---|---|---|---|---|
| world-gen | 3.2k | NOT YET MEASURED | | | |
| stress | 100k | NOT YET MEASURED | | | |
| stress | 1M | NOT YET MEASURED | | | |

## What we will measure on the box, first pass

1. The CPU ladder above (five minutes with `make bench`).
2. `make test-device` — parity (bit-exact, all kernels) + per-kernel
   device timings at 10k/100k/1M, alongside the CPU reference numbers.
   The placement thresholds in PLACEMENT.md get replaced by measured
   crossover points.
3. `compute-sanitizer --tool memcheck` over the parity run (SAFETY.md).
4. `nsys profile` of 10k ticks at 100k population — the timeline
   picture for the frame budget, including our slices vs Cycles'.
5. Optional: MIG partition A/B (sweeps isolated in the small instance)
   — only if interference shows in 4.

## Regression discipline

- `ecology-sim --golden` prints the chain hash; determinism regressions
  are caught by `tests/golden.rs` before any benchmark matters.
- Perf-sensitive PRs quote before/after p50s for the affected frame
  class at 10k and 100k, in context. "Faster" without a number and a
  population is not a reviewable claim.
