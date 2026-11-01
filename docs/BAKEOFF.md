# BAKEOFF.md — one harness, two daemons, the same three tests

*2026-11-01. The builder-ide seat's question: two admission daemons are
in flight — **thor-admit** (builder-ide `tools/thor-admit`, the
`thor-admit` branch: kernel-held leases, the 250 ms watchdog, learned
peaks feeding the cost model's memory column) and **ecology-admit**
(this repo, `tools/ecology-admit`: the same lease semantics plus the
WHY ledger — every decision a chained, replayable transition). Rather
than two, bake off: the same test (three 30 GiB hogs serialised
safely; a runaway stopped before MemAvailable hits 2 GiB; daemon
overhead). The better one wins, and the other's best idea gets folded
in. This document is the harness, the protocol, the sandbox evidence,
and the answer to the question that came with it: **is the ecology's
ledger something thor-admit should adopt?***

## The one command

```bash
# on the Thor (the real rules: 30 GiB hogs, the 25 GiB floor, the heavy lock)
tools/bakeoff.sh

# with thor-admit built from a worktree:
THOR_ADMIT_SRC=~/lm-wt/builder-ide/tools/thor-admit tools/bakeoff.sh

# a subset, or a smaller hog on a busy box:
TESTS="watchdog overhead" ECO_HOG_GB=12 tools/bakeoff.sh

# a small demonstration box (CI sandbox); prints its warning into the results
BAKEOFF_SMALL_BOX=1 ECO_HOG_GB=1 tools/bakeoff.sh
```

What it does, in order: builds both daemons (release), takes the box's
heavy lock (`flock ~/lm-wt/heavy.lock`; steps aside with exit 77 when
another seat holds it, or waits with `BAKEOFF_WAIT_SECS=<s>`), checks
≥ 25 GiB free before every heavy test, then runs — for **each** daemon,
with identical assertions and identical log parsing:

| # | test | what it proves |
|---|---|---|
| 1/4 | **serialize** | three hogs, one at a time: at most 1 concurrent lease, 3 grants, every hog finishes, the OOM counter never moves. The floor is computed from the box's own headroom so exactly one hog fits (thor-admit's own integration convention, `floor = available − 1.5×hog`). |
| 2/4 | **watchdog** | a runaway (hold 60 s) is SIGSTOPped while > 2 GiB is still free, dies after the grace, the kernel's OOM killer never fires. The emergency floor scales to the box (the Thor rule: 8 GiB of 122). |
| 3/4 | **overhead** | the daemon's own cost while idle: resident < 64 MiB, CPU < 5% of one core over 10 s. |
| 4/4 | **stall law** (ecology-admit only — the differentiator) | with memory perfectly fine, a lease that stops attesting progress for 5 s is harvested to UNKNOWN and killed after grace; a healthy lease (attesting every second) runs untouched; the journal chain verifies. |

Every step prints one line — `[1/4] serialize thor-admit PASS (max
concurrent 1, …)` — and the whole run lands in
`data/bakeoff/RESULTS.md`, per-test logs preserved beside it
(`ta-serialize/`, `eco-watchdog/`, …) so a failure's evidence survives
the next test.

## The sandbox evidence (what is already proven)

The harness ran end-to-end on this repo's 3.5 GiB CI box at 1 GiB hog
scale (`BAKEOFF_SMALL_BOX=1`, the demonstration mode whose warning is
printed into the results) — **all seven lines PASS**, and the shared
three measured identically on both daemons:

```
[1/4] serialize    thor-admit    PASS  max concurrent 1, 3 grants, box never below 2.4 GiB, OOM kills 1
[2/4] watchdog     thor-admit    PASS  stopped with 2.4 GiB free (>2), killed after grace, OOM killer never fired
[3/4] overhead     thor-admit    PASS  2 MiB resident, 0.10% of one core
[1/4] serialize    ecology-admit PASS  max concurrent 1, 3 grants, box never below 2.4 GiB, OOM kills 1
[2/4] watchdog     ecology-admit PASS  stopped with 2.4 GiB free (>2), killed after grace, OOM killer never fired
[3/4] overhead     ecology-admit PASS  2 MiB resident, 0.10% of one core
[4/4] stall-law    ecology-admit PASS  the silent lease harvested to UNKNOWN + killed at 5s stall;
                                       the healthy lease untouched; journal chain intact
```

