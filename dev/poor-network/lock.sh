#!/usr/bin/env bash
# The poor-network bench lock: every bench run on a host holds this one file around each side attempt, so two runs never
# measure side by side. This file is the only place that names it.
#
# A side attempt is one trial's start, measurement and stop, not the run's builds: a run builds before it queues, and a
# run with two sides takes the lock once per side. A side that cannot take the lock within BENCH_LOCK_WAIT_SECONDS exits
# with BENCH_LOCK_TIMEOUT_EXIT, which no trial or bench exit code uses.
#
# Sourced by run.sh; a test sources it to drive the lock on its own.

readonly BENCH_LOCK_FILE="$HOME/pn-runs/.bench.lock"
readonly BENCH_LOCK_WAIT_SECONDS=7200
# flock's own exit code when the wait runs out: one no trial or bench exit code uses.
readonly BENCH_LOCK_TIMEOUT_EXIT=75

# bench_lock <command> [args...]: exec the command under the bench lock, waiting up to BENCH_LOCK_WAIT_SECONDS for it.
bench_lock() {
  # The bench creates the lock's directory, so the lock is the bench's own and no caller prepares it.
  mkdir -p "$(dirname "$BENCH_LOCK_FILE")" || return $?
  # -o keeps the lock in flock's own process, so no process of the command holds it past the command's end.
  exec flock -o -w "$BENCH_LOCK_WAIT_SECONDS" -E "$BENCH_LOCK_TIMEOUT_EXIT" "$BENCH_LOCK_FILE" "$@"
}
