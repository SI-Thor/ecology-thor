#!/usr/bin/env bash
# thor-device.sh — the ONE script a stranger runs on the AGX Thor.
#
# Purpose: run every device-dependent step of this repo in one shot, under
# the box's rules, and produce a report bundle the seat pastes into
# docs/BENCHMARKS.md (§AGX Thor) and docs/PLACEMENT.md.
#
# What it runs (each step prints exactly one PASS/FAIL line):
#   0 preflight   — record the box state (memory, clocks, toolchain)
#   1 guard       — cargo fmt+clippy+test (no CUDA needed; proves the tree)
#   2 kernels     — make kernels  → build/ecology.ptx, must target sm_110
#   3 parity      — make test-device — THE GATE: device vs CPU, bit for bit
#                   (prints "device parity: OK (8192 entities, ...)")
#   4 ptx         — make ptx-check — ptxas -v register counts (sm_110 cubins)
#   5 bench       — make bench — the population ladder (skippable: --skip-bench)
#   6 report      — data/device-report/ bundle: box state, step timings,
#                   PASS/FAIL summary, and paste-ready doc tables
#
# BOX RULES (non-negotiable, the box OOM-crashed on 2026-10-31):
#   * every heavy step runs while THIS SCRIPT holds flock ~/lm-wt/heavy.lock
#     (steps 2–5; the light steps 0–1 run before taking it);
#   * ≥ 25 GiB must stay free — checked before the lock, and re-checked
#     between heavy steps; a step that would start below the floor FAILS
#     with a clear reason instead of running;
#   * the box's usable memory is 122 GiB (MemTotal ≈ 125.7 GiB) — the
#     figure every threshold in this repo is stated against (THOR.md).
#
# Usage:
#   tools/thor-device.sh                 # skip if the heavy lock is held
#   THOR_DEVICE_WAIT_SECS=1800 tools/thor-device.sh   # wait up to 30 min
#   tools/thor-device.sh --skip-bench    # parity + ptx only (faster)
#   THOR_DEVICE_SKIP_HEAVY_LOCK=1 tools/thor-device.sh  # ONLY for step 0–1
#                                      on a box you know is idle. Never
#                                      for steps 2–5.
#
# Expected total runtime: ~15–35 min (bench is the long part).
# Exit code: 0 iff every step that ran printed PASS.
#
# Expected outputs, exactly (see docs/DEVICE.md for the full list):
#   [1/6] guard       ... PASS   (36 tests, fmt+clippy clean)
#   [2/6] kernels     ... PASS   (build/ecology.ptx, target sm_110)
#   [3/6] parity      ... PASS   (device parity: OK (8192 entities, all columns bit-exact))
#   [4/6] ptx         ... PASS   (ptx-check OK, 0 spills — counts in the report)
#   [5/6] bench       ... PASS   (4 reports under data/)
#   [6/6] report      ... PASS   (data/device-report/REPORT.md)

set -euo pipefail

# ------------------------------------------------------------------ config
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LOCK_PATH="${HOME}/lm-wt/heavy.lock"
FLOOR_BYTES=$((25 * 1024 * 1024 * 1024))       # ≥ 25 GiB must stay free
USABLE_NOTE="usable memory is 122 GiB (MemTotal ≈ 125.7 GiB) — THOR.md"
WAIT_SECS="${THOR_DEVICE_WAIT_SECS:-0}"
SKIP_BENCH=0
for arg in "$@"; do
  case "$arg" in
    --skip-bench) SKIP_BENCH=1 ;;
    *) echo "unknown argument: $arg (see the header of this script)"; exit 64 ;;
  esac
done

OUT_DIR="$REPO_ROOT/data/device-report"
mkdir -p "$OUT_DIR"
STEPS_JSON="$OUT_DIR/steps.jsonl"
: > "$STEPS_JSON"

# ------------------------------------------------------------- helpers
step()  { printf '%s\n' "$*" | tee -a "$OUT_DIR/run.log"; }
say()   { printf '%s\n' "$*" | tee -a "$OUT_DIR/run.log" >&2; }

