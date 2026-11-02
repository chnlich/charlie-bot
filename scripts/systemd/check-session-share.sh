#!/usr/bin/env bash
set -euo pipefail

# Measures the CPU and io shares that keep one agent session's heavy work inside its own share
# (scripts/systemd/charliebot-agents.slice, the session weights the server writes, and, for io,
# scripts/systemd/setup-io-share.sh). Run it from the checkout on the CharlieBot host as the user
# whose manager runs the server; it needs no root and touches no live unit:
#
#   scripts/systemd/check-session-share.sh cpu-slice   # 1-loop session cgroup gets about half the test slice
#   scripts/systemd/check-session-share.sh cpu-login   # one login-session busy loop keeps its full core
#   scripts/systemd/check-session-share.sh io-idle     # sequential-read throughput of the root disk
#   scripts/systemd/check-session-share.sh io-contest  # two session cgroups split the disk about half each
#   scripts/systemd/check-session-share.sh all         # the four in this order
#
# The io legs need setup-io-share.sh to have run first (io delegated to the user manager and io.cost
# enabled on the root disk): io-idle once BEFORE that script and once after, to catch an idle-time
# slowdown, io-contest only after. The CPU legs need no root setup and run in transient units of
# their own under the user manager. Every unit carries the default CPUWeight=100. CHECK_SHARE_RUN=fake
# shrinks the legs to a seconds-long plumbing test; the real measurement is the default.
#
# cpu-slice: two session-like cgroups under one slice against three rival slices that saturate the
# rest of the machine, read from cpu.stat. Phase one runs 8 busy loops against 1: per-CPU weight
# scheduling gives the 1-loop cgroup's single thread about half of the one core it occupies while the
# sibling's eight threads fill the rest, so its share of the slice's usage lands below half — the leg
# shows the floor a one-process session keeps under full contention. Phase two runs 8 against 8:
# with comparable thread counts the equal weights split the slice evenly, and the share comes out
# near half.
# cpu-login: one busy loop in the login session, through a detached tmux session that the leg creates
# and removes and that touches no other tmux session. A single loop cannot exceed one core, while the
# login session's weight grants it about half the machine when it runs more processes: the leg reads
# the loop's own CPU time and reports the cores it used and its share of the machine's usage.
# io-idle: sequential reads that bypass the page cache, before and after setup-io-share.sh; a drop
# beyond 10% is the line where the plan says to undo io.cost.
# io-contest: two session cgroups with IOWeight=100, several direct readers against one; each takes
# about half of the disk's read bandwidth, read from io.stat.

MODE=${1:-}
usage() {
  echo "Usage: $0 [cpu-slice|cpu-login|io-idle|io-contest|all]" >&2
}
case "$MODE" in
  cpu-slice|cpu-login|io-idle|io-contest|all) ;;
  *) usage ;;
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
    prefix+="$parent.slice/"
  done
  echo "$CGROUP_ROOT/$prefix$slice/$unit"
}

# Read (usage_usec) from a cgroup directory's cpu.stat.
cpu_usage_usec() {
  awk '$1 == "usage_usec" {print $2}' "$1/cpu.stat"
}

# Print the machine's CPU time used since boot, user plus system, in nanoseconds at 100 Hz clock.
machine_cpu_nsec() {
  awk '/^cpu / {print ($2 + $4) * 10000000}' /proc/stat
}

# Sample two unit cgroups' cpu.stat over the window and print each side's cores and the second
# cgroup's share of the two cgroups' usage.
sample_slice_cpu() {
  local unit_a=$1 unit_b=$2 window=$3 path_a path_b a0 b0 a1 b1
  path_a=$(unit_cgroup "$unit_a" check-share.slice)
  path_b=$(unit_cgroup "$unit_b" check-share.slice)
  a0=$(cpu_usage_usec "$path_a")
  b0=$(cpu_usage_usec "$path_b")
  sleep "$window"
  a1=$(cpu_usage_usec "$path_a")
  b1=$(cpu_usage_usec "$path_b")
  echo "cores used: $unit_a $(awk "BEGIN {printf \"%.2f\", ($a1 - $a0) / ($window * 1000000)}")," \
    "$unit_b $(awk "BEGIN {printf \"%.2f\", ($b1 - $b0) / ($window * 1000000)}")"
  echo "$unit_b share of the two cgroups' usage: $(awk "BEGIN {printf \"%.1f\", 100 * ($b1 - $b0) / ($a1 - $a0 + $b1 - $b0)}")%"
}

