#!/usr/bin/env bash
set -euo pipefail

# Measures the CPU and io shares that keep one agent session's heavy work inside its own share
# (scripts/systemd/charliebot-agents.slice, the session weights the server writes, and, for io,
# scripts/systemd/setup-io-share.sh). Run it from the checkout on the CharlieBot host as the user
# whose manager runs the server; it needs no root and touches no live unit:
#
#   scripts/systemd/check-session-share.sh cpu-slice   # 1-loop session cgroup's share of the test slice
#   scripts/systemd/check-session-share.sh cpu-login   # one login-session busy loop under rival load
#   scripts/systemd/check-session-share.sh io-idle     # sequential-read throughput of the root disk
#   scripts/systemd/check-session-share.sh io-contest  # two session cgroups split the disk about half each
#   scripts/systemd/check-session-share.sh all         # the four in this order
#
# The io legs need setup-io-share.sh to have run first (io delegated to the user manager and io.cost
# enabled on the root disk): io-idle once BEFORE that script and once after, to catch an idle-time
# slowdown, io-contest only after. The CPU legs need no root setup and run in transient units of
# their own under the user manager. Every weighted unit carries CPUWeight=100 explicitly; the cpu-slice
# leg's last phase starts its units without one. The CPU legs saturate every core on purpose: run them
# only when the host can spare the cores for about two minutes.
# CHECK_SHARE_RUN=fake shrinks the CPU legs to a seconds-long plumbing test; the real measurement is
# the default.
#
# cpu-slice: two session-like cgroups under the test slice, check-share.slice, against three rival
# slices, check-rival1..3.slice, that saturate the machine. The rivals are siblings of the test slice
# under check.slice: systemd nests a dash-named slice under its prefix, so their names must not extend
# check-share. Phase one runs 8 busy loops against 1; phase two runs 8 against 8; phase three runs 8
# against 1 with the weights off — the slice stops first so systemd releases the cpu controller the
# weighted units enabled, and the next units set no CPUWeight, so nothing arbitrates between the
# cgroups. All shares are read from cpu.stat. The leg prints them, then the 1-loop cgroup's cores with
# and without the session weights, and a person judges the comparison.
# cpu-login: one busy loop in the login session, through a detached tmux session that the leg creates
# and removes and that touches no other tmux session, against the same rival slices. The login scope
# and the user manager are siblings under user-N.slice. A single loop can use at most one core, so the
# leg reports the cores the loop used (about one under contention) and its share of the machine's usage.
# io-idle: direct sequential reads of 1 GiB, bounded to 30 s, before and after setup-io-share.sh; a read
# that does not finish inside the bound fails the leg. A drop beyond 10% is the line where the plan
# says to undo io.cost.
# io-contest: two session cgroups with IOWeight=100, several direct readers against one; each takes
# about half of the disk's read bandwidth, read from io.stat.

MODE=${1:-}
usage() {
  echo "Usage: $0 [cpu-slice|cpu-login|io-idle|io-contest|all]" >&2
}
case "$MODE" in
  cpu-slice|cpu-login|io-idle|io-contest|all) ;;
  *) usage; exit 1 ;;
esac

RUN_MODE=${CHECK_SHARE_RUN:-real}
USER_UID=$(id -u)
CGROUP_ROOT=/sys/fs/cgroup/user.slice/user-${USER_UID}.slice/user@${USER_UID}.service
BUSY_LOOP='while :; do :; done'

# The window the sampling legs measure over, plus the slack the transient units stay alive for.
WINDOW=20
SLACK=8

# The cgroup of a unit started with --slice=<slice>: systemd builds a slice's hierarchy from its
# dashes, so check-share.slice sits under check.slice.
unit_cgroup() {
  local unit=$1 slice=$2 parent prefix=""
  parent=${slice%.slice}
  while [[ "$parent" == *-* ]]; do
    parent=${parent%-*}
    prefix="$parent.slice/$prefix"
  done
  echo "$CGROUP_ROOT/$prefix$slice/$unit"
}