# MemAvailable in bytes, straight from the kernel (the number the OOM
# killer and thor-admit both act on — thor-admit design §6).
mem_available() {
  awk '/^MemAvailable:/ {printf "%d", $2 * 1024}' /proc/meminfo
}
mem_total() {
  awk '/^MemTotal:/ {printf "%d", $2 * 1024}' /proc/meminfo
}
gb() { awk -v b="$1" 'BEGIN {printf "%.1f", b / 1024 / 1024 / 1024}'; }

# Record one step's outcome. $1 name, $2 status (PASS/FAIL/SKIP),
# $3 seconds, $4.. key output lines.
record() {
  local name="$1" status="$2" secs="$3"; shift 3
  local lines; lines="$(printf '%s' "$*" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))')"
  printf '{"step":%s,"status":%s,"seconds":%.1f,"detail":%s}\n' \
    "$(python3 -c "import json;print(json.dumps('$name'))")" \
    "$(python3 -c "import json;print(json.dumps('$status'))")" \
    "$secs" "$lines" >> "$STEPS_JSON"
  printf '[%s] %-12s %s' "$STEP_NO" "$name" "$status" | tee -a "$OUT_DIR/run.log"
  [ -n "$*" ] && printf '   (%s)' "$*" | tee -a "$OUT_DIR/run.log" || true
  printf '\n' | tee -a "$OUT_DIR/run.log"
}

FAILED=0
STEP_NO=0
next() { STEP_NO=$((STEP_NO + 1)); }

# The floor check: refuses to run a heavy step when the box is tight.
# Prints the reason and returns 1 (the caller records FAIL and stops).
require_floor() {
  local avail; avail="$(mem_available)"
  if [ "$avail" -lt "$FLOOR_BYTES" ]; then
    say "REFUSING: MemAvailable is $(gb "$avail") GiB, below the box's 25 GiB floor."
    say "          ($USABLE_NOTE) Free memory or wait for the other seat, then re-run."
    return 1
  fi
  say "        mem-available $(gb "$avail") GiB (floor 25.0 GiB, kept free)"
}

# Take the box's heavy lock. Without it steps 2-5 do not run: the lock is
# how the seats keep one heavy thing at a time on a 122 GiB box.
HEAVY_LOCK_FD=""
take_heavy_lock() {
  mkdir -p "$(dirname "$LOCK_PATH")"
  eval "exec 9>\"$LOCK_PATH\""
  local waited=0
  while ! flock -n 9; do
    if [ "$WAIT_SECS" -le 0 ] || [ "$waited" -ge "$WAIT_SECS" ]; then
      say "SKIPPED: another seat holds $LOCK_PATH (the box's heavy lock)."
      say "         Re-run with THOR_DEVICE_WAIT_SECS=<seconds> to wait for it."
      return 1
    fi
    [ "$waited" -eq 0 ] && say "waiting for the box's heavy lock (up to ${WAIT_SECS}s)..."
    sleep 5; waited=$((waited + 5))
  done
  HEAVY_LOCK_FD=9
  say "        holding $LOCK_PATH for steps 2-5"
}

# ---------------------------------------------------------------- 0 preflight
next
T0=$SECONDS
{
  echo "# box state — $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "host: $(hostname)"
  echo "uname: $(uname -a)"
  echo "model: $(cat /proc/device-tree/model 2>/dev/null | tr -d '\0' || echo unknown)"
  echo "kernel: $(uname -r)"
  awk '/^(MemTotal|MemAvailable|SwapTotal):/ {print}' /proc/meminfo
  echo "usable-figure: 122 GiB (project convention, THOR.md)"
  if command -v nvidia-smi >/dev/null 2>&1; then
    nvidia-smi --query-gpu=name,compute_cap,driver_version --format=csv,noheader 2>/dev/null \
      || echo "nvidia-smi: present but query failed (normal on Tegra)"
  fi
  command -v tegrastats >/dev/null 2>&1 && echo "tegrastats: present"
  nvpmodel -q 2>/dev/null | head -5 || true
  [ -f /proc/sys/kernel/sched_rt_runtime_us ] && \
    echo "sched_rt_runtime_us: $(cat /proc/sys/kernel/sched_rt_runtime_us)"
  echo "nvcc: $(nvcc --version 2>/dev/null | tail -1 || echo MISSING)"
  echo "cargo: $(cargo --version 2>/dev/null || echo MISSING)"
  echo "rustc: $(rustc --version 2>/dev/null || echo MISSING)"
} > "$OUT_DIR/box-state.txt" 2>&1

