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

### Review round — four outside eyes (zero-shot personas)

After the v0.1.0 push, the architecture docs went through four persona
reviews (Rust RT engineer, CUDA kernel engineer, game-engine architect,
SRE). Adopted / rejected, with reasons:

| Finding | Verdict | Action |
|---|---|---|
| Cascade stack was `Vec::new()` — first kill allocates (rust-rt) | **Real bug vs. our own no-alloc claim** | Preallocated to arena capacity (`why.rs`). The unbounded-growth worry was already bounded by `Config::capacity`, but the allocation was real. |
| No durability leg (SRE: "crash = complete state loss") | **Real, top 3am risk** | Added `Journal::flush_to` — self-describing binary segments (magic + count + segment chain hash + length-prefixed events), portable, concatenation-replayable. Not a database; the caller owns fsync policy. The full replay basis already existed; now it has a sink. |
| "Parity gap: three implementations, one test surface" (rust-rt) | **Already half-true** | `tests/parity.rs` now exists with two legs: CPU leg (reference mirrors vs. ledger, randomized populations, always in CI) + device leg (feature `cuda`, on-hardware gate before v0.2 adoption — the file the docs kept promising). |
| CUDA Graph for per-level launches (cuda, worldsim) | Already roadmap #5 | Priority confirmed by two reviewers independently; stays after measurement (launch overhead must *show* first). |
| Partial-select instead of full sort (worldsim) | Already roadmap #3 | Same: the sort is only superlinear pain at stress populations; realistic world-gen is 130 µs total. Measure first. |
| "Move only hot columns" (worldsim) | **Rejected — wrong for this hardware** | There are no transfers on Tegra unified memory; the mirror *is* a plain host memcpy at DRAM speed. Reviewer pattern-matched to discrete-GPU economics. |
| "Parallelize the tick across cores" (worldsim) | **Rejected — violates the single-writer law** | The phases are µs-scale; concurrency would buy latency variance, which is the one thing the kernel refuses to buy. Documented in ARCHITECTURE §3. |
| "Watchdog thread" (SRE, rust-rt) | **Already answered by design** (RT.md §5) | The summary stopping *is* the alarm; an external monitor reads it without kernel cooperation. Re-documented rather than re-designed. |
| Coalescing in justify_level (cuda) | Noted, not a defect | Level node indices are spawn-ordered (mostly contiguous); the gather over `parent[]` is the algorithm, not a layout bug. Worth a profile on-device before touching. |

What got stolen from us (the reviewers' "one thing to steal", for the
record): the safe-mode decay-only degradation pattern, the zero-copy
unified-memory placement policy, the four-state never-delete ledger,
and the bit-exact triple-implementation discipline.

**Result**: v0.1.1 — cascade preallocation, journal durability sink,
parity gate file. 36 tests green, guard green.

---

## Session 2 — 2026-11-01 — the review round: device harness, bake-off, rebases, the door, the recorded night

**The review's five asks** (builder-ide seat): make the device steps
runnable by a stranger; bake the two admission daemons off with the
same tests; rebase the three proposal branches (14 behind, units
wrong, lease unmapped); route `world://why` through a core door; and
show the auction on the recorded mini-night-2. All five landed, each
pushed with its evidence.

### What was built

1. **The device harness** (`tools/thor-device.sh` + `docs/DEVICE.md` +
   `make device`): preflight → guard → kernels → parity → ptx-check →
   bench → a paste-ready report bundle, one PASS/FAIL line per step,
   under the box rules (flock `~/lm-wt/heavy.lock`, ≥25 GiB floor,
   122 GiB usable — THOR.md corrected: 128 GB module / MemTotal
   ≈125.7 GiB / 122 GiB working figure, sources cited). The guard step
   doubles as the golden-determinism aarch64 leg. Hygiene: 207
   committed build-artifact files (`ecology-py/target/`) untracked.

2. **The mini-night-2 replay** (`tools/replay_mini_night2.py`,
   `docs/analysis/mini-night-2-replay.md`): the 18-variant ledger
   reconstructed from every published aggregate (`--check` verifies;
   robustness across 6 alternative reconstructions), then three legs.
   Parity: at the runner's own prices the auction makes the identical
   allocation. Value pricing: night-1's ledger says a third seed line's
   expected gain is 0.000 — the veto cuts all six v3 variants, 30.6%
   less GPU / 31.6% less wall at identical winners (2.857 both). The
   bar arithmetic: 11.76 = 4.508×1.5+5 was computed from the
   prop.512-mispriced ask; on the prop.night band the same night is
   53.9%. Honest caveats in the doc (n=3, Wilson upper 0.56, the
   same-seed replay, nothing cuts at the bar on this night under any
   policy).

