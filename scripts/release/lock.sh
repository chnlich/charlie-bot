# The host release lock that scripts/release.sh and scripts/switch-server.sh hold in turn. This file is the one
# definition of the lock: one lock file per host, outside the repository, and the holder record beside it.
#
#   release_lock_take SCRIPT COMMIT   take the lock without waiting; 0 this caller holds it, 1 it does not
#   release_lock_release              remove the holder record of a lock this process took
#
# The lock is `flock` on an open descriptor, so the kernel frees it when the holder dies, and a stale record from a
# dead holder blocks nobody. The holder writes its record beside the lock: one line naming the script, the commit,
# the pid and the start time, which a refused caller prints.
#
# A stage that the release flow starts while scripts/release.sh still holds the lock — the switch rehearsal's
# switches do — inherits the flow's descriptor through CHARLIEBOT_RELEASE_LOCK_FD in its environment: it takes
# nothing of its own and releases nothing, so it cannot deadlock on its caller's lock. CHARLIEBOT_RELEASE_LOCK
# replaces the lock file's path; the tests name a path under their sandbox, and an operator never sets either
# variable by hand.

RELEASE_LOCK_FILE="${CHARLIEBOT_RELEASE_LOCK:-$HOME/charliebot-release.lock}"
RELEASE_LOCK_RECORD="$RELEASE_LOCK_FILE.holder"
# RELEASE_LOCK_MINE: this process took the lock itself and owns the record. A caller on the flow's inherited
# descriptor owns nothing.
RELEASE_LOCK_MINE=0
CHARLIEBOT_RELEASE_LOCK_FD="${CHARLIEBOT_RELEASE_LOCK_FD:-}"

# release_lock_take SCRIPT COMMIT: takes the host release lock for SCRIPT working on COMMIT, and returns 0 when this
# caller holds it. It returns 1, prints the holder's record and changes nothing when another caller holds the lock,
# and 1 with the reason when the lock cannot be taken. It never waits and never retries.
release_lock_take() {
  local script=$1 commit=$2 line
  if [ -n "$CHARLIEBOT_RELEASE_LOCK_FD" ]; then
    # The release flow that started this caller holds the lock on the inherited descriptor. A descriptor the flow
    # did not pass would run this caller unserialized, so a missing one stops it instead.
    if [ ! -e "/proc/$$/fd/$CHARLIEBOT_RELEASE_LOCK_FD" ]; then
      echo "$script: CHARLIEBOT_RELEASE_LOCK_FD=$CHARLIEBOT_RELEASE_LOCK_FD is set but not open;" \
        "the release flow that started this caller does not hold the lock" >&2
      return 1
    fi
    return 0
  fi
  command -v flock > /dev/null 2>&1 || {
    echo "$script: flock is on PATH for the host release lock" >&2
    return 1
  }
  local fd
  exec {fd}>"$RELEASE_LOCK_FILE" || {
    echo "$script: cannot open the host release lock $RELEASE_LOCK_FILE" >&2
    return 1
  }
  if ! flock -n "$fd"; then
    echo "$script: the host release lock $RELEASE_LOCK_FILE is held:" >&2
    if [ -e "$RELEASE_LOCK_RECORD" ]; then
      while IFS= read -r line; do
        echo "$script:   $line" >&2
      done < "$RELEASE_LOCK_RECORD"
    else
      echo "$script:   (the holder wrote no record)" >&2
    fi
    exec {fd}>&-
    return 1
  fi
  printf 'script=%s commit=%s pid=%s started=%s\n' "$script" "$commit" "$$" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
    > "$RELEASE_LOCK_RECORD"
  CHARLIEBOT_RELEASE_LOCK_FD=$fd
  export CHARLIEBOT_RELEASE_LOCK_FD
  RELEASE_LOCK_MINE=1
  return 0
}

# release_lock_release: removes the holder record of a lock this process took. The lock itself stays with the open
# descriptor until the process exits; a caller on the flow's inherited descriptor releases nothing, because the flow
# root that took the lock removes its record at its own exit.
release_lock_release() {
  if [ "$RELEASE_LOCK_MINE" = 1 ]; then
    rm -f "$RELEASE_LOCK_RECORD"
  fi
}
