#!/usr/bin/env bash
# One improve trial side: a serve trial (scripts/release/trial.sh --serve ... -- <command>) run under the host's bench
# lock from the trial's start to its stop. The round builds its server binaries before it calls this script, so the
# builds stay outside the lock.
#
#   dev/improve/trial-side.sh <command> [args...]
#
# The side waits at most IMPROVE_TRIAL_LOCK_WAIT_SECONDS (default 900, the round's 15 minutes) for the bench lock. When
# the wait runs out, the command does not run: the side prints its stop reason on stderr and exits with
# BENCH_LOCK_TIMEOUT_EXIT. Otherwise the side exits with the command's exit code. The lock and its flock call belong to
# dev/poor-network/lock.sh; the poor-network bench's own sides take the same lock with their own wait.
set -uo pipefail

readonly SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/../poor-network/lock.sh"
# The variable lets the lock test shorten the wait; a round leaves it unset.
readonly IMPROVE_TRIAL_LOCK_WAIT_SECONDS=${IMPROVE_TRIAL_LOCK_WAIT_SECONDS:-900}

if [[ ! $IMPROVE_TRIAL_LOCK_WAIT_SECONDS =~ ^[1-9][0-9]*$ ]]; then
  printf 'improve trial side: IMPROVE_TRIAL_LOCK_WAIT_SECONDS must be a positive number of seconds, got %s\n' \
    "$IMPROVE_TRIAL_LOCK_WAIT_SECONDS" >&2
  exit 2
fi
if (( $# == 0 )); then
  printf 'improve trial side: name the trial command to run under the bench lock\n' >&2
  exit 2
fi

status=0
# The subshell keeps bench_lock_within's exec from replacing this script, so the stop reason can follow a timeout.
(bench_lock_within "$IMPROVE_TRIAL_LOCK_WAIT_SECONDS" "$@") || status=$?
if (( status == BENCH_LOCK_TIMEOUT_EXIT )); then
  printf 'improve trial side stopped: the bench lock stayed held for %s s\n' "$IMPROVE_TRIAL_LOCK_WAIT_SECONDS" >&2
fi
exit "$status"
