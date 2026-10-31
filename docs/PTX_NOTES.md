# PTX_NOTES.md — the device contract, in detail

*Why the CUDA code looks the way it does, what is bit-exact and what
isn't, and where hand-written PTX earns its keep.*

## 1. The determinism policy (the load-bearing decision)

Three implementations of every sweep must agree **bit for bit**: the
ledger (Rust, authoritative), the reference mirror
(`ecology-cuda/src/reference.rs`), and the kernel
(`kernels/ecology.cu`). That is only true if the arithmetic is pinned:

| Trap | Policy |
|---|---|
| **FMA contraction** | nvcc contracts `a*b+c` into FMA by default, rustc never does. `kernels/Makefile` passes `-fmad=false` — **load-bearing, do not remove** — and the Rust side writes the same expressions without `fma` intrinsics. The exp2 polynomial's Horner form is identical text in both languages. |
| **`ex2.approx.f32`** | The PTX instruction is 2-ulp approximate and *not* bit-stable against any CPU libm. We do **not** use it on authoritative paths: mass decay uses the shared Taylor-cubic polynomial (`freshness.rs::exp2_fast` ↔ `ecology.cu::ecology_exp2_fast`). Where `ex2.approx` *would* appear, the polynomial is 3 multiplies — cheaper than the accuracy we'd lose. |
| **Multiply order** | `attention_score` is `((((u·j)·o)·n)·ev)` — left-associative, clamped operands, NaN→0. Identical text both sides. Do not "simplify" either side. |
| **Reduction order** | Device reductions (sum/max) are warp/block-shaped; the CPU reference is sequential. Float sums are **not** bit-compatible across reduction shapes — so device reductions live only on *telemetry* paths (`score_stats_kernel`), never on spend math or justification. |
| **Atomics** | `atomicAdd` on counters (reaped/vetoed) is order-free but the counters are integers — bit-exact by construction. |
| **Saturation** | `now < last_beat` guards (age clamps to 0) are explicit in both languages ("stale beats never rewind freshness" is a law, not an accident). |
| **NaN policy** | `clamp01`: `isnan(x) || x < 0 → 0`. A nonsensical bid factor *vetoes*, it never poisons the chain. Same construction in Rust and CUDA. |

The on-device parity gate (`make test-device` → `tests/parity.rs` path)
asserts all of this against real hardware before the device output is
trusted.

## 2. The one hand-written kernel: `kernels/ptx/warp_max_f32.ptx`

Eight instructions, one job: the max of 32 non-negative floats across a
warp. It exists for two reasons:

**The monotonic integer transform.** For non-negative IEEE-754 floats,
interpreting the bit pattern as `u32` preserves order (`a < b ⇔
bits(a) < bits(b)`). So a warp float-max is a *single* instruction:

```text
mov.b32      bits, v          // float bits as ordered u32
redux.sync.max.u32  best, bits, 0xffffffff
mov.b32      v, best          // back to float
```

No shuffle ladder, no shared memory, no `__syncthreads`. Scores are
clamped to [0,1] upstream, so non-negativity is a kernel invariant here.

**`redux.sync` on sm_110, correctly.** `redux.sync.op.u32` requires
sm_80+ — fine. The **f32 flavor requires sm_100a** (arch-specific, not
portable). The integer flavor + transform is the portable trick: same
speed class, works Ampere→Blackwell. The PTX targets `sm_90` as the
portable floor and the driver JITs it for sm_110 on the Thor (PTX is
forward-compatible).

Assembled headlessly in CI (`make hand-ptx-check` via the pip wheel's
ptxas): **10 registers, 0 spills, 0 stack** — as lean as designed. It is
deliberately *not* used on any bit-exactness-critical path (reduction
order is warp-shaped; the reference is sequential). `score_stats_kernel`
uses the same transform via `atomicMax` on the u32 view — the compiled
and hand-written paths agree on the trick, and cross-check each other on
device.

## 3. Kernel inventory (`kernels/ecology.cu`)

| Kernel | Mirror of | Parallel shape | Writes |
|---|---|---|---|
| `link_freshness_kernel` | `freshness::link_freshness` | grid-stride, independent | `fresh[i]` |
| `justify_level_kernel` | `WhyLedger::recompute_aggregates` (one level) | grid-stride **per level**, parents final | chain columns + `justification[i]` |
| `reap_kernel` | `WhyLedger::reap` | grid-stride, independent | `state[i]`, atomic counter |
| `decay_kernel` | `WhyLedger::decay_mass` | grid-stride, independent | `mass[i]` |
| `auction_score_kernel` | `attention::attention_score` | grid-stride, independent | `score[i]`, atomic vetoed counter |
| `score_stats_kernel` | telemetry only | block reduce + atomics | sum/max (telemetry) |

All are grid-stride loops with no dynamic allocation, no recursion, no
dynamic shared memory sizing — a frame spent in them scales with
population and nothing else. Launch config: 256-thread blocks, grid
capped at 8 blocks/SM (20 SMs on Thor): these are bandwidth-bound column
walks; more blocks is just launch overhead.

## 4. Memory & fences on Tegra

The arena lives in `cuMemAllocManaged` — on Tegra, physically the same
LPDDR5X as host RAM, cache-coherent. Consequences we rely on and
document:

- **Mirroring is a plain host copy** — no `cudaMemcpy`, no staging.
- **Host reads of GPU-written unified memory must be ordered after the
  stream**: the sweeper calls `stream.synchronize()` before every
  read-back (this is the whole fence story at our scales; no
  `fence.sc.sys` needed for our access pattern — CPU reads happen only
  after synchronize, never concurrently with kernels).
- **No L2-residency games in v0.1.** `UnifiedSlice::prefetch()` exists
  and is the obvious first experiment if read-back latency ever shows up
  in profiles; on a shared pool it helps less than on discrete parts.

## 5. Build matrix (who compiles what, where)

| Context | Toolchain | Product | Check |
|---|---|---|---|
| On the Thor | CUDA 13.x at `/usr/local/cuda` | `build/ecology.ptx` (compute_110) + cubins (sm_110) | `make kernels && make ptx-check` |
| CI, headless | pip wheel `nvidia-cuda-nvcc-cu12` (ptxas only) | hand-PTX cubin at sm_100 | `make hand-ptx-check` — syntax + registers gate |
| CI, container | `nvidia/cuda:12.9.1-devel` (full nvcc) | `ecology.ptx` from compute_110 source | compile-only; no GPU present |

The sm_110 wrinkle, documented once so nobody relearns it: CUDA 12.9's
ptxas rejects `-arch=sm_110` (Thor is CUDA 13-era). PTX *emission* for
`compute_110` works in 12.9's nvcc; assembly at sm_110 needs 13. The
hand-PTX targets sm_90 and JITs on-device. On-device builds just use
`-arch=native` (nvcc's x86 default of sm_75 produces cubins the Thor GPU
refuses — the same trap the infinigen terrain build hit).

## 6. Why not Rust→PTX (nvptx64-nvidia-cuda)?

Checked and declined: tier-3, nightly-only, `-Zbuild-std=core`, core-only
(no `alloc`), no warp intrinsics without inline asm anyway. The CUDA C++
path with `-fmad=false` gives us intrinsics, ptxas optimization and a
CI-checkable artifact today; the *host* is Rust all the way down
(cudarc, dynamic-loading). If the nvptx64 target matures, the reference
mirrors are the porting spec.
