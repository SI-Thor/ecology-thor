# INTEGRATION.md — bridges to the SI-Thor stack

*This repo is the thing builder-ide's fit study deferred. That study
(`builder-ide/docs/research/cogeco.md`, 2026-10-30 — worth reading whole)
recommends "option (b): vendor the WHY kernel, borrow the lifecycle
behaviors, do not adopt the tick loop or the attention economy" for a
two-user daytime world — and then names exactly what would change that
verdict: "if CE's processes ever bid for actual Thor GPU-minutes, BI's
rail design is the missing currency exchange." ecology-thor is built to
be that exchange. The bridges below respect each repo's laws; none of
them requires the others to change first.*

## The three bridges (each has a proposal branch)

| Repo | Bridge | Branch | Shape |
|---|---|---|---|
| builder-ide | the deferred option, realized: a native Rust/CUDA ecology engine beside the vendored `core/why` | `research/ecology-bridge` | `core/ecology_bridge/` (pure-Python semantics mirror of the auction + rails) + `docs/research/ecology-thor.md` (the follow-up study) |
| infinigen-thor | attention-weighted admission control + UNKNOWN harvesting for generation jobs | `research/ecology-admission` | `thor/ecology_admission/` (standalone supervisor daemon, dry-run sim mode) + `thor/proposals/ecology-admission.md` |
| thor-world | a `world://why` resource riding the existing SSE watcher | `research/ecology-why-stream` | `src/world_mcp/why_stream.py` (draft module, purity-law compliant) + `docs/proposals/ecology-why-stream.md` |

## 1. builder-ide ⇄ ecology-thor (the deep one)

builder-ide is the audit-ledger core: one SQLite ledger, single writer,
closed event set, everything a fold, "admission before spend", nothing
ever deleted. Its `core/why` is a pure Python port of our Rust `why`
v0.2 (four states, mortal roots, golden corpus) — our lineage, vendored
by their team. Our job is to be the *native engine* their vendored port
mirrors, without ever touching their single-writer invariant.

The interfaces that matter:

- **Daytime tick (`core/daytime/`)** — their only tick loop (24
  ticks/day, 1 wall-minute/day budget). The ecology's tick is 100 Hz
  arbitrage *underneath* it; the bridge is that daytime's
  admission-before-spend budget becomes a *rail* (gpu_seconds), and the
  ecology's auction is what fills it. No new writer: the ecology runs in
  its own process, and builder-ide reads the seqlock summary (JSON,
  below) as a fold input through a door.
- **Night heartbeat (`core/night/heartbeat.py`)** — `night.tick` every
  10 min; gaps = lost minutes. That *is* a re-attestation protocol
  already: map night jobs to purposes, `run_under` on tick, and the
  WHY ledger prices the gaps (freshness slides) before anyone writes a
  post-mortem. The `ecology_bridge` module on the branch demonstrates
  the mapping in pure Python.
- **Cost rails (`core/night/cost/`)** — measured p50/p95 pricing in
  GPU-minutes/cents/joules. Our `Rails` was designed to be their unit
  system (`rails.rs` header says so). The auction is the exchange; the
  ledger remains the accounting truth.
- **`core/ports.py` seams** (`Telemetry`, `Power`, `RecipeStore`) — the
  ADR-shaped way a Rust engine plugs in as "a new port beside the
  existing ones" when the team is ready; nothing about it breaks the
  event set.

**What we deliberately do NOT do**: write to their SQLite, add events to
their closed set, or run a second writer in their process. Ever. Their
§1.0 rule is why thor-world exists; we honor it from the kernel side.

## 2. infinigen-thor ⇄ ecology-thor (the money bridge)

The spike established the workload facts: coarse is CPU-serial (~2/14
cores), renders are GPU-bound (98%, ~3 GB, minutes each), scenes are
embarrassingly parallel, the EXACT-Boolean hang is the dominant overnight
failure, and V1's job states (`notqueued/queued/running/succeeded/
crashed/cancelled`) have **no timeout and no UNKNOWN** — a hang is a
silent stall forever. The `--on_interrupt` SIGTERM/SIGUSR1 checkpoint
already exists.

The proposal branch (read it for details) wires the ecology as the
admission layer:

```text
region goal (until deadline) ──┐
scene jobs (spawn as children) ├─▶ WHY ledger ── justification × urgency × …
running job  → run_under (re-attest by use)      │
stalled job  → no beats → freshness slides ──────┤
past horizon → HARVESTED to UNKNOWN (never       │
               deleted; SIGUSR1 checkpoint first) ▼
                                    auction ──▶ admits next renders under
                                                rails (GPU-seconds from the
                                                128 GB box's real budget)
```

Everything on the right already runs in this repo; the branch's
`ecology_admission` daemon is the adapter (with a pure-Python fallback
ledger when the Rust wheel isn't built, so they can dogfood semantics
before compiling anything).

## 3. thor-world ⇄ ecology-thor (the visibility bridge)

world-mcp is the MCP server agents speak; it is purity-law clean (never
imports builder-ide; only typed doors via `CoreClient`). The bridge that
respects that law: a **new resource class** — `world://why` — riding the
exact pattern of `world://dirty`: the SSE watcher already fans core
events out; the draft `why_stream.py` adds a WHY-ledger digest resource
fed by the ecology's JSON snapshot (below), zero server-side state.

## The stable JSON contract (`Ecology::snapshot_json`)

The summary's JSON shape is a **frozen contract** for all bridges
(append-only fields, never reorder):

```json
{
  "tick": 10391,
  "alive": 3206, "dead": 8, "unknown": 26, "absent": 0,
  "admitted_last": 2, "vetoed_last": 0, "skipped_infeasible_last": 0,
  "spent_gpu_seconds": 2952.30, "spent_joules": 73807.21, "spent_cents": 0.0,
  "cascade_pending": 0, "overrun_streak": 0, "safe_mode": false,
  "conservation_ok": true, "mass_total": 2173.90,
  "chain_hash": "2e8dcb4169e78ce1", "events_hashed": 16438, "events_dropped": 8246
}
```

Producers: `Ecology::snapshot_json()` (Rust), `ecology_thor.EcologyPy.snapshot_json()`
(Python). Consumers: builder-ide folds, the `world://why` resource,
night-runner dashboards, `ecology-sim --report`.

## Semantics compatibility table (the part audits care about)

| Parent (cognitive-ecology `why.py` v0.3) | ecology-thor | builder-ide `core/why` |
|---|---|---|
| four states, never delete | same (`why.rs`) | same (vendored) |
| weakest-link freshness, grace h/3, linear to 0 | same, exact port | (not priced — lifecycle weights 1.0/0.25/0.0 instead) |
| lapse at horizon, per-node | same | roots mortal via `until`+grace |
| revive on parole (cap 0.5) | same, service ends parole | (n/a) |
| DAG paths, OR-semantics | **trees only** (see ARCHITECTURE §2) | explicit/inferred edges |
| attention economy | here (veto law, rails, classes) | deliberately absent — this repo is its counterpart |
