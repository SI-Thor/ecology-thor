#!/usr/bin/env python3
"""replay_mini_night2.py — replay builder-ide's mini-night-2 through the
ecology-thor attention auction, and compare it with what the night runner
actually did.

The question (from the builder-ide seat, 2026-11-01): the auction answers
"who gets the GPU tonight" — show it on a recorded night. Replay the
mini-night-2 ledger through the auction and compare its allocation to
what the night runner actually did (spend vs the 11.76-min GPU bar, what
got cut). If the auction would have cut differently and better, that is
the case for adopting the tick loop.

Everything here is stdlib-only and self-contained: every input number is
either embedded with its citation or derived, and `--check` re-verifies
the reconstruction against every aggregate the lab record published.

Sources (read them before changing any constant):
  [MN2]  SI-Thor/builder-ide docs/lab/mini-night-2.md (2026-10-31)
  [MN1]  SI-Thor/builder-ide docs/lab/mini-night.md (2026-10-31)
  [LED1] SI-Thor/builder-ide core/night/cost/data/mini_night.json — the
         committed night-1 ledger: 18 samples, per-seed gpu/cpu/wall.
         This is the ex-ante information set: everything the box knew
         the evening before mini-night-2 (same seeds — the night
         replays).
  [RUN]  SI-Thor/builder-ide core/night/runner.py — the runner's
         admission semantics this tool mirrors (task asks, spend booked
         at actuals, the 80/95/100% graduated throttle).
  [POL]  core/records.py IterationPolicy: variants_per_round=3,
         max_rounds=3, min_gain=0.02 — the runner's own epsilon.
  [BAR]  core/genbatch/night.py _rails_for: bar = ask*1.5 + 5 GPU-min,
         ask*1.5 + 10 wall-min — verified: 4.508*1.5+5 = 11.76,
         18.487*1.5+10 = 37.73, exactly the recorded bars.
  [AU]   ecology-thor crates/ecology-core/src/attention.rs — the auction
         law this tool mirrors: score = u*j*o*n*ev (canonical order,
         clamped [0,1], NaN vetoes), veto <= 0, order (class, score,
         id), greedy admit while the rails pay, infeasible skipped.

Run:
  python3 tools/replay_mini_night2.py            # the full report
  python3 tools/replay_mini_night2.py --check    # verify, exit 1 on drift
  python3 tools/replay_mini_night2.py --json     # machine-readable
"""

from __future__ import annotations

import json
import math
import sys
from dataclasses import dataclass, field

# --------------------------------------------------------------------------
# [MN2] §2 — the published aggregates this reconstruction must match.
# --------------------------------------------------------------------------

# Per-task measured (GPU minutes, wall minutes), exactly as the lab table.
TASKS = [  # (stage, request, ask_gpu_min, ask_wall_min, meas_gpu_min, meas_wall_min)
    ("img",  "crate",   0.699, 0.834, 0.697, 0.820),
    ("img",  "lantern", 0.699, 0.834, 0.683, 0.795),
    ("img",  "anvil",   0.699, 0.834, 0.776, 0.878),
    ("prop", "crate",   0.804, 5.328, 2.871, 9.735),
    ("prop", "lantern", 0.804, 5.328, 2.313, 8.174),
    ("prop", "anvil",   0.804, 5.328, 2.081, 9.727),
]

TOTAL_ASK_GPU_MIN = 4.508    # [MN2] §2 table
TOTAL_ASK_WALL_MIN = 18.487
TOTAL_MEAS_GPU_MIN = 9.421
TOTAL_MEAS_WALL_MIN = 30.129
GPU_BAR_MIN = 11.76          # [MN2] §2 "GPU rail spent 9.42 of 11.76 min"
WALL_BAR_MIN = 37.73
LANE_GPU_SECONDS = 565.0     # [MN2] §2 "GPU lane 565 s"
PROMPT_WALL_SECONDS = 1808.0  # [MN2] §2 "wall (sum of prompt windows) 1,808 s"

# [MN2] §2 per-variant measured ranges and medians (9 image + 9 prop).
IMAGE_GPU_RANGE_S = (11.8, 20.3)
IMAGE_GPU_MEDIAN_S = 14.0
IMAGE_WALL_RANGE_S = (13.3, 22.2)
IMAGE_WALL_MEDIAN_S = 16.1
PROP_GPU_RANGE_S = (18.3, 65.9)
PROP_GPU_MEDIAN_S = 54.9
PROP_GPU_MEAN_S = 48.4
PROP_WALL_RANGE_S = (131.2, 241.6)
PROP_WALL_MEDIAN_S = 180.5
PROP_WALL_MEAN_S = 184.2

