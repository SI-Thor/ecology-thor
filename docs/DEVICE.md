# DEVICE.md — running this repo's device steps on the AGX Thor

*The one-entry-point contract: a stranger with ssh access to the box runs
one command, gets unambiguous PASS/FAIL lines, and hands back one report
bundle. This page documents exactly what that command does, what its
output looks like when everything works, and the box rules it obeys.*

## The one command

```bash
cd /path/to/ecology-thor
tools/thor-device.sh                      # ~15–35 min, bench included
tools/thor-device.sh --skip-bench         # ~8–12 min, parity + ptx only
THOR_DEVICE_WAIT_SECS=1800 tools/thor-device.sh   # wait for the box's lock
```

Prerequisites: JetPack 7 / CUDA 13 toolchain (`nvcc` on PATH), Rust
stable (rustup is fine), ~2 GB free disk for `target/`. No sudo needed —
the script is user-level; `tools/thor-tune.sh` (clock pinning) is
optional and separate, on purpose: it changes the box's state and is the
operator's call, not a stranger's.

## What "good" looks like

```
[1/6] guard       PASS   (fmt+clippy clean, 36 passed (logs in data/device-report/))
[2/6] kernels     PASS   (build/ecology.ptx (84K, target compute_110))
        mem-available 96.4 GiB (floor 25.0 GiB, kept free)
        holding ~/lm-wt/heavy.lock for steps 2-5
[3/6] parity      PASS   (device parity: OK (8192 entities, all columns bit-exact) — THE GATE (sm_110, bit for bit))
[4/6] ptx         PASS   (ptx-check OK, 0 spills (register counts in ptx-check.log))
[5/6] bench       PASS   (4 reports under data/ (stress 10k/50k/100k + world-gen 50k))
[6/6] report      PASS   (data/device-report/REPORT.md — paste its tables into BENCHMARKS.md + PLACEMENT.md)
```

Any FAIL line names its log under `data/device-report/` — attach that log
when reporting back. Exit code 0 means every step passed; 77 means the
box's heavy lock was held by another seat (nothing ran past step 1);
anything else is a real failure.

## The box rules the script obeys (and why)

The Thor OOM-crashed at 13:22 on 2026-10-31 from co-scheduled heavy jobs
(builder-ide's thor-admit design §0 documents the incident). The seats'
stopgap — and still the rule while thor-admit is not yet the box's
admission controller — is:

1. **Heavy steps run while the script holds `flock ~/lm-wt/heavy.lock`.**
   Steps 2–5 (kernels, parity, ptx, bench) never run outside the lock;
   the light steps (preflight, guard) take nothing. Default behavior when
   the lock is held: skip with a clear line and exit 77 — this script
   steps aside rather than fight another seat for the box.
2. **≥ 25 GiB stays free.** Checked before the lock is taken and re-checked
   before each heavy step. A step that would start below the floor FAILS
   with the current `MemAvailable` instead of running — the floor is what
   the box keeps for everything that is not a lease (thor-admit §2.1's
   default, same number).
3. **Usable memory is 122 GiB, not 128.** The module is 128 GB;
   `MemTotal` is ≈ 125.7 GiB; the seats' working figure — the one the
   25 GiB floor and every memory threshold in this repo is stated
   against — is 122 GiB. (THOR.md carries the sources.)

`THOR_DEVICE_SKIP_HEAVY_LOCK=1` exists, does what it says, and is
documented as "only for a small, bounded demonstration on an idle box you
own" — the same wording and the same semantics as thor-admit's own test
gate. Do not use it to jump another seat's queue.

## What each step is

| # | step | what it proves | logs |
|---|---|---|---|
| 0 | preflight | the box is what we think it is: model, kernel, `MemTotal`/`MemAvailable`, nvcc/cargo versions, power mode | `box-state.txt` |
| 1 | guard | the tree is sound before any device work: fmt + clippy `-D warnings` + the full workspace test suite. **On-device, this is also the aarch64 leg of the golden determinism contract** — the journal chain hash must equal the committed `37a37a2c4ec5a9a9` (tests/golden.rs) | `guard-*.log` |
| 2 | kernels | `nvcc -ptx -arch=compute_110 -fmad=false` builds `crates/ecology-cuda/kernels/build/ecology.ptx`. The `.target` line in the PTX must say `sm_110`/`compute_110` — building for any other arch is a hard FAIL | `kernels.log` |
| 3 | parity | **THE GATE** (`make test-device`): the Blackwell kernels vs the CPU reference, every f32 column `.to_bits()`-equal, 8192 entities, every mechanism driven (kills, cascades, revives, auctions). This is the precondition for the v0.2 device-authoritative sweeps — if it fails, the device path is *not adopted*, per docs/PTX_NOTES.md | `parity.log` |
| 4 | ptx | `ptxas -v` assembles both cubins for sm_110; any spill line is a FAIL (a spill is a defect in this codebase — the hand-PTX budget is 10 registers, 0 spills) | `ptx-check.log` |
| 5 | bench | the population ladder: stress 10k/50k/100k + world-gen 50k, release build, reports under `data/` | `bench.log`, `data/*.json` |
| 6 | report | `tools/render_device_report.py` assembles `data/device-report/REPORT.md`: the paste-ready §AGX Thor table, the PLACEMENT crossover section, parity/PTX evidence, and the box state | `REPORT.md` |

## What the seat does with the report

The report is built to be pasted, not interpreted:

1. **`REPORT.md` → `docs/BENCHMARKS.md` §AGX Thor** — the rendered table
   rows, plus a context line (power mode, clocks pinned or not, thermal
   state). The chain-hash column doubles as the cross-platform
   determinism receipt: same scenario, same hash, aarch64 == x86_64.
2. **`REPORT.md` → `docs/PLACEMENT.md`** — the CPU sweep ladder measured
   on the box. The device-sweep column stays NOT YET MEASURED until the
   `--device-bench` mode exists (it is named in the report so it cannot
   be forgotten); placement thresholds become measured crossovers only
   then.
3. **`box-state.txt` + `steps.jsonl`** — the raw evidence bundle, attached
   as-is to the report-back.

## Known non-goals

- The script does not edit the docs — a stranger's script editing
  version-controlled docs is how numbers lose their context. It renders
  paste-ready tables; the seat pastes and annotates.
- It does not run `tools/thor-tune.sh` (clock pinning) — that changes
  box-wide state. If the operator pins clocks first, the box state in
  the report says so, and the numbers carry that context.
- It does not run `compute-sanitizer` (BENCHMARKS.md's third first-pass
  item) — sanitizer runs want an isolated box and a time budget; that is
  an operator step, documented in SAFETY.md, not a stranger's one-shot.