# Read (usage_usec) from a cgroup directory's cpu.stat.
cpu_usage_usec() {
  awk '$1 == "usage_usec" {print $2}' "$1/cpu.stat"
}

# Print the machine's CPU time used since boot, user plus nice plus system, in nanoseconds at 100 Hz clock.
machine_cpu_nsec() {
  awk '/^cpu / {print ($2 + $3 + $4) * 10000000}' /proc/stat
}

# The cores each side of the last sample_slice_cpu call used. The leg reads them for its comparison.
CORES_A=""
CORES_B=""

# Sample two unit cgroups' cpu.stat over the window, leave each side's cores in CORES_A and CORES_B,
# and print the cores and the second cgroup's share of the two cgroups' usage.
sample_slice_cpu() {
  local unit_a=$1 unit_b=$2 window=$3 path_a path_b a0 b0 a1 b1
  path_a=$(unit_cgroup "$unit_a" check-share.slice)
  path_b=$(unit_cgroup "$unit_b" check-share.slice)
  a0=$(cpu_usage_usec "$path_a")
  b0=$(cpu_usage_usec "$path_b")
  sleep "$window"
  a1=$(cpu_usage_usec "$path_a")
  b1=$(cpu_usage_usec "$path_b")
  CORES_A=$(awk "BEGIN {printf \"%.2f\", ($a1 - $a0) / ($window * 1000000)}")
  CORES_B=$(awk "BEGIN {printf \"%.2f\", ($b1 - $b0) / ($window * 1000000)}")
  echo "cores used: $unit_a $CORES_A," "$unit_b $CORES_B"
  echo "$unit_b share of the two cgroups' usage:" \
    "$(awk "BEGIN {printf \"%.1f\", 100 * ($b1 - $b0) / ($a1 - $a0 + $b1 - $b0)}")%"
}

# Three rival slices of two busy loops each, siblings of the test slice under check.slice: with equal
# weights they take three quarters of the machine and leave the test slice wanting every core it can
# get. $1 is how long they stay up.
start_rivals() {
  local rival lifetime=$1
  for rival in 1 2 3; do
    systemd-run --user --unit=check-rival$rival --slice=check-rival$rival.slice -p CPUWeight=100 \
      bash -c "$BUSY_LOOP & $BUSY_LOOP & sleep $lifetime" >/dev/null
  done
}

stop_rivals() {
  local rival
  for rival in 1 2 3; do
    systemctl --user stop check-rival$rival.service check-rival$rival.slice 2>/dev/null || true
  done
}

# The phase's two session-like units under check-share.slice: $1 and $2 are the loop counts, $3 the
# window, and $4 the CPUWeight both carry — empty starts them without one, so the cpu controller stays
# out of the slice and nothing arbitrates between the cgroups.
start_share_units() {
  local loops_a=$1 loops_b=$2 window=$3 weight=${4:-} property=""
  [ -n "$weight" ] && property="-p CPUWeight=$weight"
  systemd-run --user --unit=check-share-a --slice=check-share.slice $property \
    bash -c "for i in \$(seq $loops_a); do $BUSY_LOOP & done; sleep $((window + SLACK))" >/dev/null
  if [ "$loops_b" -eq 1 ]; then
    systemd-run --user --unit=check-share-b --slice=check-share.slice $property \
      bash -c "$BUSY_LOOP & sleep $((window + SLACK))" >/dev/null
  else
    systemd-run --user --unit=check-share-b --slice=check-share.slice $property \
      bash -c "for i in \$(seq $loops_b); do $BUSY_LOOP & done; sleep $((window + SLACK))" >/dev/null
  fi
}

