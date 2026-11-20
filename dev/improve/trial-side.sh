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
#
# The side's marker lives in /tmp/improve-<IMPROVE_DIRECTION>, the round's own scratch directory, which the round runner
# exports as IMPROVE_DIRECTION. The marker is not under TMPDIR: the round's TMPDIR lost a side's marker while the side
# waited for the lock (improve-ux 20261120T1726Z), and a side outside a round has no direction to name.
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
if [[ ! ${IMPROVE_DIRECTION:-} =~ ^[a-z][a-z0-9_]*$ ]]; then
  printf 'improve trial side: IMPROVE_DIRECTION names the round direction, got %s\n' "${IMPROVE_DIRECTION:-}" >&2
  exit 2
fi
readonly MARKER_ROOT="/tmp/improve-$IMPROVE_DIRECTION"

# A side that ends without its EXIT trap (a SIGKILL, a stopped scope) leaves its marker behind. The marker's name carries
# the side's pid, so a marker whose pid has no process is a dead side's: the side names it and removes it. Each side reads
# only its own marker, so a dead side's marker never counts as this side's start.
mkdir -p -- "$MARKER_ROOT" || exit 1
for stale_marker in "$MARKER_ROOT"/trial-side.*; do
  [[ -d $stale_marker ]] || continue
  stale_pid=${stale_marker##*/trial-side.}
  stale_pid=${stale_pid%%.*}
  [[ $stale_pid =~ ^[0-9]+$ && ! -d /proc/$stale_pid ]] || continue
  printf 'improve trial side: removed the marker of dead side %s: %s\n' "$stale_pid" "$stale_marker" >&2
  rm -rf -- "$stale_marker" || exit 1
done

if ! marker_dir=$(mktemp -d "$MARKER_ROOT/trial-side.$$.XXXXXX"); then
  exit 1
fi
started_marker="$marker_dir/started"
cleanup_marker() {
  if [[ -e $started_marker ]]; then
    rm -- "$started_marker"
  fi
  rmdir -- "$marker_dir"
}
trap cleanup_marker EXIT

status=0
# The subshell keeps bench_lock_within's exec from replacing this script, so the stop reason can follow a timeout.
(bench_lock_within "$IMPROVE_TRIAL_LOCK_WAIT_SECONDS" bash -c \
  'started=$1; shift; : > "$started" || exit $?; exec "$@"' improve-trial-side "$started_marker" "$@") || status=$?
if (( status == BENCH_LOCK_TIMEOUT_EXIT )) && [[ ! -e $started_marker ]]; then
  printf 'improve trial side stopped: the bench lock stayed held for %s s\n' "$IMPROVE_TRIAL_LOCK_WAIT_SECONDS" >&2
fi
exit "$status"