PRE_OK=1
grep -q "nvcc: MISSING" "$OUT_DIR/box-state.txt" && PRE_OK=0
grep -q "cargo: MISSING" "$OUT_DIR/box-state.txt" && PRE_OK=0
if [ "$PRE_OK" -eq 1 ]; then
  record preflight PASS "$((SECONDS - T0))" "box state in data/device-report/box-state.txt"
else
  record preflight FAIL "$((SECONDS - T0))" "nvcc or cargo missing — install JetPack CUDA + Rust first (docs/DEVICE.md)"
  FAILED=1
fi
cd "$REPO_ROOT"

# ---------------------------------------------------------------- 1 guard
next
T0=$SECONDS
if cargo fmt --all -- --check > "$OUT_DIR/guard-fmt.log" 2>&1 \
   && cargo clippy --workspace --all-targets -- -D warnings > "$OUT_DIR/guard-clippy.log" 2>&1 \
   && cargo test --workspace > "$OUT_DIR/guard-test.log" 2>&1; then
  TESTS_RUN=$(grep -oE '[0-9]+ passed' "$OUT_DIR/guard-test.log" | tail -1 || true)
  record guard PASS "$((SECONDS - T0))" "fmt+clippy clean, ${TESTS_RUN:-tests ok} (logs in data/device-report/)"
else
  record guard FAIL "$((SECONDS - T0))" "see data/device-report/guard-*.log"
  FAILED=1
fi

# Everything below needs the heavy lock. If the caller explicitly opts
# out (they know the box is theirs and idle), we still enforce the floor.
if [ "${THOR_DEVICE_SKIP_HEAVY_LOCK:-0}" = "1" ]; then
  say "WARNING: running heavy steps WITHOUT the box's heavy lock — only on an idle box you own."
else
  if ! take_heavy_lock; then
    next; record kernels SKIP 0 "no heavy lock"; next; record parity SKIP 0 "no heavy lock"
    next; record ptx SKIP 0 "no heavy lock";   next; record bench SKIP 0 "no heavy lock"
    next; record report SKIP 0 "nothing to report"
    exit 77   # skipped, not failed — distinguishable from a real failure
  fi
fi
trap '[ -n "$HEAVY_LOCK_FD" ] && eval "exec $HEAVY_LOCK_FD>&-" || true' EXIT

# ---------------------------------------------------------------- 2 kernels
next
T0=$SECONDS
if ! require_floor; then record kernels FAIL "$((SECONDS - T0))" "below the 25 GiB floor"; FAILED=1; exit 1; fi
if make kernels > "$OUT_DIR/kernels.log" 2>&1; then
  PTX="crates/ecology-cuda/kernels/build/ecology.ptx"
  if [ -f "$PTX" ] && grep -qE '^\.target (sm_110|compute_110)' "$PTX"; then
    TARGET=$(grep -m1 -oE 'sm_110|compute_110' "$PTX")
    SIZE=$(du -h "$PTX" | cut -f1)
    record kernels PASS "$((SECONDS - T0))" "build/ecology.ptx ($SIZE, target $TARGET)"
  else
    record kernels FAIL "$((SECONDS - T0))" "PTX missing or wrong arch — see data/device-report/kernels.log"
    FAILED=1
  fi
else
  record kernels FAIL "$((SECONDS - T0))" "nvcc failed — see data/device-report/kernels.log"
  FAILED=1
