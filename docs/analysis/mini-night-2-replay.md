# The mini-night-2 replay — the auction on a recorded night

*2026-11-01. The question, from the builder-ide seat: the attention
auction answers "who gets the GPU tonight" — show it on a recorded
night. This is the replay of builder-ide's mini-night-2
(`docs/lab/mini-night-2.md` there, 2026-10-31) through the ecology-thor
auction, next to what the night runner actually did. The tool is
`tools/replay_mini_night2.py`; every number below is its output, and
`--check` re-verifies the reconstruction against every aggregate the lab
record published.*

## 0. The setup, honestly

The lab record publishes aggregates, not the raw per-variant ledger (the
artifacts are gitignored), so the replay **reconstructs** the 18-variant
ledger from exactly the published numbers: per-task GPU/wall totals
exact, per-variant values inside the published ranges, pooled
medians/means on their published values. The reconstruction is a
constraint-solve (seeded, deterministic, verified by `--check`), and the
robustness pass at the end reruns every leg across six alternative
solutions of the same constraint set: the allocation decisions do not
move; only the savings number does (2.76–3.32 GPU-min).

The bids are priced from **night-1's committed ledger**
(`core/night/cost/data/mini_night.json` — 18 samples, per-seed) and
night-1's published scores. That is the ex-ante information set: night-1
ran the night before, and the night replays the same seeds. Night-2's
realized scores are used only to score what each policy kept — never as
a bid input.

The auction law is the kernel's own (`ecology-core/src/attention.rs`):
score = urgency · justification · opportunity · novelty ·
expected_value, canonical order, clamped, NaN vetoes; **score ≤ 0 never
transacts**; admission while the rails pay. On this night urgency,
justification and opportunity are all 1.0 (no deadlines filed, nothing
stalled, the lane free and the model resident) — the value factors do
the work, and the doc says so rather than pretending otherwise.

## 1. Leg A — parity: the auction is the runner, at the runner's prices

Priced with the same bands the schedule used (image p50 14.0 s,
mesh **prop.512** 16.1 s — the mispricing the lab record itself
documents in its §6), the auction admits all 18 variants in the same
order and lands on the same numbers:

| policy | GPU spent | of the 11.76-min bar | wall spent | cut | winners |
|---|---:|---:|---:|---:|---|
| the night runner (as run) | 9.42 min | 80.1% | 30.13 min | 0 | crate v1, lantern v1, anvil v2 |
| auction, the runner's prices | 9.42 min | 80.1% | 30.13 min | 0 | identical |

Held across all six reconstructions. This is the drop-in claim: at the
same information, the auction makes the same calls the runner made — it
is not a different policy by default. The runner is not wrong; it is
*incomplete*, which is what the next legs show.

## 2. Leg B — the veto cuts what the runner's own ε already knew

The runner already believes something about marginal seeds:
`IterationPolicy.min_gain = 0.02` — "a round that gains no more than ε
ends the loop." It asks that question **once per 3-variant round**. The
tick loop asks it at **every admission**, priced from the only score
history the box had (night-1, same seeds):

| seed line k | E[gain over best of the first k−1], from night-1 | EV priced (gain/ε) | bid score |
|---|---:|---:|---:|
| 1 | — (the first draw is the value) | 1.00 | 1.000 |
| 2 | 0.003 | 0.15 | 0.075 |
| 3 | **0.000** (0, 0, 0 across all three requests) | **0.00** | **0.000 → vetoed** |

Every third seed line in night-1 scored **at or below** the best of its
first two (crate 0.727 < 0.750; lantern 0.684 = 0.684; anvil 0.654 <
0.661). The pooled expected gain of a third seed was exactly zero. The
veto law then does the only thing it may do with a zero-attention bid:
it refuses it. Six variants (the image *and* mesh of each request's
third seed line) never run:

| policy | GPU spent | of bar | wall spent | cut | winners (night-2 scores) | value kept |
|---|---:|---:|---:|---:|---|---:|
| runner | 9.42 min | 80.1% | 30.13 min | 0 | v1, v1, v2 | 2.857 |
| auction, value-priced | **6.54 min** | **55.7%** | **20.62 min** | 6 (all v3) | **v1, v1, v2** | **2.857** |

- **2.88 GPU-min saved (30.6%), 9.51 wall-min saved (31.6%), at exactly
  zero winner loss** — the winners were determined by the first two
  seeds in *both* nights.
- The night's wall overrun past its own ask shrinks from **11.64 min to
  ~2.1 min** (the value-priced night lands at 1.12× its ask; the runner
  landed at 1.63×).