Two things that evidence already says:

1. **The lease semantics match.** ecology-admit reproduces thor-admit's
   behavior on the shared tests to the digit — that was the point of
   adopting its protocol, arithmetic, lock discipline and log
   vocabulary verbatim. Whichever daemon wins, nothing about the
   *memory* behavior changes for its callers.
2. **The difference is not memory behavior; it is what the daemon
   knows.** The stall-law line is the only line thor-admit cannot
   produce at any setting, because it is not about memory.

What the Thor run adds: the real hog scale (30 GiB, the number in the
brief), the real floor arithmetic (25 GiB of 122 GiB), the real
`~/lm-wt/heavy.lock` contention, and the numbers for
`docs/design/thor-admit.md` §7's table to sit beside.

## The answer to the question: is the ledger something thor-admit should adopt?

Having now built both sides, the honest answer splits into three parts,
one of which is "no":

**1. Yes — the chained decision journal, and it is cheap.** thor-admit
writes an excellent flat JSON event log; the fold-in is to chain it:
each line carries `seq` and a running FNV-1a hash folded over the
line's bytes (the mixer is already exported as
`ecology_core::journal::fnv1a64`), and a `--verify` flag re-walks the
file. That is ~50 lines of Rust on their side, zero protocol change,
zero log-format change (the chain fields ride along), and it converts
every post-mortem from "correlate daemon.log with dmesg and hope" into
"run verify, then read one narrative". The sandbox bake-off's own
debugging made the case: when a test failed, the first question was
*"is this log even the whole log?"* — the chain answers that question
before it is asked.

**2. Yes — the stall semantics, as a client convention before a daemon
feature.** The silent hang (a job that holds its lease and stops
progressing) is a class the memory watchdog cannot see, and the
infinigen side has measured it as *the* dominant overnight failure. The
fold-in: thor-admit's protocol gains one op (`{"op":"progress",
"lease":N}`), its `run` client gains `--progress-every`, and a lease
silent past a `--stall-horizon` gets its existing stop→grace→kill
treatment, journaled as `stall.harvested`. The memory axis stays
exactly theirs.

**3. No — the four-state WHY machinery should not go inside the memory
daemon.** A lease is not a purpose. The WHY ledger's states earn their
keep where history matters across days (purposes, recipes, tiles,
regions); a lease lives minutes, and its lifecycle is fully described
by two journal words: *stopped* (UNKNOWN — questionable, not gone) and
*killed* (dead, reason recorded). ecology-admit keeps the full ledger
on the ecology's side of the seam — where the infinigen bridges already
map jobs to purposes — and the daemon needs only the vocabulary. This
is the same line the builder-ide fit study drew for `core/why`, one
layer down: borrow the semantics, don't embed the engine.

**And one thing that should go the other way.** thor-admit's measured
subtleties — the fork-inherits-fd race in the lock probe (§3 of its
design), the rusage-vs-samples peak rule, the tegrastats-vs-meminfo
choice — are the kind of box knowledge the ecology's cost model wants;
`peaks.jsonl` is already schema-compatible so the memory column reads
either daemon. The bake-off's winner is the box; the ledger is how the
box remembers why.

## The protocol (for the seat running it)

Both daemons speak JSON lines over the socket; the harness drives them
through their CLIs (`run --label L --peak P [--priority N] -- CMD`),
which are flag-compatible. The one flag thor-admit does not have is
`--progress-every` (the stall attestations) — the harness only passes
it in test 4.

| knob | default | notes |
|---|---|---|
| `--floor` | 25 GiB | the reservation floor (both daemons) |
| `--emergency-floor` | 8 GiB | below this the watchdog acts |
| `--sample-interval-ms` | 250 | the box's pulse |
| `--stop-grace-samples` | 8 | 2 s of stopped before the kill |
| `--min-victim-rss` | 2 GiB | a lease too small to be the cause is never the victim |
| `--stall-horizon-s` | off | ecology-admit: the silent-hang harvest |

Results table columns, for whoever writes the decision: max concurrent
(test 1), free memory at the stop (test 2), resident + CPU (test 3),
and — the tie-breaker if the shared three tie, which the sandbox says
they will — whether the daemon can tell a healthy lease from a silent
one (test 4), and whether its log can prove itself (journal
`--verify`).
