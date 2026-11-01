#!/usr/bin/env python3
"""daemon_ledger.py — read a thor-admit `ledger.jsonl` and say where the
box's time went, by hour and by cause.

Why this exists (study doc §7.7, builder-ide research/ecology-bridge):
the presence round gave thor-admit a time axis — one bucket per hour in
`~/.local/state/thor-admit/ledger.jsonl`, the tier seconds, the idle
seconds by cause, the GPU's power and busy share. The second recorded
night (the overnight Infinigen sweep) will be the first night that runs
under the presence scheduler, so its hours land in this file. This tool
is the box's side of that comparison: what the box was *actually doing*
while the night ran, so the replay can tell two failures apart that
look identical from the night's own ledger —

  `waiting_for_memory`  the box had no room: the night under-budgeted,
                        or something else held the memory;
  `no_work_registered`  the box had room and nobody asked: the presence
                        gap, a registration problem, never a budget one.

Two different failures, two different fixes. The night's runner ledger
cannot tell them apart; this file can.

Everything here is stdlib-only and read-only. Lines are JSON objects;
lines written by the chained build (research/ecology-ledger) carry
`seq`/`chain` and the reader ignores fields it does not use, so one file
can hold both kinds. `--check` re-verifies the reader against the two
windows the lab record published.

Sources (read them before changing any constant):
  [SCHEMA] SI-Thor/builder-ide tools/thor-admit/src/ledger.rs — the
           Bucket/TierStat/IdleStat/GpuStat/CpuStat/MemStat field
           names, as merged to main (the presence round).
  [EXP]    SI-Thor/builder-ide docs/lab/exp-thor-admit.md §12 — the two
           measured windows (21:44–21:45 and 21:27–21:28 AKDT,
           2026-10-31). Every fixture number below is that table's.
  [README] SI-Thor/builder-ide tools/thor-admit/README.md — the idle
           causes and what each one means.

Run:
  python3 tools/daemon_ledger.py PATH            # report one ledger file
  python3 tools/daemon_ledger.py PATH --json     # machine-readable
  python3 tools/daemon_ledger.py PATH --hour 2026-10-31T21
  python3 tools/daemon_ledger.py --check         # verify, exit 1 on drift
"""

from __future__ import annotations

import json
import sys
from dataclasses import dataclass, field

# --------------------------------------------------------------------------
# [SCHEMA] the fields the daemon writes. Unknown fields (seq, chain from
# the chained build; anything added later) are ignored, never fatal.
# --------------------------------------------------------------------------

TIERS = ("live", "preparation", "practice", "study")

# [README] the idle causes and what each means, in the file's own words.
IDLE_CAUSES = {
    "no_work_registered": "nothing was registered to do, and nothing was asked for",
    "waiting_for_memory": "work was ready, but the box had no room for it",
    "waiting_for_gpu": "work was ready, but the GPU was busy with higher-priority work",
    "preempted_for_live_work": "work was stopped so live work could run",
    "unattributed": "the ledger cannot say — the number that should be zero",
}


@dataclass
class Hour:
    """One bucket, kept as the file said it (numbers, not interpretations)."""

    hour: str
    started_at: str
    window_s: float
    samples: int
    # tier -> (running_s, runs, preemptions, preempted_s)
    tiers: dict[str, tuple[float, int, int, float]] = field(default_factory=dict)
    idle_total_s: float = 0.0
    idle_by_reason: dict[str, float] = field(default_factory=dict)
    unattributed_s: float = 0.0
    no_work_s: float = 0.0
    gpu_power_mw_mean: float | None = None
    gpu_power_mw_max: int | None = None
    gpu_power_samples: int = 0
    gpu_busy_pct_mean: float | None = None
    gpu_busy_samples: int = 0
    gpu_source: str | None = None
    cpu_cores: int = 0
    cpu_busy_share_mean: float | None = None
    mem_available_min: int | None = None
    mem_available_max: int | None = None
    chained: bool = False


