# ecology-thor

**The persistent cognitive ecology, purpose-built for the NVIDIA Jetson AGX Thor.**

A real-time substrate for a box that creates worlds: it keeps long-horizon
purpose alive under a conservation law, arbitrates the box's real
currencies (GPU-seconds, joules, cents) with an attention auction, and
degrades honestly when the machine can't keep up. The successor of
[SuperInstance/cognitive-ecology](https://github.com/SuperInstance/cognitive-ecology),
redesigned for the SI-Thor world-creation stack
([infinigen-thor](https://github.com/SI-Thor/infinigen-thor) ·
[thor-world](https://github.com/SI-Thor/thor-world) ·
[builder-ide](https://github.com/SI-Thor/builder-ide)).

```
     WHY ledger (never deleted)          attention auction (rails: gpu-s, J, ¢)
  ┌───────────────────────────────┐   ┌─────────────────────────────────────┐
  │ alive ─ lapse ─▶ unknown      │   │  urgency · justification (chain     │
  │  ▲                 │ revive   │   │  freshness) · opportunity · novelty │
  │  └─ service ───────┘ (parole) │──▶│  · expected_value  — veto law:      │
  │ kill ▶ dead; children ▶ unknown│  │  score ≤ 0 never transacts          │
  └───────────────────────────────┘   └─────────────────────────────────────┘
        single-writer tick (100 Hz)        Rust core · CUDA/PTX sweeps · sm_110
```

## Why this exists

The parent repo proved the laws — purpose as a conserved quantity,
attention as an economy — on a laptop, in Python. The Thor changes the
engineering, not the philosophy:

- **Population scale, real time.** SoA arenas, preallocated, zero
  allocation in steady state; harmonic frames (10 ms / 100 ms / 1 s);
  rate-limited cascades; watchdog → safe mode (decay-only); a seqlock
  summary telemetry can read without ever blocking the tick.
- **One memory pool.** The Thor's CPU and Blackwell GPU share one 273 GB/s
  LPDDR5X pool, so the hot columns live in unified memory and the O(N)
  sweeps (freshness, chain justification, reap, decay, auction scoring)
  run on the GPU with **zero-copy mirrors** — the same math, bit-exact,
  in three implementations (ledger / CPU reference / CUDA kernel) that
  the parity gate keeps honest.
- **Real currencies.** The auction spends GPU-seconds, joules and cents —
  the units builder-ide's cost model already prices. When an infinigen
  render wants the GPU, it bids; when a purpose goes unserved, its
  freshness slides and it is *harvested to UNKNOWN* (never deleted) by
  the re-attestation economics — the silent-hang killer.

## Quick start

```bash
# anywhere (no CUDA needed):
cargo test --workspace          # 34 tests, incl. golden determinism
cargo run --release -p ecology-sim -- --profile world-gen --ticks 6000 --seed 42

# benchmark suite (release):
make bench

# the full gate (fmt + clippy -D warnings + tests + secret scan):
make guard

# on the AGX Thor:
sudo tools/thor-tune.sh         # pin clocks, MAXN/120W, RT checklist
make kernels                    # nvcc → build/ecology.ptx (sm_110)
make test-device                # full device parity vs CPU, bit for bit
make python                     # maturin wheel of the Python bindings
```

## The crates

| Crate | What | Deps |
|---|---|---|
| `ecology-core` | The kernel: WHY ledger, freshness economics, attention auction, rails, journal, tick. **std-only, zero dependencies, one `unsafe`.** | 0 |
| `ecology-cuda` | CPU reference mirrors + cudarc device shim (feature `cuda`) + `kernels/ecology.cu` + the hand-written `warp_max_f32.ptx`. | cudarc (opt) |
| `ecology-sim` | The world-creation workload model (regions → scenes, stalls, revives, root deaths) and benchmark harness. | ecology-core |
| `ecology-py` | pyo3 `abi3-py310` bindings — the orchestration edge (world-mcp, builder-ide experiments). Built on-device. | pyo3 |

## The laws (short form — ARCHITECTURE.md has the long form)

1. **A purpose node is never deleted.** Four states (`Alive`/`Dead`/
   `Unknown`/`Absent`); killing cascades *living* descendants to UNKNOWN
   — questionable, not gone; every transition lands in a chain-hashed,
   append-only journal.
2. **Purpose is a subscription.** Justification = alive-fraction of the
   root path × weakest-link freshness. Stale purposes bid weaker before
   anyone questions them; past the horizon they lapse on their own;
   revival is explicit and lands on parole (capped at half strength
   until served).
3. **The veto law.** attention = urgency · justification · opportunity ·
   novelty · expected_value; a bid scoring ≤ 0 never transacts — not
   even for the kernel's own importance class.
4. **Budgets and mass are conserved.** Auctions spend only what the rails
   hold; `spawned − dissipated == current` is asserted every sweep.
5. **The tick never lies.** No clock inside, no allocation in steady
   state, no unbounded bucket; overrun → safe mode (decay-only);
   same scenario ⇒ same journal chain hash on any platform
   (`tests/golden.rs`).

## Performance posture

(BENCHMARKS.md — every number carries its machine.)

- Realistic world-gen population (~3.2k live entities): **130 µs per
  auction tick** on a 2-vCPU dev box — ~1.6% of the 8 ms frame budget.
- Pathological stress (100k all-bidding): ~8 ms — the case the device
  sweeps exist for; the crossover table lives in PLACEMENT.md.
- On-device numbers: BENCHMARKS.md §AGX Thor, to be filled by
  `make bench` + `make test-device` on the box.

## Where it's going

- **v0.1 (this)**: Rust kernel + reference mirrors + device kernels +
  parity harness + the three sibling-repo bridges (INTEGRATION.md).
- **v0.2**: device-authoritative sweeps (the ledger writes *through*
  unified memory), partial-select auction, on-device measured placement
  thresholds.
- **v0.3**: durable journal (replay is already a complete basis — every
  transition is an event), snapshot anchors, `world://why` resource live
  in thor-world, builder-ide rail exchange running against real renders.

## Documentation

| Doc | What's in it |
|---|---|
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | the laws, the frame table, determinism policy, module map |
| [docs/THOR.md](docs/THOR.md) | the verified hardware map (sm_110, 273 GB/s, power modes, what the box is good/bad at) |
| [docs/PLACEMENT.md](docs/PLACEMENT.md) | which engine computes what, and the measurement rule for changing it |
| [docs/RT.md](docs/RT.md) | the real-time envelope, safe mode, box tuning (PREEMPT_RT, isolation) |
| [docs/PTX_NOTES.md](docs/PTX_NOTES.md) | the bit-exactness policy, the hand-PTX microkernel, the build matrix |
| [docs/SAFETY.md](docs/SAFETY.md) | the memory-safety review policy (zero-dep core, the one `unsafe`, leak rules) |
| [docs/BENCHMARKS.md](docs/BENCHMARKS.md) | protocol + honest numbers (sandbox + on-device tables) |
| [docs/INTEGRATION.md](docs/INTEGRATION.md) | bridges to builder-ide / infinigen-thor / thor-world + the frozen JSON contract |
| [docs/JOURNAL.md](docs/JOURNAL.md) | the build log — decisions and their reasons |

## License

MIT — same as the parent repo.
