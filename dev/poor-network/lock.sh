#!/usr/bin/env bash
# The host's bench lock: every bench run on a host holds this one file around each side attempt, so two runs never
# measure side by side. This file is the only place that names the file and calls flock on it.
#
# The holders are the poor-network bench's sides (run.sh), the remote-wake bench's container run
# (dev/remote-wake/lock.sh) and each improve-ux and improve-perf trial side (dev/improve/trial-side.sh).
#
# A side attempt is one trial's start, measurement and stop, not the run's builds: a run builds before it queues, and a
# run with two sides takes the lock once per side. A side that cannot take the lock within its wait exits with
# BENCH_LOCK_TIMEOUT_EXIT, which no trial or bench exit code uses.
#
# Sourced by run.sh and the other holders; a test sources it to drive the lock on its own.

readonly BENCH_LOCK_FILE="$HOME/pn-runs/.bench.lock"
# The bench's own wait. A holder with another wait passes it to bench_lock_within.
readonly BENCH_LOCK_WAIT_SECONDS=7200
# flock's own exit code when the wait runs out: one no trial or bench exit code uses.
readonly BENCH_LOCK_TIMEOUT_EXIT=75

# bench_lock_within <wait seconds> <command> [args...]: exec the command under the bench lock, waiting up to the wait.
bench_lock_within() {
  local wait_seconds=$1
  shift
  # The bench creates the lock's directory, so the lock is the bench's own and no caller prepares it.
  mkdir -p "$(dirname "$BENCH_LOCK_FILE")" || return $?
  # -o keeps the lock in flock's own process, so no process of the command holds it past the command's end.
  exec flock -o -w "$wait_seconds" -E "$BENCH_LOCK_TIMEOUT_EXIT" "$BENCH_LOCK_FILE" "$@"
}

# bench_lock <command> [args...]: exec the command under the bench lock, waiting up to BENCH_LOCK_WAIT_SECONDS for it.
bench_lock() {
  bench_lock_within "$BENCH_LOCK_WAIT_SECONDS" "$@"
}

# bench_lock_hold <wait seconds>: hold the bench lock on this shell's file descriptor 9 until the shell exits, so every
# process the shell starts lives under it. Returns flock's status: BENCH_LOCK_TIMEOUT_EXIT when the wait runs out.
bench_lock_hold() {
  mkdir -p "$(dirname "$BENCH_LOCK_FILE")" || return $?
  exec 9>"$BENCH_LOCK_FILE"
  flock -w "$1" -E "$BENCH_LOCK_TIMEOUT_EXIT" 9
}