# [MN2] §6 — the two bands. The schedule priced the mesh stage as
# prop.512 (the bare generator, 2 samples); the model already carried
# prop.night (the mini-night's own 9 prop jobs). Per-variant p50s:
BAND_PROP512 = {"gpu_s": 16.1, "wall_s": 106.6}   # what the runner priced
BAND_PROPNIGHT = {"gpu_s": 41.5, "wall_s": 139.8}  # what the data said
BAND_IMAGE = {"gpu_s": 0.233 * 60, "wall_s": 0.278 * 60}  # 14.0 / 16.7 s

# --------------------------------------------------------------------------
# [LED1] night-1 per-seed measurements (gpu_s, cpu_s, wall_s) — the ex-ante
# information set. Same seeds ran in mini-night-2 (the night replays).
# --------------------------------------------------------------------------

NIGHT1 = {
    ("img", "crate", 1):   (10.78, 1.54, 12.32),   # seed 1064508603
    ("img", "crate", 2):   (10.52, 1.61, 12.13),   # seed 455794046
    ("img", "crate", 3):   (13.28, 2.08, 15.36),   # seed 833331073
    ("img", "lantern", 1): (13.98, 2.71, 16.68),   # seed 424809304
    ("img", "lantern", 2): (13.21, 1.99, 15.20),   # seed 1427939119
    ("img", "lantern", 3): (17.35, 1.84, 19.19),   # seed 913610340
    ("img", "anvil", 1):   (17.74, 2.70, 20.44),   # seed 1597230408
    ("img", "anvil", 2):   (24.45, 1.95, 26.40),   # seed 64961400
    ("img", "anvil", 3):   (22.94, 1.49, 24.43),   # seed 6984999
    ("prop", "crate", 1):   (67.16, 112.92, 180.09),
    ("prop", "crate", 2):   (22.08, 117.67, 139.75),
    ("prop", "crate", 3):   (88.36, 124.11, 212.47),
    ("prop", "lantern", 1): (57.07, 86.34, 143.42),
    ("prop", "lantern", 2): (37.30, 127.78, 165.07),
    ("prop", "lantern", 3): (41.52, 71.42, 112.94),
    ("prop", "anvil", 1):   (16.19, 95.51, 111.71),
    ("prop", "anvil", 2):   (46.69, 73.30, 119.99),
    ("prop", "anvil", 3):   (28.73, 102.06, 130.80),
}

# [MN1] §3 — night-1 critic scores per (request, variant). The winner of
# each request and what it beat; these are the ex-ante value data.
NIGHT1_SCORES = {
    ("crate", 1): 0.750, ("crate", 2): 0.707, ("crate", 3): 0.727,
    ("lantern", 1): 0.684, ("lantern", 2): 0.668, ("lantern", 3): 0.684,
    ("anvil", 1): 0.652, ("anvil", 2): 0.661, ("anvil", 3): 0.654,
}

# [MN2] §3 — night-2 realized scores (same seeds; the repair lifted every
# prop by ~+0.25 uniformly and reordered nothing). Used to score what each
# policy's allocation would have kept, never as a bid input.
NIGHT2_SCORES = {
    ("crate", 1): 1.000, ("crate", 2): 0.960, ("crate", 3): 0.979,
    ("lantern", 1): 0.939, ("lantern", 2): 0.924, ("lantern", 3): 0.939,
    ("anvil", 1): 0.910, ("anvil", 2): 0.918, ("anvil", 3): 0.911,
}

# The runner's own epsilon [POL]: a gain below this is "no improvement".
MIN_GAIN = 0.02

REQUESTS = ("crate", "lantern", "anvil")


# --------------------------------------------------------------------------
# Reconstruction of night-2's per-variant ledger from the aggregates.
# --------------------------------------------------------------------------

@dataclass
class Variant:
    stage: str      # "img" | "prop"
    request: str
    index: int      # 1..3 (the seed line)
    gpu_s: float = 0.0
    wall_s: float = 0.0
    run: bool = True
    reason: str = ""


def _water_fill(target_sum: float, lo: float, hi: float, prior: list[float],
                iterations: int = 60) -> list[float]:
    """Distribute `target_sum` over 3 values in [lo, hi], staying close to
    `prior` (night-1's per-seed shape, scaled). Deterministic."""
    scale = target_sum / max(sum(prior), 1e-9)
    values = [p * scale for p in prior]
    for _ in range(iterations):
        # pull toward the exact sum
        delta = target_sum - sum(values)
        values = [v + delta / len(values) for v in values]
        # clip into the published range, redistributing the clip
        for _ in range(50):
            excess = 0.0
            fixed = []
            for i, v in enumerate(values):
                if v < lo:
                    excess += lo - v
                    values[i] = lo
                elif v > hi:
                    excess += v - hi
                    values[i] = hi
                else:
                    fixed.append(i)
            if excess <= 1e-9 or not fixed:
                break
            values = [v + (excess / len(fixed) if i in fixed else 0.0)
                      for i, v in enumerate(values)]
    # final nudge to kill float drift
    drift = target_sum - sum(values)
    values[-1] += drift
    return values


