#!/usr/bin/env bash
# One improve trial side: a serve trial (scripts/release/trial.sh --serve ... -- <command>) run from the trial's
# start to its stop. The round builds its server binaries before it calls this script, so the builds stay outside
# the side.
#
#   dev/improve/trial-side.sh <command> [args...]
#
# A serve trial side leaves one side record in $CHARLIEBOT_RELEASE_RUN_DIR. Each side run gets its own record, which
# dev/improve/side-record.sh writes, and the record holds these four lines:
#
#   commit=<the full sha that the trial serves: its <commit> argument>
#   binary_sha256=<sha256 of its <server binary>, taken as the trial command starts>
#   output=<the side's raw output directory, new for each side run>
#   exit_status=<the side's exit status: the command's, 75 when the lock wait runs out, or 128 plus the signal number
#   when a signal stopped the side>
#
# The command runs with IMPROVE_SIDE_OUTPUT and CHARLIEBOT_RELEASE_RUN_DIR set to the output directory, and it writes
# its raw output there. The commit and the binary come from the trial's own arguments, so the record names the binary
# that trial.sh serves. A lock timeout has no binary hash because the trial command never starts. A command without
# --serve runs as a side with no record.
#
# IMPROVE_TRIAL_USE_LOCK chooses whether the side holds the host's bench lock around its command. 1, the default,
# holds the lock: the side waits at most IMPROVE_TRIAL_LOCK_WAIT_SECONDS (default 900, the round's 15 minutes) for
# it. When the wait runs out, the command does not run: the side prints its stop reason on stderr and exits with
# BENCH_LOCK_TIMEOUT_EXIT. 0 is available only to improve-ux sides, which never wait for the lock and never refuse to
# start because of it. Such a side writes the lock's holder, or none, at its start and at its end to a bench-lock
# record under $CHARLIEBOT_RELEASE_RUN_DIR; when the holder cannot be read, it writes that unreadable holder with the
# reader's reason, and it runs its command anyway. The side serves on a port range the bench's default does not use.
# Either way the side exits with the command's exit code. The lock and its flock call belong to
# dev/poor-network/lock.sh; the poor-network bench's own sides take the same lock with their own wait.
#
# Lock-free serve sides in one round share a lock in the round's record directory. A side holds it through the trial
# command, so the next side starts after the previous trial has stopped its server and scope.
#
# A signal stops the side. SIGTERM, SIGINT or SIGHUP goes once to each process of the trial, and the lock helper, which
# holds the lock until the trial has ended, gets the signal only while no trial process runs. The side waits
# IMPROVE_TRIAL_STOP_WAIT_SECONDS (default 120) for the trial's processes to end, then SIGKILLs them; the helper ends
# after its trial and releases the lock. The side records exit_status as 128 plus the signal number, prints one line
# that names the signal and the trial pid (none when the trial had not started), and exits with that status: 143 for
# SIGTERM. SIGKILL cannot be caught, so a SIGKILL to this script leaves its trial running. landing.md's "A side past
# one tool call" section names the form that runs a side past one tool call.
#
# The side's marker lives in /tmp/improve-<IMPROVE_DIRECTION>, the round's own scratch directory, which the round runner
# exports as IMPROVE_DIRECTION. The marker is not under TMPDIR: the round's TMPDIR lost a side's marker while the side
# waited for the lock (improve-ux 20261120T1726Z), and a side outside a round has no direction to name.
set -uo pipefail

