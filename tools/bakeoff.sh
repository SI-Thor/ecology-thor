#!/usr/bin/env bash
# bakeoff.sh — one command, both daemons, the same three tests.
#
# The question (builder-ide seat, 2026-11-01): two admission daemons are
# in flight — thor-admit (builder-ide, tools/thor-admit) and ecology-admit
# (this repo, tools/ecology-admit). Rather than two, bake off: the same
# test (three 30 GiB hogs serialised safely; a runaway stopped before
# MemAvailable hits 2 GiB; daemon overhead). The better one wins, and the
# other's best idea gets folded in.
#
# This harness runs the SAME assertions against BOTH daemons, prints one
# PASS/FAIL line per test per daemon, and a comparison table at the end.
# It also runs the differentiator leg (the stall law) — ecology-admit
# only, clearly marked: it is the thing a memory-only daemon structurally
# cannot do, not a part of the shared test.
#
# BOX RULES (the box OOM-crashed on 2026-10-31):
#   * everything heavy runs under flock ~/lm-wt/heavy.lock (this script
#     takes it once and holds it for the whole run);
#   * >= 25 GiB must stay free — checked before each heavy test;
#   * usable memory is 122 GiB (MemTotal ~125.7 GiB).
#
# Usage:
#   tools/bakeoff.sh                     # the full bake-off (~10-15 min)
#   THOR_ADMIT_BIN=/path/to/thor-admit tools/bakeoff.sh
#   ECO_HOG_GB=12 tools/bakeoff.sh       # smaller hogs on a busy box
#   BAKEOFF_WAIT_SECS=1800 tools/bakeoff.sh   # wait for the heavy lock
#
# Expected output shape (every line matters to the report):
#   [1/4] serialize        thor-admit    PASS  (max concurrent 1, ...)
#   [1/4] serialize        ecology-admit PASS  (max concurrent 1, ...)
#   [2/4] watchdog         thor-admit    PASS  (stopped with X GiB free, ...)
#   ...
#   and the comparison table + data/bakeoff/RESULTS.md at the end.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$REPO_ROOT/data/bakeoff"
mkdir -p "$OUT"
RESULTS="$OUT/RESULTS.md"
: > "$RESULTS"

GIB=$((1024 * 1024 * 1024))
HOG_GB="${ECO_HOG_GB:-30}"
LOCK_PATH="${HOME}/lm-wt/heavy.lock"
WAIT_SECS="${BAKEOFF_WAIT_SECS:-0}"

# ------------------------------------------------------------ the daemons
# thor-admit: the incumbent (builder-ide). Found where the seats keep it,
# built from a worktree if THOR_ADMIT_SRC is given, or skipped with a
# clear line (never a silent pass).
THOR_ADMIT_BIN="${THOR_ADMIT_BIN:-}"
if [ -z "$THOR_ADMIT_BIN" ]; then
  for candidate in \
    "$HOME/lm-wt/builder-ide/tools/thor-admit/target/release/thor-admit" \
    "$HOME/src/builder-ide/tools/thor-admit/target/release/thor-admit" \
    "$(pwd)/../builder-ide/tools/thor-admit/target/release/thor-admit" \
    "$(pwd)/../thor-admit-wt/tools/thor-admit/target/release/thor-admit"; do
    if [ -x "$candidate" ]; then THOR_ADMIT_BIN="$candidate"; break; fi
  done
fi
if [ -z "$THOR_ADMIT_BIN" ] && [ -n "${THOR_ADMIT_SRC:-}" ]; then
  echo "building thor-admit from $THOR_ADMIT_SRC (release)..."
  (cd "$THOR_ADMIT_SRC" && cargo build -q --release) || {
    echo "FAIL: cannot build thor-admit from $THOR_ADMIT_SRC"; exit 1; }
  THOR_ADMIT_BIN="$THOR_ADMIT_SRC/target/release/thor-admit"
fi
ECO_BIN="$REPO_ROOT/target/release/ecology-admit"
build_eco() {
  (cd "$REPO_ROOT" && cargo build -q --release -p ecology-admit)
}
if ! build_eco; then
  echo "FAIL: cannot build ecology-admit"; exit 1
