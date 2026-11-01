//
// ecology.cu — the device sweeps for ecology-thor.
//
// Target : NVIDIA Jetson AGX Thor (T5000), Blackwell GPU, compute capability
//          11.0 (sm_110), CUDA 13.x, aarch64. Also compiles for neighboring
//          architectures by overriding ECO_ARCH.
//
// Build  : `make -f kernels/Makefile` (on device, or in CI with a headless
//          nvcc) → kernels/build/ecology.ptx + warp_max_f32.cubin checks.
//
// THE CONTRACT (docs/PTX_NOTES.md is the long version):
//
//   Every kernel here is a *bit-exact* mirror of a CPU function in
//   ecology-core (see crates/ecology-cuda/src/reference.rs). The float
//   expressions are written in plain IEEE ops, compiled with
//   `-fmad=false`, so rustc and nvcc emit the same arithmetic:
//     · same canonical multiply order in attention_score
//     · same exp2 polynomial in decay (no `ex2.approx.f32` here — that
//       instruction is 2-ulp approximate and NOT bit-stable vs the CPU;
//       we use it only in the hand-written stats microkernel where
//       exactness is not part of the contract)
//     · same saturation guards (`now < last` → age 0)
//
//   The golden parity test (`tests/parity.rs`, run on device) asserts
//   device output == CPU reference, bit for bit, before any result is
//   adopted.
//
// Layout: all arrays are the kernel's SoA tables mirrored into *unified*
// memory (cudaMallocManaged) — on Tegra the CPU and GPU share one LPDDR5X
// pool, so "mirroring" is plain host writes with zero staging copies
// (docs/PLACEMENT.md).
//
// No dynamic allocation, no malloc on device, no recursion: every kernel is
// a bounded grid-stride loop — a frame spent here has a WCET that scales
// with population and nothing else.

#include <stdint.h>

// WhyState byte values — must match crates/ecology-core/src/why.rs.
#define WHY_ALIVE    1u
#define WHY_DEAD     3u
#define WHY_UNKNOWN  2u
#define WHY_ABSENT   0u

#define NO_PARENT    0xFFFFFFFFu

// ---------------------------------------------------------------------------
// link_freshness_kernel — mirror of ecology_core::freshness::link_freshness
// ---------------------------------------------------------------------------
// Full freshness through horizon/3 (grace), then linear to 0 at horizon.
// Non-ALIVE links are neutral (1.0) — their doubt is priced by the
// alive-fraction, not by freshness (why.py semantics).
extern "C" __global__ void link_freshness_kernel(
    unsigned            n,
    unsigned            now,
    unsigned            horizon,
    const unsigned char* __restrict__ state,
    const long long*    __restrict__ last_beat,
    const unsigned*     __restrict__ born,
    float*              __restrict__ fresh)
{
    unsigned i = blockIdx.x * blockDim.x + threadIdx.x;
    const unsigned stride = gridDim.x * blockDim.x;
    const unsigned grace = horizon / 3u;
    for (; i < n; i += stride) {
        if (state[i] != WHY_ALIVE) { fresh[i] = 1.0f; continue; }
        const long long lb = last_beat[i];
        const unsigned last = (lb >= 0) ? (unsigned)lb : born[i];
        // Saturating age: a stale/future beat never rewinds freshness.
        const unsigned age = (now > last) ? (now - last) : 0u;
        if (age <= grace) {
            fresh[i] = 1.0f;
        } else if (age >= horizon) {
            fresh[i] = 0.0f;
        } else {
            // One division, same operands/order as the CPU side.
            fresh[i] = 1.0f - (float)(age - grace) / (float)(horizon - grace);
        }
    }
}