def parse(path: str) -> tuple[list[Hour], list[str]]:
    """Read the buckets. Lines that are not JSON objects are named, not
    silently dropped: a ledger a post-mortem depends on is a file whose
    every line is accounted for."""
    hours: list[Hour] = []
    bad: list[str] = []
    with open(path, encoding="utf-8") as handle:
        for number, line in enumerate(handle, start=1):
            text = line.strip()
            if not text:
                continue
            try:
                bucket = json.loads(text)
            except json.JSONDecodeError as error:
                bad.append(f"line {number}: not JSON ({error})")
                continue
            if not isinstance(bucket, dict) or "hour" not in bucket:
                bad.append(f"line {number}: not a bucket (no 'hour')")
                continue
            tiers: dict[str, tuple[float, int, int, float]] = {}
            for tier, stat in (bucket.get("tiers") or {}).items():
                tiers[tier] = (
                    float(stat.get("running_s", 0.0)),
                    int(stat.get("runs", 0)),
                    int(stat.get("preemptions", 0)),
                    float(stat.get("preempted_s", 0.0)),
                )
            idle = bucket.get("idle") or {}
            gpu = bucket.get("gpu") or {}
            cpu = bucket.get("cpu") or {}
            mem = bucket.get("memory") or {}
            hours.append(Hour(
                hour=str(bucket["hour"]),
                started_at=str(bucket.get("started_at", "")),
                window_s=float(bucket.get("window_s", 0.0)),
                samples=int(bucket.get("samples", 0)),
                tiers=tiers,
                idle_total_s=float(idle.get("total_s", 0.0)),
                idle_by_reason={
                    str(k): float(v)
                    for k, v in (idle.get("by_reason") or {}).items()
                },
                unattributed_s=float(idle.get("unattributed_s", 0.0)),
                no_work_s=float(idle.get("no_work_s", 0.0)),
                gpu_power_mw_mean=(
                    float(gpu["power_mw_mean"]) if gpu.get("power_mw_mean") is not None else None
                ),
                gpu_power_mw_max=(
                    int(gpu["power_mw_max"]) if gpu.get("power_mw_max") is not None else None
                ),
                gpu_power_samples=int(gpu.get("power_samples", 0)),
                gpu_busy_pct_mean=(
                    float(gpu["busy_pct_mean"]) if gpu.get("busy_pct_mean") is not None else None
                ),
                gpu_busy_samples=int(gpu.get("busy_samples", 0)),
                gpu_source=gpu.get("source"),
                cpu_cores=int(cpu.get("cores", 0)),
                cpu_busy_share_mean=(
                    float(cpu["busy_share_mean"]) if cpu.get("busy_share_mean") is not None else None
                ),
                mem_available_min=(
                    int(mem["available_min"]) if mem.get("available_min") is not None else None
                ),
                mem_available_max=(
                    int(mem["available_max"]) if mem.get("available_max") is not None else None
                ),
                chained="seq" in bucket and "chain" in bucket,
            ))
    return hours, bad


def _gib(bytes_: int | None) -> str:
    return "-" if bytes_ is None else f"{bytes_ / (1024 ** 3):.1f} GiB"


def report(hours: list[Hour], bad: list[str]) -> None:
    """The human report: one block per hour, then the totals the §7.7
    comparison actually reads (idle by cause, the two failures apart)."""
    width = 74
    for hour in hours:
        print("=" * width)
        print(f"{hour.hour}  ({hour.window_s:.1f} s, {hour.samples} samples,"
              f" {'chained' if hour.chained else 'unchained'})")
        print("=" * width)
        for tier in TIERS:
            if tier not in hour.tiers:
                continue
            running, runs, preemptions, preempted = hour.tiers[tier]
            note = ""
            if preempted:
                note = f", {preempted:.2f} s stopped by pre-emption"
            if preemptions:
                note += f" ({preemptions}x)"
            if running or preempted:
                print(f"  {tier:<12} {running:7.2f} s running, {runs} run(s){note}")
        print(f"  idle         {hour.idle_total_s:7.2f} s")
        for cause in ("no_work_registered", "waiting_for_memory",
                      "waiting_for_gpu", "preempted_for_live_work"):
            if cause in hour.idle_by_reason and hour.idle_by_reason[cause]:
                print(f"    {cause:<28} {hour.idle_by_reason[cause]:7.2f} s"
                      f"  — {IDLE_CAUSES[cause]}")
        flag = "  <-- the number that should be zero" if hour.unattributed_s else ""
        print(f"  unattributed {hour.unattributed_s:7.2f} s{flag}")
        if hour.gpu_power_mw_mean is not None:
            top = f", max {hour.gpu_power_mw_max} mW" if hour.gpu_power_mw_max else ""
            src = f" [{hour.gpu_source}]" if hour.gpu_source else ""
            print(f"  GPU          {hour.gpu_power_mw_mean:,.1f} mW mean{top},"
                  f" {hour.gpu_power_samples} samples{src}")
        if hour.gpu_busy_pct_mean is not None:
            print(f"               busy {hour.gpu_busy_pct_mean:.1f} %"
                  f" ({hour.gpu_busy_samples} samples)")
        if hour.cpu_busy_share_mean is not None:
            print(f"  CPU          {hour.cpu_busy_share_mean:.3f} of the box,"
                  f" {hour.cpu_cores} cores")
        if hour.mem_available_min is not None:
            print(f"  free memory  {_gib(hour.mem_available_min)} lowest,"
                  f" {_gib(hour.mem_available_max)} highest")
        print()

    if not hours:
        print("(no buckets in the file)")
        return

    total = sum(h.window_s for h in hours)
    print("-" * width)
    print(f"totals over {len(hours)} hour(s), {total:,.1f} s")
    print("-" * width)
    for tier in TIERS:
        running = sum(h.tiers[tier][0] for h in hours if tier in h.tiers)
        preempted = sum(h.tiers[tier][3] for h in hours if tier in h.tiers)
        if running or preempted:
            print(f"  {tier:<12} {running:9.2f} s running"
                  f" ({running / total:5.1%} of the window)"
                  f"{f', {preempted:.1f} s pre-empted' if preempted else ''}")
    idle = sum(h.idle_total_s for h in hours)
    print(f"  {'idle':<12} {idle:9.2f} s ({idle / total:5.1%} of the window)")
    # The two failures §7.7 says to tell apart, side by side.
    no_work = sum(h.idle_by_reason.get("no_work_registered", 0.0) for h in hours)
    waiting_mem = sum(h.idle_by_reason.get("waiting_for_memory", 0.0) for h in hours)
    waiting_gpu = sum(h.idle_by_reason.get("waiting_for_gpu", 0.0) for h in hours)
    unattributed = sum(h.unattributed_s for h in hours)
    print()
    print("  the morning-after lines:")
    print(f"    room and nobody asked    {no_work:9.2f} s  (no_work_registered)")
    print(f"    work ready, no room      {waiting_mem:9.2f} s  (waiting_for_memory)")
    print(f"    work ready, GPU busy     {waiting_gpu:9.2f} s  (waiting_for_gpu)")
    print(f"    the ledger cannot say    {unattributed:9.2f} s  (unattributed"
          f"{'  <-- should be zero' if unattributed else ''})")
    if bad:
        print()
        for line in bad:
            print(f"  !! {line}")