fi
[ "$FAILED" -eq 1 ] && exit 1

# ---------------------------------------------------------------- 3 parity
next
T0=$SECONDS
if ! require_floor; then record parity FAIL "$((SECONDS - T0))" "below the 25 GiB floor"; FAILED=1; exit 1; fi
if make test-device > "$OUT_DIR/parity.log" 2>&1; then
  PARITY_LINE=$(grep -m1 'device parity: OK' "$OUT_DIR/parity.log" || true)
  if [ -n "$PARITY_LINE" ]; then
    record parity PASS "$((SECONDS - T0))" "$PARITY_LINE — THE GATE (sm_110, bit for bit)"
  else
    record parity FAIL "$((SECONDS - T0))" "tests passed but no parity line — see parity.log"
    FAILED=1
  fi
else
  record parity FAIL "$((SECONDS - T0))" "device leg failed — see data/device-report/parity.log (do NOT adopt the device path until understood)"
  FAILED=1
fi
[ "$FAILED" -eq 1 ] && exit 1

# ---------------------------------------------------------------- 4 ptx
next
T0=$SECONDS
if ! require_floor; then record ptx FAIL "$((SECONDS - T0))" "below the 25 GiB floor"; FAILED=1; exit 1; fi
if make ptx-check > "$OUT_DIR/ptx-check.log" 2>&1; then
  SPILLS=$(grep -c 'spill' "$OUT_DIR/ptx-check.log" || true)
  if [ "$SPILLS" -eq 0 ]; then
    record ptx PASS "$((SECONDS - T0))" "ptx-check OK, 0 spills (register counts in ptx-check.log)"
  else
    record ptx FAIL "$((SECONDS - T0))" "$SPILLS spill line(s) — see ptx-check.log (a spill is a defect, PTX_NOTES.md)"
    FAILED=1
  fi
else
  record ptx FAIL "$((SECONDS - T0))" "ptxas failed — see ptx-check.log"
  FAILED=1
fi
[ "$FAILED" -eq 1 ] && exit 1

# ---------------------------------------------------------------- 5 bench
next
if [ "$SKIP_BENCH" -eq 1 ]; then
  record bench SKIP 0 "--skip-bench given"
else
  T0=$SECONDS
  if ! require_floor; then record bench FAIL "$((SECONDS - T0))" "below the 25 GiB floor"; FAILED=1; exit 1; fi
  if make bench > "$OUT_DIR/bench.log" 2>&1; then
    N=$(ls data/*.json 2>/dev/null | wc -l)
    if [ "$N" -ge 4 ]; then
      record bench PASS "$((SECONDS - T0))" "$N reports under data/ (stress 10k/50k/100k + world-gen 50k)"
    else
      record bench FAIL "$((SECONDS - T0))" "expected 4 reports, found $N — see bench.log"
      FAILED=1
    fi
  else
    record bench FAIL "$((SECONDS - T0))" "see data/device-report/bench.log"
    FAILED=1
  fi
fi

# ---------------------------------------------------------------- 6 report
next
T0=$SECONDS
python3 tools/render_device_report.py "$OUT_DIR" > "$OUT_DIR/REPORT.md" 2> "$OUT_DIR/report.err" \
  && record report PASS "$((SECONDS - T0))" "data/device-report/REPORT.md — paste its tables into BENCHMARKS.md + PLACEMENT.md" \
  || { record report FAIL "$((SECONDS - T0))" "$(cat "$OUT_DIR/report.err" | head -1)"; FAILED=1; }

say ""
say "==================================================================="
if [ "$FAILED" -eq 0 ]; then
  say "ALL STEPS PASSED. Hand data/device-report/REPORT.md to the seat:"
  say "  - BENCHMARKS.md §AGX Thor   ← the rendered table rows"
  say "  - PLACEMENT.md thresholds   ← the measured crossovers section"
else
  say "A STEP FAILED — see the FAIL line above and its log under data/device-report/."
fi
say "==================================================================="
exit "$FAILED"