// ---------------------------------------------------------------------------
// justify_level_kernel — mirror of WhyLedger::recompute_aggregates (per level)
// ---------------------------------------------------------------------------
// Level-synchronous chain aggregation: for the level's node indices `nodes`,
// read the parent's (already-final, one level up) aggregates and write the
// child's. Parents are strictly one level up, so single in/out arrays are
// race-free. This is the level loop of the CPU sweep, one kernel per level,
// each level embarrassingly parallel — the shape that made the CPU sweep
// worth mirroring on the GPU at all.
extern "C" __global__ void justify_level_kernel(
    unsigned                 level_len,
    const unsigned*          __restrict__ nodes,   // this level's indices
    const unsigned*          __restrict__ parent,
    const unsigned char*     __restrict__ state,
    const unsigned char*     __restrict__ harvests, // parole rungs (u8)
    const float*             __restrict__ fresh,   // link_freshness output
    unsigned*                __restrict__ chain_alive,
    unsigned*                __restrict__ chain_len,
    float*                   __restrict__ chain_min_fresh,
    unsigned char*           __restrict__ chain_dead,
    float*                   __restrict__ chain_parole_cap,
    float*                   __restrict__ justification)
{
    unsigned k = blockIdx.x * blockDim.x + threadIdx.x;
    const unsigned stride = gridDim.x * blockDim.x;
    for (; k < level_len; k += stride) {
        const unsigned i = nodes[k];
        const unsigned p = parent[i];
        const unsigned char alive = (state[i] == WHY_ALIVE);
        // The parole cap for a link harvested h times: 2^-h, assembled
        // from exponent bits — the exact twin of Rust's
        // `ecology_core::why::parole_cap` (`f32::from_bits((127-e)<<23)`
        // ≡ `__int_as_float((127-e)<<23)`), so the ladder's arithmetic
        // has one definition and two bit-identical spellings. h clamps
        // at 126: the cap never goes subnormal, and the veto law — not
        // the cap — is what kills lineages.
        const unsigned char h = harvests[i];
        const float own = alive
            ? __int_as_float((127u - (unsigned)(h > 126u ? 126u : h)) << 23)
            : 1.0f;
        if (p == NO_PARENT) {
            chain_alive[i] = alive;
            chain_len[i] = 1u;
            chain_min_fresh[i] = fresh[i];
            chain_dead[i] = (state[i] == WHY_DEAD) ? 1u : 0u;
            chain_parole_cap[i] = own;
        } else {
            chain_alive[i] = chain_alive[p] + alive;
            chain_len[i] = chain_len[p] + 1u;
            // Plain IEEE min via compare — bit-exact vs Rust's `if/else`.
            const float f = fresh[i];
            const float pf = chain_min_fresh[p];
            chain_min_fresh[i] = (f < pf) ? f : pf;
            chain_dead[i] = (chain_dead[p] || state[i] == WHY_DEAD) ? 1u : 0u;
            // The path is as trustworthy as its weakest paroled link —
            // min() again, bit-exact the same way.
            const float up = chain_parole_cap[p];
            chain_parole_cap[i] = (own < up) ? own : up;
        }
        if (chain_dead[i] || chain_alive[i] == 0u) {
            justification[i] = 0.0f;
        } else {
            const float base = (float)chain_alive[i] / (float)chain_len[i];
            float j = base * chain_min_fresh[i];
            if (j > chain_parole_cap[i]) j = chain_parole_cap[i];
            justification[i] = j;
        }
    }
}

// ---------------------------------------------------------------------------
// reap_kernel — mirror of WhyLedger::reap (the economic harvest)
// ---------------------------------------------------------------------------
// ALIVE nodes unattested for longer than the horizon lapse to UNKNOWN —
// per-node, no cascade (an unserved purpose questions itself).
// Transitions are counted atomically for the host to journal.
extern "C" __global__ void reap_kernel(
    unsigned            n,
    unsigned            now,
    unsigned            horizon,
    unsigned char*      __restrict__ state,
    const long long*    __restrict__ last_beat,
    const unsigned*     __restrict__ born,
    unsigned*           __restrict__ reaped_count)
{
    unsigned i = blockIdx.x * blockDim.x + threadIdx.x;
    const unsigned stride = gridDim.x * blockDim.x;
    for (; i < n; i += stride) {
        if (state[i] != WHY_ALIVE) continue;
        const long long lb = last_beat[i];
        const unsigned last = (lb >= 0) ? (unsigned)lb : born[i];
        const unsigned age = (now > last) ? (now - last) : 0u;
        if (age > horizon) {
            state[i] = WHY_UNKNOWN;
            atomicAdd(reaped_count, 1u);
        }
    }
}

// ---------------------------------------------------------------------------
// decay_kernel — mirror of WhyLedger::decay_mass
// ---------------------------------------------------------------------------
// ALIVE mass decays by 2^(-λ·Δt) with the *same polynomial* as the CPU
// (ecology_core::freshness::exp2_fast) — NOT __exp2f, whose 2-ulp
// approximation would break bit-exact parity. -fmad=false in the Makefile
// guards the polynomial from FMA contraction.
extern "C" __device__ __forceinline__ float ecology_exp2_fast(float x)
{
    if (x <= -126.0f) return 0.0f;
    if (x >= 127.0f)  return __int_as_float(0x7f800000); // +inf
    const float i = floorf(x);
    const float f = x - i;
    // Taylor cubic, Horner form — identical to the Rust source. Do NOT let
    // the compiler fuse these (Makefile: -fmad=false).
    const float poly = 1.0f + (0.6931472f + (0.2402265f + 0.0555041f * f) * f) * f;
    const float scale = __int_as_float(((int)i + 127) << 23);
    return scale * poly;
}