fi
# sanity: both binaries actually run in THIS environment (an executable
# that cannot run is a confusing failure mode; fail loudly instead)
if [ -n "$THOR_ADMIT_BIN" ]; then
  "$THOR_ADMIT_BIN" --help > /dev/null 2>&1 || {
    echo "FAIL: $THOR_ADMIT_BIN cannot execute in this environment"; exit 1; }
fi
"$ECO_BIN" --help > /dev/null 2>&1 || {
  echo "FAIL: $ECO_BIN cannot execute in this environment"; exit 1; }

pass()  { printf '[%s] %-12s %-12s %s\n' "$1" "$2" "$3" "PASS  $4" | tee -a "$RESULTS"; }
fail()  { printf '[%s] %-12s %-12s %s\n' "$1" "$2" "$3" "FAIL  $4" | tee -a "$RESULTS"; }
skip()  { printf '[%s] %-12s %-12s %s\n' "$1" "$2" "$3" "SKIP  $4" | tee -a "$RESULTS"; }

mem_available_bytes() {
  awk '/^MemAvailable:/ {printf "%d", $2 * 1024}' /proc/meminfo
}

# BAKEOFF_SMALL_BOX=1: for a small demonstration box only (a CI sandbox
# with a few GiB). The Thor run keeps the real rules — 25 GiB floor, the
# heavy lock, 30 GiB hogs — and this flag is printed in the results so a
# small-box run can never be mistaken for the real one.
SMALL_BOX="${BAKEOFF_SMALL_BOX:-0}"
FLOOR_BYTES=$((25 * GIB))
if [ "$SMALL_BOX" = "1" ]; then
  FLOOR_BYTES=$((500 * 1024 * 1024))
  echo "WARNING: BAKEOFF_SMALL_BOX=1 — a small demonstration box; the Thor"
  echo "         run keeps the real rules (25 GiB floor, heavy lock, 30 GiB hogs)."
fi
oom_kills() {
  awk '/^oom_kill / {print $2}' /proc/vmstat 2>/dev/null | tail -1
}
gib() { awk -v b="$1" 'BEGIN {printf "%.1f", b / 1073741824}'; }

require_floor() {
  local avail; avail="$(mem_available_bytes)"
  if [ "$avail" -lt "$FLOOR_BYTES" ]; then
    echo "REFUSING: MemAvailable $(gib "$avail") GiB < the $(gib "$FLOOR_BYTES") GiB floor"
    return 1
  fi
}

# ------------------------------------------------------------ the heavy lock
take_lock() {
  mkdir -p "$(dirname "$LOCK_PATH")"
  exec 9>"$LOCK_PATH"
  local waited=0
  while ! flock -n 9; do
    if [ "$WAIT_SECS" -le 0 ] || [ "$waited" -ge "$WAIT_SECS" ]; then
      echo "SKIPPED: another seat holds $LOCK_PATH. Re-run with BAKEOFF_WAIT_SECS=<s>."
      exit 77
    fi
    echo "waiting for the box's heavy lock (up to ${WAIT_SECS}s)..."
    sleep 5; waited=$((waited + 5))
  done
  echo "holding $LOCK_PATH for the bake-off"
}

# ------------------------------------------------------------ daemon driver
# Both daemons speak the same wire protocol; their CLIs differ only in
# the binary name. This function abstracts the difference:
#   start_daemon <name> <bin> <socket> <statedir> [extra args...]
#   leased_run  <bin> <socket> <label> <peak> <hold_s> [progress-every]
start_daemon() {
  local name="$1" bin="$2" socket="$3" statedir="$4"; shift 4
  # name is only for the log lines; bin is what runs
  rm -rf "$statedir"; mkdir -p "$statedir"
  # a stale socket (or a directory left by a crashed run) is not a daemon
  rm -rf "$socket" "$socket.pid" "$socket.pid".pid 2>/dev/null || true
  rm -f "$statedir/journal.jsonl"
  "$bin" daemon --socket "$socket" --state-dir "$statedir" --idle-exit-s 3600 "$@" \
    > "$statedir/stdout.log" 2>&1 &
  DAEMON_PID=$!
  for _ in $(seq 100); do
    [ -S "$socket" ] && break
    sleep 0.1
  done
  [ -S "$socket" ] || { echo "FAIL: $bin daemon did not start"; return 1; }
}