def _solve_axis(task_sums: list[float], lo: float, hi: float,
                median_target: float, seed: int) -> list[float]:
    """Nine values, three per task: each task sums exactly, every value in
    [lo, hi], and the pooled median (5th of 9) lands on `median_target`.

    Method: seeded random in-range starts, water-filled to the task sums,
    then an in-task mass-moving loop that walks the pooled median to its
    target without breaking sums or bounds. Deterministic per seed; the
    analysis reruns it across a seed ladder to show its conclusions are
    not an artifact of the particular solution.
    """
    import random

    rng = random.Random(seed)
    best: list[float] | None = None
    best_miss = float("inf")
    for attempt in range(200):
        values: list[float] = []
        for s in task_sums:
            start = [lo + rng.random() * (hi - lo) for _ in range(3)]
            values.extend(_water_fill(s, lo, hi, start, iterations=80))
        # median walk: move the pooled 5th toward the target by shifting
        # mass inside its task, bounds respected, sums preserved.
        for _ in range(400):
            order = sorted(range(9), key=lambda i: values[i])
            mid = order[4]
            miss = median_target - values[mid]
            if abs(miss) < 0.05:
                break
            # partner: same task, room to absorb -miss without leaving range
            task = mid // 3
            partners = [task * 3 + k for k in range(3) if task * 3 + k != mid]
            rng.shuffle(partners)
            moved = False
            for p in partners:
                if miss > 0 and values[p] - miss >= lo \
                        and values[mid] + miss <= hi:
                    values[p] -= miss
                    values[mid] += miss
                    moved = True
                    break
                if miss < 0 and values[p] - miss <= hi \
                        and values[mid] + miss >= lo:
                    values[p] -= miss  # -miss > 0: partner gains
                    values[mid] += miss
                    moved = True
                    break
            if not moved:
                # try a smaller step
                for p in partners:
                    step = miss * 0.5
                    if abs(step) < 1e-6:
                        break
                    if lo <= values[p] - step <= hi and lo <= values[mid] + step <= hi:
                        values[p] -= step
                        values[mid] += step
                        moved = True
                        break
                if not moved:
                    break
        order = sorted(values)
        miss = abs(order[4] - median_target)
        if miss < best_miss:
            best_miss, best = miss, list(values)
        if miss < 0.05:
            return values
    assert best is not None
    return best


def reconstruct(seed: int = 20261101) -> list[Variant]:
    """The 18 variants in run order, per-variant costs fitted to every
    published aggregate: task sums exact, per-variant values inside the
    published ranges, pooled medians on their published values.

    The lab record publishes aggregates, not the raw per-variant ledger
    (it lives in the gitignored artifacts), so this reconstruction solves
    the constraint set the record defines. The analysis below reruns it
    across a seed ladder to show its conclusions are not an artifact of
    any particular solution. Every published number is re-verified by
    `--check`.
    """
    variants: list[Variant] = []
    # solve each axis pooled over its three tasks
    img_tasks = [t for t in TASKS if t[0] == "img"]
    prop_tasks = [t for t in TASKS if t[0] == "prop"]
    axes = {
        ("img", "gpu"): _solve_axis([t[4] * 60 for t in img_tasks],
                                     IMAGE_GPU_RANGE_S[0], IMAGE_GPU_RANGE_S[1],
                                     IMAGE_GPU_MEDIAN_S, seed),
        ("img", "wall"): _solve_axis([t[5] * 60 for t in img_tasks],
                                      IMAGE_WALL_RANGE_S[0], IMAGE_WALL_RANGE_S[1],
                                      IMAGE_WALL_MEDIAN_S, seed + 1),
        ("prop", "gpu"): _solve_axis([t[4] * 60 for t in prop_tasks],
                                      PROP_GPU_RANGE_S[0], PROP_GPU_RANGE_S[1],
                                      PROP_GPU_MEDIAN_S, seed + 2),
        ("prop", "wall"): _solve_axis([t[5] * 60 for t in prop_tasks],
                                       PROP_WALL_RANGE_S[0], PROP_WALL_RANGE_S[1],
                                       PROP_WALL_MEDIAN_S, seed + 3),
    }
    for si, stage in enumerate(("img", "prop")):
        for ti, (s, request, _ag, _aw, _mg, _mw) in enumerate(
                img_tasks if stage == "img" else prop_tasks):
            for k in (1, 2, 3):
                idx = ti * 3 + (k - 1)
                variants.append(Variant(
                    stage, request, k,
                    gpu_s=axes[(stage, "gpu")][idx],
                    wall_s=axes[(stage, "wall")][idx]))
    return variants