leg_cpu_slice() {
  local loops_a=8 window=$WINDOW
  if [[ $RUN_MODE == fake ]]; then
    loops_a=2
    window=2
  fi
  echo "== cpu-slice: $loops_a busy loops against 1, then $loops_a against $loops_a, then $loops_a" \
    "against 1 with the weights off, $window s windows =="
  # The rivals stay up across the phases.
  start_rivals $((3 * window + 4 * SLACK + 5))
  # Phase 1, the process-count case: a cgroup with one loop against one with eight. The share is printed
  # as measured; a single thread can use at most one core.
  start_share_units "$loops_a" 1 "$window" 100
  sleep 2
  echo "-- $loops_a loops against 1"
  sample_slice_cpu check-share-a.service check-share-b.service "$window"
  local with_weights=$CORES_B
  sleep "$SLACK"
  systemctl --user stop check-share-a.service check-share-b.service 2>/dev/null || true
  # Phase 2, the weight case: comparable thread counts, so the equal weights should split the slice
  # about evenly.
  start_share_units "$loops_a" "$loops_a" "$window" 100
  sleep 2
  echo "-- $loops_a loops against $loops_a"
  sample_slice_cpu check-share-a.service check-share-b.service "$window"
  sleep "$SLACK"
  # Stopping the slice with its units is what makes systemd release the cpu controller the weighted
  # units enabled, so phase 3 runs without any weight in the chain.
  systemctl --user stop check-share-a.service check-share-b.service check-share.slice 2>/dev/null || true
  # Phase 3, the weights-off case: the same shape with no CPUWeight anywhere. The assertion proves the
  # controller really left, so the comparison answers what the weights changed.
  start_share_units "$loops_a" 1 "$window" ""
  sleep 2
  if grep -qw cpu "$CGROUP_ROOT/check.slice/check-share.slice/cgroup.subtree_control"; then
    echo "cpu-slice: the cpu controller stayed in check-share.slice's cgroup.subtree_control;" \
      "the weights-off phase did not run without weights" >&2
    exit 1
  fi
  echo "-- $loops_a loops against 1, weights off"
  sample_slice_cpu check-share-a.service check-share-b.service "$window"
  local without_weights=$CORES_B
  sleep "$SLACK"
  systemctl --user stop check-share-a.service check-share-b.service check-share.slice 2>/dev/null || true
  stop_rivals
  echo "1-loop cgroup cores, $loops_a against 1: with session weights $with_weights," \
    "without $without_weights"
}

leg_cpu_login() {
  local window=$WINDOW
  if [[ $RUN_MODE == fake ]]; then
    window=2
  fi
  echo "== cpu-login: one busy loop in the login session against rival slices, $window s window =="
  start_rivals $((window + 2 * SLACK))
  local session=check-share-login
  local pid_file=/tmp/check-share-login-loop.$$.pid
  tmux new-session -d -s "$session" \
    "bash -c '$BUSY_LOOP & echo \$! > $pid_file; sleep $((window + SLACK))'"
  trap 'tmux kill-session -t '"$session"' 2>/dev/null || true; rm -f '"$pid_file" EXIT
  sleep 2
  local pid tick0 tick1 cpu0 cpu1
  pid=$(cat "$pid_file")
  echo "loop pid $pid runs in $(sed -n 's/^0:://p' "/proc/$pid/cgroup")"
  tick0=$(awk '{print $14 + $15}' "/proc/$pid/stat")
  cpu0=$(machine_cpu_nsec)
  sleep "$window"
  tick1=$(awk '{print $14 + $15}' "/proc/$pid/stat")
  cpu1=$(machine_cpu_nsec)
  local loop_cores machine_cores
  loop_cores=$(awk "BEGIN {printf \"%.2f\", ($tick1 - $tick0) * 10000000 / ($window * 1000000000)}")
  machine_cores=$(awk "BEGIN {printf \"%.2f\", ($cpu1 - $cpu0) / ($window * 1000000000)}")
  echo "loop used $loop_cores cores; machine used $machine_cores cores"
  echo "loop share of the machine's usage: $(awk "BEGIN {printf \"%.1f\", 100 * $loop_cores / $machine_cores}")%" \
    "(one busy loop cannot exceed one core; under the rival slices it should keep about one)"
  kill "$pid" 2>/dev/null || true
  tmux kill-session -t "$session" 2>/dev/null || true
  rm -f "$pid_file"
  trap - EXIT
  stop_rivals
}

# The MiB one io-idle read covers. Its rate is that size over the time the read took, so a read that the
# bound cuts short has no rate.
IO_READ_MIB=1024

