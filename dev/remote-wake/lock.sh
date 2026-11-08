#!/usr/bin/env bash
# The remote-wake bench lock: one file per host, held around the container and the measured runs, so two bench runs
# never measure side by side. This file is the only place that names it.
#
# The lock is taken after the builds, so a run does not hold it while compiling. A run that cannot take the lock
# within REMOTE_WAKE_LOCK_WAIT_SECONDS exits with REMOTE_WAKE_LOCK_TIMEOUT_EXIT, a code no measurement uses.
#
# Sourced by run.sh; a test sources it to drive the lock on its own.

readonly REMOTE_WAKE_LOCK_FILE="$HOME/remote-wake-runs/.bench.lock"
readonly REMOTE_WAKE_LOCK_WAIT_SECONDS=7200
readonly REMOTE_WAKE_LOCK_TIMEOUT_EXIT=75

# remote_wake_lock: hold the lock on this shell's file descriptor, waiting up to the limit. The lock stays held until
# the shell exits, so every process the run starts lives under it.
remote_wake_lock() {
  mkdir -p "$(dirname "$REMOTE_WAKE_LOCK_FILE")" || return $?
  exec 9>"$REMOTE_WAKE_LOCK_FILE"
  flock -w "$REMOTE_WAKE_LOCK_WAIT_SECONDS" -E "$REMOTE_WAKE_LOCK_TIMEOUT_EXIT" 9
}