def fit_report(variants: list[Variant]) -> list[str]:
    lines = []
    images = [v for v in variants if v.stage == "img"]
    props = [v for v in variants if v.stage == "prop"]
    total_gpu = sum(v.gpu_s for v in variants)
    total_wall = sum(v.wall_s for v in variants)

    def med(xs):
        xs = sorted(xs)
        n = len(xs)
        return xs[n // 2] if n % 2 else (xs[n // 2 - 1] + xs[n // 2]) / 2

    lines.append(f"  task sums: exact by construction (water-fill)")
    lines.append(f"  total gpu: {total_gpu/60:.3f} min (published {TOTAL_MEAS_GPU_MIN})")
    lines.append(f"  total wall: {total_wall/60:.3f} min (published {TOTAL_MEAS_WALL_MIN})")
    for name, group, ranges, axis in (
        ("image gpu", [v.gpu_s for v in images], IMAGE_GPU_RANGE_S, "s"),
        ("image wall", [v.wall_s for v in images], IMAGE_WALL_RANGE_S, "s"),
        ("prop gpu", [v.gpu_s for v in props], PROP_GPU_RANGE_S, "s"),
        ("prop wall", [v.wall_s for v in props], PROP_WALL_RANGE_S, "s"),
    ):
        vals = [abs(min(group) - ranges[0]) < 1e-6 or min(group) >= ranges[0] - 1e-9,
                max(group) <= ranges[1] + 1e-9]
        lines.append(f"  {name}: range [{min(group):.1f}, {max(group):.1f}] "
                     f"(published {ranges[0]}–{ranges[1]}) "
                     f"median {med(group):.1f} mean {sum(group)/len(group):.1f} "
                     f"in-range={'yes' if all(vals) else 'NO'}")
    return lines


def check(variants: list[Variant]) -> bool:
    """Every published aggregate, re-verified. Exit-nonzero material."""
    ok = True
    images = [v for v in variants if v.stage == "img"]
    props = [v for v in variants if v.stage == "prop"]

    def med(xs):
        xs = sorted(xs)
        return xs[len(xs) // 2]

    # task sums exact
    for stage, request, _ag, _aw, mg, mw in TASKS:
        group = [v for v in variants if v.stage == stage and v.request == request]
        if abs(sum(v.gpu_s for v in group) / 60.0 - mg) > 1e-6:
            print(f"CHECK FAIL: {stage}:{request} gpu sum"); ok = False
        if abs(sum(v.wall_s for v in group) / 60.0 - mw) > 1e-6:
            print(f"CHECK FAIL: {stage}:{request} wall sum"); ok = False
    # totals
    if abs(sum(v.gpu_s for v in variants) / 60.0 - TOTAL_MEAS_GPU_MIN) > 0.005:
        print("CHECK FAIL: total gpu"); ok = False
    if abs(sum(v.wall_s for v in variants) / 60.0 - TOTAL_MEAS_WALL_MIN) > 0.005:
        print("CHECK FAIL: total wall"); ok = False
    # ranges
    for group, (lo, hi), axis in ((images, IMAGE_GPU_RANGE_S, "gpu"),
                                  (props, PROP_GPU_RANGE_S, "gpu"),
                                  (images, IMAGE_WALL_RANGE_S, "wall"),
                                  (props, PROP_WALL_RANGE_S, "wall")):
        vals = [getattr(v, f"{axis}_s") for v in group]
        if min(vals) < lo - 1e-9 or max(vals) > hi + 1e-9:
            print(f"CHECK FAIL: {axis} range [{min(vals):.1f},{max(vals):.1f}] "
                  f"outside published {lo}-{hi}"); ok = False
    # pooled medians/means within the doc's rounding
    if abs(med([v.gpu_s for v in images]) - IMAGE_GPU_MEDIAN_S) > 0.6:
        print(f"CHECK FAIL: image gpu median {med([v.gpu_s for v in images]):.1f} "
              f"vs published {IMAGE_GPU_MEDIAN_S}"); ok = False
    if abs(med([v.gpu_s for v in props]) - PROP_GPU_MEDIAN_S) > 0.6:
        print(f"CHECK FAIL: prop gpu median {med([v.gpu_s for v in props]):.1f} "
              f"vs published {PROP_GPU_MEDIAN_S}"); ok = False
    if abs(sum(v.gpu_s for v in props) / 9 - PROP_GPU_MEAN_S) > 0.8:
        print(f"CHECK FAIL: prop gpu mean {sum(v.gpu_s for v in props)/9:.1f} "
              f"vs published {PROP_GPU_MEAN_S}"); ok = False
    if abs(med([v.wall_s for v in props]) - PROP_WALL_MEDIAN_S) > 3.0:
        print(f"CHECK FAIL: prop wall median vs published {PROP_WALL_MEDIAN_S}"); ok = False
    if abs(sum(v.wall_s for v in props) / 9 - PROP_WALL_MEAN_S) > 3.0:
        print(f"CHECK FAIL: prop wall mean vs published {PROP_WALL_MEAN_S}"); ok = False
    # the bars, from the ask [BAR]
    if abs(TOTAL_ASK_GPU_MIN * 1.5 + 5.0 - GPU_BAR_MIN) > 0.01:
        print("CHECK FAIL: GPU bar is not ask*1.5+5"); ok = False
    if abs(TOTAL_ASK_WALL_MIN * 1.5 + 10.0 - WALL_BAR_MIN) > 0.01:
        print("CHECK FAIL: wall bar is not ask*1.5+10"); ok = False
    return ok


# --------------------------------------------------------------------------
# The two policies.
# --------------------------------------------------------------------------

@dataclass
class PolicyResult:
    name: str
    ran: list[Variant] = field(default_factory=list)
    cut: list[tuple[Variant, str]] = field(default_factory=list)
    gpu_spent_min: float = 0.0
    wall_spent_min: float = 0.0
    trace: list[str] = field(default_factory=list)

    def winners(self) -> dict[str, tuple[int, float]]:
        """Best surviving seed line per request, scored by night-2 [MN2 §3]."""
        out = {}
        for request in REQUESTS:
            best = None
            for k in (1, 2, 3):
                prop = next((v for v in self.ran
                             if v.stage == "prop" and v.request == request
                             and v.index == k), None)
                if prop is not None and prop.run \
                        and (best is None or NIGHT2_SCORES[(request, k)] > best[1]):
                    best = (k, NIGHT2_SCORES[(request, k)])
            out[request] = best if best else (0, 0.0)
        return out


def runner_model(variants: list[Variant]) -> PolicyResult:
    """The night runner as it ran [RUN]: task-level admission by the
    priced ask, spend booked at the actual, graduated throttle 80/95/100
    on the binding axis, one prompt at a time. Its asks are the bands the
    schedule priced [MN2 §6]: image p50 14.0/16.7 s, prop prop.512
    16.1/106.6 s per variant."""
    result = PolicyResult("night runner (as run)")
    gpu_spent = 0.0  # minutes, booked at actuals [RUN _record_spend]
    wall_spent = 0.0

    def throttle(gpu: float, wall: float) -> float:
        """The binding axis's utilisation [RUN throttle_level]."""
        ratios = [gpu / GPU_BAR_MIN, wall / WALL_BAR_MIN]
        return max(ratios)

    for v in variants:
        # the ask the runner would admit this variant against: the band
        # p50 the schedule priced ([MN2] §6: prop.512 for the mesh stage)
        ask_gpu = (BAND_IMAGE if v.stage == "img" else BAND_PROP512)["gpu_s"] / 60.0
        ask_wall = (BAND_IMAGE if v.stage == "img" else BAND_PROP512)["wall_s"] / 60.0
        util = throttle(gpu_spent, wall_spent)
        if util >= 0.80:  # drain: stop opening new work [RUN DRAIN_AT]
            v.run, v.reason = False, f"cut at the 80% bar (utilisation {util:.0%})"
            result.cut.append((v, v.reason))
            continue
        # admission before spend, by the ASK [RUN _admit]
        if gpu_spent + ask_gpu > GPU_BAR_MIN or wall_spent + ask_wall > WALL_BAR_MIN:
            v.run, v.reason = False, "ask did not fit the remaining rail"
            result.cut.append((v, v.reason))
            continue
        # the work runs; the spend is booked at the ACTUAL [RUN]
        gpu_spent += v.gpu_s / 60.0
        wall_spent += v.wall_s / 60.0
        result.ran.append(v)
        result.trace.append(
            f"    ran {v.stage}:{v.request}_v{v.index}  ask {ask_gpu:.3f} gpu-min, "
            f"actual {v.gpu_s/60:.3f} — bank now {gpu_spent:.2f}/{GPU_BAR_MIN} gpu, "
            f"{wall_spent:.2f}/{WALL_BAR_MIN} wall")
    result.gpu_spent_min = gpu_spent
    result.wall_spent_min = wall_spent
    return result


def _percentile(values: list[float], q: float) -> float:
    """Linear interpolation, NumPy's convention — the same one
    core/night/cost/model.py and thor-admit's learned peaks use."""
    ordered = sorted(values)
    if len(ordered) == 1:
        return ordered[0]
    position = (len(ordered) - 1) * q
    low, high = math.floor(position), math.ceil(position)
    if low == high:
        return ordered[int(position)]
    return ordered[low] + (ordered[high] - ordered[low]) * (position - low)


def _clamp01(x: float) -> float:
    if math.isnan(x) or x < 0.0:
        return 0.0
    return x if x <= 1.0 else 1.0


def attention_score(u: float, j: float, o: float, n: float, ev: float) -> float:
    """The law [AU attention.rs]: canonical multiply order, clamped."""
    return _clamp01(u) * _clamp01(j) * _clamp01(o) * _clamp01(n) * _clamp01(ev)


def auction_model(variants: list[Variant], *, value_priced: bool,
                  learned_peaks: bool) -> PolicyResult:
    """The tick-loop auction [AU]: one admission decision per variant, in
    run order, each bid priced with everything known at that moment.

    Factors on this night (documented inert or active):
      urgency 1.0        — no per-request deadlines were filed
      justification 1.0  — nothing stalled; every purpose fresh
      opportunity 1.0    — the lane is free and the model resident
      novelty 1/k        — the k-th seed of a request explores less
      expected_value     — leg A: 1.0 (rails only, runner parity)
                           leg B: E[gain of seed line k] / MIN_GAIN, from
                           night-1's ledger — the runner's own epsilon
                           [POL], asked at every admission instead of
                           once per 3-variant round.
    Cost: the priced ask (leg A: the runner's bands; learned peaks: the
    p95 of the label's own measured history, thor-admit's --peak auto
    convention) — admission still checks the ask, the bank still pays the
    actual.
    """
    label = ("auction" if not value_priced and not learned_peaks else
             "auction + value pricing" if value_priced and not learned_peaks else
             "auction + value pricing + learned peaks" if value_priced else
             "auction + learned peaks")
    result = PolicyResult(label)
    gpu_spent = 0.0
    wall_spent = 0.0
    history: dict[str, list[float]] = {"img": [], "prop": []}
    # night-1's within-request score data, for expected-gain pricing
    n1 = NIGHT1_SCORES

    for v in variants:
        # ---- the bid's cost: the priced ask, learned in flight ----
        band = BAND_IMAGE if v.stage == "img" else BAND_PROP512
        if learned_peaks and len(history[v.stage]) >= 2:
            ask_gpu = max(band["gpu_s"],
                          _percentile(history[v.stage], 0.95)) / 60.0
            ask_wall = max(band["wall_s"],
                           _percentile([h * (band["wall_s"] / band["gpu_s"])
                                        for h in history[v.stage]], 0.95)) / 60.0
        else:
            ask_gpu = band["gpu_s"] / 60.0
            ask_wall = band["wall_s"] / 60.0

        # ---- the bid's value factors ----
        novelty = 1.0 / v.index
        if value_priced:
            if v.index == 1:
                expected_gain = MIN_GAIN  # the first draw IS the value
            else:
                # E[gain of seed line k] from night-1's ledger, pooled over
                # the three requests: the mean of (score_k - best of the
                # first k-1)+. Night-1 ran BEFORE night-2, so this is the
                # ex-ante information set, not hindsight; the same-seed
                # replay caveat applies to night-2's realized scores, which
                # are never bid inputs. The pooled estimate is what a
                # deployed auction would compute from its history.
                gains = []
                for other in REQUESTS:
                    prior_best = max(n1[(other, i)] for i in range(1, v.index))
                    gains.append(max(0.0, n1[(other, v.index)] - prior_best))
                expected_gain = sum(gains) / len(gains) if gains else 0.0
            ev = _clamp01(expected_gain / MIN_GAIN)
        else:
            ev = 1.0

        score = attention_score(1.0, 1.0, 1.0, novelty, ev)

        # ---- the auction law ----
        if score <= 0.0:
            v.run, v.reason = False, (f"VETOED: score {score:.3f} "
                                      f"(ev {ev:.2f}, novelty {novelty:.2f}) — "
                                      f"zero-attention never transacts")
            result.cut.append((v, v.reason))
            continue
        if gpu_spent + ask_gpu > GPU_BAR_MIN or wall_spent + ask_wall > WALL_BAR_MIN:
            v.run, v.reason = False, (f"skipped infeasible: ask {ask_gpu:.2f} gpu / "
                                      f"{ask_wall:.2f} wall vs remaining "
                                      f"{GPU_BAR_MIN - gpu_spent:.2f} / "
                                      f"{WALL_BAR_MIN - wall_spent:.2f}")
            result.cut.append((v, v.reason))
            continue
        gpu_spent += v.gpu_s / 60.0
        wall_spent += v.wall_s / 60.0
        history[v.stage].append(v.gpu_s)
        result.ran.append(v)
        result.trace.append(
            f"    admitted {v.stage}:{v.request}_v{v.index}  score {score:.3f} "
            f"(n {novelty:.2f}, ev {ev:.2f}) ask {ask_gpu:.3f} gpu-min, "
            f"actual {v.gpu_s/60:.3f} — bank {gpu_spent:.2f}/{GPU_BAR_MIN} gpu, "
            f"{wall_spent:.2f}/{WALL_BAR_MIN} wall")
    result.gpu_spent_min = gpu_spent
    result.wall_spent_min = wall_spent
    return result


# --------------------------------------------------------------------------
# The report.
# --------------------------------------------------------------------------

def main(argv: list[str]) -> int:
    check_only = "--check" in argv
    as_json = "--json" in argv
    variants = reconstruct()

    if check_only:
        return 0 if check(variants) else 1

    runner = runner_model([Variant(**vars(v)) for v in variants])
    leg_a = auction_model([Variant(**vars(v)) for v in variants],
                          value_priced=False, learned_peaks=False)
    leg_b = auction_model([Variant(**vars(v)) for v in variants],
                          value_priced=True, learned_peaks=False)
    leg_c = auction_model([Variant(**vars(v)) for v in variants],
                          value_priced=True, learned_peaks=True)

    # The bar arithmetic [BAR], end to end: what the bars would have been
    # had they been derived from the correctly-priced asks.
    correct_ask_gpu = 9 * BAND_IMAGE["gpu_s"] / 60 + 9 * BAND_PROPNIGHT["gpu_s"] / 60
    correct_ask_wall = 9 * BAND_IMAGE["wall_s"] / 60 + 9 * BAND_PROPNIGHT["wall_s"] / 60
    correct_gpu_bar = correct_ask_gpu * 1.5 + 5.0
    correct_wall_bar = correct_ask_wall * 1.5 + 10.0

    if as_json:
        def dump(p: PolicyResult):
            return {
                "name": p.name,
                "ran": [f"{v.stage}:{v.request}_v{v.index}" for v in p.ran if v.run],
                "cut": [{"variant": f"{v.stage}:{v.request}_v{v.index}", "reason": r}
                        for v, r in p.cut],
                "gpu_spent_min": round(p.gpu_spent_min, 3),
                "wall_spent_min": round(p.wall_spent_min, 3),
                "gpu_pct_of_bar": round(p.gpu_spent_min / GPU_BAR_MIN, 3),
                "winners": {k: {"seed_line": w[0], "score": w[1]}
                            for k, w in p.winners().items()},
            }
        print(json.dumps({
            "reconstruction": "fitted to [MN2] aggregates; run tools/... --check",
            "gpu_bar_min": GPU_BAR_MIN, "wall_bar_min": WALL_BAR_MIN,
            "runner": dump(runner), "leg_a": dump(leg_a),
            "leg_b": dump(leg_b), "leg_c": dump(leg_c),
            "correct_pricing": {
                "ask_gpu_min": round(correct_ask_gpu, 3),
                "gpu_bar_min": round(correct_gpu_bar, 3),
                "night_pct": round(TOTAL_MEAS_GPU_MIN / correct_gpu_bar, 3),
                "ask_wall_min": round(correct_ask_wall, 3),
                "wall_bar_min": round(correct_wall_bar, 3),
                "night_wall_pct": round(TOTAL_MEAS_WALL_MIN / correct_wall_bar, 3)},
        }, indent=1))
        return 0

    W = 78
    print("=" * W)
    print("mini-night-2, replayed through the attention auction")
    print("=" * W)
    print()
    print("reconstruction (per-variant ledger fitted to the published")
    print("aggregates of [MN2]; every number below is checkable):")
    for line in fit_report(variants):
        print(line)
    print()
    print(f"the bars [BAR]: gpu {GPU_BAR_MIN} = ask {TOTAL_ASK_GPU_MIN} * 1.5 + 5;")
    print(f"                wall {WALL_BAR_MIN} = ask {TOTAL_ASK_WALL_MIN} * 1.5 + 10")
    print()

    for p in (runner, leg_a, leg_b, leg_c):
        print("-" * W)
        print(f"{p.name}")
        print("-" * W)
        for line in p.trace:
            print(line)
        print(f"  spend: {p.gpu_spent_min:.2f} of {GPU_BAR_MIN} gpu-min "
              f"({p.gpu_spent_min/GPU_BAR_MIN:.1%}), "
              f"{p.wall_spent_min:.2f} of {WALL_BAR_MIN} wall-min "
              f"({p.wall_spent_min/WALL_BAR_MIN:.1%})")
        if p.cut:
            print(f"  cut ({len(p.cut)}):")
            for v, reason in p.cut:
                print(f"    {v.stage}:{v.request}_v{v.index} — {reason}")
        else:
            print("  cut: nothing")
        winners = p.winners()
        print("  winners: " + ", ".join(
            f"{r} = v{k} ({s:.3f})" for r, (k, s) in winners.items()))
        print()

    print("-" * W)
    print("the comparison (the question: spend vs the 11.76-min GPU bar,")
    print("what got cut)")
    print("-" * W)
    rows = [("what the runner did", runner), ("auction, runner's prices", leg_a),
            ("auction, value-priced", leg_b), ("auction, value + learned", leg_c)]
    print(f"{'policy':28s} {'gpu spent':>10s} {'of bar':>8s} {'wall spent':>10s} "
          f"{'cut':>4s} {'winners (night-2 scores)':>28s}")
    for name, p in rows:
        w = p.winners()
        print(f"{name:28s} {p.gpu_spent_min:8.2f}m {p.gpu_spent_min/GPU_BAR_MIN:7.1%} "
              f"{p.wall_spent_min:8.2f}m {len(p.cut):4d}   "
              + ", ".join(f"v{k}" for k, _ in w.values()))
    print()
    print("value kept (sum of the winners' night-2 scores; the runner's")
    print("realized answer is the reference):")
    ref = sum(s for _, s in runner.winners().values())
    for name, p in rows:
        kept = sum(s for _, s in p.winners().values())
        print(f"  {name:28s} {kept:.3f}  ({kept - ref:+.3f} vs the runner)")
    print()
    print("-" * W)
    print("the bar arithmetic, done on the right band [MN2 §6]")
    print("-" * W)
    print(f"  the runner priced the mesh stage as prop.512 and built the bar")
    print(f"  from that ask: {TOTAL_ASK_GPU_MIN:.3f} gpu-min ask -> {GPU_BAR_MIN} bar;")
    print(f"  the night measured {TOTAL_MEAS_GPU_MIN} -> {TOTAL_MEAS_GPU_MIN/GPU_BAR_MIN:.1%},")
    print(f"  the report's 'one bad night from cutting work' alarm.")
    print(f"  priced as prop.night (the data was in the cost model already):")
    print(f"  ask {correct_ask_gpu:.3f} gpu-min -> bar {correct_gpu_bar:.2f}; the same")
    print(f"  night measures {TOTAL_MEAS_GPU_MIN/correct_gpu_bar:.1%} — the night was")
    print(f"  never tight on the GPU axis; the alarm was the wrong band.")
    print(f"  (wall: ask {correct_ask_wall:.1f} -> bar {correct_wall_bar:.1f}; "
          f"the night measured {TOTAL_MEAS_WALL_MIN/correct_wall_bar:.1%}.)")
    print()

    # ------------------------------------------------------------------
    # Robustness: the reconstruction solves a constraint set, and the
    # analysis must not depend on the particular solution. Rerun the
    # legs across a seed ladder; report what moves and what does not.
    # ------------------------------------------------------------------
    print("-" * W)
    print("robustness (the legs across 6 alternative reconstructions of")
    print("the same published aggregates)")
    print("-" * W)
    parities = 0
    vetoes_stable = 0
    savings = []
    for i in range(6):
        alt = reconstruct(seed=20261101 + 101 * i)
        if not check(alt):
            print(f"  seed {i}: reconstruction failed --check (skipped)")
            continue
        r = runner_model([Variant(**vars(v)) for v in alt])
        a = auction_model([Variant(**vars(v)) for v in alt],
                          value_priced=False, learned_peaks=False)
        b = auction_model([Variant(**vars(v)) for v in alt],
                          value_priced=True, learned_peaks=False)
        same_alloc = (len(a.ran) == len(r.ran)
                      and all(x.stage == y.stage and x.request == y.request
                              and x.index == y.index
                              for x, y in zip(a.ran, r.ran)))
        parities += 1 if same_alloc else 0
        cut_names = sorted(f"{v.stage}:{v.request}_v{v.index}" for v, _ in b.cut)
        expected = sorted([f"img:{r}_v3" for r in REQUESTS]
                          + [f"prop:{r}_v3" for r in REQUESTS])
        if cut_names == expected:
            vetoes_stable += 1
        savings.append(r.gpu_spent_min - b.gpu_spent_min)
        print(f"  seed {i}: parity={'yes' if same_alloc else 'NO'}, "
              f"leg-B cuts the six v3s={'yes' if cut_names == expected else cut_names}, "
              f"gpu saved {r.gpu_spent_min - b.gpu_spent_min:.2f} min "
              f"(winner value {'kept' if b.winners() == r.winners() else 'CHANGED'})")
    print(f"  -> parity held {parities}/6, the veto set held {vetoes_stable}/6, "
          f"gpu saved {min(savings):.2f}-{max(savings):.2f} min "
          f"(the savings number moves with the reconstruction; the")
    print(f"     allocation decisions do not: the veto is priced from")
    print(f"     night-1's scores, which no reconstruction touches.)")
    print()
    print("verify: python3 tools/replay_mini_night2.py --check")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