readonly SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/../poor-network/lock.sh"
source "$SCRIPT_DIR/side-record.sh"
# The variables let the lock test shorten the wait and switch the lock; a round leaves them unset.
readonly IMPROVE_TRIAL_LOCK_WAIT_SECONDS=${IMPROVE_TRIAL_LOCK_WAIT_SECONDS:-900}
readonly IMPROVE_TRIAL_USE_LOCK=${IMPROVE_TRIAL_USE_LOCK-1}
# A stop waits this long for the trial's processes to end after its signal, then SIGKILLs them. The stop test shortens
# it.
readonly IMPROVE_TRIAL_STOP_WAIT_SECONDS=${IMPROVE_TRIAL_STOP_WAIT_SECONDS:-120}
# trial.sh's default port range is the bench's. A side that runs beside the bench serves on this range instead, unless
# its caller names a range of its own.
readonly BESIDE_BENCH_PORT_MIN=18670
readonly BESIDE_BENCH_PORT_MAX=18689
# The serve trial's commit and server binary (side_parse_serve) and the side record's paths (side_record_open). Each
# stays empty for a command that is not a serve trial.
SIDE_COMMIT=
SIDE_BINARY=
SIDE_RECORD=
SIDE_OUTPUT=
# The bench-lock record of a side that runs beside the lock (IMPROVE_TRIAL_USE_LOCK=0). Empty for a locked side.
SIDE_LOCKLESS_RECORD=
# SIDE_JOB is the background job that runs the side's command. It heads a session of its own, so its pid is the session
# id and the process group id of its processes. The job is a helper whose one child is the trial: the flock helper of a
# locked side, or the shell that runs the command of a side without the lock.
SIDE_JOB=

if [[ ! $IMPROVE_TRIAL_LOCK_WAIT_SECONDS =~ ^[1-9][0-9]*$ ]]; then
  printf 'improve trial side: IMPROVE_TRIAL_LOCK_WAIT_SECONDS must be a positive number of seconds, got %s\n' \
    "$IMPROVE_TRIAL_LOCK_WAIT_SECONDS" >&2
  exit 2
fi
if [[ ! $IMPROVE_TRIAL_STOP_WAIT_SECONDS =~ ^[1-9][0-9]*$ ]]; then
  printf 'improve trial side: IMPROVE_TRIAL_STOP_WAIT_SECONDS must be a positive number of seconds, got %s\n' \
    "$IMPROVE_TRIAL_STOP_WAIT_SECONDS" >&2
  exit 2
fi
if [[ ! $IMPROVE_TRIAL_USE_LOCK =~ ^[01]$ ]]; then
  printf 'improve trial side: IMPROVE_TRIAL_USE_LOCK must be 1 (hold the bench lock) or 0 (run beside it), got %s\n' \
    "$IMPROVE_TRIAL_USE_LOCK" >&2
  exit 2
fi
if (( $# == 0 )); then
  printf 'improve trial side: name the trial command to run under the bench lock\n' >&2
  exit 2
fi

# side_parse_serve <command args...>: for a serve trial, set SIDE_COMMIT and SIDE_BINARY from the arguments that
# scripts/release/trial.sh reads after --serve: [--tree-root <dir>] [--seed-home <dir>] [--in-caller-scope] <commit>
# <server binary> -- <command>. A command without --serve leaves both empty. Arguments of any other shape exit 2 before
# the side takes the lock, since trial.sh would refuse them.
side_parse_serve() {
  local -a args=("$@")
  local i=0 count=${#args[@]} commit binary
  while (( i < count )) && [[ ${args[i]} != --serve ]]; do
    i=$((i + 1))
  done
  (( i < count )) || return 0
  i=$((i + 1))
  while (( i < count )); do
    case ${args[i]} in
      --tree-root | --seed-home) i=$((i + 2)) ;;
      --in-caller-scope) i=$((i + 1)) ;;
      *) break ;;
    esac
  done
  commit=${args[i]:-}
  binary=${args[i + 1]:-}
  if (( i + 3 >= count )) || [[ ${args[i + 2]} != -- ]] \
    || [[ ! $commit =~ ^[0-9a-fA-F]{40}$ ]] || [[ -z $binary ]]; then
    printf 'improve trial side: a serve trial takes --serve [--tree-root <dir>] [--seed-home <dir>]' >&2
    printf ' [--in-caller-scope] <commit> <server binary> -- <command>\n' >&2
    exit 2
  fi
  SIDE_COMMIT=${commit,,}
  SIDE_BINARY=$binary
}
side_parse_serve "$@"
if [[ -n $SIDE_COMMIT && -z ${CHARLIEBOT_RELEASE_RUN_DIR:-} ]]; then
  printf 'improve trial side: a serve trial needs CHARLIEBOT_RELEASE_RUN_DIR for its side record\n' >&2
  exit 2
