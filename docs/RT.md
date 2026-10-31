# RT.md — the real-time envelope

*The kernel is real-time shaped, not real-time certified. This page is the
contract we hold ourselves to, and the box configuration that makes it
hold.*

## 1. The frame contract

```text
tick = 10 ms (default; 100 Hz)              ── the minor frame
├─ every tick      : necrology drain, cascade budget, rails refill, publish
├─ every 10 ticks  : AUCTION  — chain aggregates + auction        (O(N))
└─ every 100 ticks : SWEEP    — reap, deadlines, decay, conservation (O(N))
```

Promises:

- **Every bucket is bounded.** No bucket contains unbounded work. The only
  structurally unbounded operation in the kernel — subtree cascade — is
  rate-limited (`cascade_budget` conversions per tick, remainder queued)
  so a kill can never blow a frame, no matter the tree.
- **No allocation in steady state.** All tables preallocated; the
  journal ring and scratch buffers reuse capacity. A GC pause is
  impossible in Rust, but so is a `malloc` hiccup — we don't call it.
- **Single writer.** All kernel mutation happens on the tick thread.
  Readers (telemetry, Python, future IDE overlay) see seqlock snapshots
  and never take locks the writer could feel.
- **No clock inside.** Time enters as the tick counter; the caller maps
  ticks to the wall. This is what makes the journal replayable and the
  golden test possible.

## 2. Overruns and safe mode

The caller times each tick and reports via `note_frame_time(micros)`:

```text
frame > budget (default 8 ms of the 10 ms):
    overrun_streak += 1, journal an Overrun event
streak >= overrun_tolerance (default 5):
    SAFE MODE — auctions suspended (decay-only), summary flags it
clean ticks >= safe_mode_cooloff (default 100):
    recovery, journaled
```

Safe mode is **decay-only**: the world keeps aging honestly (freshness
slides, mass decays, stale purposes still lapse), but no new work is
admitted. This is the flight-software discipline — a broken frame never
gets to keep spending — mapped onto a world-creation scheduler. The
recovery is automatic because the *only* way out of safe mode is the
thing you wanted anyway: clean frames.

Design note: overruns are detected by measurement, not by a kernel-side
timer thread, because a second thread watching the first is a second
writer of timing decisions. The caller owns the stopwatch; the kernel
owns the policy.

## 3. Box configuration (the executable version: `tools/thor-tune.sh`)

Verified against NVIDIA's r38.4 power/performance docs and field reports:

1. **Power mode.** `sudo nvpmodel -m 0` (MAXN) for peak, but for
   *repeatable* numbers prefer mode 1 (120 W, default) — MAXN is
   unconstrained and the box *will* thermally throttle under sustained
   load (observed ~13% drift with DVFS on early firmware).
2. **Fixed clocks.** `sudo jetson_clocks --fan`. Removes DVFS ramp jitter
   — the single highest-leverage determinism knob. Make it a systemd
   unit; it does not persist across reboot.
3. **Core isolation** (kernel boot args, manual by design):
   `isolcpus=10,11,12,13 nohz_full=10,11,12,13 rcu_nocbs=10,11,12,13
   threadirqs`, then move IRQs off those cores (`/proc/irq/*/smp_affinity_list`)
   and pin the tick thread there (`taskset` / `sched_setaffinity`; the
   Rust side is a two-line `libc` call when we wire it).
4. **SCHED_FIFO** for the tick thread — with one caution from the field:
   RT-priority CUDA submission threads have caused kernel crashes on
   Orin-class boxes (forum 273906). Keep the tick thread RT, but submit
   device work from a normal-priority helper until that's re-verified on
   Thor's L4T.
5. **PREEMPT_RT**: NVIDIA ships rt-kernel packages for Thor
   (`nvidia-l4t-rt-kernel*`, 6.8.12-rt-tegra; production use is reported
   in the field). Prefer those over custom builds; disable UEFI runtime
   services for the last bit of latency.
6. **Memory**: `mlockall` the process (the arena is preallocated — pin
   it), and note DRAM ECC patrol scrubbing lives on the R52 island; it is
   not ours to schedule.

## 4. What "bounded" means, in numbers

From `docs/BENCHMARKS.md` (2-vCPU x86_64 dev sandbox, release, the *slow*
machine we could get — Thor's 14 V3AE cores will do better):

| Population | plain tick p50 | auction tick p50 | sweep tick p50 |
|---|---|---|---|
| 3.2k (realistic world-gen) | 28 µs | 130 µs | 170 µs |
| 10k (all bidding, all fresh) | 5.6 µs | 555 µs | 591 µs |
| 100k (stress) | 56 µs | 7.8 ms | 8.2 ms |

Read: at realistic world-gen populations the kernel costs **1.3% of its
frame budget** on a machine with 1/7th the cores. At pathological 100k
stress populations the auction bucket eats the budget — which is exactly
the case the device sweep exists for (PLACEMENT.md), and exactly why the
safe-mode policy exists meanwhile.

## 5. The watchdog story (why there is no watchdog thread)

The parent repo's law: `why_should_this_run()` is the heartbeat. Here,
the tick itself is the heartbeat: every tick publishes a seqlock summary;
a monitor (the Python edge, builder-ide's night runner, a shell loop) can
check `tick` is advancing and `safe_mode` is false without any kernel
cooperation. If the tick thread dies, the summary stops moving — that
*is* the alarm. External watchdogs then have a clean choice: restart the
process (the journal chain survives in memory only in v0.1; durable
replay is the v0.3 bridge) — the arena rebuilds from replay.

## 6. Latency budget of one auction tick (realistic population, CPU)

```text
necrology drain + cascade budget          ~1 µs
chain-aggregate sweep (3.2k entities)   ~40 µs
auction: score + select + allocate       ~80 µs
journal append (auction event)           ~1 µs
summary publish (seqlock)              ~0.1 µs
                                       ───────
                                       ~130 µs   of an 8000 µs budget
```

(The numbers are the p50s from BENCHMARKS.md, attributed; re-measure on
the box, in context, before citing.)