stop_daemon() {
  local bin="$1" socket="$2"
  "$bin" stop --socket "$socket" 2>/dev/null || kill "$DAEMON_PID" 2>/dev/null || true
  wait "$DAEMON_PID" 2>/dev/null || true
}

# The hog command differs by daemon: ecology-admit ships a `hog`
# subcommand; thor-admit ships a separate `thor-admit-hog` binary beside
# its daemon. Resolve it once.
hog_command() {
  local bin="$1"
  if [[ "$bin" == *ecology-admit* ]]; then
    printf '%s hog' "$bin"
  else
    local dir hog
    dir="$(dirname "$bin")"
    for hog in "$dir/thor-admit-hog" "$dir/thor-admit hog"; do
      if [[ "$hog" == *" hog" ]] || [ -x "$hog" ]; then printf '%s' "$hog"; return; fi
    done
    printf '%s hog' "$bin"  # last resort: a subcommand named hog
  fi
}

leased_run() {
  # echoes the run client's pid; the caller waits on exactly these
  local bin="$1" socket="$2" label="$3" peak="$4" hold="$5" progress="${6:-}"
  local hog_cmd
  hog_cmd="$(hog_command "$bin")"
  if [ -n "$progress" ]; then
    "$bin" run --socket "$socket" --label "$label" --peak "$peak" \
      --progress-every "$progress" -- $hog_cmd --bytes "${HOG_GB}G" --hold "$hold" \
      > /dev/null 2>&1 &
  else
    "$bin" run --socket "$socket" --label "$label" --peak "$peak" \
      -- $hog_cmd --bytes "${HOG_GB}G" --hold "$hold" \
      > /dev/null 2>&1 &
  fi
  echo $!
}

# ------------------------------------------------------- the shared tests

# Let the box settle before a heavy test measures it: the previous
# test's hogs must be gone and MemAvailable must be stable (two reads
# within 2%), or the floors below are computed from a lie.
settle() {
  local deadline=$((SECONDS + 15))
  while pgrep -f 'hog --bytes' > /dev/null 2>&1; do
    [ $SECONDS -ge $deadline ] && break
    sleep 0.5
  done
  local prev=0
  for _ in $(seq 6); do
    local now; now=$(mem_available_bytes)
    if [ "$prev" -gt 0 ]; then
      local delta=$(( now > prev ? now - prev : prev - now ))
      [ "$delta" -lt $((prev / 50)) ] && break
    fi
    prev=$now
    sleep 0.5
  done
}

# Test 1: three 30 GiB hogs, serialized. The floor is computed from the
# box's real headroom (available - hog - 8 GiB) so exactly one fits —
# thor-admit's own integration.rs convention, applied identically here.
test_serialize() {
  local name="$1" bin="$2" socket="$3" statedir="$4"
  settle
  local avail; avail="$(mem_available_bytes)"
  local hog_bytes=$((HOG_GB * GIB))
  if [ "$avail" -lt $((2 * hog_bytes + 512 * 1024 * 1024)) ]; then
    skip 1/4 serialize "$name" \
      "the box ($(gib "$avail") GiB) cannot hold two ${HOG_GB} GiB hogs; use ECO_HOG_GB=<smaller>"
    return 0
  fi
  # exactly one hog fits: projected(avail, hog) = avail - hog >= floor,
  # projected with a second = avail - 2*hog < floor. Midpoint:
  local floor=$(( avail - hog_bytes - hog_bytes / 2 ))
  start_daemon "$name" "$bin" "$socket" "$statedir" --floor "$floor" || return 1
  local oom_before; oom_before="$(oom_kills)"

  local p0 p1 p2
  p0=$(leased_run "$bin" "$socket" hog0 "${HOG_GB}G" 4)
  sleep 0.4
  p1=$(leased_run "$bin" "$socket" hog1 "${HOG_GB}G" 4)
  sleep 0.4
  p2=$(leased_run "$bin" "$socket" hog2 "${HOG_GB}G" 4)

  # poll the daemon's own status for concurrency (same shape both daemons)
  local max_active=0 min_avail=$avail deadline=$((SECONDS + 90))
  while [ $SECONDS -lt $deadline ]; do
    local active
    active=$("$bin" status --socket "$socket" --json 2>/dev/null | python3 -c \
      'import json,sys; print(len(json.load(sys.stdin).get("leases", [])))' 2>/dev/null || echo "?")
    [ "$active" != "?" ] && [ "$active" -gt "$max_active" ] && max_active="$active"
    local a; a="$(mem_available_bytes)"
    [ "$a" -lt "$min_avail" ] && min_avail="$a"
    # done when the queue is empty and no leases remain
    local pending
    pending=$("$bin" status --socket "$socket" --json 2>/dev/null | python3 -c \
      'import json,sys; v=json.load(sys.stdin); print(len(v.get("leases",[]))+len(v.get("queue",[])))' 2>/dev/null || echo 1)
    [ "$pending" = "0" ] && break
    sleep 0.2
  done
  wait "$p0" "$p1" "$p2" 2>/dev/null || true
  local oom_after; oom_after="$(oom_kills)"

  local log="$statedir/stdout.log"
  local grants
  grants=$(grep -c '"lease.granted"' "$log" 2>/dev/null || echo 0)
  stop_daemon "$bin" "$socket"

  if [ "$max_active" = "1" ] && [ "$grants" = "3" ] && [ "$oom_after" = "$oom_before" ]; then
    pass 1/4 serialize "$name" \
      "max concurrent $max_active, 3 grants, box never below $(gib "$min_avail") GiB, OOM kills $oom_after"
  else
    fail 1/4 serialize "$name" \
      "max concurrent $max_active (want 1), grants $grants (want 3), OOM $oom_before->$oom_after; log: $log"
    return 1
  fi
}