fi

# bench_lock_record <record file> <point>: append the bench lock's holder at one point of the side, start or end. A
# failed holder read is recorded as an unreadable holder with the reader's reason, and the side goes on: a side beside
# the bench lock never waits for the lock and never refuses to run because of its holder.
bench_lock_record() {
  local holder reason
  if holder=$(bench_lock_holder 2>&1); then
    printf '%s side %s: bench lock holder %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$2" "${holder:-none}" >>"$1" \
      || return 1
    # The status is explicit: a bare return in a function that a trap handler calls returns the status of the command
    # that the trap interrupted, which a stop's interrupted wait makes 128 plus the signal number.
    return 0
  fi
  # The holder read's reason arrives on stderr; a record line holds one line.
  reason=${holder//$'\n'/ }
  printf 'improve trial side: the bench lock holder at the side %s is unreadable; recorded with its reason\n' "$2" >&2
  printf '%s side %s: bench lock holder unreadable: %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$2" \
    "${reason:-the holder read failed without a message}" >>"$1"
}

# open_side_record: for a serve trial, create its side record and export its output directory to the command.
open_side_record() {
  [[ -n $SIDE_COMMIT ]] || return 0
  side_record_open "$CHARLIEBOT_RELEASE_RUN_DIR" "$SIDE_COMMIT" || exit 1
  export IMPROVE_SIDE_OUTPUT=$SIDE_OUTPUT
}

# close_side_record <exit status>: end a serve trial's side record with the side's exit status.
close_side_record() {
  [[ -n $SIDE_RECORD ]] || return 0
  side_record_close "$SIDE_RECORD" "$1" || exit 1
}

# side_procs: print `<pid> <parent pid> <process group>` for each live process, one per line. A zombie is not live. The
# scan reads /proc only, so it neither starts a process nor takes a lock.
side_procs() {
  local stat_path stat pid state ppid pgrp rest
  for stat_path in /proc/[0-9]*/stat; do
    # A process that exits during the scan has no stat file left, so its read fails and the scan skips it.
    read -r stat 2>/dev/null <"$stat_path" || continue
    pid=${stat_path#/proc/}
    pid=${pid%/stat}
    # The command name in field 2 is in parentheses, so the fields start after its last ")".
    rest=${stat##*\) }
    read -r state ppid pgrp _ <<<"$rest"
    if [[ $state == [ZX] ]]; then
      continue
    fi
    printf '%s %s %s\n' "$pid" "$ppid" "$pgrp"
  done
}

# side_group_pids: print the pid of each live process of the side's group, the job included. The job heads its own
# session, so its group is the session's: a process that leaves the group is not listed.
side_group_pids() {
  local pid ppid pgrp
  while read -r pid ppid pgrp; do
    if [[ $pgrp == "$SIDE_JOB" ]]; then
      printf '%s\n' "$pid"
    fi
  done < <(side_procs)
}

# side_trial_pid: print the trial's pid, which is the job's child. It prints nothing before the trial starts.
side_trial_pid() {
  local pid ppid pgrp
  while read -r pid ppid pgrp; do
    if [[ $ppid == "$SIDE_JOB" ]]; then
      printf '%s\n' "$pid"
    fi
  done < <(side_procs)
}

# side_trial_members: print the pid of each live process of the side's session except the job, the lock helper. These
# are the trial's processes; the helper holds the lock until they have ended.
side_trial_members() {
  local pid
  while read -r pid; do
    if [[ $pid != "$SIDE_JOB" ]]; then
      printf '%s\n' "$pid"
    fi
  done < <(side_group_pids)
}

# side_wait_group <seconds>: wait until no process of the side's session is left. The job holds the lock until it ends,
# so an empty session means the lock is free. It fails when processes remain at the bound.
side_wait_group() {
  local deadline=$((SECONDS + $1))
  while [[ -n $(side_group_pids) ]]; do
    if (( SECONDS >= deadline )); then
      return 1
    fi
    sleep 0.1
  done
}

# side_finish <exit status>: write the side's bench-lock end record, end its side record with the status, and exit.
side_finish() {
  local status=$1
  if [[ -n $SIDE_LOCKLESS_RECORD ]]; then
    bench_lock_record "$SIDE_LOCKLESS_RECORD" end || status=1
  fi
  close_side_record "$status"
  exit "$status"
}

# side_stop <signal name> <signal number>: stop the side for a signal. Each process of the trial gets the signal once,
# and the lock helper gets it once while no trial process runs. The side watches the session until it is empty; at the
# bound, it SIGKILLs the trial's processes, and the helper ends after its trial, which frees the lock. The stop line
# names the trial pid, and the side records the stop with its exit status.
side_stop() {
  local name=$1 status=$(( 128 + $2 )) trial detail pid adopted deadline escalated=0
  local -a members=()
  local -A sent=()
  # A stop runs to its end: a second signal during it does not restart the sequence or interrupt its teardown.
  trap '' INT TERM HUP
  if [[ -z $SIDE_JOB ]]; then
    # A signal can land between the job's start and the assignment of its pid. The side starts no other background
    # process, so the side's only child is then the job.
    adopted=$(pgrep -P "$$") || adopted=
    SIDE_JOB=${adopted%%$'\n'*}
  fi
  if [[ -z $SIDE_JOB ]]; then
    printf 'improve trial side stopped by %s: trial pid none; the side had no job to stop\n' "$name" >&2
    side_finish "$status"
  fi
  trial=$(side_trial_pid)
  detail='the trial ended after the signal'
  if [[ -z $trial ]]; then
    detail='the trial had not started'
  fi
  deadline=$((SECONDS + IMPROVE_TRIAL_STOP_WAIT_SECONDS))
  while :; do
    mapfile -t members < <(side_trial_members)
    if (( ${#members[@]} > 0 )); then
      for pid in "${members[@]}"; do
        if [[ -z ${sent[$pid]-} ]]; then
          sent[$pid]=1
          # A member that ended since the scan needs no signal, so the kill's failure is not an error.
          kill -s "$name" -- "$pid" 2>/dev/null
        fi
      done
    elif [[ -z ${sent[$SIDE_JOB]-} ]]; then
      sent[$SIDE_JOB]=1
      kill -s "$name" -- "$SIDE_JOB" 2>/dev/null
    fi
    if [[ -z $(side_group_pids) ]]; then
      break
    fi
    if (( SECONDS >= deadline )); then
      escalated=1
      break
    fi
    sleep 0.1
  done
  if (( escalated == 1 )); then
    detail="the trial did not end within $IMPROVE_TRIAL_STOP_WAIT_SECONDS s, so SIGKILL ended its processes"
    mapfile -t members < <(side_trial_members)
    for pid in "${members[@]}"; do
      # A member that ended since the scan needs no KILL, as in the signal loop above.
      kill -s KILL -- "$pid" 2>/dev/null
    done
  fi
  if ! side_wait_group 10; then
    printf 'improve trial side: processes of the job %s remain after the stop\n' "$SIDE_JOB" >&2
    status=1
  fi
  printf 'improve trial side stopped by %s: trial pid %s; %s\n' "$name" "${trial:-none}" "$detail" >&2
  side_finish "$status"
}

# The stop traps are set before any side work, so a signal from the first line of the side reaches side_stop.
trap 'side_stop TERM 15' TERM
trap 'side_stop INT 2' INT
trap 'side_stop HUP 1' HUP

if (( IMPROVE_TRIAL_USE_LOCK == 0 )); then
  if [[ ${IMPROVE_DIRECTION:-} != ux ]]; then
    printf 'improve trial side: IMPROVE_TRIAL_USE_LOCK=0 requires IMPROVE_DIRECTION=ux\n' >&2
    exit 2
  fi
  if [[ -z ${CHARLIEBOT_RELEASE_RUN_DIR:-} ]]; then
    printf 'improve trial side: IMPROVE_TRIAL_USE_LOCK=0 needs CHARLIEBOT_RELEASE_RUN_DIR for its record\n' >&2
    exit 2
  fi
  export TRIAL_PORT_MIN=${TRIAL_PORT_MIN:-$BESIDE_BENCH_PORT_MIN}
  export TRIAL_PORT_MAX=${TRIAL_PORT_MAX:-$BESIDE_BENCH_PORT_MAX}
  mkdir -p -- "$CHARLIEBOT_RELEASE_RUN_DIR" || exit 1
  SIDE_LOCKLESS_RECORD=$(mktemp "$CHARLIEBOT_RELEASE_RUN_DIR/bench-lock-$(date -u +%Y%m%dT%H%M%SZ).XXXXXX") || exit 1
  bench_lock_record "$SIDE_LOCKLESS_RECORD" start || exit 1
  open_side_record
  status=0
  if [[ -n $SIDE_RECORD ]]; then
    side_record_take_binary "$SIDE_RECORD" "$SIDE_BINARY" || status=$?
    if (( status == 0 )); then
      # Keep the lock in flock so a server left by failed cleanup cannot hold it from an inherited descriptor; the next
      # side must reach trial.sh's port-holder check and name the teardown defect.
      serve_lock="$CHARLIEBOT_RELEASE_RUN_DIR/.serve-side.lock"
      (
        trap - INT
        CHARLIEBOT_RELEASE_RUN_DIR=$SIDE_OUTPUT exec setsid flock -o -x "$serve_lock" "$@"
      ) &
      SIDE_JOB=$!
      wait "$SIDE_JOB" || status=$?
    fi
  else
    (
      trap - INT
      exec setsid bash -c '"$@"; exit $?' improve-trial-side "$@"
    ) &
    SIDE_JOB=$!
    wait "$SIDE_JOB" || status=$?
  fi
  side_finish "$status"
fi

if [[ ! ${IMPROVE_DIRECTION:-} =~ ^[a-z][a-z0-9_]*$ ]]; then
  printf 'improve trial side: IMPROVE_DIRECTION names the round direction, got %s\n' "${IMPROVE_DIRECTION:-}" >&2
  exit 2
fi
readonly MARKER_ROOT="/tmp/improve-$IMPROVE_DIRECTION"

# A side that ends without its EXIT trap (a SIGKILL, a stopped scope) leaves its marker behind. The marker's name
# carries the side's pid, so a marker whose pid has no process is a dead side's: the side names it and removes it. Each
# side reads only its own marker, so a dead side's marker never counts as this side's start.
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

open_side_record

status=0
# The job heads a session of its own: setsid runs a bash that loads lock.sh and calls bench_lock_within, which execs
# flock in its place, so the lock's helper is the job. The helper's child takes the record's binary line once the lock
# is held, just before the command starts, so the hash names the binary that the trial starts with, not one from before
# the wait.
(
  trap - INT
  exec setsid bash -c '. "$1"; shift; bench_lock_within "$@"' improve-trial-side-lock \
    "$SCRIPT_DIR/../poor-network/lock.sh" "$IMPROVE_TRIAL_LOCK_WAIT_SECONDS" bash -c \
    'started=$1; module=$2; record=$3; binary=$4; shift 4; : > "$started" || exit $?
     if [[ -n $record ]]; then
       source "$module" || exit $?
       side_record_take_binary "$record" "$binary" || exit $?
       export CHARLIEBOT_RELEASE_RUN_DIR=$IMPROVE_SIDE_OUTPUT
     fi
     exec "$@"' improve-trial-side "$started_marker" "$SCRIPT_DIR/side-record.sh" "$SIDE_RECORD" "$SIDE_BINARY" "$@"
) &
SIDE_JOB=$!
wait "$SIDE_JOB" || status=$?
if (( status == BENCH_LOCK_TIMEOUT_EXIT )) && [[ ! -e $started_marker ]]; then
  printf 'improve trial side stopped: the bench lock stayed held for %s s\n' "$IMPROVE_TRIAL_LOCK_WAIT_SECONDS" >&2
fi
side_finish "$status"
