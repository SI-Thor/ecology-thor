# THOR.md — the machine this kernel is built for

*Verified against NVIDIA's AGX Thor documentation and the SI-Thor team's
on-box spike (`infinigen-thor` branch `thor-spike`, `thor/SPIKE.md`,
2026-10-30/31 — first-party measurements, not marketing numbers).*

## Why a page for the hardware

Because on this box, **where compute runs is the whole design**. The AGX
Thor is not a fast x86 server with a GPU bolted on; it is one LPDDR5X pool
with fourteen Arm cores, a Blackwell GPU, and a handful of fixed-function
engines all drinking from the same 273 GB/s. A scheduler that treats "CPU"
and "GPU" as separate machines with a PCIe toll between them wastes the
box's one superpower. ecology-thor is organized around that pool.

## The verified hardware map (devkit / T5000 module)

| Engine | What it is | What ecology-thor uses it for |
|---|---|---|
| **CPU: 14× Neoverse-V3AE** ("Poseidon-AE") @ up to 2.6 GHz, 1 MB L2/core, 16 MB shared L3 | ~2.6× Orin's CPU; a proper server-class Arm cluster | The kernel tick (single writer, pinned), auction select, journal, Python edge. Reserve 2–4 cores isolated (`docs/RT.md`) |
| **GPU: Blackwell, 2560 CUDA cores (20 SMs × 128), up to 1575 MHz, compute capability 11.0 (sm_110)**, 96 Tensor cores, MIG-capable (2 instances) | ~8 TFLOPS FP32, 517 INT8 TOPS dense, FP4-sparse 2070 TFLOPS for the LLM crowd — **we don't use tensor cores at all** | The O(population) sweeps: link freshness, level-synchronous justification, reap, decay, auction scoring (`kernels/ecology.cu`). Between sweeps: the *same GPU renders infinigen scenes* — the admission controller arbitrates |
| **Unified LPDDR5X: 128 GB module, 256-bit, 273 GB/s** (T4000: 64 GB) | One physical pool, zero-copy between all engines. **What the box actually gives you: `MemTotal` ≈ 125.7 GiB, and the project's working figure is 122 GiB usable** — the SI-Thor seats' measured convention (builder-ide's thor-admit design §0; the box OOM-crashed at 13:22 on 2026-10-31 with no shared memory view, which is why the 25 GiB floor and the heavy lock exist) | The whole arena lives here via `cuMemAllocManaged`; "mirroring" state to the device is a plain memcpy at DRAM speed. No staging, no pinned buffers, no `cudaMemcpy` |
| **PVA v3** (2× vector + 2× pixel processors) | Vision/geometry accelerator (filter, warp, pyramid, FFT) via VPI | Future: score-normalization image ops if world-telemetry ever needs raster-adjacent math. Not in v0.1 |
| **VIC gen-4.2** | 2D engine: scaling, compositing, CSC | Future: thumbnail/overlay composition for the IDE's world view. Not in v0.1 |
| **2× NVENC (H.264/265), 2× NVDEC (+AV1, VP9) + NVJPEG** | Encode/decode offload | Future: streaming the world view out (NVENC), decoding incoming asset video (NVDEC). Not in v0.1 |
| **Safety island: 4× lockstep Cortex-R52** | Runs NVIDIA FSI firmware only (DRAM ECC patrol, watchdogs) — **not user-programmable from Jetson Linux** | Documented, not used. Our watchdog lives in software (`docs/RT.md`) |
| **No DLA** | Thor dropped Orin's NVDLA entirely (verified: no DLA row in the r38.4 mode tables) | Nothing. Any design that assumed DLA offload is wrong for this chip |
| **PCIe Gen5 ×12, 4× 25GbE QSFP28 + 5GbE RJ45** | The box's doors to the outside | Tile streaming to/from cloud authors (thor-world's lane), asset ingest |

## Power, clocks, determinism

| nvpmodel | Budget | CPU | GPU |
|---|---|---|---|
| 0 MAXN | unconstrained (HW throttle) | 14 @ 2601 MHz | 10 TPC @ 1575 MHz |
| 1 (default) | 120 W | 14 @ 2601 | 10 @ 1386 |
| 2 | 90 W | 12 @ 2601 | 6 @ 1530 |
| 3 | 70 W | 12 @ 1998 | 6 @ 1530 |

- **MAXN is thermally non-deterministic** (observed ~13% GEMV drift on early
  firmware with DVFS ramping). For comparable numbers: pin clocks with
  `jetson_clocks`, and for *sustained* load prefer mode 1.
- Memory clock is 4266 MHz in **every** mode — our sweeps are bandwidth-bound
  more than clock-bound, so power mode hits them less than you'd fear.
- Field notes worth keeping: OC3 over-current events latch CPUs to 242 MHz
  until reboot; the NTFS-over-USB shelf wedges hard (keep runtime artifacts
  on NVMe) — both documented in the team's SPIKE.
- `tools/thor-tune.sh` encodes the full checklist.

## Software floor

JetPack **7.x** (not 6 — that's Orin): L4T r38/r39, Ubuntu 24.04, kernel
6.8 (PREEMPT_RT available as NVIDIA packages), **CUDA 13.x**,
aarch64-only, driver 580+. The GPU is `sm_110` — kernels must be built
with a CUDA ≥ 13 nvcc or `-arch=native` on-device (nvcc's x86 default of
sm_75 produces cubins the GPU refuses; this bit the infinigen terrain
build until they switched to `-arch=native`).

No OptiX on this part (no RT cores — NVIDIA staff, dev forums 2025-09);
Cycles renders CUDA-only on Thor. Blender/bpy has **no Linux-aarch64
wheel** — the team builds it from source (`thor/build-bpy.sh`, ~70 min,
5 patches, sm_110 Cycles config).

## What the box is good and bad at (for us)

**Good:** unified-memory bandwidth and capacity (a 100k-purpose arena is
~8 MB — noise); a genuinely fast Arm cluster for the control plane; a
small-but-modern GPU that eats wide, shallow, memory-bound sweeps (exactly
our kernel shape: 20 SMs streaming SoA columns); ECC for 24/7 operation;
25GbE for streaming worlds out.

**Bad:** single-thread speed (one 2.6 GHz server core — serial pipeline
stages bottleneck here, not in the kernel); FP64 (avoid — not a tensor
precision; our math is f32 by design); thermal headroom (130 W class —
the box is a laptop, not a rack); the GPU is *shared* with the actual
world-rendering workload — which is precisely why the ecology's sweeps
are designed to cost microseconds, and why the auction exists to arbitrate
who gets the next GPU-seconds.

## Sources

- NVIDIA Jetson AGX Thor Technical Brief TB-12572-001 (SoC blocks, MIG,
  codecs, power modes)
- L4T r38.4/r39.2 Developer Guide (PlatformPowerAndPerformance/JetsonThor,
  DramEcc, RealTimeKernel, 25GbE on QSFP)
- AGX Thor Devkit User Guide (deviceQuery: cc 11.0, 20 SMs)
- NVIDIA forums: sm_110 threads (compute capability), topic 384828 (MIG on
  Thor), 361188 (OC3 latch), 384295 (MAXN under DeepStream)
- `infinigen-thor` `thor/SPIKE.md` (on-box verification of all of the
  above plus the bpy/Blender/OptiX findings)