# --------------------------------------------------------------------------
# [EXP] §12 — the two measured windows, as fixtures. The numbers are the
# lab record's own table (rounded there to two decimals); the shape is
# [SCHEMA]'s Bucket. Fields the record does not publish (runs for the
# live tier in the first window, memory sums) are zero/absent — the
# fixture carries what was measured, and --check asserts exactly that.
# --------------------------------------------------------------------------

def _window_a() -> dict:
    """21:44–21:45 AKDT: the seat's work on the GPU; the presence job
    pre-empted for 44.25 s. [EXP] §12, first table."""
    return {
        "hour": "2026-10-31T21", "started_at": "2026-10-31T21:44:00Z",
        "window_s": 51.0, "samples": 204,
        "tiers": {
            "live": {"running_s": 44.5, "runs": 0, "preemptions": 0, "preempted_s": 0.0},
            "study": {"running_s": 1.0, "runs": 0, "preemptions": 1, "preempted_s": 44.25},
        },
        "gpu": {"power_mw_mean": 39388.0, "power_mw_max": 47548,
                "power_samples": 204, "busy_pct_mean": 90.0, "busy_samples": 204,
                "source": "tegrastats VDD_GPU / nvidia-smi"},
        "cpu": {"cores": 14, "busy_share_mean": 0.993, "per_core_busy_share": [],
                "samples": 204},
        "idle": {"total_s": 5.75, "by_reason": {"no_work_registered": 5.75},
                 "unattributed_s": 0.0, "no_work_s": 5.75},
        "memory": {"available_min": int(47.7 * 1024 ** 3),
                   "available_max": int(48.4 * 1024 ** 3),
                   "available_sum": 0, "samples": 204},
        "samples": 204,
    }


def _window_b() -> dict:
    """21:27–21:28 AKDT: the GPU idle, the CPU busy; the presence job
    ran the whole window. [EXP] §12, second table."""
    return {
        "hour": "2026-10-31T21", "started_at": "2026-10-31T21:27:00Z",
        "window_s": 51.5, "samples": 206,
        "tiers": {
            "live": {"running_s": 44.75, "runs": 0, "preemptions": 0, "preempted_s": 0.0},
            "study": {"running_s": 45.5, "runs": 1, "preemptions": 0, "preempted_s": 0.0},
        },
        "gpu": {"power_mw_mean": 3201.6, "power_mw_max": 5116,
                "power_samples": 206, "busy_pct_mean": 0.0, "busy_samples": 206,
                "source": "tegrastats VDD_GPU / nvidia-smi"},
        "cpu": {"cores": 14, "busy_share_mean": 0.667, "per_core_busy_share": [],
                "samples": 206},
        "idle": {"total_s": 6.0, "by_reason": {"no_work_registered": 6.0},
                 "unattributed_s": 0.0, "no_work_s": 6.0},
        "memory": {"available_min": None, "available_max": None,
                   "available_sum": 0, "samples": 206},
        "samples": 206,
    }