3. **The admission bake-off** (`tools/ecology-admit`, `tools/bakeoff.sh`,
   `docs/BAKEOFF.md`): our daemon adopts thor-admit's lease semantics
   verbatim (reservation arithmetic, kernel-held locks, watchdog
   discipline, peaks.jsonl schema, wire protocol) so the harness runs
   the same three tests against both — plus the ecology's three: the
   chained decision journal (FNV-1a per line, `journal --verify`
   breaks on tampering — unit-tested), the lease-as-purpose WHY states
   (stop → UNKNOWN, not dead), and the stall law (a lease silent past
   the horizon is harvested with memory fine — the silent-hang class a
   byte watchdog cannot see). **Verified end-to-end on the 3.5 GiB CI
   box at 1 GiB scale: all seven lines PASS, the shared three
   identical on both daemons.** The ledger answer (BAKEOFF.md): yes to
   the chained journal (~50 lines on their side), yes to stall
   semantics as a client convention, no to the four-state machinery
   inside a memory daemon.

4. **The three rebases**: builder-ide `research/ecology-bridge` onto
   core-judge main (Rails now speak the night's `Rail` axes verbatim —
   gpu_minutes/hosted_cents/wall_minutes; `lease_for` maps a winning
   bid onto a thor-admit acquire; `LEASE_EVENTS_TO_WHY` maps the
   daemon lifecycle onto the ledger; the study addendum re-checks
   core/judge (the typed EV source — a below-τ judgment sets no EV,
   which the veto law already agrees with), tiles+OPE (journal the bid
   set, not just the winner, or the log is not off-policy evaluable —
   the replay's reconstruction exists because the runner logs only
   what ran), core-doors (the agent claim on every bid) and B3's
   always-on daytime (a permanent Maintenance-class bidder).
   infinigen-thor `research/ecology-admission`: the lease composition
   (`lease.py` — Null/Recording/ThorAdmit clients; a refused lease
   leaves the job queued and ALIVE, tested) + the proposal addendum
   (the honest note: our first prototype had no memory story at all).
   thor-world's why-stream was already merged; its follow-up is (5).

5. **The why door** (builder-ide `docs/design/why-door.md` +
   thor-world `research/ecology-why-door`): the review was right —
   world-mcp is a pure client, so the why digest must arrive through
   the core, not a side channel. The proposal: `POST /v1/why/digest`
   (the ecology's publisher door; one `why.digest` audit event, the
   single writer, the closed set grown by one the tiles way) +
   `GET /v1/why/summary` (the fold; unwired → 501 with the seam named,
   the `/v1/world/scene` precedent) + no new stream door (the SSE
   fanout already is one). thor-world's branch reads the door
   (`fetch_digest`, `why_door_events`), registers `world://why`, and
   degrades honestly; five new door tests with a fake CoreClient.

### The session's own bugs (kept honest)

- The journal chain resumed from a **swapped (seq, chain) tuple** —
  seqs started at 2216829734 (the FNV basis read as a tick). Found by
  the chain test; fixed; the test now catches it forever.
- **`rss_by_pgid` read /proc/stat field 23 (starttime) as rss** — the
  watchdog's first live run showed an 8 TB victim. Field 24 verified
  against `/proc/<pid>/status` VmRSS; fixed.
- The connection thread **returned after acquire**, closing the
  connection before held/release could arrive — leases leaked until
  the held-grace reaped them. The connection now stays open for its
  whole lease.
- The flock probe raced the **grant→held window** (first client_gone
  at 0.05 s); the probe now applies only to leases that said held,
  with a 10 s grace for the rest.
- The bake-off harness itself: a misaligned `start_daemon` arg list,
  stale sockets left as directories, per-test state dirs overwriting
  failure evidence, scale math that only worked at 122 GiB, and a
  journal-unwrap parser that broke thor-admit's flat log lines. Each
  found by a red line, fixed, and kept honest in the commit.
- The CI sandbox **deletes release binaries between tool calls**
  (debug binaries and small files survive); the harness now builds
  both daemons itself in one invocation — which was the
  runnable-by-a-stranger requirement all along.

### Open threads (next sessions)

1. **The Thor run**: `make device` (the seat fills BENCHMARKS/PLACEMENT
   from the report) and `tools/bakeoff.sh` (the real 30 GiB rules) —
   the two scripts this session existed to make one-command.
2. The bake-off's fold-in, whichever way it goes: the chained journal
   on thor-admit's side, or thor-admit's measured subtleties on ours.
3. The why door's landing review (token class, retention, units —
   the three open questions in the proposal).
4. v0.2 device-authoritative sweeps once parity passes on sm_110;
   the `--device-bench` mode the report names so PLACEMENT's
   crossovers become measurements.
5. A second recorded night for the replay — ideally one where the bar
   truly binds, so the "cut by score vs cut by position" claim moves
   from mechanism to measurement.
