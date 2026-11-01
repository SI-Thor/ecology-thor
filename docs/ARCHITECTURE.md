# ARCHITECTURE.md — ecology-thor from the inside

*The successor of
[SuperInstance/cognitive-ecology](https://github.com/SuperInstance/cognitive-ecology),
purpose-built for the SI-Thor world-creation stack on the AGX Thor. The
laws are inherited; the machine discipline is new.*

## 1. What this is

An **always-on cognitive substrate** for a box that creates worlds. Three
jobs:

1. **Keep purpose alive** — a WHY ledger where nothing is ever deleted,
   only questioned (`UNKNOWN`) or terminally refuted (`DEAD`), with
   liveness as a *subscription* (re-attestation economics) rather than a
   purchase.
2. **Arbitrate the box's real currencies** — an attention auction that
   admits work against budgets of GPU-seconds, joules and cents (the
   "currency exchange" builder-ide's fit study asked for).
3. **Degrade honestly** — a real-time tick with framed budgets, watchdog
   discipline and a safe mode that stops admitting work while the world
   keeps aging truthfully.

The kernel is Rust, std-only, zero dependencies (`ecology-core`). The
device sweeps are CUDA C++ compiled to PTX for sm_110, loaded through
cudarc into unified memory (`ecology-cuda`). Python sees only a thin
orchestration edge (`ecology-py`). A workload simulator/benchmark drives
it all (`ecology-sim`).

## 2. The laws (inherited from the parent repo, ported exactly)

| Law | Statement | Where it lives |
|---|---|---|
| **WHY conservation** | A purpose node is never deleted; no API exists for it. Four states: `Alive`/`Dead`/`Unknown`/`Absent`. Killing cascades *living* descendants to UNKNOWN (questionable, not gone). Every transition is journaled. | `why.rs` (`WhyLedger`), `journal.rs` |
| **Re-attestation economics** | Purpose is a subscription. Justification = alive-fraction of the root path × weakest-link freshness (grace = horizon/3, then linear to 0 at horizon). Stale purposes bid weaker *before* they are questioned; unattested past the horizon they lapse to UNKNOWN on their own; revival is explicit and lands on parole (justification capped at 0.5 until served). | `why.rs` (chain aggregates), `freshness.rs` |
| **Veto law** | attention = urgency · justification · opportunity · novelty · expected_value; a bid scoring ≤ 0 never transacts — for any importance class, including the kernel's own. | `attention.rs` |
| **Budget conservation** | Auctions spend only what the rails hold (gpu_seconds, joules, cents); balances never go negative; spend is journaled. | `rails.rs`, `attention.rs` |
| **Mass conservation** | Purpose mass is spawned explicitly, decays exponentially for ALIVE nodes, freezes for UNKNOWN, annihilates on DEAD; `spawned − dissipated == current` is asserted every sweep. | `why.rs` (`decay_mass`, `mass_conservation_ok`) |

One deliberate restriction vs. the parent: **trees, not DAGs**. The parent
supports multiply-justified purposes (path OR-semantics); our arena uses
single-parent trees because the chain aggregate then becomes one
level-synchronous O(N) sweep — exactly parallelizable on the GPU
(the parent's own docstring notes a tree has exactly one path, so the
score is the exact v0.2/v0.3 value). If a purpose needs two warrants,
spawn a child under each — the semantics are richer, not poorer.

## 3. The organism, mechanically

```text
                 ┌────────────────────────── single writer (the tick) ─────────┐
                 │                                                              │
  spawn/attest ─▶│  WhyLedger (SoA)      AttentionTable (SoA)   Journal (ring) │
  kill/doubt     │  state, last_beat,     urgency, o, n, ev,    chain-hashed   │
  set_bid        │  parent, children,     cost, class, score,   necrology      │
                 │  mass, aggregates      present, admitted                    │
                 │        │                     ▲                             │
                 │        │ justification (j)   │ scores                       │
                 │        └───────┐   ┌─────────┘                             │
                 │          ┌─────▼───▼─────┐                                 │
                 │          │  THE AUCTION  │  veto law · rails · top-k       │
                 │          └───────────────┘                                 │
                 │  TickSummary ──seqlock──▶ readers (never blocked)          │
                 └────────────────────────────────────────────────────────────┘
                                        ▲
                   CUDA sweeps (optional, feature `cuda`): the O(N) columns
                   mirrored into unified LPDDR5X; same math, bit-exact.
```

### Frame table (the AUTOSAR habit, adapted)

| Frame | Period (default) | Work | Bounded by |
|---|---|---|---|
| minor | every tick (100 Hz) | necrology drain → cascade budget → rails refill → summary publish | O(transitions) + cascade_budget |
| auction | every 10 ticks | chain-aggregate sweep → auction | O(N) scan + O(C log C) select (C = candidates) |
| sweep | every 100 ticks | reap → root deadlines → mass decay → conservation check | O(N) |

Overrun policy: the *caller* measures the frame (the kernel has no clock)
and feeds it back via `note_frame_time`. After `overrun_tolerance`
consecutive overruns the kernel enters **safe mode**: auctions are
suspended (decay-only — the world keeps aging, nothing new is admitted),
the summary flags it, the journal records it. `safe_mode_cooloff` clean
ticks later it recovers. A slow box starves gracefully; it never lies.

### Determinism policy

No clock reads, no system randomness, no hash-map iteration, canonical
float order, strict total order (class, score-bits, id) in the auction.
Same scenario ⇒ same journal chain hash on x86_64 and aarch64
(`tests/golden.rs` pins it; `BLESS=1` re-blesses consciously). The
journal chain is FNV-1a over fixed-width little-endian encodings — the
replay basis and the tamper-evidence of the necrology.

### Memory discipline

Everything is preallocated at `Config::capacity` and **never grows**: the
arena is the organism's memory budget (1M purposes ≈ 40 MB — noise on a
122 GiB usable box — see THOR.md, but the *discipline* is what matters
for WCET). Steady-state
ticks allocate nothing. When the arena fills, spawn is refused
(`SpawnError::Capacity`) — the organism would rather refuse to grow than
degrade silently.

The one `unsafe` in the core is the seqlock cell
(`snapshot.rs`), single-writer by construction, with the invariant
documented at the `unsafe impl` and exercised by a concurrent-reader test.

## 4. The device side (see also PLACEMENT.md, PTX_NOTES.md)

Every O(N) column operation has three implementations that must agree
bit-for-bit: the ledger's (authoritative, CPU), the reference mirror
(`ecology-cuda/src/reference.rs` — the reviewable spec), and the CUDA
kernel (`kernels/ecology.cu`, compiled `-fmad=false`). The device sweeper
mirrors the arena into `cuMemAllocManaged` memory — on Tegra that is the
same physical LPDDR5X, so mirroring is a plain memcpy — launches the
sweeps on a stream, and reads results back directly. v0.1 keeps the CPU
authoritative and uses the device path as parity gate + timing harness;
v0.2 adopts the device as the authoritative writer once parity is proven
on hardware (the tables are already the same shape).

The level-synchronous justification sweep is the design's hinge: the CPU
walks levels 0..D sequentially, each level embarrassingly parallel — and
that is *exactly* the launch pattern the device version uses, one kernel
per level. The algorithm was chosen for the box, not the other way round.

## 5. What is deliberately NOT here

- **No LLM anything.** The Thor renders and schedules; the cloud thinks
  (the builder-ide world's rule: "no LLMs on the Thor — the cloud
  authors, the Thor plays"). Our rails even price that split: local work
  costs gpu_seconds/joules, cloud work costs cents.
- **No DAG justification, no probabilistic scores, no learned policies
  (yet).** v0.1 is the deterministic substrate; richer bidding policies
  are additive (they arrive as better `Bid`s, not kernel changes).
- **No persistence layer.** The journal is in-memory + hash-chained;
  durable replay is the v0.3 bridge (the encoding is already a complete
  replay basis — every transition is an event).

## 6. Module map

| Crate | Role |
|---|---|
| `ecology-core` | The kernel: ids, why, freshness, attention, rails, journal, snapshot, tick. std-only, zero deps. |
| `ecology-cuda` | `reference` (CPU mirrors) + `device` (cudarc shim, feature-gated) + `kernels/ecology.cu`, `kernels/ptx/warp_max_f32.ptx`, `kernels/Makefile`. |
| `ecology-sim` | The world-creation workload model (regions → scenes, stalls, revives, root deaths) and the benchmark harness. |
| `ecology-py` | pyo3 abi3 bindings — the orchestration edge (world-mcp, builder-ide experiments). |

## 7. Lineage and bridges

- **cognitive-ecology** (SuperInstance): the theory, the laws, the Python
  reference (`why.py` v0.3 semantics ported exactly).
- **builder-ide** (SI-Thor): vendored our WHY kernel as `core/why` and
  wrote the fit study (`docs/research/cogeco.md`) that deferred the tick
  loop and attention economy — this repo *is* the deferred thing.
  `docs/INTEGRATION.md` maps the doors.
- **thor-world** (SI-Thor): the MCP wire agents speak to the world;
  `docs/INTEGRATION.md` sketches the `world://why` resource.
- **infinigen-thor** (SI-Thor): the workload being scheduled; the
  admission-controller proposal lives on its branch
  (`research/ecology-admission`).