def check() -> bool:
    """Re-verify the reader against every number [EXP] §12 published."""
    ok = True

    def want(what: str, got, expected) -> None:
        nonlocal ok
        if got != expected:
            print(f"  FAIL {what}: read {got!r}, expected {expected!r}")
            ok = False

    import tempfile
    from pathlib import Path

    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "ledger.jsonl"
        # A real file's shape: an unchained line (the daemon on main
        # today), then a chained one (the folded-in build), then the two
        # recorded windows — the reader must take all four kinds in one
        # file without complaint.
        chained = _window_a()
        chained["seq"] = 1
        chained["chain"] = "0" * 16
        path.write_text("\n".join([
            json.dumps(_window_b()),
            json.dumps(chained),
        ]) + "\n", encoding="utf-8")
        hours, bad = parse(str(path))
        want("bad lines", bad, [])
        want("buckets read", len(hours), 2)
        b, a = hours  # file order: window B, then (chained) window A

        want("B window_s", b.window_s, 51.5)
        want("B samples", b.samples, 206)
        want("B live running_s", b.tiers["live"][0], 44.75)
        want("B study running_s", b.tiers["study"][0], 45.5)
        want("B study runs", b.tiers["study"][1], 1)
        want("B idle total_s", b.idle_total_s, 6.0)
        want("B no_work_registered", b.idle_by_reason.get("no_work_registered"), 6.0)
        want("B unattributed_s", b.unattributed_s, 0.0)
        want("B gpu power mean mW", b.gpu_power_mw_mean, 3201.6)
        want("B gpu power max mW", b.gpu_power_mw_max, 5116)
        want("B gpu busy pct", b.gpu_busy_pct_mean, 0.0)
        want("B cpu busy share", b.cpu_busy_share_mean, 0.667)
        want("B cpu cores", b.cpu_cores, 14)

        want("A chained", a.chained, True)
        want("B unchained", b.chained, False)
        want("A window_s", a.window_s, 51.0)
        want("A live running_s", a.tiers["live"][0], 44.5)
        want("A study running_s", a.tiers["study"][0], 1.0)
        want("A study preempted_s", a.tiers["study"][3], 44.25)
        want("A study preemptions", a.tiers["study"][2], 1)
        want("A idle total_s", a.idle_total_s, 5.75)
        want("A unattributed_s", a.unattributed_s, 0.0)
        want("A gpu power mean mW", a.gpu_power_mw_mean, 39388.0)
        want("A gpu power max mW", a.gpu_power_mw_max, 47548)
        want("A gpu busy pct", a.gpu_busy_pct_mean, 90.0)
        want("A cpu busy share", a.cpu_busy_share_mean, 0.993)
        want("A mem min GiB", round((a.mem_available_min or 0) / 1024 ** 3, 1), 47.7)
        want("A mem max GiB", round((a.mem_available_max or 0) / 1024 ** 3, 1), 48.4)

        # The invariant the idle law implies: the idle seconds and the
        # busiest tier's running seconds cannot exceed the window (tier
        # seconds may overlap each other; they cannot overlap idle).
        for hour in hours:
            busiest = max((t[0] for t in hour.tiers.values()), default=0.0)
            if hour.idle_total_s + busiest > hour.window_s + 0.5:
                print(f"  FAIL {hour.hour}: idle {hour.idle_total_s} + busiest"
                      f" {busiest} > window {hour.window_s}")
                ok = False

    print("  the two recorded windows (exp-thor-admit §12): every published"
          " number reproduced" if ok else "  DRIFT — see FAILs above")
    return ok


def main(argv: list[str]) -> int:
    if "--check" in argv:
        print("daemon_ledger: verifying against the lab record")
        return 0 if check() else 1
    paths = [a for a in argv if not a.startswith("--")]
    if len(paths) != 1:
        print(__doc__)
        return 2
    as_json = "--json" in argv
    hour_filter = None
    if "--hour" in argv:
        hour_filter = argv[argv.index("--hour") + 1]
    hours, bad = parse(paths[0])
    if hour_filter:
        hours = [h for h in hours if h.hour == hour_filter]
    if as_json:
        out = []
        for h in hours:
            out.append({
                "hour": h.hour, "window_s": h.window_s, "samples": h.samples,
                "chained": h.chained,
                "tiers": {t: {"running_s": v[0], "runs": v[1],
                              "preemptions": v[2], "preempted_s": v[3]}
                          for t, v in h.tiers.items()},
                "idle": {"total_s": h.idle_total_s,
                         "by_reason": h.idle_by_reason,
                         "unattributed_s": h.unattributed_s},
                "gpu": {"power_mw_mean": h.gpu_power_mw_mean,
                        "busy_pct_mean": h.gpu_busy_pct_mean},
                "bad_lines": bad,
            })
        print(json.dumps(out, indent=2))
        return 0 if not bad else 1
    report(hours, bad)
    return 0 if not bad else 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