extern "C" __global__ void decay_kernel(
    unsigned            n,
    float               lambda_per_tick,
    unsigned            dt,
    unsigned char*      __restrict__ state,
    float*              __restrict__ mass)
{
    const float factor = ecology_exp2_fast(-lambda_per_tick * (float)dt);
    unsigned i = blockIdx.x * blockDim.x + threadIdx.x;
    const unsigned stride = gridDim.x * blockDim.x;
    for (; i < n; i += stride) {
        if (state[i] != WHY_ALIVE) continue;   // UNKNOWN frozen, DEAD already 0
        mass[i] = mass[i] * factor;
    }
}

// ---------------------------------------------------------------------------
// auction_score_kernel — mirror of ecology_core::attention::attention_score
// ---------------------------------------------------------------------------
// The veto law lives here too: a score <= 0 contributes nothing and is
// counted (atomicAdd) as vetoed. Absent drafts cannot bid at all.
// Canonical multiply order: ((((u·j)·o)·n)·ev). Do not reassociate.
extern "C" __device__ __forceinline__ float clamp01_eco(float x)
{
    // NaN-safe: a nonsensical factor vetoes (0), never poisons — matches
    // the Rust clamp01 (x.is_nan() || x < 0.0f).
    if (isnan(x) || x < 0.0f) return 0.0f;
    if (x > 1.0f) return 1.0f;
    return x;
}

extern "C" __global__ void auction_score_kernel(
    unsigned                 n,
    const unsigned char*     __restrict__ state,
    const unsigned char*     __restrict__ present,
    const float*             __restrict__ urgency,
    const float*             __restrict__ justification,
    const float*             __restrict__ opportunity,
    const float*             __restrict__ novelty,
    const float*             __restrict__ expected_value,
    float*                   __restrict__ score,
    unsigned*                __restrict__ vetoed_count)
{
    unsigned i = blockIdx.x * blockDim.x + threadIdx.x;
    const unsigned stride = gridDim.x * blockDim.x;
    for (; i < n; i += stride) {
        if (!present[i] || state[i] == WHY_ABSENT) { score[i] = 0.0f; continue; }
        const float u  = clamp01_eco(urgency[i]);
        const float j  = clamp01_eco(justification[i]);
        const float o  = clamp01_eco(opportunity[i]);
        const float nv = clamp01_eco(novelty[i]);
        const float ev = clamp01_eco(expected_value[i]);
        const float s  = u * j * o * nv * ev;   // canonical order
        score[i] = s;
        if (s <= 0.0f) atomicAdd(vetoed_count, 1u);   // the veto law
    }
}

// ---------------------------------------------------------------------------
// score_stats_kernel — sum + max of scores (telemetry; NOT bit-exact-critical)
// ---------------------------------------------------------------------------
// The max uses the monotonic integer transform: for non-negative f32,
// the bit pattern compares like an unsigned int, so warp/block max can ride
// redux/atomicMax on u32 — the same trick as the hand-written
// kernels/ptx/warp_max_f32.ptx (which documents it in full). Sum is an
// approximate float accumulation (telemetry only; the authoritative spend
// math is the CPU auction, which never leaves the single writer).
extern "C" __global__ void score_stats_kernel(
    unsigned        n,
    const float*    __restrict__ score,
    float*          __restrict__ out_sum,
    float*          __restrict__ out_max)
{
    __shared__ float red_s[256];
    __shared__ float red_m[256];
    const unsigned tid = threadIdx.x;
    float s = 0.0f;
    float m = 0.0f;
    for (unsigned i = blockIdx.x * blockDim.x + tid; i < n; i += gridDim.x * blockDim.x) {
        const float v = score[i];
        s += v;
        if (v > m) m = v;
    }
    red_s[tid] = s;
    red_m[tid] = m;
    __syncthreads();
    for (unsigned off = blockDim.x >> 1; off > 0; off >>= 1) {
        if (tid < off) {
            red_s[tid] += red_s[tid + off];
            if (red_m[tid + off] > red_m[tid]) red_m[tid] = red_m[tid + off];
        }
        __syncthreads();
    }
    if (tid == 0) {
        atomicAdd(out_sum, red_s[0]);
        // Monotonic transform for a true cross-block float max.
        atomicMax((unsigned*)out_max, __float_as_uint(red_m[0]));
    }
}
