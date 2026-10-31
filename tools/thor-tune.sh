#!/usr/bin/env bash
# thor-tune.sh — configure an AGX Thor devkit for deterministic, max-stable
# kernel performance. Run ON THE THOR (needs sudo). Idempotent.
#
# The full rationale lives in docs/RT.md; this script is the executable
# version of the "tuning checklist" section. Every step is verbose on
# purpose: on a box this expensive, you want to see what changed.
#
# What it does (in order):
#   1. power mode  -> MAXN (mode 0), the 130W-class envelope
#   2. clocks      -> jetson_clocks --fan (fixed clocks: no DVFS jitter)
#   3. target      -> multi-user (headless), if requested
#   4. kernel args -> document-only (isolcpus etc. need an extlinux edit +
#                     reboot; we print the recommended line instead of
#                     editing boot config automatically)
#   5. verify      -> nvpmodel -q, jetson_clocks --show, first tegrastats
#
# NOTES
#   * MAXN is unconstrained: the box may thermally throttle under sustained
#     load. For *repeatable benchmarks* prefer mode 1 (120W) + fixed clocks
#     — docs/BENCHMARKS.md explains the tradeoff (stability vs peak).
#   * OC3 events latch CPUs to 242MHz until reboot — if the box suddenly
#     gets slow, check dmesg for over-current (seen in the field on Thor).

set -euo pipefail

if [[ ${EUID} -ne 0 ]]; then
  echo "thor-tune: needs sudo (power modes and clocks are root operations)" >&2
  exit 1
fi

echo "== 1/5 power mode: MAXN =="
nvpmodel -m 0
nvpmodel -q || true

echo "== 2/5 fixed clocks (and max fan) =="
jetson_clocks --fan
jetson_clocks --show | head -20 || true

echo "== 3/5 (optional) headless target =="
if [[ "${1:-}" == "--headless" ]]; then
  systemctl set-default multi-user.target
  echo "default target -> multi-user (reboot to apply)"
else
  echo "skipped (pass --headless to switch off the desktop)"
fi

echo "== 4/5 kernel boot args (manual, by design) =="
echo "For real-time isolation add to /boot/extlinux/extlinux.conf APPEND:"
echo "  isolcpus=10,11,12,13 nohz_full=10,11,12,13 rcu_nocbs=10,11,12,13 threadirqs"
echo "…then move IRQs off those cores and pin the kernel tick thread there."
echo "For PREEMPT_RT, prefer NVIDIA's rt-kernel packages over a custom build"
echo "(see docs/RT.md). Reboot required; this script will not touch boot config."

echo "== 5/5 verify =="
nvpmodel -q
echo "-- first tegrastats sample --"
timeout 2 tegrastats --interval 1000 || true

echo "thor-tune: done. For benchmark protocol, see docs/BENCHMARKS.md."