# A direct read of the first IO_READ_MIB MiB of `file` that bypasses the page cache, as the idle user,
# bounded to 30 s. A read that does not finish inside the bound fails the leg.
direct_read_throughput() {
  local file=$1 before after
  before=$(date +%s%N)
  timeout 30 ionice -c 3 nice -n 19 dd if="$file" of=/dev/null iflag=direct bs=1M count="$IO_READ_MIB" status=none \
    || { echo "io-idle: reading $IO_READ_MIB MiB did not finish within 30 s" >&2; exit 1; }
  after=$(date +%s%N)
  awk "BEGIN {printf \"throughput: %.0f MiB/s over $IO_READ_MIB MiB\\n\", \
    $IO_READ_MIB / (($after - $before) / 1000000000)}"
}

leg_io_idle() {
  local file=${CHECK_SHARE_READ_FILE:-}
  if [[ -z "$file" || ! -f "$file" ]]; then
    echo "io-idle: set CHECK_SHARE_READ_FILE to a file of at least $IO_READ_MIB MiB to read" >&2
    exit 1
  fi
  if (( $(stat -c %s "$file") < IO_READ_MIB * 1048576 )); then
    echo "io-idle: $file is smaller than $IO_READ_MIB MiB" >&2
    exit 1
  fi
  echo "== io-idle: direct sequential read of $IO_READ_MIB MiB, 30 s bound =="
  direct_read_throughput "$file"
}

# The read bytes of a cgroup directory's io.stat, summed over the devices.
cgroup_read_bytes() {
  awk '{for (i = 2; i <= NF; i++) if ($i ~ /^rbytes=/) {split($i, kv, "="); s += kv[2]}} END {print s + 0}' \
    "$1/io.stat"
}

leg_io_contest() {
  if ! grep -qw io "$CGROUP_ROOT/cgroup.subtree_control" 2>/dev/null; then
    echo "io-contest: io is not in $CGROUP_ROOT/cgroup.subtree_control; run setup-io-share.sh first" >&2
    exit 1
  fi
  local file=${CHECK_SHARE_READ_FILE:-}
  if [[ -z "$file" || ! -f "$file" ]]; then
    echo "io-contest: set CHECK_SHARE_READ_FILE to a file of at least 2 GiB to read" >&2
    exit 1
  fi
  echo "== io-contest: several direct readers against one, $WINDOW s window =="
  systemd-run --user --unit=check-io-a --slice=check-io.slice -p IOWeight=100 \
    bash -c "for i in 1 2 3; do timeout $WINDOW dd if=$file of=/dev/null iflag=direct bs=1M status=none & done; \
      sleep $SLACK; wait" >/dev/null
  systemd-run --user --unit=check-io-b --slice=check-io.slice -p IOWeight=100 \
    bash -c "timeout $WINDOW dd if=$file of=/dev/null iflag=direct bs=1M status=none; sleep $SLACK" >/dev/null
  sleep "$WINDOW"
  local path_a path_b bytes_a bytes_b
  path_a=$(unit_cgroup check-io-a.service check-io.slice)
  path_b=$(unit_cgroup check-io-b.service check-io.slice)
  bytes_a=$(cgroup_read_bytes "$path_a")
  bytes_b=$(cgroup_read_bytes "$path_b")
  echo "read bytes: check-io-a $bytes_a, check-io-b $bytes_b"
  echo "check-io-b share of the reads: $(awk "BEGIN {printf \"%.1f\", 100 * $bytes_b / ($bytes_a + $bytes_b)}")%"
  systemctl --user stop check-io-a.service check-io-b.service check-io.slice 2>/dev/null || true
}

if [[ $MODE == all || $MODE == cpu-slice ]]; then
  leg_cpu_slice
fi
if [[ $MODE == all || $MODE == cpu-login ]]; then
  leg_cpu_login
fi
if [[ $MODE == all || $MODE == io-idle ]]; then
  leg_io_idle
fi
if [[ $MODE == all || $MODE == io-contest ]]; then
  leg_io_contest
fi