# Test 2: a runaway stopped before MemAvailable hits 2 GiB. The emergency
# floor is scaled to the box (available - 8 GiB) so the watchdog acts
# after the hog takes ~8 GiB — with the whole box still free (thor-admit's
# measured approach; the kernel OOM killer must never fire).
test_watchdog() {
  local name="$1" bin="$2" socket="$3" statedir="$4"
  settle
  local avail; avail="$(mem_available_bytes)"
  # the watchdog acts after the runaway has taken a measurable bite:
  # the Thor-scale rule is 8 GiB of 122; a smaller box scales it to a
  # quarter of what it has
  local quarter=$((avail / 4))
  local margin=$((8 * GIB))
  [ "$quarter" -lt "$margin" ] && margin="$quarter"
  local emergency=$(( avail > margin ? avail - margin : 512 * 1024 * 1024 ))
  # the victim must be big enough to be the cause: a quarter of the hog
  local min_victim=$((HOG_GB * GIB / 4))
  # the admission floor must let the runaway in: a quarter of the box
  # (the default 25 GiB is the Thor's rule; a smaller box cannot use it)
  local floor=$((avail / 4))
  start_daemon "$name" "$bin" "$socket" "$statedir" \
    --floor "$floor" --emergency-floor "$emergency" --min-victim-rss "$min_victim" || return 1
  local oom_before; oom_before="$(oom_kills)"

  local runaway_job
  runaway_job=$(leased_run "$bin" "$socket" runaway "${HOG_GB}G" 60)

  local deadline=$((SECONDS + 60)) stopped_avail=""
  while [ $SECONDS -lt $deadline ]; do
    local line
    line=$(grep -m1 '"watchdog.stopped"' "$statedir/stdout.log" 2>/dev/null || true)
    if [ -n "$line" ]; then
      # thor-admit's log lines are flat JSON (event is the name string);
      # ecology-admit's are journal-wrapped ({"chain",...,"event":{...}}).
      # Handle either: unwrap only when "event" is a dict.
      stopped_avail=$(printf '%s' "$line" | python3 -c \
        'import json,sys
v = json.loads(sys.stdin.read())
e = v.get("event", v)
if not isinstance(e, dict):
    e = v
print(e.get("mem_available_bytes", 0))' 2>/dev/null || echo 0)
      break
    fi
    sleep 0.1
  done

  if [ -z "$stopped_avail" ] || [ "$stopped_avail" = "0" ]; then
    kill "$runaway_job" 2>/dev/null || true
    fail 2/4 watchdog "$name" "the watchdog never stopped the runaway; log: $statedir/stdout.log"
    stop_daemon "$bin" "$socket"
    return 1
  fi

  # it must be stopped (state T) and then die after the grace
  local deadline2=$((SECONDS + 30)) died=no
  while [ $SECONDS -lt $deadline2 ]; do
    if ! kill -0 "$runaway_job" 2>/dev/null && ! pgrep -f "hog --bytes ${HOG_GB}G" >/dev/null 2>&1; then
      died=yes; break
    fi
    sleep 0.3
  done
  local oom_after; oom_after="$(oom_kills)"
  stop_daemon "$bin" "$socket"

  if [ "$stopped_avail" -gt $((2 * GIB)) ] && [ "$oom_after" = "$oom_before" ] && [ "$died" = "yes" ]; then
    pass 2/4 watchdog "$name" \
      "stopped with $(gib "$stopped_avail") GiB free (>2), killed after grace, OOM killer never fired"
  else
    fail 2/4 watchdog "$name" \
      "stopped at $(gib "$stopped_avail") GiB, died=$died, OOM $oom_before->$oom_after; log: $statedir/stdout.log"
    return 1
  fi
}

# Test 3: daemon overhead — resident memory and CPU while doing nothing.
test_overhead() {
  local name="$1" bin="$2" socket="$3" statedir="$4"
  start_daemon "$name" "$bin" "$socket" "$statedir" || return 1
  local pid="$DAEMON_PID"
  sleep 2  # warm up
  local rss_before cpu_before
  rss_before=$(awk '/^VmRSS:/ {print $2}' "/proc/$pid/status" 2>/dev/null || echo 0)
  cpu_before=$(awk '{print $14 + $15}' "/proc/$pid/stat" 2>/dev/null || echo 0)
  sleep 10
  local rss_after cpu_after
  rss_after=$(awk '/^VmRSS:/ {print $2}' "/proc/$pid/status" 2>/dev/null || echo 0)
  cpu_after=$(awk '{print $14 + $15}' "/proc/$pid/stat" 2>/dev/null || echo 0)
  stop_daemon "$bin" "$socket"

  # ticks are 100 Hz; 10 s of wall => percent = delta / 10
  local cpu_percent
  cpu_percent=$(awk -v d="$cpu_after" -v b="$cpu_before" 'BEGIN {printf "%.2f", (d - b) / 10.0}')
  if [ "$rss_after" -lt 65536 ] && awk -v p="$cpu_percent" 'BEGIN {exit !(p < 5.0)}'; then
    pass 3/4 overhead "$name" "$((rss_after / 1024)) MiB resident, ${cpu_percent}% of one core"
  else
    fail 3/4 overhead "$name" "$((rss_after / 1024)) MiB resident, ${cpu_percent}% of one core (bars: <64 MiB, <5%)"
    return 1
  fi
}

# Test 4 (the differentiator, ecology-admit only): the stall law. A lease
# that stops attesting progress is harvested to UNKNOWN and killed — with
# memory perfectly fine. A healthy lease (attesting) is never touched.
test_stall() {
  local bin="$ECO_BIN" socket="$OUT/eco-stall.sock" statedir="$OUT/eco-stall"
  local avail; avail="$(mem_available_bytes)"
  # the emergency floor far below anything this test will produce: the
  # watchdog must NOT be the thing that acts
  start_daemon ecology-admit "$bin" "$socket" "$statedir" \
    --floor 1G --emergency-floor "$(( avail / 4 ))" --stall-horizon-s 5 \
    --stop-grace-samples 8 || return 1

  # the healthy lease: attests every second, holds 30 s
  local healthy_job
  healthy_job=$(leased_run "$bin" "$socket" healthy "${HOG_GB}G" 30 1)
  sleep 1
  # the silent hang: never attests
  local hang_job
  hang_job=$(leased_run "$bin" "$socket" silent "${HOG_GB}G" 60)

  local deadline=$((SECONDS + 60)) harvested=""
  while [ $SECONDS -lt $deadline ]; do
    if grep -q '"stall.harvested"' "$statedir/stdout.log" 2>/dev/null; then
      harvested=yes; break
    fi
    sleep 0.5
  done
  sleep 4  # the grace: 8 samples * 250 ms = 2 s
  local killed=""
  grep -q '"watchdog.killed"' "$statedir/stdout.log" 2>/dev/null && killed=yes
  # and the healthy lease is still running, untouched
  local healthy_ok=""
  grep -q '"stall.harvested".*"label":"healthy"' "$statedir/stdout.log" 2>/dev/null || healthy_ok=yes

  kill "$hang_job" 2>/dev/null || true
  pkill -f "hog --bytes ${HOG_GB}G" 2>/dev/null || true
  local chain
  chain=$("$bin" journal --state-dir "$statedir" --verify 2>/dev/null || echo "BROKEN")
  stop_daemon "$bin" "$socket"

  if [ "$harvested" = "yes" ] && [ "$killed" = "yes" ] && [ "$healthy_ok" = "yes" ] \
     && [[ "$chain" == *"chain intact"* ]]; then
    pass 4/4 stall-law ecology-admit \
      "the silent lease harvested to UNKNOWN + killed at 5s stall; the healthy lease untouched; journal chain intact"
  else
    fail 4/4 stall-law ecology-admit \
      "harvested=$harvested killed=$killed healthy-untouched=$healthy_ok chain='$chain'; log: $statedir/stdout.log"
  fi
}

# ------------------------------------------------------------------- run
{
  echo "# The admission bake-off — $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo
  echo "| setting | value |"
  echo "|---|---|"
  echo "| box | $(hostname), MemTotal $(awk '/^MemTotal:/ {printf "%.1f GiB", $2/1048576}' /proc/meminfo), load $(cut -d' ' -f1-3 /proc/loadavg) |"
  echo "| hog | ${HOG_GB} GiB |"
  if [ "$SMALL_BOX" = "1" ]; then
    echo "| small-box mode | YES (demonstration only — not the Thor run) |"
  fi
  echo "| thor-admit | ${THOR_ADMIT_BIN:-not found} |"
  echo "| ecology-admit | $ECO_BIN |"
  echo
} > "$RESULTS"

echo "the admission bake-off: thor-admit vs ecology-admit, same three tests"
echo "hog size: ${HOG_GB} GiB; box: $(gib "$(mem_available_bytes)") GiB available"
echo

take_lock
FAILED=0

# TESTS="serialize watchdog overhead stall" selects a subset (default: all)
TESTS="${TESTS:-serialize watchdog overhead stall}"
wants() { case " $TESTS " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }

if [ -n "$THOR_ADMIT_BIN" ] && [ -x "$THOR_ADMIT_BIN" ]; then
  # per-test state dirs: a failed test's log survives the next test
  wants serialize && require_floor && { test_serialize thor-admit "$THOR_ADMIT_BIN" "$OUT/ta-serialize.sock" "$OUT/ta-serialize" || FAILED=1; }
  wants watchdog  && require_floor && { test_watchdog  thor-admit "$THOR_ADMIT_BIN" "$OUT/ta-watchdog.sock" "$OUT/ta-watchdog" || FAILED=1; }
  wants overhead  && { test_overhead thor-admit "$THOR_ADMIT_BIN" "$OUT/ta-overhead.sock" "$OUT/ta-overhead" || FAILED=1; }
else
  wants serialize && skip 1/4 serialize thor-admit "binary not found (set THOR_ADMIT_BIN=... or THOR_ADMIT_SRC=...)"
  wants watchdog  && skip 2/4 watchdog  thor-admit "binary not found"
  wants overhead  && skip 3/4 overhead  thor-admit "binary not found"
fi

wants serialize && require_floor && { test_serialize ecology-admit "$ECO_BIN" "$OUT/eco-serialize.sock" "$OUT/eco-serialize" || FAILED=1; }
wants watchdog  && require_floor && { test_watchdog  ecology-admit "$ECO_BIN" "$OUT/eco-watchdog.sock" "$OUT/eco-watchdog" || FAILED=1; }
wants overhead  && { test_overhead ecology-admit "$ECO_BIN" "$OUT/eco-overhead.sock" "$OUT/eco-overhead" || FAILED=1; }
wants stall     && require_floor && { test_stall || FAILED=1; }

echo
echo "==================================================================="
echo "the results (also in data/bakeoff/RESULTS.md):"
echo "==================================================================="
cat "$RESULTS"
echo
if [ "$FAILED" -eq 0 ]; then
  echo "ALL RUN TESTS PASSED."
else
  echo "A TEST FAILED — see the FAIL lines above."
fi
exit "$FAILED"