leg_cpu_slice() {
  local loops_a=8 window=$WINDOW
  if [[ $RUN_MODE == fake ]]; then
    loops_a=2
    window=2
  fi
  echo "== cpu-slice: $loops_a busy loops against 1, then $loops_a against $loops_a, $window s windows =="
  # Three rival slices, two loops each: with equal weights they take three quarters of the machine
  # and leave the test slice, with its 9 loops, wanting every core it can get. They stay up across
  # both phases.
  local rival lifetime=$((2 * window + 2 * SLACK + 5))
  for rival in 1 2 3; do
    systemd-run --user --unit=check-share-r$rival --slice=check-share-r$rival.slice -p CPUWeight=100 \
      bash -c "$BUSY_LOOP & $BUSY_LOOP & sleep $lifetime" >/dev/null
  done
  # Phase 1, the process-count case: a cgroup with one loop competes with one with eight. Per-CPU
  # weight scheduling caps a single-thread cgroup at about half of the one core it occupies while its
  # siblings' threads fill the rest, so its share of the slice's usage lands well below half; what
  # the leg shows is the floor the one-loop session keeps under full contention.
  systemd-run --user --unit=check-share-a --slice=check-share.slice -p CPUWeight=100 \
    bash -c "for i in \$(seq $loops_a); do $BUSY_LOOP & done; sleep $((window + SLACK))" >/dev/null
  systemd-run --user --unit=check-share-b --slice=check-share.slice -p CPUWeight=100 \
    bash -c "$BUSY_LOOP & sleep $((window + SLACK))" >/dev/null
  sleep 2
  echo "-- 8 loops against 1"
  sample_slice_cpu check-share-a.service check-share-b.service "$window"
  sleep "$SLACK"
  systemctl --user stop check-share-a.service check-share-b.service 2>/dev/null || true
  # Phase 2, the weight case: comparable thread counts, so the equal weights must split the slice
  # evenly; the share comes out near half.
  systemd-run --user --unit=check-share-a --slice=check-share.slice -p CPUWeight=100 \
    bash -c "for i in \$(seq $loops_a); do $BUSY_LOOP & done; sleep $((window + SLACK))" >/dev/null
  systemd-run --user --unit=check-share-b --slice=check-share.slice -p CPUWeight=100 \
    bash -c "for i in \$(seq $loops_a); do $BUSY_LOOP & done; sleep $((window + SLACK))" >/dev/null
  sleep 2
  echo "-- $loops_a loops against $loops_a"
  sample_slice_cpu check-share-a.service check-share-b.service "$window"
  sleep "$SLACK"
  systemctl --user stop check-share-a.service check-share-b.service check-share.slice 2>/dev/null || true
  for rival in 1 2 3; do
    systemctl --user stop check-share-r$rival.service check-share-r$rival.slice 2>/dev/null || true
  done
}

leg_cpu_login() {
  local window=$WINDOW
  if [[ $RUN_MODE == fake ]]; then
    window=2
  fi
  echo "== cpu-login: one busy loop in the login session, $window s window =="
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
    "(one busy loop cannot exceed one core; the login session's weight grants it about half the machine when it runs more)"
  kill "$pid" 2>/dev/null || true
  tmux kill-session -t "$session" 2>/dev/null || true
  rm -f "$pid_file"
  trap - EXIT
}

# A direct read of `file` that bypasses the page cache, as the idle user.
direct_read_throughput() {
  local file=$1 seconds=$2
  local before after bytes
  before=$(date +%s%N)
  ionice -c 3 nice -n 19 timeout "$seconds" dd if="$file" of=/dev/null iflag=direct bs=1M status=none || true
  after=$(date +%s%N)
  bytes=$(stat -c %s "$file")
  awk "BEGIN {printf \"%.0f MB/s over ${seconds}s\", $bytes / (($after - $before) / 1000000000) / 1048576}"
}

leg_io_idle() {
  local file=${CHECK_SHARE_READ_FILE:-}
  if [[ -z "$file" || ! -f "$file" ]]; then
    echo "io-idle: set CHECK_SHARE_READ_FILE to a file of at least 2 GiB to read" >&2
    exit 1
  fi
  echo "== io-idle: direct sequential read, 10 s =="
  echo "throughput: $(direct_read_throughput "$file" 10)"
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
    bash -c "for i in 1 2 3; do timeout $WINDOW dd if=$file of=/dev/null iflag=direct bs=1M status=none & done; sleep $SLACK; wait" >/dev/null
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
