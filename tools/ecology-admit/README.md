# ecology-admit — the ecology's memory-admission daemon

One binary, six verbs:

```bash
ecology-admit daemon [--socket P] [--state-dir D] [--floor 25G] \
    [--emergency-floor 8G] [--sample-interval-ms 250] \
    [--stop-grace-samples 8] [--min-victim-rss 2G] \
    [--stall-horizon-s S] [--idle-exit-s S]
ecology-admit run --label trellis --peak auto [--priority N] \
    [--progress-every S] -- CMD...      # lease → run → release
ecology-admit status [--socket P]       # leases, queue, purposes, chain hash
ecology-admit stop   [--socket P]
ecology-admit journal [--state-dir D] [--verify]   # the forensic artifact
ecology-admit hog --bytes 30G [--hold 5]           # the test hog
```

## What it is, honestly

The lease semantics are **thor-admit's** (builder-ide, `tools/thor-admit`),
deliberately: the same reservation arithmetic (`projected = available −
Σ(reserved − current); admit ⟺ projected − peak ≥ floor`), the same
kernel-held lease locks (a dead client cannot leak a lease), the same
watchdog discipline (SIGSTOP the largest lease below the emergency
floor, grace, SIGTERM+SIGCONT), the same learned peaks
(`peaks.jsonl`, the schema `core/night/cost`'s memory column reads),
and the same wire protocol (JSON lines over a unix socket: acquire /
held / release / status / ping). The bake-off
([`docs/BAKEOFF.md`](../../docs/BAKEOFF.md)) runs the **same three
tests** against both daemons through one harness; whichever wins, the
other's best idea gets folded in.

## What it adds — the ecology's three

1. **The chained decision journal.** Every decision is one line in
   `journal.jsonl`, chained with FNV-1a (the same mixer
   `ecology-core`'s necrology uses, exported as `fnv1a64`):
   `{"seq", "chain", "event"}`. `ecology-admit journal --verify`
   re-walks the chain and names the line where it breaks — a post-mortem
   can *prove* the log it read is the log the daemon wrote. The daemon
   also mirrors each line to stdout, so a harness (or `tail -f`) reads
   both daemons the same way.
2. **A lease is a purpose.** Each lease is a WHY node in the embedded
   ledger (`ecology-core::why`): granted is service (`run_under`), a
   watchdog stop is a **harvest to UNKNOWN — questionable, not gone**,
   the grace expiry is dead with a reason, and a re-request of the same
   label revives its purpose on parole. The morning question "what
   happened to the trellis run at 3 a.m. and why" is answered by
   state + chain, not by log correlation.
3. **The stall law.** A lease may attest progress
   (`{"op":"progress"}`; the `run` client sends it every
   `--progress-every` seconds). With `--stall-horizon-s S` set, a lease
   that stops attesting for longer than S gets the harvest treatment —
   SIGSTOP, journaled `stall.harvested`, grace, kill — **with memory
   perfectly fine**. A stall is not a memory problem: a job can hold
   its lease, make no progress for an hour, and never trip a
   byte-based watchdog. This is the silent-hang class the memory
   watchdog structurally cannot see.

## The tests

`cargo test -p ecology-admit` — sizes, meminfo, percentile/history
round-trips, the chain (verifies, resumes across restarts, breaks on
tampering), the lease-as-purpose lifecycle (stop → UNKNOWN → revive on
parole). All stdlib, no GPU, no sockets.

The live behavior (serialization, watchdog, overhead, stall) is
exercised by the bake-off harness — `tools/bakeoff.sh` — which drives
this daemon and thor-admit through the identical tests. Verified
end-to-end at 1 GiB scale on a 3.5 GiB CI box (all seven lines PASS,
identical numbers for the shared three); the Thor run uses the real
rules (30 GiB hogs, the 25 GiB floor, the heavy lock).

## Box rules

Heavy work runs under `flock ~/lm-wt/heavy.lock` with ≥ 25 GiB kept
free; the box's usable memory is 122 GiB (MemTotal ≈ 125.7 GiB). The
daemon's defaults follow thor-admit's (25 GiB floor, 8 GiB emergency,
250 ms samples, 8-sample grace, 2 GiB min victim) so the bake-off
compares like with like.