- Robustness: across six reconstructions the veto set is the six v3s
  every time (the veto is priced from night-1's *scores*, which no
  reconstruction touches), and parity holds 6/6.

**The honest caveat, stated in full:** this is three data points, all
zero. The 95% Wilson upper bound on "a third seed line ever beats the
best of the first two" at n=3 is 0.56 — thin. The claim is not "third
seeds never win"; it is that *the pricing worked as designed on the only
data the box had*, and night-2 confirmed it (realized third-seed gains:
0.000, 0.000, 0.000 — the same three zeros again). The mechanism is
symmetric: the moment a third seed wins somewhere, E[gain] > 0, the EV
factor rises, and the auction prices third seeds back in. That is what
a marginal-value scheduler is.

## 3. Leg C — learned peaks: the ask that stops lying

The runner admitted variant 16 against a **0.268 GPU-min ask** (the
prop.512 band) while every prop was actually costing ~0.9 — the bank
drained at 3.5× the admission price, silently, all night. With learned
peaks (the p95 of the label's own measured history — the same
convention thor-admit uses for memory and `core/night/cost` uses for
time), the auction's asks converge to reality as the night runs:

```
prop asks: 0.268 → 0.268 → 0.268 (the band p50, until 2 samples)
         → 0.978 → 1.047 → 1.043 → 1.040 ...  (learned p95, in flight)
```

Same allocation on this night (the bar was never truly binding — see
§4), but the *projected* spend now tracks the actuals, which is the
difference between an admission check and a lucky one.

## 4. The bar itself was the artifact

The bars are arithmetic, not judgement
(`core/genbatch/night.py _rails_for`: ask × 1.5 + 5 GPU-min, × 1.5 + 10
wall-min — verified: 4.508 × 1.5 + 5 = **11.76**, 18.487 × 1.5 + 10 =
**37.73**). The ask was built from the **wrong band** (prop.512, the
bare generator, instead of prop.night, the mini-night's own jobs — the
lab record's own §6 finding). Priced on the right band, which the cost
model already carried:

| | ask | bar | the night's 9.42 min measures |
|---|---:|---:|---:|
| as priced (prop.512) | 4.508 GPU-min | 11.76 | **80.1% — "one bad night from cutting work"** |
| on the right band (prop.night) | 8.322 GPU-min | 17.48 | **53.9%** |

The near-miss the morning report showed was the wrong price, not a real
shortage. The auction closes that loop by construction: its bids are
its prices, and its prices learn.

## 5. The verdict, and what it is not

**On this recorded night, no policy should have cut at the bar** — the
night fits, and the replay's runner model reproduces the record exactly
(nothing cut, 80.1%/79.9%, drain line crossed only on the final
variant). The case for the tick loop is not "the auction cuts at the
bar differently." It is three smaller things, each verifiable above:

1. **The veto prices value, not just cost.** The runner's admission
   check asks "can the rails pay?" and never "is it worth it?" Its own
   ε = 0.02 — the belief that some gains are too small to chase — is
   asked once per round instead of once per variant. On this night the
   difference was **~30% of the GPU bar spent on variants whose expected
   gain, by the box's own prior, was zero**, with the winners untouched.
2. **The prices learn in flight** (leg C), so the admission check stops
   trusting a stale band — the exact failure that made the runner's bar
   both wrong (§4) and invisible while it drained (§3).
3. **The alarm was the price** (§4): the same night on the right band is
   a 54% night, not an 80% night.

What this replay does **not** show: a night where the bar truly binds
and the two policies cut *different work*. That night has not happened
yet; when it does, the runner's cut is positional (whatever is next in
the queue at the 80% line) while the auction's cut is by score (the
lowest-value bids go unfunded first, and the v3s never made it to the
line at all). The mechanism is ready for that night; the replay's job
was to show it is the *same* mechanism on a normal night, and cheaper.

## 6. What the auction needs from builder-ide to do this live

Exactly the seams this repo's bridges already propose
(`docs/INTEGRATION.md`): bids priced in the night's own units
(`Rail`: gpu-minutes / hosted-cents / wall-minutes — the bridge branch's
`ecology_bridge` speaks them), asks from `core/night/cost`'s bands plus
the learned-peak history, scores from the critic's verdicts, and a
winning bid mapped onto a thor-admit lease when the work launches. No
new writer, no new event types on our side — the ecology is its own
process, and the night runner stays the runner.

---

*Reproduce: `python3 tools/replay_mini_night2.py` (the report),
`--check` (verify the reconstruction against every published
aggregate), `--json` (machine-readable). The tool is stdlib-only and
embeds every input with its citation.*
