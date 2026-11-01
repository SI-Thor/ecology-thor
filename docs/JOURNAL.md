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

---

## Session 3 — 2026-11-01 — the quiet hours: fold-ins, the rebase that waited, and the questions answered

**Where the round started.** The five directives of session 2 were all
pushed before the box went quiet; this session began by reading what
moved upstream while we slept. Three findings:

1. **The bridge was merged.** builder-ide main took
   `research/ecology-bridge` at `74e8e55` ("a pure-Python mirror …
   read-only", pinned `verify-eb`) — the v1 mirror, 500 lines, rails
   in the box's own units. The two session-2 commits that followed
   (the night's units + the lease mapping, the why door) were pushed
   *after* the collaborator's last activity (17:21), so they sat
   unreviewed on a branch 34 commits behind a main that kept moving.
2. **Main moved 34 commits**, and two of them touch our seams:
   `b565e88` re-priced the mesh stage at what the night spends
   (`prop.mesh.512/1024` — the old band sold the mesh stage 3.4×
   cheap on the GPU lane), and `655ac8f` landed the never-wait
   conductor (request-level, the cascade's cloud half — a different
   layer from the night-level auction; complementary, not competing).
   Re-checked both against the bridge: nothing after the merge
   touched `core/ecology_bridge` or `core.records.Rail`'s axes, so
   the units wiring holds as-is; the re-pricing changes *prices*,
   not *units*, which is exactly the separation the bridge was built
   to have.
3. **thor-admit moved 13 commits** (clocks named, the judge's four
   calls, tests enforcing the brief's memory cap) — and the bake-off
   verdict's cheapest test was still unclaimed.

**What this session did, in order:**

1. **The rebased delta** (`25c4849`): the two unmerged bridge commits
   rebased onto current main — the merge-base content was
   byte-identical to what main merged, so the rebase was clean and
   the branch is now exactly two commits on top of main: "the night's
   own units + the winning bid as a thor-admit lease" and "the why
   door". Tests: 19/19 ecology_bridge, 332/332 night+judge on this
   box (the full suite needs the USD wheels and a bigger clock
   budget than the sandbox gives; the delta touches no file any
   other test imports — verified by grep, which is the isolation the
   mirror was promised to have).
2. **The fold-in, built** (builder-ide `research/ecology-ledger`,
   `1193626`): BAKEOFF.md's verdict said the chained journal was the
   one thing worth adopting and priced it at ~50 lines — the
   cheapest way to test that claim is to build it on top of
   thor-admit as it stands today. `src/journal.rs`: every daemon.log
   line carries `seq` (unique across restarts; the chain resumes
   from the tail) and `chain` (running FNV-1a over the line's own
   bytes); `thor-admit verify` re-walks the file and either reports
   the head or names the seq and the reason (edited / numbering
   jumped / not a chained event); lines an older daemon wrote are a
   reported prefix, not a failure. The mixer is inlined — same
   constants as `ecology_core::journal::fnv1a64`, so chains walked
   here and in the ecology agree on identical input — and the crate
   stays dependency-free. Unit tests (edit breaks at its line, a gap
   or a mid-file restart breaks the numbering, old prefix reported,
   resume numbers on) plus an integration test (the real daemon's
   log verifies through the CLI; a post-mortem's "fix" of a granted
   peak is named by seq). The integration test passes its own floor
   so it runs identically on a 3 GiB CI box and on the Thor. Clippy
   clean, all targets; the one lib failure on this box is the
   device-gated 122 GiB assertion, which fails on the pristine
   baseline too.
3. **The why door's three questions, answered** (`e9370d9`): each
   from the core's code, not preference — token class `seat` (a new
   `service` class means editing every door's allowed set, which is
   new gating; `seat` already sits in `TOKEN_CLASSES` used by no
   door); retention keep-everything (the store has no retention
   machinery at all, the stream is smaller than a night's
   `day.change` traffic, the replay methodology depends on
   completeness, and the publisher owns the decimation valve);
   joules as-published with units named in the contract's next
   append-only version (a door that converts is a second place
   accounting arithmetic lives — the 11.76-min bar was a
   units/pricing confusion that survived review for exactly that
   reason).

**The session's own bugs (kept honest)**

- The integration test's first run hung for a minute: the daemon's
  default 25 GiB floor against this sandbox's 3.4 GiB `MemAvailable`
  means a 64 MiB lease queues forever. The test now passes its own
  floor (`--floor 64M --emergency-floor 512M`) — the right fix, not
  a skip: the test is about the log's chain, not the admission
  arithmetic, and it must run the same on any box.
- `cargo fmt --check` fails on ~50 spots — on the pristine baseline
  too; the crate's convention is clippy-clean, not rustfmt-clean,
  and the new code matches the house style (long prose comments,
  which rustfmt wants to rewrap).

**Open threads (carried, plus what this session added)**

1. The Thor run, unchanged from session 2: `make device` (the seat
   fills BENCHMARKS/PLACEMENT) and `tools/bakeoff.sh` — plus, now,
   `cargo test` on `research/ecology-ledger`, whose integration test
   is written to run on any box.
2. The ledger fold-in's review: the branch is PR-ready against
   thor-admit's head; if the seat prefers its own ~50 lines, the
   branch is also a worked example of the exact semantics
   (restart-resume, prefix tolerance, tamper naming) the verdict
   described.
3. The why door's landing review: the three answers in §4 are
   arguments, not landings — the door itself still waits on Casey.
4. The second recorded night (session 2's thread 5, unchanged): a
   night where the bar truly binds.
5. New: when the conductor work (never-wait, `655ac8f`) meets the
   night level, the two schedulers should be written down against
   each other — request-level deadlines vs night-level auctions are
   different layers today, and the note that says so belongs in the
   study doc the day they touch.

---

## Session 4 — 2026-11-01 — the swarm answers, and the ladder, built

**Where the round started.** Main moved again while our branches wait
(62c9f4e → 1719665, +9: critic-select and tiles-retrieval merged at
1,627 tests, the dsswarm4 research note, the crew's constraint code
re-measured, thor-fp4 published as its own repo). Nothing touched our
seams (`core/ecology_bridge`, `Rail`'s axes, the why-door doc) — the
three bridge commits ride clean. Our two research reads:

1. **dsswarm 1–3 and dsswarm4 name the ecology four times** — the
   shadow conductor "applies to the ecology auction", object leases
   "pair with per-agent tokens and thor-admit's lease idea", the
   trust ladder governs "what may run unattended", and the power
   auction is "a seed for the ecology auction and thor-admit".
2. **thor-fp4 is real and measured** (371–456 TFLOPS at K ≥ ~2048,
   *slower* below K ~1280, weights 3.56× smaller, DIRECTIONS with a
   certified-FP4 arithmetic bound).

**What this session did, in order:**

1. **The swarm addendum** (builder-ide `research/ecology-bridge`,
   `0296507` + `0ae1a53`): study-doc §7, six answers from the ecology
   side, code cited — the four layers "conductor" now names and why
   none should grow the others' logic (the replay already is the
   offline shadow); the trust ladder mapped onto class +
   justification + the WHY machine; object leases as a doors problem
   with the one law (object implies memory, never the reverse); the
   power auction as the learned-peaks pattern on another rail, with
   the `Rail`-grows-joules decision kept in the core; WAL + snapshot
   (the chain names the log side, a checkpoint is a cache of the
   fold); and thor-fp4's two regime asymmetries for whoever prices
   the first FP4 night.
2. **The harvest ladder, built** (ecology-thor `2339ef6`): §7.2's
   honest gap — parole was re-request-triggered at a flat
   first-harvest cap — closed in the kernel where the semantics are
   ours. `harvests: u8` replaces the probation bool (+1 on every
   ALIVE→UNKNOWN — the one seam doubt, the kill cascade and the
   stall-law harvest all share; −1 on service, floor 0); the cap is
   `2^-h` built from exponent bits (exact on CPU and device by
   construction, h clamped at 126 so the *veto law* — never the cap —
   kills lineages); the path takes its weakest paroled link (min);
   and the compatibility law is tested: a never-harvested population
   is **bit-identical** to the old formula, `parole_cap(1) ==
   PROBATION_CAP`. All four implementations moved together per the
   parity gate's rule (ledger, reference mirror, device launcher,
   sm_110 kernel — the cap now derived in-register from the u8
   column, no launch parameter). 49 tests on CPU; the randomized
   parity population now exercises the whole ladder
   (doubt→revive→doubt→serve); the device leg rides the harness the
   seat already runs. Clippy clean across the workspace.

**The session's own bugs (kept honest)**

- A docstring edit joined two lines in `why.rs`'s module header (the
  replace ate a newline); caught in the post-edit review, fixed
  before it could mislead.
- The unused-import warning after the sweep stopped using
  `PROBATION_CAP` directly — moved into the test module where its one
  remaining user (the first-rung identity test) lives.

**Open threads (carried)**

1. The Thor run, now three-legged: `make device` + `tools/bakeoff.sh`
   + `cargo test -p ecology-cuda --features cuda` (the ladder's
   bit-identity leg).
2. The ledger fold-in's review (`research/ecology-ledger`, PR-ready).
3. The why door's landing review (three answers in §4 of the proposal).
4. The second recorded night, ideally one where the bar binds.
5. From the addendum, if the swarm round adopts any of it: the shadow
   run of the auction against a live night (the runner journals the
   bid set — tiles rule 8 — and the read-only mirror scores it), and
   the object-lease door as its own proposal with the one law written
   in.

---

## Session 5 — 2026-11-01 — the round the box got busy

**Where the round started.** The sandbox had been recycled (fresh box:
3 GiB, 2 cores, no toolchain — rebuilt), but the remotes told the real
story: while we were away, the collaborator merged thor-admit to
builder-ide main — grown into the **presence round** (a presence queue
that runs registered work when no live request waits, pre-empted by
memory SIGTERM or GPU SIGSTOP; four accounting tiers; a time ledger
that books every hour with its idle causes) — and pushed main past our
branches again (110 commits beyond the bridge's base). The overnight
Infinigen sweep (the second recorded night) is in flight on their side.
Nobody had taken our three waiting branches yet.

**What this session did, in order:**

1. **The fold-in, rebuilt on the daemon as it stands today**
   (`research/ecology-ledger`, 97cbb04 + b1f3386, force-with-lease):
   the bake-off verdict priced the chained decision journal at ~50
   lines; the branch that built it rode the pre-presence daemon, and
   the merge took everything but the chain. Rebuilt against the new
   daemon: every daemon.log line carries `seq`/`chain` (restart-resume,
   prefix tolerance, tamper naming — same semantics, same FNV
   constants), and a second commit chains the ledger's hours too —
   `ledger.jsonl` is now an accounting record, and an edited hour is a
   report that lies the same way an edited decision line does.
   `thor-admit verify` walks both. peaks.jsonl deliberately unchained
   (advisory, derived from decision lines — the README says why).
   Tests: unit (edit breaks at its line, resume numbers on, the
   round-trip untouched) + integration (the real daemon's own log
   verifies through the CLI; a post-mortem's "fix" of a granted peak is
   named by seq; the test passes its own floor, so any box runs it) +
   the smoke test ends in `verify` now. Clippy clean. On this box: lib
   88/89 and integration 12/14 — both shortfalls are the device-gated
   tests that fail on the pristine baseline here too (the 122 GiB
   meminfo assertion; the hog-growth tests that need real memory).
2. **The bridge, rebased, and the presence mapping written down**
   (`research/ecology-bridge`, cf8594f; clean rebase — the delta
   touches no file main touched — bridge 19/19, night+judge 374/374):
   study doc §7.7. The daemon grew a time axis, so the note the
   journals promised ("when two schedulers touch, write them down
   against each other") got written: the auction prices what the night
   buys and its jurisdiction ends at the night's ledger; the presence
   scheduler prices what the gaps do and its jurisdiction ends where a
   live lease wants the memory or the GPU. The tier mapping for our
   work (winning bids live; the replay/shadow scoring practice — the
   tier's own description; parity study; pre-generation preparation),
   the law (neither scheduler ever schedules inside the other's
   jurisdiction — the 11.76-min bar is what two schedulers both
   thinking they own the box's time looks like), the units note (the
   ledger measures power in mW, the core prices joules, the one
   conversion stays in the core), and three concrete first residents
   for the presence queue — the exp doc's own open question, answered
   from our side, each priced by the daemon's learned peaks.
3. **The why-door branch, repaired onto the stream round**
   (`research/ecology-why-door` on thor-world, 6311f3b): the branch
   rode 19 commits behind main, and rebasing surfaced two real breaks
   it had never been run against — the resource listing is seven now
   (world://why), and the purity test's import scan caught the mock's
   `random`. Fixed: the mock draws from the same LCG its chain hash
   already uses (one less import, and the mock's behaviour and its
   chain now come from the same arithmetic — my own sort-order mistake
   in the seven-resource list, caught by the suite on the first run,
   is kept honest below). Full thor-world suite on this box: 54/54
   (after `pip install -e` — the harness boots the server as a
   subprocess per test, which needs the package installed; the
   pristine-main comparison run had the same two environment failures
   before the install).
4. **daemon_ledger.py** (ecology-thor main, f7735c9): the box's side of
   the second-night comparison — reads thor-admit's ledger.jsonl,
   reports tier seconds, idle by cause (with the README's own meaning
   beside each), the unattributed number flagged, GPU power/busy with
   sources, and the morning-after lines that tell
   `waiting_for_memory` from `no_work_registered`. Validated three
   ways: `--check` reproduces every number the lab record published
   for the two measured windows (exp-thor-admit §12); real files
   written through Ledger's own close path — one from main's build
   (unchained), one through the chained branch — both parse and
   report. Stdlib-only, read-only. The night's side (the replay's
   shadow allocation against these hours) waits for the recorded
   night; this half runs the morning after, on any box.

**The session's own bugs (kept honest)**

- The sort order in the seven-resource list: I wrote `world://why`
  before `world://vocabulary`, the suite failed on the first run, and
  the actual server had been right all along — my manual check had
  even shown the resource present, truncated out of view by a `tail`.
  Read the whole output before blaming the box.
- The first push of the rebuilt ledger branch was rejected
  ("stale info"): force-with-lease computes its lease from a tracking
  ref, and pushing to a raw URL has none. The fix is the explicit
  expected value (`--force-with-lease=branch:sha`) — the safer form,
  and now the habit.

**Open threads (carried)**

1. The Thor run, now four-legged: `make device` + `tools/bakeoff.sh` +
   `cargo test -p ecology-cuda --features cuda` + the fold-in's
   integration test (any box).
2. Three reviews pending: the fold-in (PR-ready on main's daemon), the
   bridge's §7 answers + §7.7, and the why-door landing (the core's
   `/v1/why/summary` end still waits on Casey — the thor-world side is
   green and ready).
3. The second recorded night: when the sweep's hours land in
   ledger.jsonl, run daemon_ledger.py on them and put the two
   morning-after lines beside the replay's shadow allocation — the
   comparison §7.7 describes, first real instance.
4. The presence queue's first residents (§7.7's three): register them
   on the box when the fold-in lands, `--peak auto`, and let the
   daemon's own history price them.
5. The collaborator's research stream (OpenSenseNova, PhysAg3D,
   OpenEBFM, the waiting list) keeps naming energy — the joules-rail
   decision (§7.4) will come back around; the answer is written.
